//! Manual verification harness for the host display stack (EPIC 7 + host half
//! of EPIC 9). Nothing here reaches inside the crate: the "guest" is a plain
//! thread driving [`display::DisplayHandle`], exactly like the future
//! `virtio-gpu` device will.
//!
//! ```bash
//! cargo run --release -p display --example demo
//! RUST_LOG=display=debug,demo=debug cargo run --release -p display --example demo
//! ```
//!
//! What to check in the window:
//!
//! - the gradient scrolls and the frame counter ticks (partial updates land);
//! - resizing keeps the 16:9 image centered with black letterbox bars, scaled
//!   with linear filtering, and cannot go below 640x360;
//! - minimizing stops presenting and restoring resumes it without artifacts;
//! - the title says `[click to grab input]` and no keystroke reaches the "guest"
//!   until you click on the image; then it says
//!   `[input grabbed, Ctrl+Alt releases]` and the host cursor disappears over
//!   the image but comes back over the black bars (WIN-1501);
//! - `Ctrl+Alt` (nothing else in between) releases the grab and the cursor;
//!   `Ctrl+Alt+G` toggles it explicitly (WIN-1502);
//! - while grabbed: `S` writes `entangled-screenshot-<frame>.png` into the
//!   working directory, `R` cycles the guest resolution
//!   (1920x1080 → 1280x720 → 800x600), `Esc` quits;
//! - `F11` toggles borderless fullscreen and `Ctrl+Alt+O` toggles 1:1 pixel mode
//!   (title gains ` — 1:1`); neither appears in the guest input stream
//!   (WIN-1504);
//! - `Ctrl+Alt+P` and `Ctrl+Alt+R` ask the VM supervisor to pause or reboot
//!   (ADR-0005); this demo has no VM behind it and only logs them.
//! - `Ctrl+Alt+Q` quits — like the other reserved shortcuts it never shows up in
//!   the drained guest input stream;
//! - FPS and copy statistics appear once per second on `display=info`.
//!
//! `expect()` at init time is deliberate and confined to this binary; the
//! library itself never unwraps on a runtime path.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use display::{ControlEvent, DisplayConfig, DisplayHandle, DisplayHost};
use virtio_input::{ev, InputEvent};

/// Linux `KEY_*` codes the demo reacts to in the guest input stream.
const KEY_S: u16 = 31;
const KEY_R: u16 = 19;
const KEY_ESC: u16 = 1;

/// Guest resolutions cycled by `R`.
const MODES: [(u32, u32); 3] = [(1920, 1080), (1280, 720), (800, 600)];

fn main() -> Result<(), display::DisplayError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let host =
        DisplayHost::new(DisplayConfig::default())?.with_title("Entangled Desktop display demo");
    tracing::info!(
        "click the image to grab input; Ctrl+Alt releases it, F11 fullscreen, \
         Ctrl+Alt+O 1:1, Ctrl+Alt+P / Ctrl+Alt+R are lifecycle requests (no VM here), \
         Ctrl+Alt+Q quits"
    );
    let handle = host.handle();
    let input = host.input_queue();
    let control = host.control_queue();
    let running = Arc::new(AtomicBool::new(true));

    let guest = {
        let running = Arc::clone(&running);
        std::thread::Builder::new()
            .name("fake-guest".to_owned())
            .spawn(move || guest_thread(handle, input, control, &running))
            .expect("spawning the demo guest thread")
    };

    let result = host.run();
    running.store(false, Ordering::Release);
    let _ = guest.join();
    result
}

