//! Presenting the BIOS's text screen (ADR-0012).
//!
//! A `mode = "bios"` machine starts in VGA text mode 3, and until the machine
//! has a VGA that is nothing but guest RAM at `B800:0000`: the BIOS writes
//! characters there and so does the loader it started, with no exit either
//! way. So, like the boot framebuffer's presenter, a thread looks: it reads
//! the 4000 bytes of the active page and the cursor position out of the BIOS
//! data area, and when either changed it draws the page with
//! `pc_bios::font` and hands the display the picture.
//!
//! It yields to virtio-gpu while a guest driver has a scanout bound, and it
//! takes the VM's pause gate (ADR-0005) before each read.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use pc_bios::font;
use virtio_core::{GuestMem, Quiesce};

/// How often the screen is looked at. Text does not need the refresh rate.
const PERIOD: Duration = Duration::from_millis(33);

/// The presenter thread; stopped and joined on drop.
pub(super) struct Presenter {
    stop: Arc<AtomicBool>,
    quiesce: Arc<Quiesce>,
    thread: Option<JoinHandle<()>>,
}

impl Presenter {
    pub(super) fn start(
        mem: Arc<GuestMem>,
        display: display::DisplayHandle,
        quiesce: Arc<Quiesce>,
        virtio_scanout: Option<Arc<AtomicBool>>,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            let quiesce = Arc::clone(&quiesce);
            std::thread::Builder::new()
                .name("bios-text-present".into())
                .spawn(move || run(&mem, &display, &quiesce, &stop, virtio_scanout.as_deref()))?
        };
        Ok(Self {
            stop,
            quiesce,
            thread: Some(thread),
        })
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.quiesce.wake();
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                tracing::error!("the text screen presenter panicked");
            }
        }
    }
}

/// A page of text and where its cursor was drawn.
type Shown = (Vec<u8>, Option<(usize, usize)>);

fn run(
    mem: &GuestMem,
    display: &display::DisplayHandle,
    quiesce: &Quiesce,
    stop: &AtomicBool,
    virtio_scanout: Option<&AtomicBool>,
) {
    let keep_going = || !stop.load(Ordering::Acquire);
    let glyphs = font::Font::default();
    let mut cells = Vec::new();
    // What the display shows now: nothing, until the first frame.
    let mut shown: Option<Shown> = None;
    let mut pixels = Vec::new();
    while keep_going() {
        std::thread::sleep(PERIOD);
        let Some(_pass) = quiesce.wait_while_paused(keep_going) else {
            break;
        };
        if virtio_scanout.is_some_and(|bound| bound.load(Ordering::Acquire)) {
            // virtio-gpu has the display; when it lets go, draw afresh.
            shown = None;
            continue;
        }
        let cursor = pc_bios::read_text_screen(mem, &mut cells);
        if shown
            .as_ref()
            .is_some_and(|(text, at)| *text == cells && *at == cursor)
        {
            continue;
        }
        font::render(&glyphs, &cells, cursor, &mut pixels);
        let sent = (|| {
            if display.resolution() != (font::WIDTH, font::HEIGHT) {
                display.set_resolution(font::WIDTH, font::HEIGHT)?;
            }
            display.update_scanout(0, 0, font::WIDTH, font::HEIGHT, &pixels)
        })();
        match sent {
            Ok(()) => shown = Some((cells.clone(), cursor)),
            Err(error) => tracing::warn!(%error, "text screen not presented"),
        }
    }
}
