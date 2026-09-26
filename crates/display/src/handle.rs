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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use winit::event_loop::EventLoopProxy;

use crate::scanout::{lock_scanout, Scanout, ScanoutStats, SharedScanout};
use crate::shared::{SharedSlot, SharedStats};
use crate::DisplayError;

/// Wakeups the event loop understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostEvent {
    /// The scanout changed (or someone asked for a redraw).
    Redraw,
    /// The guest used the GPU while the window was not boosting it
    /// ([`crate::boost`]): look again.
    GpuActive,
    /// Tear the window down and return from [`crate::DisplayHost::run`].
    Shutdown,
}

/// Coalescing wakeup channel to the event loop. Detached wakers (no proxy) are
/// what [`DisplayHandle::detached`] uses.
#[derive(Debug, Clone)]
pub(crate) struct Waker {
    proxy: Option<EventLoopProxy<HostEvent>>,
    pending: Arc<AtomicBool>,
    /// Guest frames published since the display came up (every flush of the
    /// scanout, shared or copied), so the window can count the ones it
    /// never drew ([`crate::FrameStats::unshown`]).
    frames: Arc<AtomicU64>,
    /// When the guest last used the GPU, for the boost ([`crate::boost`]).
    activity: Arc<Activity>,
}

/// When the guest last used the GPU — a flip reached the display or a
/// `SUBMIT_3D` the device — as the window's GPU boost reads it
/// ([`crate::boost`], ADR-0004, the GPU-boost amendment).
///
/// Written by device threads (an atomic store per event), read by the
/// window. `armed` is how the window, once it has let the boost go, asks to
/// be woken by the next activity: one wakeup per idle-to-active transition,
/// never one per submit.
#[derive(Debug)]
pub(crate) struct Activity {
    base: Instant,
    /// Nanoseconds after `base` of the latest activity, plus one; 0: none.
    last: AtomicU64,
    armed: AtomicBool,
}

impl Activity {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            last: AtomicU64::new(0),
            armed: AtomicBool::new(false),
        }
    }

    /// Records activity now; whether the window asked to be woken for it.
    fn note(&self) -> bool {
        let ns = u64::try_from(self.base.elapsed().as_nanos())
            .unwrap_or(u64::MAX - 1)
            .saturating_add(1);
        self.last.fetch_max(ns, Ordering::SeqCst);
        self.armed.swap(false, Ordering::SeqCst)
    }

    /// The latest activity, if any.
    pub(crate) fn last(&self) -> Option<Instant> {
        match self.last.load(Ordering::SeqCst) {
            0 => None,
            ns => Some(self.base + Duration::from_nanos(ns - 1)),
        }
    }

    /// The window is not boosting: wake it at the next activity. The caller
    /// looks at [`Self::last`] again *after* arming, so an activity that
    /// landed in between is not lost.
    pub(crate) fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    /// The window is boosting: activity needs no wakeup.
    pub(crate) fn disarm(&self) {
        self.armed.store(false, Ordering::SeqCst);
    }
}

impl Waker {
    pub(crate) fn new(proxy: EventLoopProxy<HostEvent>) -> Self {
        Self {
            proxy: Some(proxy),
            pending: Arc::new(AtomicBool::new(false)),
            frames: Arc::new(AtomicU64::new(0)),
            activity: Arc::new(Activity::new()),
        }
    }

