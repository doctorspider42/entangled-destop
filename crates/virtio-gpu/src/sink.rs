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
    }

    #[test]
    fn boxed_sinks_forward_every_method() {
        let sink: Box<dyn ScanoutSink> = Box::new(Nothing);
        assert_eq!(sink.resolution(), (8, 4));
        assert!(sink.update_scanout(0, 0, 1, 1, &[0; 4]).is_ok());
        let error = sink.set_resolution(1, 1).expect_err("Nothing refuses");
        assert_eq!(error.to_string(), "fixed size");
    }
}