/// Stands in for the guest + `virtio-gpu` + `virtio-input` devices.
fn guest_thread(
    handle: DisplayHandle,
    input: display::InputQueue,
    control: display::ControlQueue,
    running: &AtomicBool,
) {
    let frame_budget = Duration::from_millis(16);
    let mut painter = Painter::new(MODES[0].0, MODES[0].1);
    let mut mode = 0usize;
    let mut frame: u64 = 0;
    let mut screenshots = 0u32;
    let started = Instant::now();

    while running.load(Ordering::Acquire) {
        let frame_start = Instant::now();

        // A real device would push the guest's own pixels here; the shape of the
        // calls is the same: several partial updates, then a redraw.
        if let Err(err) = painter.paint(&handle, frame) {
            tracing::error!(%err, "scanout update rejected");
        }
        handle.request_redraw();

        // Drain the captured input stream, standing in for virtio-input.
        for event in input.drain() {
            if event.event_type == ev::KEY && event.value == 1 {
                match event.code {
                    KEY_S => {
                        screenshots += 1;
                        let path = format!("entangled-screenshot-{screenshots:03}.png");
                        match handle.screenshot(&path) {
                            Ok(()) => tracing::info!(path, "screenshot saved"),
                            Err(err) => tracing::error!(%err, "screenshot failed"),
                        }
                    }
                    KEY_R => {
                        mode = (mode + 1) % MODES.len();
                        let (w, h) = MODES[mode];
                        if let Err(err) = handle.set_resolution(w, h) {
                            tracing::error!(%err, "resolution change rejected");
                        } else {
                            painter = Painter::new(w, h);
                        }
                    }
                    KEY_ESC => {
                        tracing::info!("Esc: quitting");
                        handle.shutdown();
                        return;
                    }
                    _ => {}
                }
            }
            log_pointer(&event);
        }

        for event in control.drain() {
            match event {
                ControlEvent::QuitRequested | ControlEvent::WindowCloseRequested => {
                    tracing::info!(?event, "guest side asked to stop");
                    handle.shutdown();
                    return;
                }
                ControlEvent::GrabToggled(grabbed) => {
                    tracing::info!(grabbed, "host toggled the pointer grab");
                }
                // This demo has no VM behind it, so there is nothing to
                // freeze, reboot or write to a file; logging them proves the
                // shortcuts reach a supervisor at all (ADR-0005, ADR-0006).
                ControlEvent::PauseToggleRequested
                | ControlEvent::ResetRequested
                | ControlEvent::SaveRequested => {
                    tracing::info!(?event, "lifecycle request (no VM behind this demo)");
                }
            }
        }

        frame += 1;
        if frame % 300 == 0 {
            let stats = handle.stats();
            tracing::debug!(
                frame,
                secs = format_args!("{:.1}", started.elapsed().as_secs_f64()),
                guest_updates = stats.updates,
                guest_mib = format_args!("{:.1}", stats.bytes as f64 / (1024.0 * 1024.0)),
                "fake guest progress"
            );
        }
        if let Some(rest) = frame_budget.checked_sub(frame_start.elapsed()) {
            std::thread::sleep(rest);
        }
    }
}

fn log_pointer(event: &InputEvent) {
    if event.event_type == ev::ABS {
        tracing::trace!(code = event.code, value = event.value, "guest pointer");
    }
}

/// Draws the animated test pattern: a scrolling gradient plus a frame counter,
/// pushed as several partial updates per frame.
struct Painter {
    width: u32,
    height: u32,
    /// One horizontal strip of pixels, reused between updates.
    strip: Vec<u8>,
    strip_height: u32,
    /// Scratch buffer for the frame-counter overlay.
    counter: Vec<u8>,
}

impl Painter {
    /// Height of the counter overlay: 7 font rows scaled by [`Self::GLYPH_SCALE`].
    const GLYPH_SCALE: u32 = 6;
    const COUNTER_DIGITS: u32 = 8;
    const STRIPS: u32 = 4;

    fn new(width: u32, height: u32) -> Self {
        let strip_height = height.div_ceil(Self::STRIPS).max(1);
        let counter_w = Self::COUNTER_DIGITS * 6 * Self::GLYPH_SCALE;
        let counter_h = 7 * Self::GLYPH_SCALE;
        Self {
            width,
            height,
            strip: vec![0; (width * strip_height * 4) as usize],
            strip_height,
            counter: vec![0; (counter_w.min(width) * counter_h.min(height) * 4) as usize],
        }
    }