    pub(crate) fn detached() -> Self {
        Self {
            proxy: None,
            pending: Arc::new(AtomicBool::new(false)),
            frames: Arc::new(AtomicU64::new(0)),
            activity: Arc::new(Activity::new()),
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

    /// A guest frame was published: counted, noted as GPU activity, then
    /// [`Self::wake`] (which is also what an armed boost needs).
    pub(crate) fn frame(&self) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.activity.note();
        self.wake();
    }

    /// The guest submitted GPU work ([`virtio_gpu::ScanoutSink::gpu_work`]):
    /// noted, and the window woken only if it asked to be.
    pub(crate) fn gpu_work(&self) {
        if !self.activity.note() {
            return;
        }
        let Some(proxy) = &self.proxy else {
            return;
        };
        if proxy.send_event(HostEvent::GpuActive).is_err() {
            tracing::trace!("GPU activity wakeup dropped: event loop has exited");
        }
    }

    /// The guest's GPU activity, as the window reads it.
    pub(crate) fn activity(&self) -> &Activity {
        &self.activity
    }

    /// Guest frames published so far.
    pub(crate) fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
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
    /// The display's shared presenter, once it has one (ADR-0004, zero-copy
    /// presentation): the window fills it when its GPU is up, a headless
    /// display with [`Self::attach_offscreen_gpu`].
    shared: SharedSlot,
}

impl DisplayHandle {
    pub(crate) fn new(scanout: SharedScanout, waker: Waker, shared: SharedSlot) -> Self {
        Self {
            scanout,
            waker,
            shared,
        }
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
        Ok(Self::new(scanout, Waker::detached(), SharedSlot::default()))
    }

    /// Gives a display with no window a GPU of its own for **shared
    /// presentation** (ADR-0004, zero-copy presentation): a renderer's scanout
    /// buffers are then imported onto an off-screen Vulkan device and each
    /// flipped frame is copied there on the GPU, exactly as a window's would
    /// be, and screenshots read that copy back. Without it a headless display
    /// takes every frame through the copy path.
    ///
    /// Windows only, where renderers share handle blobs; elsewhere, and on a
    /// host whose GPU cannot import them, an error that says why.
    pub fn attach_offscreen_gpu(&self) -> Result<(), DisplayError> {
        #[cfg(windows)]
        {
            let presenter = crate::gpu_scanout::GpuScanout::open_offscreen()
                .map_err(DisplayError::SharedDevice)?;
            self.shared.set(Some(Arc::new(presenter)));
            Ok(())
        }
        #[cfg(not(windows))]
        {
            Err(DisplayError::SharedDevice(
                "shared presentation is built for Windows hosts only".into(),
            ))
        }
    }

    /// Installs `presenter` as this display's shared presenter (every clone of
    /// the handle, and the window, see it): what
    /// [`Self::attach_offscreen_gpu`] does with the GPU one, and what a test
    /// does with a fake. `None` removes it — every flush then takes the copy
    /// path.
    pub fn attach_presenter(&self, presenter: Option<Arc<dyn crate::shared::SharedPresenter>>) {
        self.shared.set(presenter);
    }

    /// Whether this display presents a renderer's scanout itself (it has a
    /// shared presenter).
    pub fn shares_scanout(&self) -> bool {
        self.shared.get().is_some()
    }

