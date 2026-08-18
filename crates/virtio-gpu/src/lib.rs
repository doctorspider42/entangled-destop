//! virtio-gpu 2D device (backlog EPIC 8, MVP-801…810).
//!
//! A guest that loads `virtio_gpu` gets `/dev/dri/card0`, a 1920×1080 (or
//! whatever the window says) scanout and a 2D command stream that ends up as
//! pixels in the host window. VirGL/3D (`VIRTIO_GPU_F_VIRGL`, Rutabaga) is a
//! separate post-MVP milestone; this crate implements the 2D subset only.
//!
//! # Layout
//!
//! * [`protocol`] — the wire format: header, rects, every command and response
//!   struct, parsed with explicit `from_le_bytes` and no `unsafe`. Pure logic,
//!   portable, heavily unit-tested.
//! * [`resource`] — host 2D resources: the BGRA image, the guest backing page
//!   list and the checked copy paths between them.
//! * [`error`] — [`CommandError`], one variant per way a guest command can be
//!   wrong, each mapping to a `VIRTIO_GPU_RESP_ERR_*` code.
//! * [`sink`] — [`ScanoutSink`], the three-method contract the host display
//!   implements.
//! * [`device`] — [`GpuDevice`], the `virtio_core::VirtioDevice`.
//!
//! # Constructor contract
//!
//! ```text
//! GpuDevice::new(display) where display: ScanoutSink
//! ```
//!
//! The MVP's [`ScanoutSink`] is `display::DisplayHandle` — cloneable, `Send`,
//! and already shaped exactly like this trait (`update_scanout`,
//! `set_resolution`, `resolution`), so wiring the device into a VM is:
//!
//! ```no_run
//! # use virtio_gpu::{GpuDevice, ScanoutSink};
//! # fn wire<S: ScanoutSink + 'static>(handle: S) -> Box<dyn virtio_core::VirtioDevice> {
//! // `handle` is the DisplayHandle the window handed out; clone it if the
//! // supervisor wants to keep one for screenshots.
//! Box::new(GpuDevice::new(handle))
//! # }
//! ```
//!
//! The device is deliberately *not* wired into `apps/vmhost-cli` here: the
//! run-a-VM path (window on the main thread, device on a worker) lands with the
//! VM assembly work. `DisplayHandle::detached(width, height)` gives a windowless
//! sink, which is how the end-to-end tests in `tests/gpu_queue.rs` assert real
//! pixels with `screenshot_png()`.
//!
//! Why a trait instead of depending on `display` directly: `display` already
//! depends on this crate for the pixel format and [`Rect`], so the dependency
//! can only point one way. See [`sink`].
//!
//! # The guest is untrusted
//!
//! Resource ids, rects, offsets, page addresses and entry counts all come from
//! the guest. The rules this crate follows (workspace hard rules):
//!
//! * every rect goes through [`Rect::fits_within`] before any copy, and every
//!   copy destination is a checked slice index;
//! * every guest page read goes through the checked `vm-memory` API, so a
//!   backing entry outside guest RAM fails one command
//!   (`ERR_INVALID_PARAMETER`) instead of touching host memory — the EPIC 8
//!   acceptance criterion "the VM cannot force copies outside its memory";
//! * every allocation is bounded first ([`MAX_RESOURCE_PIXELS`],
//!   [`resource::MAX_TOTAL_RESOURCE_PIXELS`], [`resource::MAX_RESOURCES`],
//!   [`resource::MAX_BACKING_ENTRIES`], [`device::MAX_COMMAND_BYTES`]) and made
//!   with `try_reserve`, so a greedy guest gets `ERR_OUT_OF_MEMORY`;
//! * no `panic!`, `unwrap()` or `expect()` on a guest-controlled path.

pub mod device;
pub mod error;
pub mod protocol;
pub mod resource;
pub mod sink;

pub use device::{GpuDevice, CONTROL_QUEUE, CURSOR_QUEUE, NUM_CAPSETS, NUM_QUEUES, NUM_SCANOUTS};
pub use error::CommandError;
pub use protocol::{cmd, resp, CtrlHdr, Rect, FLAG_FENCE, FLAG_INFO_RING_IDX};
pub use resource::{Resource, ResourceTable};
pub use sink::{ScanoutSink, SinkError};

/// `VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM` (MVP-809): 32-bit little-endian pixels in
/// byte order B, G, R, A — the guest's `DRM_FORMAT_ARGB8888`.
///
/// This and [`FORMAT_B8G8R8X8_UNORM`] are the only formats the MVP accepts, and
/// they share one byte layout, which is why the host scanout texture
/// (`wgpu::TextureFormat::Bgra8Unorm`) can take guest pixels verbatim.
pub const FORMAT_B8G8R8A8_UNORM: u32 = 1;

/// `VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM`: the same bytes with the fourth one
/// ignored — the guest's `DRM_FORMAT_XRGB8888`.
///
/// This is what Linux' `virtio_gpu` actually sends for its framebuffer
/// (`virtio_gpu_translate_format(DRM_FORMAT_XRGB8888)`), so a device that only
/// accepted the alpha variant would reject the very first resource the guest
/// creates. The host ignores the alpha channel either way (see
/// `display::Scanout::to_png`).
pub const FORMAT_B8G8R8X8_UNORM: u32 = 2;

/// Bytes per pixel for both accepted formats.
pub const BYTES_PER_PIXEL: u32 = 4;

/// True for the pixel formats `RESOURCE_CREATE_2D` accepts (MVP-809). Anything
/// else is answered with `ERR_INVALID_PARAMETER`.
pub const fn is_supported_format(format: u32) -> bool {
    matches!(format, FORMAT_B8G8R8A8_UNORM | FORMAT_B8G8R8X8_UNORM)
}

/// Upper bound on a single 2D resource, sized for 4K with headroom; blocks a
/// guest from requesting absurd host allocations.
pub const MAX_RESOURCE_PIXELS: u64 = 4096 * 2304;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_constants_match_the_spec() {
        // `enum virtio_gpu_formats` (spec 1.2, section 5.7.6.1).
        assert_eq!(FORMAT_B8G8R8A8_UNORM, 1);
        assert_eq!(FORMAT_B8G8R8X8_UNORM, 2);
        assert_eq!(BYTES_PER_PIXEL, 4);
        assert_eq!(MAX_RESOURCE_PIXELS, 9_437_184);
    }

    #[test]
    fn only_the_two_bgra_layouts_are_supported() {
        assert!(is_supported_format(FORMAT_B8G8R8A8_UNORM));
        assert!(is_supported_format(FORMAT_B8G8R8X8_UNORM));
        // A8R8G8B8, X8R8G8B8, R8G8B8A8, X8B8G8R8, A8B8G8R8, R8G8B8X8 and
        // nonsense values all need a swizzle the MVP does not do.
        for unsupported in [0, 3, 4, 67, 68, 121, 134, u32::MAX] {
            assert!(!is_supported_format(unsupported), "{unsupported}");
        }
    }
}