    fn paint(&mut self, handle: &DisplayHandle, frame: u64) -> Result<(), display::DisplayError> {
        let phase = (frame % 256) as u32;
        // Partial update #1..#4: the gradient, one horizontal strip at a time.
        for strip in 0..Self::STRIPS {
            let y = strip * self.strip_height;
            if y >= self.height {
                break;
            }
            let rows = self.strip_height.min(self.height - y);
            let width = self.width;
            for row in 0..rows {
                let global_y = y + row;
                let row_start = (row * width * 4) as usize;
                let Some(row_bytes) = self
                    .strip
                    .get_mut(row_start..row_start + (width * 4) as usize)
                else {
                    continue;
                };
                for (x, px) in row_bytes.chunks_exact_mut(4).enumerate() {
                    let x = x as u32;
                    // BGRA: a diagonal gradient that scrolls with the frame.
                    let b = ((x + phase * 3) % 256) as u8;
                    let g = ((global_y + phase * 2) % 256) as u8;
                    let r = ((x + global_y + phase * 5) % 256) as u8;
                    px.copy_from_slice(&[b, g, r, 0xff]);
                }
            }
            // Grid lines every 128 px make scaling and letterboxing obvious.
            for row in 0..rows {
                let global_y = y + row;
                if global_y % 128 != 0 {
                    continue;
                }
                let row_start = (row * width * 4) as usize;
                if let Some(row_bytes) = self
                    .strip
                    .get_mut(row_start..row_start + (width * 4) as usize)
                {
                    for px in row_bytes.chunks_exact_mut(4) {
                        px.copy_from_slice(&[0xff, 0xff, 0xff, 0xff]);
                    }
                }
            }
            handle.update_scanout(0, y, width, rows, &self.strip)?;
        }

        // Partial update #5: the frame counter, a tiny rect on its own.
        self.paint_counter(handle, frame)
    }

    fn paint_counter(
        &mut self,
        handle: &DisplayHandle,
        frame: u64,
    ) -> Result<(), display::DisplayError> {
        let scale = Self::GLYPH_SCALE;
        let glyph_w = 6 * scale; // 5 px glyph + 1 px gap
        let width = (Self::COUNTER_DIGITS * glyph_w).min(self.width);
        let height = (7 * scale).min(self.height);
        if width == 0 || height == 0 {
            return Ok(());
        }
        let needed = (width * height * 4) as usize;
        if self.counter.len() < needed {
            self.counter.resize(needed, 0);
        }
        // Dark background so the digits stay readable over the gradient.
        for px in self.counter[..needed].chunks_exact_mut(4) {
            px.copy_from_slice(&[0x20, 0x10, 0x10, 0xff]);
        }
        let text = format!("{:>width$}", frame, width = Self::COUNTER_DIGITS as usize);
        for (index, ch) in text.chars().enumerate() {
            let glyph = glyph(ch);
            let x0 = index as u32 * glyph_w;
            for (row, bits) in glyph.iter().enumerate() {
                for col in 0..5u32 {
                    if bits & (1 << (4 - col)) == 0 {
                        continue;
                    }
                    for dy in 0..scale {
                        for dx in 0..scale {
                            let x = x0 + col * scale + dx;
                            let y = row as u32 * scale + dy;
                            if x >= width || y >= height {
                                continue;
                            }
                            let off = ((y * width + x) * 4) as usize;
                            if let Some(px) = self.counter.get_mut(off..off + 4) {
                                px.copy_from_slice(&[0x40, 0xff, 0x40, 0xff]);
                            }
                        }
                    }
                }
            }
        }
        handle.update_scanout(0, 0, width, height, &self.counter[..needed])
    }
}

/// 5x7 bitmap font, digits only (plus a blank for padding).
fn glyph(ch: char) -> [u8; 7] {
    match ch {
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        '3' => [
            0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        '6' => [
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
        ],
        _ => [0; 7],
    }
}