    /// The shared presenter's counters, if it has one.
    pub fn shared_stats(&self) -> Option<SharedStats> {
        self.shared.get().map(|p| p.stats())
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
        // The mirror is what the scanout shows now.
        if let Some(presenter) = self.shared.get() {
            presenter.deactivate();
        }
        self.waker.frame();
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
        if let Some(presenter) = self.shared.get() {
            presenter.deactivate();
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

    /// The current scanout as tightly packed BGRA, `(width, height, pixels)`,
    /// with the cursor plane composited in — what [`Self::screenshot_png`]
    /// encodes, for a caller that compares pixels rather than files. A shared
    /// frame is read back from the display's GPU.
    pub fn screenshot_bgra(&self) -> Result<(u32, u32, Vec<u8>), DisplayError> {
        let snapshot = self.snapshot()?;
        let (width, height) = snapshot.size();
        Ok((width, height, snapshot.pixels().to_vec()))
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

    /// Shows or replaces the cursor plane (MVP-812); see
    /// [`Scanout::set_cursor`].
    #[allow(clippy::too_many_arguments)]
    pub fn set_cursor(
        &self,
        width: u32,
        height: u32,
        hot_x: u32,
        hot_y: u32,
        x: u32,
        y: u32,
        data: &[u8],
    ) -> Result<(), DisplayError> {
        {
            let mut scanout = lock_scanout(&self.scanout);
            scanout.set_cursor(width, height, hot_x, hot_y, x, y, data)?;
        }
        self.waker.wake();
        Ok(())
    }

    /// Moves the cursor plane's hotspot.
    pub fn move_cursor(&self, x: u32, y: u32) {
        {
            let mut scanout = lock_scanout(&self.scanout);
            scanout.move_cursor(x, y);
        }
        self.waker.wake();
    }

    /// Hides the cursor plane.
    pub fn hide_cursor(&self) {
        {
            let mut scanout = lock_scanout(&self.scanout);
            scanout.hide_cursor();
        }
        self.waker.wake();
    }

    /// Detached copy of the current scanout with the cursor plane composited
    /// in — screenshots must show what the window shows; the lock is released
    /// before the caller does anything slow with it.
    ///
    /// A shared frame (ADR-0004, zero-copy presentation) is not in the mirror:
    /// it is drawn on the display's GPU into an off-screen target through the
    /// window's own image pipeline and read back, and the cursor is composited
    /// over it on the CPU exactly as it is over the mirror.
    fn snapshot(&self) -> Result<Scanout, DisplayError> {
        if let Some(presenter) = self.shared.get() {
            if presenter.shown() {
                let (width, height, pixels) = presenter.read_back()?;
                let based = {
                    let scanout = lock_scanout(&self.scanout);
                    (scanout.size() == (width, height)).then(|| scanout.with_base(&pixels))
                };
                if let Some(based) = based {
                    let mut snapshot = based?;
                    if let Some((rect, composited)) = snapshot.cursor_overlay() {
                        snapshot.update(rect.x, rect.y, rect.width, rect.height, &composited)?;
                    }
                    return Ok(snapshot);
                }
                // A mode change raced the read: the mirror's frame is the
                // current one.
            }
        }
        let (width, height, pixels, overlay) = {
            let scanout = lock_scanout(&self.scanout);
            let (w, h) = scanout.size();
            (w, h, scanout.pixels().to_vec(), scanout.cursor_overlay())
        };
        let mut snapshot = Scanout::new(width, height)?;
        snapshot.update(0, 0, width, height, &pixels)?;
        if let Some((rect, composited)) = overlay {
            snapshot.update(rect.x, rect.y, rect.width, rect.height, &composited)?;
        }
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

    fn set_cursor(
        &self,
        width: u32,
        height: u32,
        hot_x: u32,
        hot_y: u32,
        x: u32,
        y: u32,
        data: &[u8],
    ) -> Result<(), virtio_gpu::SinkError> {
        DisplayHandle::set_cursor(self, width, height, hot_x, hot_y, x, y, data).map_err(sink_error)
    }

    fn move_cursor(&self, x: u32, y: u32) -> Result<(), virtio_gpu::SinkError> {
        DisplayHandle::move_cursor(self, x, y);
        Ok(())
    }

    fn hide_cursor(&self) -> Result<(), virtio_gpu::SinkError> {
        DisplayHandle::hide_cursor(self);
        Ok(())
    }

    fn accepts_shared_scanout(&self) -> bool {
        self.shares_scanout()
    }

    fn present_shared(
        &self,
        frame: &virtio_gpu::SharedScanoutFrame,
        lease: virtio_gpu::SharedScanoutLease,
    ) -> virtio_gpu::SharedPresent {
        let Some(presenter) = self.shared.get() else {
            return virtio_gpu::SharedPresent::not_now("the display's GPU is not up yet");
        };
        let (w, h) = self.resolution();
        if (frame.visible.width, frame.visible.height) != (w, h) {
            return virtio_gpu::SharedPresent::not_now(format!(
                "a {}x{} frame on a {w}x{h} scanout",
                frame.visible.width, frame.visible.height
            ));
        }
        let presented = presenter.present(frame, lease);
        if presented == virtio_gpu::SharedPresent::Presented {
            self.waker.frame();
        }
        presented
    }

    fn forget_shared(&self, resource_id: Option<u32>) {
        if let Some(presenter) = self.shared.get() {
            presenter.forget(resource_id);
        }
    }

    fn gpu_work(&self) {
        self.waker.gpu_work();
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

    // ------------------------------------------ shared presentation

    use crate::shared::{SharedPresenter, SharedStats, SharedTexture};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use virtio_gpu::shared::{
        ExternalHandle, ImageRelease, SharedImageInfo, SharedScanoutImage,
        HANDLE_TYPE_OPAQUE_WIN32, QUEUE_FAMILY_FOREIGN, VK_FORMAT_B8G8R8A8_UNORM,
    };
    use virtio_gpu::{Rect, SharedPresent, SharedScanoutFrame, SharedScanoutLease};

    fn lease(frame: &SharedScanoutFrame) -> SharedScanoutLease {
        SharedScanoutLease::unclaimed(Arc::clone(&frame.image), frame.release)
    }

    fn present(sink: &dyn ScanoutSink, frame: &SharedScanoutFrame) -> SharedPresent {
        sink.present_shared(frame, lease(frame))
    }

    #[derive(Default)]
    struct Fake {
        presents: AtomicUsize,
        shown: AtomicBool,
    }

    impl SharedPresenter for Fake {
        fn present(
            &self,
            _frame: &SharedScanoutFrame,
            _lease: SharedScanoutLease,
        ) -> SharedPresent {
            self.presents.fetch_add(1, Ordering::SeqCst);
            self.shown.store(true, Ordering::SeqCst);
            SharedPresent::Presented
        }
        fn forget(&self, _resource_id: Option<u32>) {}
        fn deactivate(&self) {
            self.shown.store(false, Ordering::SeqCst);
        }
        fn current(&self) -> Option<SharedTexture> {
            None
        }
        fn shown(&self) -> bool {
            self.shown.load(Ordering::SeqCst)
        }
        fn read_back(&self) -> Result<(u32, u32, Vec<u8>), DisplayError> {
            Ok((4, 2, [9u8, 8, 7, 0xff].repeat(8)))
        }
        fn stats(&self) -> SharedStats {
            SharedStats::default()
        }
    }

    fn frame(width: u32, height: u32) -> SharedScanoutFrame {
        let visible = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        SharedScanoutFrame {
            image: Arc::new(SharedScanoutImage {
                serial: SharedScanoutImage::next_serial(),
                resource_id: 3,
                handle: ExternalHandle::placeholder(),
                handle_type: HANDLE_TYPE_OPAQUE_WIN32,
                allocation_size: 4096,
                memory_type_index: 0,
                device_uuid: [0; 16],
                driver_uuid: [0; 16],
                info: SharedImageInfo {
                    format: VK_FORMAT_B8G8R8A8_UNORM,
                    flags: 0,
                    view_formats: Vec::new(),
                    usage: 1,
                    width,
                    height,
                },
            }),
            release: ImageRelease {
                layout: 6,
                family: QUEUE_FAMILY_FOREIGN,
            },
            visible,
            damage: visible,
        }
    }

    /// No presenter: nothing is accepted, and a frame handed over anyway is
    /// declined for now (the window's GPU may still come up).
    #[test]
    fn a_display_without_a_presenter_declines_for_now() {
        let handle = DisplayHandle::detached(4, 2).unwrap();
        let sink: &dyn ScanoutSink = &handle;
        assert!(!sink.accepts_shared_scanout());
        assert!(matches!(
            present(sink, &frame(4, 2)),
            SharedPresent::Declined { retry: true, .. }
        ));
        sink.forget_shared(None);
        #[cfg(not(windows))]
        assert!(handle.attach_offscreen_gpu().is_err());
    }

    /// With one: a frame of the scanout's size is presented, one of another
    /// size never reaches it; a mirror update or a mode change makes the
    /// mirror what is shown again; and a screenshot of a shared frame is the
    /// presenter's pixels with the mirror's cursor plane composited over.
    #[test]
    fn the_last_update_decides_what_is_shown_and_screenshots_follow_it() {
        let handle = DisplayHandle::detached(4, 2).unwrap();
        let fake = Arc::new(Fake::default());
        handle.attach_presenter(Some(Arc::clone(&fake) as Arc<dyn SharedPresenter>));
        let sink: &dyn ScanoutSink = &handle;
        assert!(sink.accepts_shared_scanout());
        assert!(handle.shares_scanout());
        assert!(matches!(
            present(sink, &frame(8, 2)),
            SharedPresent::Declined { retry: true, .. }
        ));
        assert_eq!(fake.presents.load(Ordering::SeqCst), 0);
        assert_eq!(present(sink, &frame(4, 2)), SharedPresent::Presented);
        assert!(fake.shown());

        // The screenshot is the shared frame, the cursor over it.
        handle
            .set_cursor(1, 1, 0, 0, 3, 1, &[0xff, 0xff, 0xff, 0xff])
            .unwrap();
        let (w, h, pixels) = handle.screenshot_bgra().unwrap();
        assert_eq!((w, h), (4, 2));
        assert_eq!(&pixels[..4], &[9, 8, 7, 0xff], "the presenter's pixels");
        let cursor_at = (4 + 3) * 4;
        assert_eq!(
            &pixels[cursor_at..cursor_at + 4],
            &[0xff, 0xff, 0xff, 0xff],
            "the cursor plane over them"
        );

        // A mirror update: the mirror is shown again.
        handle.update_scanout(0, 0, 1, 1, &[1, 2, 3, 4]).unwrap();
        assert!(!fake.shown());
        let (_, _, pixels) = handle.screenshot_bgra().unwrap();
        assert_eq!(&pixels[..4], &[1, 2, 3, 4], "the mirror's pixels");
        assert_eq!(present(sink, &frame(4, 2)), SharedPresent::Presented);
        handle.set_resolution(8, 8).unwrap();
        assert!(!fake.shown(), "a mode change shows the (new) mirror");
        handle.attach_presenter(None);
        assert!(!handle.shares_scanout());
    }

    #[test]
    fn guest_frames_and_submits_are_gpu_activity() {
        let handle = DisplayHandle::detached(4, 2).unwrap();
        let activity = handle.waker.activity();
        assert_eq!(activity.last(), None, "nothing yet");
        let before = Instant::now();
        let sink: &dyn ScanoutSink = &handle;
        sink.gpu_work();
        let first = activity.last().expect("a submit is activity");
        assert!(first >= before);
        std::thread::sleep(Duration::from_millis(2));
        handle.update_scanout(0, 0, 1, 1, &[1, 2, 3, 4]).unwrap();
        let second = activity.last().expect("a flip is activity");
        assert!(second > first, "the latest wins");
        // A cursor move or a redraw request is not the guest using the GPU.
        handle.move_cursor(1, 1);
        handle.request_redraw();
        assert_eq!(activity.last(), Some(second));
    }

    #[test]
    fn an_armed_window_is_woken_once_per_idle_to_active_transition() {
        let activity = Activity::new();
        assert!(!activity.note(), "not armed: no wakeup");
        activity.arm();
        assert!(activity.note(), "the first activity after arming wakes");
        assert!(!activity.note(), "and only the first");
        assert!(!activity.note());
        activity.arm();
        activity.disarm();
        assert!(!activity.note(), "a boosting window is never woken");
    }

    #[test]
    fn activity_never_goes_backwards_across_threads() {
        let activity = Arc::new(Activity::new());
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let activity = Arc::clone(&activity);
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        activity.note();
                    }
                })
            })
            .collect();
        let mut seen = None;
        for _ in 0..1000 {
            let now = activity.last();
            assert!(now >= seen, "went backwards");
            seen = now;
        }
        for t in threads {
            t.join().unwrap();
        }
        assert!(activity.last().is_some());
    }
}
