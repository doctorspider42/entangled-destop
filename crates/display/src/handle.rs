//! The device-facing half of the display: a cloneable, `Send` handle that the
//! future `virtio-gpu` device thread uses to publish scanout updates.
//!
//! # Why a shared mirror plus a wakeup, not a channel of pixels
//!
//! `TRANSFER_TO_HOST_2D` hands the host a rect of guest pixels; `RESOURCE_FLUSH`
//! asks for it to be shown. Sending those bytes down a channel would allocate a
//! copy per transfer and let a fast guest grow an unbounded queue in the host.
//! Instead the pixels land in one shared [`Scanout`] mirror (the critical
//! section is a `memcpy`) whose dirty rects coalesce naturally, and only a
//! coalesced *wakeup* is sent to the event loop. The event loop therefore never
//! waits on guest state, and a guest that transfers 1000 rects between two
//! frames costs one upload, not 1000.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use winit::event_loop::EventLoopProxy;

use crate::scanout::{lock_scanout, Scanout, ScanoutStats, SharedScanout};
use crate::DisplayError;

/// Wakeups the event loop understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostEvent {
    /// The scanout changed (or someone asked for a redraw).
    Redraw,
    /// Tear the window down and return from [`crate::DisplayHost::run`].
    Shutdown,
}

/// Coalescing wakeup channel to the event loop. Detached wakers (no proxy) are
/// what [`DisplayHandle::detached`] uses.
#[derive(Debug, Clone)]
pub(crate) struct Waker {
    proxy: Option<EventLoopProxy<HostEvent>>,
    pending: Arc<AtomicBool>,
}

