//! Presenting a BIOS machine's text screen (ADR-0012).
//!
//! A `mode = "bios"` machine starts in VGA text mode 3, and text mode is
//! guest RAM at `B800:0000`: the BIOS writes characters there and so do the
//! loader and the operating system after it, with no exit either way. So,
//! like the boot framebuffer's presenter, a thread looks.
//!
//! What it draws comes from whoever owns the display at that moment:
//!
//! - until the guest programs the VGA, the BIOS: the active page and the
//!   cursor from the BIOS data area, in the BIOS's own font and colours;
//! - once it has (`machine_x86::vga`), the VGA's registers: the geometry,
//!   the cursor from the CRT controller, the palette through the attribute
//!   controller and the DAC, and the font the guest loaded.
//!
//! A graphics mode is drawn from the VGA's planes, when the host can trap
//! the display memory (`machine_x86::vga`); otherwise the last text frame
//! stays up.
//!
//! It yields to virtio-gpu while a guest driver has a scanout bound, and it
//! takes the VM's pause gate (ADR-0005) before each read.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use machine_x86::vga::{TextMode, Vga};
use pc_bios::font;
use virtio_core::{GuestMem, Quiesce};
use vm_memory::{Bytes, GuestAddress};

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
        vga: Option<Arc<Mutex<Vga>>>,
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
                .spawn(move || {
                    run(
                        &mem,
                        vga.as_deref(),
                        &display,
                        &quiesce,
                        &stop,
                        virtio_scanout.as_deref(),
                    )
                })?
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

/// Everything one frame is drawn from; two equal ones draw the same picture.
#[derive(PartialEq, Eq, Default)]
struct Source {
    cells: Vec<u8>,
    /// The BIOS's cursor, when the BIOS owns the display.
    bios_cursor: Option<(usize, usize)>,
    /// The VGA's text mode, when the guest has programmed it.
    vga: Option<TextMode>,
    /// The guest's font, when it loaded one: 256 glyphs of 32 bytes.
    font: Vec<u8>,
}

/// Who owns the display now.
enum Owner {
    Bios,
    /// Text mode as the VGA describes it, and the font the guest loaded
    /// through the planes, if it did.
    Vga(TextMode, Option<Vec<u8>>),
    /// The guest programmed a graphics mode.
    Graphics,
}

fn owner(vga: Option<&Mutex<Vga>>) -> Owner {
    let Some(Ok(vga)) = vga.map(|vga| vga.lock()) else {
        return Owner::Bios;
    };
    if !vga.programmed() {
        return Owner::Bios;
    }
    match vga.text_mode() {
        Some(mode) => {
            let font = vga.font_plane().map(<[u8]>::to_vec);
            Owner::Vga(mode, font)
        }
        None => Owner::Graphics,
    }
}

/// Hands the display one whole picture.
fn present(
    display: &display::DisplayHandle,
    width: u32,
    height: u32,
    pixels: &[u8],
) -> Result<(), display::DisplayError> {
    if display.resolution() != (width, height) {
        display.set_resolution(width, height)?;
    }
    display.update_scanout(0, 0, width, height, pixels)
}

fn run(
    mem: &GuestMem,
    vga: Option<&Mutex<Vga>>,
    display: &display::DisplayHandle,
    quiesce: &Quiesce,
    stop: &AtomicBool,
    virtio_scanout: Option<&AtomicBool>,
) {
    let keep_going = || !stop.load(Ordering::Acquire);
    let bios_glyphs = font::Font::default();
    // What the display shows now: nothing, until the first frame.
    let mut shown: Option<Source> = None;
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
        let mut source = Source::default();
        match owner(vga) {
            Owner::Graphics => {
                // Drawn from the planes, under the VGA's lock, and only when
                // something changed (or the last frame was text).
                if let Some(Ok(mut vga)) = vga.map(|vga| vga.lock()) {
                    if vga.take_dirty() || shown.is_some() {
                        if let Some((width, height)) = vga.render_graphics(&mut pixels) {
                            drop(vga);
                            match present(display, width, height, &pixels) {
                                Ok(()) => shown = None,
                                Err(error) => {
                                    tracing::warn!(%error, "graphics screen not presented");
                                }
                            }
                        }
                    }
                }
                continue;
            }
            Owner::Bios => {
                source.bios_cursor = pc_bios::read_text_screen(mem, &mut source.cells);
            }
            Owner::Vga(mode, loaded) => {
                source.cells.resize(mode.columns * mode.rows * 2, 0);
                let _ = mem.read_slice(&mut source.cells, GuestAddress(mode.cells_at));
                if mode.guest_font {
                    match loaded {
                        Some(loaded) => source.font = loaded,
                        // No planes on this host: the load went to the RAM
                        // where plane 2 would have been mapped.
                        None => {
                            source.font.resize(font::GLYPH_PLANE_BYTES, 0);
                            let _ = mem.read_slice(&mut source.font, GuestAddress(0x000a_0000));
                        }
                    }
                }
                source.vga = Some(mode);
            }
        }
        if shown.as_ref() == Some(&source) {
            continue;
        }
        let (width, height) = match &source.vga {
            None => {
                font::render(&bios_glyphs, &source.cells, source.bios_cursor, &mut pixels);
                (font::WIDTH, font::HEIGHT)
            }
            Some(mode) => font::render_frame(
                &font::TextFrame {
                    cells: &source.cells,
                    columns: mode.columns,
                    rows: mode.rows,
                    cell_height: mode.cell_height,
                    glyphs: if source.font.is_empty() {
                        bios_glyphs.plane()
                    } else {
                        &source.font
                    },
                    palette: &mode.palette,
                    blink: mode.blink,
                    cursor: mode
                        .cursor
                        .map(|c| (c.column, c.row, c.first_line, c.last_line)),
                },
                &mut pixels,
            ),
        };
        match present(display, width, height, &pixels) {
            Ok(()) => shown = Some(source),
            Err(error) => tracing::warn!(%error, "text screen not presented"),
        }
    }
}
