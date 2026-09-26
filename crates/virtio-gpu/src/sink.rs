//! The host presentation seam: what [`crate::GpuDevice`] needs from the
//! window, and nothing more.
//!
//! # Why a trait and not `display::DisplayHandle` directly
//!
//! `crates/display` already depends on this crate (it takes
//! [`crate::FORMAT_B8G8R8A8_UNORM`], [`crate::Rect`] and
//! [`crate::MAX_RESOURCE_PIXELS`] from here), so a direct dependency back would
//! be a build cycle. More importantly, the device has no business knowing about
//! `winit` or `wgpu`: it produces BGRA rects, and *something* presents them.
//! [`ScanoutSink`] is exactly that contract — three methods, all of which
//! `display::DisplayHandle` already implements verbatim:
//!
//! ```text
//! GpuDevice ──ScanoutSink──▶ DisplayHandle ──▶ Scanout mirror ──▶ wgpu texture
//!                       └──▶ TestSink (unit tests, no window at all)
//! ```
//!
//! The end-to-end tests in `tests/gpu_queue.rs` drive a real
//! `DisplayHandle::detached(w, h)` through this trait and read the pixels back
//! with `screenshot_png()`, so the trait is not a test-only abstraction: it is
//! the same path the window uses.

use thiserror::Error;

use crate::shared::SharedPresent;

/// The host side refused a scanout update.
///
/// Deliberately opaque: rejections come from the display implementation (a rect
/// that left the scanout, a resolution the host cannot allocate) and the device
/// only ever turns them into `VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER`. Keeping
/// the concrete error out of this crate is what allows `display` to depend on
/// `virtio-gpu` and not the other way round.
#[derive(Debug, Error)]
#[error("{reason}")]
pub struct SinkError {
    reason: String,
}

impl SinkError {
    /// Wraps a host-side rejection reason.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// Everything the virtio-gpu device needs from the host display.
///
/// Implementations must be non-blocking (the device runs on a vCPU or device
/// thread) and must never panic on the values passed in: the rects come from
/// the guest, even though the device has already validated them against the
/// resource.
pub trait ScanoutSink: Send {
    /// Current guest resolution, i.e. the size of the scanout the host is
    /// presenting. Reported to the guest by `GET_DISPLAY_INFO`.
    fn resolution(&self) -> (u32, u32);

    /// Changes the guest resolution (`SET_SCANOUT` with a differently sized
    /// region). The contents are undefined afterwards; the guest always
    /// follows up with a transfer and a flush.
    fn set_resolution(&self, width: u32, height: u32) -> Result<(), SinkError>;

    /// Copies a tightly packed `width`×`height` block of
    /// [`crate::FORMAT_B8G8R8A8_UNORM`] pixels to (`x`, `y`) of the scanout and
    /// asks for a redraw. `data` may be longer than the rect needs.
    fn update_scanout(
        &self,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        data: &[u8],
    ) -> Result<(), SinkError>;

    /// Shows (or replaces) the hardware-cursor plane (MVP-812): a
    /// `width`×`height` BGRA image with its hotspot at (`hot_x`, `hot_y`),
    /// positioned so the hotspot lands on (`x`, `y`) of the scanout. The image
    /// alpha is premultiplied (the DRM cursor-plane convention, which is what
    /// Linux' virtio_gpu driver puts in the cursor resource).
    ///
    /// The device has already bounded the image ([`crate::MAX_CURSOR_DIM`]) and
    /// validated `data`'s length; the host clips the position, which may hang
    /// off any scanout edge.
    #[allow(clippy::too_many_arguments)]
    fn set_cursor(
        &self,
        width: u32,
        height: u32,
        hot_x: u32,
        hot_y: u32,
        x: u32,
        y: u32,
        data: &[u8],
    ) -> Result<(), SinkError>;

    /// Moves the cursor plane's hotspot to (`x`, `y`) of the scanout.
    fn move_cursor(&self, x: u32, y: u32) -> Result<(), SinkError>;

    /// Hides the cursor plane (`UPDATE_CURSOR` with resource 0).
    fn hide_cursor(&self) -> Result<(), SinkError>;

    // ------------------------- shared presentation (ADR-0004, zero-copy)

    /// Whether [`Self::present_shared`] could take a frame right now — a
    /// cheap question the device asks before it asks the renderer for a
    /// lease, so a sink that never shares costs the copy path nothing. The
    /// default: never.
    fn accepts_shared_scanout(&self) -> bool {
        false
    }

