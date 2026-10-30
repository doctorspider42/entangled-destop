//! Presenting the boot framebuffer (ADR-0008 §1).
//!
//! virtio-gpu tells the host when the guest drew: `RESOURCE_FLUSH` carries
//! the rect. The boot framebuffer has nothing of the kind. The guest writes
//! VRAM, which is host memory mapped into it, and no exit happens. So this
//! thread looks at the display's refresh rate: it reads the visible rows,
//! compares them with what it sent last, and hands the display only the
//! band of rows that changed. An idle desktop costs one VRAM read and
//! compare per frame, and sends nothing.
//!
//! The VRAM is the guest's own 32 bpp XRGB, which is the display's BGRA in
//! byte order. The X byte is not alpha, so every row sent gets an opaque
//! alpha.
//!
//! The thread takes the VM's pause gate (ADR-0005) before each read. It
//! only reads, so a paused VM loses nothing by it, but the rule is that no
//! host thread touches guest-visible memory while the VM is paused.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use control_api::{DisplayAdapter, VmConfig};
use machine_x86::framebuffer::{Framebuffer, FramebufferConfig, ScanoutMode};
use virtio_core::Quiesce;

/// What the machine attaches for `cfg`, or `None` when the VM has no boot
/// framebuffer. The EDID's preferred timing is the profile's resolution, which
/// is the mode the firmware's GOP and so Windows' `BasicDisplay` come up in.
pub(super) fn config_for(cfg: &VmConfig) -> Option<FramebufferConfig> {
    if cfg.display.adapter != DisplayAdapter::Framebuffer {
        return None;
    }
    let edid = virtio_gpu::edid::edid_block(
        cfg.display.width,
        cfg.display.height,
        cfg.display.refresh_hz,
    );
    if edid.is_none() {
        tracing::warn!(
            width = cfg.display.width,
            height = cfg.display.height,
            "the resolution has no EDID encoding; the firmware picks its own mode"
        );
    }
    Some(FramebufferConfig {
        edid,
        ..FramebufferConfig::default()
    })
}

/// The presenter thread; stopped and joined on drop.
pub(super) struct Presenter {
    stop: Arc<AtomicBool>,
    quiesce: Arc<Quiesce>,
    thread: Option<JoinHandle<()>>,
}

impl Presenter {
    pub(super) fn start(
        device: Arc<Framebuffer>,
        display: display::DisplayHandle,
        quiesce: Arc<Quiesce>,
        refresh_hz: u32,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let period = Duration::from_secs(1) / refresh_hz.max(1);
        let thread = {
            let stop = Arc::clone(&stop);
            let quiesce = Arc::clone(&quiesce);
            std::thread::Builder::new()
                .name("framebuffer-present".into())
                .spawn(move || run(&device, &display, &quiesce, &stop, period))?
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
                tracing::error!("the framebuffer presenter panicked");
            }
        }
    }
}

fn run(
    device: &Framebuffer,
    display: &display::DisplayHandle,
    quiesce: &Quiesce,
    stop: &AtomicBool,
    period: Duration,
) {
    let keep_going = || !stop.load(Ordering::Acquire);
    let mut frame = Frame::default();
    let mut next = Instant::now();
    while keep_going() {
        next += period;
        let now = Instant::now();
        match next.checked_duration_since(now) {
            Some(wait) => std::thread::sleep(wait),
            // Behind (a paused VM, a busy host): start again from now rather
            // than presenting a burst to catch up.
            None => next = now,
        }
        let Some(_pass) = quiesce.wait_while_paused(keep_going) else {
            break;
        };
        let Some(mode) = device.scanout() else {
            continue;
        };
        if let Err(error) = frame.present(device, display, mode) {
            tracing::warn!(%error, "framebuffer frame not presented");
        }
    }
}

/// The last frame sent, to diff the next one against.
#[derive(Default)]
struct Frame {
    mode: Option<ScanoutMode>,
    sent: Vec<u8>,
    next: Vec<u8>,
}

impl Frame {
    fn present(
        &mut self,
        device: &Framebuffer,
        display: &display::DisplayHandle,
        mode: ScanoutMode,
    ) -> Result<(), String> {
        let width = mode.width as usize;
        let height = mode.height as usize;
        let row = width * 4;
        if self.mode != Some(mode) {
            if display.resolution() != (mode.width, mode.height) {
                display
                    .set_resolution(mode.width, mode.height)
                    .map_err(|e| e.to_string())?;
            }
            self.mode = Some(mode);
            // Nothing sent in this mode yet: every row differs from this.
            self.sent.clear();
        }
        self.next.resize(row * height, 0);
        for (y, out) in self.next.chunks_exact_mut(row).enumerate() {
            let at = mode.offset + y as u64 * u64::from(mode.stride);
            device.read_vram(at, out).map_err(|e| e.to_string())?;
            for pixel in out.chunks_exact_mut(4) {
                pixel[3] = 0xff;
            }
        }
        let Some((first, last)) = changed_rows(&self.sent, &self.next, row) else {
            return Ok(());
        };
        let band = &self.next[first * row..(last + 1) * row];
        display
            .update_scanout(0, first as u32, mode.width, (last - first + 1) as u32, band)
            .map_err(|e| e.to_string())?;
        std::mem::swap(&mut self.sent, &mut self.next);
        Ok(())
    }
}

/// The first and last row where `next` differs from `sent`, or `None` when
/// nothing changed. A `sent` of another size differs everywhere.
fn changed_rows(sent: &[u8], next: &[u8], row: usize) -> Option<(usize, usize)> {
    if row == 0 || next.is_empty() {
        return None;
    }
    let rows = next.len() / row;
    if sent.len() != next.len() {
        return Some((0, rows - 1));
    }
    let differs = |y: usize| sent[y * row..(y + 1) * row] != next[y * row..(y + 1) * row];
    let first = (0..rows).find(|&y| differs(y))?;
    let last = (first..rows).rev().find(|&y| differs(y)).unwrap_or(first);
    Some((first, last))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_changed_sends_nothing() {
        let a = vec![1u8; 4 * 4 * 3];
        assert_eq!(changed_rows(&a, &a, 16), None);
    }

    #[test]
    fn the_changed_band_is_first_to_last_differing_row() {
        let sent = vec![0u8; 16 * 5];
        let mut next = sent.clone();
        next[16 + 3] = 9;
        next[3 * 16] = 9;
        assert_eq!(changed_rows(&sent, &next, 16), Some((1, 3)));
    }

    #[test]
    fn a_first_frame_is_sent_whole() {
        let next = vec![0u8; 16 * 5];
        assert_eq!(changed_rows(&[], &next, 16), Some((0, 4)));
    }

    #[test]
    fn only_the_framebuffer_adapter_gets_a_device() {
        let mut cfg: VmConfig = VmConfig::from_toml(
            "name = \"t\"\nmemory_mib = 512\nvcpus = 1\ntransport = \"pci\"\n\
             [boot]\nmode = \"uefi\"\nfirmware = \"CLOUDHV.fd\"\n",
        )
        .expect("minimal config");
        assert!(config_for(&cfg).is_none());
        cfg.display.adapter = DisplayAdapter::Framebuffer;
        let config = config_for(&cfg).expect("a framebuffer");
        let edid = config.edid.expect("1920x1080 has an EDID");
        assert_eq!(&edid[..8], &[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
    }
}