impl Waker {
    pub(crate) fn new(proxy: EventLoopProxy<HostEvent>) -> Self {
        Self {
            proxy: Some(proxy),
            pending: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn detached() -> Self {
        Self {
            proxy: None,
            pending: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Requests a redraw. Repeated calls before the event loop gets around to
    /// drawing collapse into a single wakeup.
    pub(crate) fn wake(&self) {
        let Some(proxy) = &self.proxy else {
            return;
        };
        if self
            .pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        if proxy.send_event(HostEvent::Redraw).is_err() {
            // The window is gone; the device side is not an error path.
            self.pending.store(false, Ordering::Release);
            tracing::trace!("redraw wakeup dropped: event loop has exited");
        }
    }

    /// Marks the pending wakeup consumed; called by the event loop per frame.
    pub(crate) fn clear(&self) {
        self.pending.store(false, Ordering::Release);
    }

    /// Asks the event loop to exit.
    pub(crate) fn shutdown(&self) {
        let Some(proxy) = &self.proxy else {
            return;
        };
        if proxy.send_event(HostEvent::Shutdown).is_err() {
            tracing::trace!("shutdown request dropped: event loop has exited");
        }
    }
}

/// The device-facing display API (backlog MVP-703/704/707).
///
/// Cloneable and `Send`: hand one to the `virtio-gpu` device thread, keep one
/// for the VM supervisor. Every method is non-blocking apart from the scanout
/// mutex, which is only ever held for a `memcpy`.
#[derive(Debug, Clone)]
pub struct DisplayHandle {
    scanout: SharedScanout,
    waker: Waker,
}

impl DisplayHandle {
    pub(crate) fn new(scanout: SharedScanout, waker: Waker) -> Self {
        Self { scanout, waker }
    }

    /// A handle with no window behind it: updates and screenshots work, wakeups
    /// go nowhere.
    ///
    /// This is the headless path for graphical acceptance tests (golden-image
    /// comparison against [`DisplayHandle::screenshot_png`], see the
    /// `vm-testing` skill) and for unit tests of guest-facing code that must not
    /// open a window.
    pub fn detached(width: u32, height: u32) -> Result<Self, DisplayError> {
        let scanout = Arc::new(Mutex::new(Scanout::new(width, height)?));
        Ok(Self::new(scanout, Waker::detached()))
    }

    /// Copies a `width`×`height` block of tightly packed BGRA pixels
    /// ([`virtio_gpu::FORMAT_B8G8R8A8_UNORM`]) into the scanout at (`x`, `y`)
    /// and asks for a redraw — the host equivalent of
    /// `TRANSFER_TO_HOST_2D` + `RESOURCE_FLUSH` (backlog MVP-704).
    ///
    /// Rejects rects that leave the scanout and pixel buffers that are too
    /// short; a rejected update changes nothing.
    pub fn update_scanout(
        &self,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        data: &[u8],
    ) -> Result<(), DisplayError> {
        {
            let mut scanout = lock_scanout(&self.scanout);
            scanout.update(x, y, width, height, data)?;
        }
        self.waker.wake();
        Ok(())
    }

    /// Changes the guest resolution, reallocating the mirror and (on the next
    /// frame) the GPU texture. The window keeps its size; the image is
    /// re-letterboxed.
    pub fn set_resolution(&self, width: u32, height: u32) -> Result<(), DisplayError> {
        {
            let mut scanout = lock_scanout(&self.scanout);
            scanout.set_resolution(width, height)?;
        }
        tracing::info!(width, height, "guest scanout resolution changed");
        self.waker.wake();
        Ok(())
    }

    /// Asks the event loop to present the current scanout.
    pub fn request_redraw(&self) {
        self.waker.wake();
    }

    /// Asks the event loop to close the window and return from
    /// [`crate::DisplayHost::run`].
    pub fn shutdown(&self) {
        self.waker.shutdown();
    }

    /// Current guest resolution.
    pub fn resolution(&self) -> (u32, u32) {
        lock_scanout(&self.scanout).size()
    }

    /// Copy statistics of the scanout mirror.
    pub fn stats(&self) -> ScanoutStats {
        lock_scanout(&self.scanout).stats()
    }

    /// Encodes the current scanout as a PNG (backlog MVP-707).
    ///
    /// Reads the CPU mirror, so it works with the window minimized, before the
    /// first frame, and from any thread. The pixels are copied out under the
    /// lock and encoded outside it.
    pub fn screenshot_png(&self) -> Result<Vec<u8>, DisplayError> {
        let snapshot = self.snapshot()?;
        snapshot.to_png()
    }

    /// Writes a PNG screenshot of the current scanout to `path`.
    pub fn screenshot(&self, path: impl AsRef<std::path::Path>) -> Result<(), DisplayError> {
        let path = path.as_ref();
        let snapshot = self.snapshot()?;
        snapshot.write_png(path)?;
        let (width, height) = snapshot.size();
        tracing::info!(path = %path.display(), width, height, "screenshot written");
        Ok(())
    }

    /// The shared mirror, for a device that wants to write pixels in place
    /// instead of handing them over as a slice. Callers must mark their rects
    /// dirty via [`Scanout::update`] or [`Scanout::mark_all_dirty`] and then
    /// call [`DisplayHandle::request_redraw`].
    pub fn scanout(&self) -> SharedScanout {
        Arc::clone(&self.scanout)
    }

    /// Detached copy of the current scanout; the lock is released before the
    /// caller does anything slow with it.
    fn snapshot(&self) -> Result<Scanout, DisplayError> {
        let (width, height, pixels) = {
            let scanout = lock_scanout(&self.scanout);
            let (w, h) = scanout.size();
            (w, h, scanout.pixels().to_vec())
        };
        let mut snapshot = Scanout::new(width, height)?;
        snapshot.update(0, 0, width, height, &pixels)?;
        Ok(snapshot)
    }
}

/// The device-facing contract `virtio-gpu` drives (backlog EPIC 8): the GPU
/// device only ever sees these three methods, which is what keeps `virtio-gpu`
/// free of any dependency on `winit`, `wgpu` — or this crate.
impl virtio_gpu::ScanoutSink for DisplayHandle {
    fn resolution(&self) -> (u32, u32) {
        DisplayHandle::resolution(self)
    }

    fn set_resolution(&self, width: u32, height: u32) -> Result<(), virtio_gpu::SinkError> {
        DisplayHandle::set_resolution(self, width, height).map_err(sink_error)
    }

    fn update_scanout(
        &self,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        data: &[u8],
    ) -> Result<(), virtio_gpu::SinkError> {
        DisplayHandle::update_scanout(self, x, y, width, height, data).map_err(sink_error)
    }
}

/// A display rejection as the GPU device sees it: a message it turns into
/// `VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER`. The typed [`DisplayError`] stays on
/// this side of the seam.
fn sink_error(error: DisplayError) -> virtio_gpu::SinkError {
    virtio_gpu::SinkError::new(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use virtio_gpu::ScanoutSink;

    #[test]
    fn the_handle_is_a_scanout_sink() {
        let handle = DisplayHandle::detached(4, 2).expect("detached handle");
        let sink: &dyn ScanoutSink = &handle;
        assert_eq!(sink.resolution(), (4, 2));
        sink.update_scanout(0, 0, 1, 1, &[1, 2, 3, 4])
            .expect("in-bounds update");
        let error = sink
            .update_scanout(9, 9, 1, 1, &[1, 2, 3, 4])
            .expect_err("out-of-bounds update is refused");
        assert!(error.to_string().contains("9"), "{error}");
        sink.set_resolution(8, 8).expect("resolution change");
        assert_eq!(sink.resolution(), (8, 8));
        assert!(sink.set_resolution(0, 8).is_err());
    }

    #[test]
    fn detached_handle_accepts_updates_and_screenshots() {
        let handle = DisplayHandle::detached(4, 2).unwrap();
        assert_eq!(handle.resolution(), (4, 2));
        handle.request_redraw();
        handle.shutdown();
        handle.update_scanout(0, 0, 1, 1, &[1, 2, 3, 4]).unwrap();
        assert!(handle.update_scanout(9, 9, 1, 1, &[1, 2, 3, 4]).is_err());
        assert_eq!(handle.stats().updates, 1);
        assert_eq!(handle.stats().rejected, 1);

        handle.set_resolution(2, 2).unwrap();
        assert_eq!(handle.resolution(), (2, 2));
        assert!(handle.set_resolution(0, 5).is_err());

        let png = handle.screenshot_png().unwrap();
        assert_eq!(&png[1..4], b"PNG");
    }

    #[test]
    fn screenshot_snapshot_is_independent_of_later_updates() {
        let handle = DisplayHandle::detached(2, 1).unwrap();
        handle
            .update_scanout(0, 0, 2, 1, &[10, 20, 30, 0, 40, 50, 60, 0])
            .unwrap();
        let before = handle.screenshot_png().unwrap();
        handle.update_scanout(0, 0, 1, 1, &[0, 0, 0, 0]).unwrap();
        let after = handle.screenshot_png().unwrap();
        assert_ne!(before, after);
    }
}