    /// Present a renderer's image instead of pixels ([`crate::shared`]): copy
    /// `frame.damage` of `frame.image` — or more, if the sink's own copy of
    /// the frame needs it — into the sink's copy of the scanout on the GPU,
    /// ordered by the guest's release (`frame.release`).
    ///
    /// `lease` is the renderer's claim on the image's payload: while it
    /// lives, no guest GPU work touching the image starts. The sink keeps it
    /// until its copy has **finished** on the GPU and drops it then — which
    /// may be after this call has returned [`SharedPresent::Presented`], so
    /// the device's queue worker never waits for the GPU. Declining or
    /// failing drops it before returning.
    ///
    /// The device has validated the frame against the scanout: `visible` is
    /// the bound region of a `frame.image.info.width` × `height` image, its
    /// size the current resolution, and `damage` lies inside it. Everything
    /// about the *host* — the image's GPU, its handle, whether it imports —
    /// is the sink's to judge, and any doubt is
    /// [`SharedPresent::Declined`]: the copy path serves the flush.
    ///
    /// The default declines for good.
    fn present_shared(
        &self,
        frame: &crate::shared::SharedScanoutFrame,
        lease: crate::shared::SharedScanoutLease,
    ) -> SharedPresent {
        let _ = (frame, lease);
        SharedPresent::never("this display has no GPU of its own")
    }

    /// Resource `resource_id` (every one, for `None`) is gone or no longer
    /// scanned out: whatever the sink imported of it goes. What the sink
    /// shows is its own copy and stays. The default holds nothing.
    fn forget_shared(&self, resource_id: Option<u32>) {
        let _ = resource_id;
    }

    /// The guest submitted GPU work (`SUBMIT_3D`: a Venus ring's doorbell, a
    /// virgl command stream). A hint for a host that keeps its GPU's clocks
    /// up while the guest uses it (ADR-0004, the GPU-boost amendment); flips
    /// are the other half of that signal and arrive through the calls above.
    /// Called on the device's queue worker for every submit, so it must cost
    /// no more than an atomic store. The default ignores it.
    fn gpu_work(&self) {}
}

/// Blanket forwarding so a device can be handed `&`-shared or boxed sinks.
impl<S: ScanoutSink + ?Sized> ScanoutSink for Box<S> {
    fn resolution(&self) -> (u32, u32) {
        (**self).resolution()
    }

    fn set_resolution(&self, width: u32, height: u32) -> Result<(), SinkError> {
        (**self).set_resolution(width, height)
    }

    fn update_scanout(
        &self,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        data: &[u8],
    ) -> Result<(), SinkError> {
        (**self).update_scanout(x, y, width, height, data)
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
    ) -> Result<(), SinkError> {
        (**self).set_cursor(width, height, hot_x, hot_y, x, y, data)
    }

    fn move_cursor(&self, x: u32, y: u32) -> Result<(), SinkError> {
        (**self).move_cursor(x, y)
    }

    fn hide_cursor(&self) -> Result<(), SinkError> {
        (**self).hide_cursor()
    }

    fn accepts_shared_scanout(&self) -> bool {
        (**self).accepts_shared_scanout()
    }

    fn present_shared(
        &self,
        frame: &crate::shared::SharedScanoutFrame,
        lease: crate::shared::SharedScanoutLease,
    ) -> SharedPresent {
        (**self).present_shared(frame, lease)
    }

    fn forget_shared(&self, resource_id: Option<u32>) {
        (**self).forget_shared(resource_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Nothing;

    impl ScanoutSink for Nothing {
        fn resolution(&self) -> (u32, u32) {
            (8, 4)
        }

        fn set_resolution(&self, _width: u32, _height: u32) -> Result<(), SinkError> {
            Err(SinkError::new("fixed size"))
        }

        fn update_scanout(
            &self,
            _x: u32,
            _y: u32,
            _width: u32,
            _height: u32,
            _data: &[u8],
        ) -> Result<(), SinkError> {
            Ok(())
        }

        fn set_cursor(
            &self,
            _width: u32,
            _height: u32,
            _hot_x: u32,
            _hot_y: u32,
            _x: u32,
            _y: u32,
            _data: &[u8],
        ) -> Result<(), SinkError> {
            Ok(())
        }

        fn move_cursor(&self, _x: u32, _y: u32) -> Result<(), SinkError> {
            Err(SinkError::new("no cursor plane"))
        }

        fn hide_cursor(&self) -> Result<(), SinkError> {
            Ok(())
        }
    }

    #[test]
    fn boxed_sinks_forward_every_method() {
        let sink: Box<dyn ScanoutSink> = Box::new(Nothing);
        assert_eq!(sink.resolution(), (8, 4));
        assert!(sink.update_scanout(0, 0, 1, 1, &[0; 4]).is_ok());
        let error = sink.set_resolution(1, 1).expect_err("Nothing refuses");
        assert_eq!(error.to_string(), "fixed size");
        assert!(sink.set_cursor(1, 1, 0, 0, 0, 0, &[0; 4]).is_ok());
        let error = sink.move_cursor(1, 1).expect_err("Nothing refuses");
        assert_eq!(error.to_string(), "no cursor plane");
        assert!(sink.hide_cursor().is_ok());
        // The shared-presentation defaults: never, and forgetting is free.
        assert!(!sink.accepts_shared_scanout());
        sink.forget_shared(None);
    }
}
