//! The display's half of shared presentation (ADR-0004, "zero-copy
//! presentation of the GPU-composited desktop"): what the device's
//! [`virtio_gpu::ScanoutSink::present_shared`] reaches, and the decisions
//! behind it, all portable and unit-tested.
//!
//! # What happens to a frame
//!
//! The GPU device hands over a [`SharedScanoutFrame`] — a renderer's image in
//! exportable device-local memory, an owned duplicate of its OS handle, and how
//! the guest released it — instead of pixels. A [`SharedPresenter`] (on
//! Windows, `gpu_scanout::GpuScanout` on the display's own Vulkan device)
//! imports the image once, then on every flip copies the damaged rect **on the
//! GPU** into a texture of its own, acquiring the image from the guest's
//! release and handing it straight back. The call returns once the copy is
//! submitted; the presenter keeps the device's lease — the renderer's claim
//! on the image — until the copy has finished on the GPU, and drops it then.
//! The window samples that texture exactly as it samples the mirror's.
//!
//! No readback, no CPU copy, no upload, no wait on the device's queue worker —
//! and the guest's buffer is only ever held for the length of one GPU copy,
//! never for the window's frame. Why it is
//! a copy into a texture of the display's own and not the imported image
//! sampled in place is ADR-0004's to tell ("the texture wgpu cannot be told
//! about"); in short, wgpu tracks every texture's layout from `UNDEFINED`, and
//! the first use of an imported guest image would discard its contents.
//!
//! # Which image is on screen
//!
//! The display shows one of two sources, and the last update decides: a mirror
//! update (`update_scanout`, a mode change) makes the mirror current and
//! [`SharedPresenter::deactivate`]s the shared texture; a presented frame makes
//! the shared texture current. A shared texture that is not current may hold
//! an old frame, so the first shared present after the mirror copies the whole
//! visible region ([`copy_region`]).

use std::sync::{Arc, Mutex};

use virtio_gpu::shared::{ImageRelease, QUEUE_FAMILY_EXTERNAL, QUEUE_FAMILY_FOREIGN};
use virtio_gpu::{Rect, SharedPresent, SharedScanoutFrame, SharedScanoutLease};

use crate::sync::lock;
use crate::DisplayError;

/// Imports a presenter keeps at once: a compositor flips between two or
/// three buffers, and one more covers a mode change in flight.
pub const MAX_SHARED_IMPORTS: usize = 4;

/// `VK_IMAGE_LAYOUT_GENERAL`.
pub const LAYOUT_GENERAL: i32 = 1;
/// `VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL`.
pub const LAYOUT_COLOR_ATTACHMENT: i32 = 2;
/// `VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL`.
pub const LAYOUT_SHADER_READ_ONLY: i32 = 5;
/// `VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL`.
pub const LAYOUT_TRANSFER_SRC: i32 = 6;
/// `VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL`.
pub const LAYOUT_TRANSFER_DST: i32 = 7;

/// The layout the copy reads a released image in: the release's own when a
/// copy may read in it, else `TRANSFER_SRC_OPTIMAL` — the renderer's scanout
/// device's rule exactly (`venus::executor::scanout::copy_layout`), so both
/// paths acquire and release the guest's buffer the same way. `None` for a
/// release the display will not acquire from: `UNDEFINED` or `PREINITIALIZED`
/// (the frame would be discarded, or never was), and every layout outside the
/// colour layouts of core 1.0.
#[must_use]
pub fn copy_layout(released: i32) -> Option<i32> {
    match released {
        LAYOUT_GENERAL | LAYOUT_TRANSFER_SRC => Some(released),
        LAYOUT_COLOR_ATTACHMENT | LAYOUT_SHADER_READ_ONLY | LAYOUT_TRANSFER_DST => {
            Some(LAYOUT_TRANSFER_SRC)
        }
        _ => None,
    }
}

/// What a presenter's device is, for [`judge`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresenterDevice {
    /// Its physical device's `deviceUUID`.
    pub device_uuid: [u8; 16],
    /// And `driverUUID`.
    pub driver_uuid: [u8; 16],
    /// `maxImageDimension2D`.
    pub max_dimension: u32,
}

/// Whether `frame` is one this presenter can take at all, before anything is
/// imported or recorded. `Err` is the answer to give the device.
///
/// The device validated the frame against the guest's binding; this is the
/// host's side of it — the same GPU, a handle type and an image this path
/// knows, a release it can acquire from — plus the geometry again, because a
/// presenter is a public API and must not trust its caller with a copy region.
///
/// # Errors
/// [`SharedPresent::Declined`]: permanent for another GPU (it will never
/// import), for now for everything a later frame may get right.
pub fn judge(frame: &SharedScanoutFrame, device: &PresenterDevice) -> Result<(), SharedPresent> {
    let image = &frame.image;
    if (image.device_uuid, image.driver_uuid) != (device.device_uuid, device.driver_uuid) {
        return Err(SharedPresent::never(
            "the renderer's image is on another GPU or driver than the display's",
        ));
    }
    if image.handle_type != virtio_gpu::shared::HANDLE_TYPE_OPAQUE_WIN32 {
        return Err(SharedPresent::not_now(format!(
            "handle type {:#x} is not one the display imports",
            image.handle_type
        )));
    }
    if image.handle.is_placeholder() {
        return Err(SharedPresent::not_now("the image's handle names nothing"));
    }
    if !image.info.is_bgra8() {
        return Err(SharedPresent::not_now(format!(
            "format {} is not a BGRA scanout format",
            image.info.format
        )));
    }
    let (w, h) = (image.info.width, image.info.height);
    if w == 0 || h == 0 || w > device.max_dimension || h > device.max_dimension {
        return Err(SharedPresent::not_now(format!(
            "a {w}x{h} image is outside what the display's device holds"
        )));
    }
    if frame.visible.width == 0 || frame.visible.height == 0 || !frame.visible.fits_within(w, h) {
        return Err(SharedPresent::not_now(
            "the visible region is outside the image",
        ));
    }
    if !contains(frame.visible, frame.damage) {
        return Err(SharedPresent::not_now(
            "the damage is outside the visible region",
        ));
    }
    judge_release(frame.release)
}

/// The release half of [`judge`].
///
/// # Errors
/// As [`judge`].
pub fn judge_release(release: ImageRelease) -> Result<(), SharedPresent> {
    if release.family != QUEUE_FAMILY_FOREIGN && release.family != QUEUE_FAMILY_EXTERNAL {
        return Err(SharedPresent::not_now(
            "the guest released its image to a family inside its instance",
        ));
    }
    if copy_layout(release.layout).is_none() {
        return Err(SharedPresent::not_now(format!(
            "the guest released its image in layout {}, which the display does not acquire from",
            release.layout
        )));
    }
    Ok(())
}

/// Whether `inner` lies inside `outer` (u64, so nothing wraps).
fn contains(outer: Rect, inner: Rect) -> bool {
    let (ox, oy) = (u64::from(outer.x), u64::from(outer.y));
    let (ix, iy) = (u64::from(inner.x), u64::from(inner.y));
    inner.width > 0
        && inner.height > 0
        && ix >= ox
        && iy >= oy
        && ix + u64::from(inner.width) <= ox + u64::from(outer.width)
        && iy + u64::from(inner.height) <= oy + u64::from(outer.height)
}

/// One copy out of the shared image: `src` in the image's coordinates, landing
/// at `dst` of the display's texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyRegion {
    /// The rect of the image to copy.
    pub src: Rect,
    /// Where its top-left lands in the display's texture.
    pub dst: (u32, u32),
}

/// What to copy for `frame`: only its damage when the display's texture holds
/// the frame before it (`current`), else the whole visible region — a new
/// texture, or one the mirror was shown over since, holds nothing the guest's
/// damage is relative to.
#[must_use]
pub fn copy_region(frame: &SharedScanoutFrame, current: bool) -> CopyRegion {
    let src = if current { frame.damage } else { frame.visible };
    CopyRegion {
        src,
        dst: (
            src.x.saturating_sub(frame.visible.x),
            src.y.saturating_sub(frame.visible.y),
        ),
    }
}

/// The shared texture the window should draw, when it is what the scanout
/// shows.
#[derive(Debug, Clone)]
pub struct SharedTexture {
    /// Changes whenever the texture is replaced (a mode change), so the window
    /// knows to rebuild what it binds.
    pub generation: u64,
    /// The texture's view, `Bgra8Unorm`, the scanout's size.
    pub view: wgpu::TextureView,
    /// Its size.
    pub size: (u32, u32),
}

/// Counters of a presenter, for the diagnostics line and the tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SharedStats {
    /// Frames presented.
    pub presented: u64,
    /// Of those, copies of the whole visible region.
    pub full_copies: u64,
    /// Pixels copied on the GPU.
    pub pixels: u64,
    /// Images imported.
    pub imports: u64,
    /// Frames declined.
    pub declined: u64,
    /// Frames that failed.
    pub failed: u64,
    /// Total time of the presents, from call to submitted copy, in µs —
    /// what the device's queue worker spends in them.
    pub present_us: u64,
    /// The worst.
    pub present_max_us: u64,
    /// Copies that finished on the GPU and had their lease dropped.
    pub retired: u64,
    /// Their total time from submit to lease dropped, in µs.
    pub retire_us: u64,
    /// The worst.
    pub retire_max_us: u64,
    /// Copies whose lease was dropped at the bound, unfinished.
    pub abandoned: u64,
}

/// A presenter of shared frames: the display's GPU. See the module docs.
pub trait SharedPresenter: Send + Sync {
    /// Take `frame`, holding `lease` until its copy has finished on the GPU
    /// — see [`virtio_gpu::ScanoutSink::present_shared`], whose contract
    /// this is.
    fn present(&self, frame: &SharedScanoutFrame, lease: SharedScanoutLease) -> SharedPresent;

    /// Drop the imports of `resource_id`, or of everything for `None`.
    fn forget(&self, resource_id: Option<u32>);

    /// The mirror is what the scanout shows from now on.
    fn deactivate(&self);

    /// The shared texture, while it is what the scanout shows.
    fn current(&self) -> Option<SharedTexture>;

    /// Whether a shared frame is what the scanout shows — [`Self::current`]
    /// without the texture, for a caller with no GPU of its own
    /// (screenshots).
    fn shown(&self) -> bool;

    /// The shown shared frame as tightly packed BGRA, `(width, height,
    /// pixels)` — screenshots of a scanout the mirror does not hold.
    ///
    /// # Errors
    /// Nothing shared is shown, or the GPU did not answer.
    fn read_back(&self) -> Result<(u32, u32, Vec<u8>), DisplayError>;

    /// Its counters.
    fn stats(&self) -> SharedStats;
}

/// Where a display's presenter lives once it has one: shared between the
/// device-facing [`crate::DisplayHandle`] (every clone of it) and the window,
/// which fills it when its GPU is up. Empty — no GPU of the display's own yet,
/// or none that can share — means every flush takes the copy path.
#[derive(Clone, Default)]
pub struct SharedSlot(Arc<Mutex<Option<Arc<dyn SharedPresenter>>>>);

impl std::fmt::Debug for SharedSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedSlot")
            .field("filled", &self.get().is_some())
            .finish()
    }
}

impl SharedSlot {
    /// The presenter, if there is one.
    pub fn get(&self) -> Option<Arc<dyn SharedPresenter>> {
        lock(&self.0, "shared presenter").clone()
    }

    /// Installs (or, with `None`, removes) the presenter.
    pub fn set(&self, presenter: Option<Arc<dyn SharedPresenter>>) {
        *lock(&self.0, "shared presenter") = presenter;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use virtio_gpu::shared::{
        ExternalHandle, SharedImageInfo, SharedScanoutImage, HANDLE_TYPE_OPAQUE_WIN32,
        VK_FORMAT_B8G8R8A8_SRGB, VK_FORMAT_B8G8R8A8_UNORM,
    };

    const GPU: PresenterDevice = PresenterDevice {
        device_uuid: [1; 16],
        driver_uuid: [2; 16],
        max_dimension: 16384,
    };

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn frame(format: i32, handle: ExternalHandle) -> SharedScanoutFrame {
        SharedScanoutFrame {
            image: Arc::new(SharedScanoutImage {
                serial: SharedScanoutImage::next_serial(),
                resource_id: 31,
                handle,
                handle_type: HANDLE_TYPE_OPAQUE_WIN32,
                allocation_size: 0x7f_a000,
                memory_type_index: 1,
                device_uuid: [1; 16],
                driver_uuid: [2; 16],
                info: SharedImageInfo {
                    format,
                    flags: 0x8,
                    view_formats: vec![VK_FORMAT_B8G8R8A8_UNORM, VK_FORMAT_B8G8R8A8_SRGB],
                    usage: 0x1f,
                    width: 1920,
                    height: 1080,
                },
            }),
            release: ImageRelease {
                layout: LAYOUT_TRANSFER_SRC,
                family: QUEUE_FAMILY_FOREIGN,
            },
            visible: rect(0, 0, 1920, 1080),
            damage: rect(100, 200, 300, 40),
        }
    }

    /// A frame whose handle is real enough to pass [`judge`]'s placeholder
    /// check: only Windows has handles, so elsewhere every frame is refused
    /// there, which is the right answer on a host with no handle blobs.
    fn live_frame() -> Option<SharedScanoutFrame> {
        #[cfg(windows)]
        {
            use std::os::windows::io::OwnedHandle;
            // Any real handle will do for a judgement that never imports it:
            // a duplicate of this process's own.
            let file = std::fs::File::open(std::env::current_exe().ok()?).ok()?;
            let handle: OwnedHandle = file.into();
            Some(frame(
                VK_FORMAT_B8G8R8A8_UNORM,
                ExternalHandle::from_owned(handle),
            ))
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    #[test]
    fn the_copy_layout_rule_is_the_scanout_devices() {
        assert_eq!(copy_layout(LAYOUT_GENERAL), Some(LAYOUT_GENERAL));
        assert_eq!(copy_layout(LAYOUT_TRANSFER_SRC), Some(LAYOUT_TRANSFER_SRC));
        for layout in [
            LAYOUT_COLOR_ATTACHMENT,
            LAYOUT_SHADER_READ_ONLY,
            LAYOUT_TRANSFER_DST,
        ] {
            assert_eq!(copy_layout(layout), Some(LAYOUT_TRANSFER_SRC));
        }
        // UNDEFINED, PREINITIALIZED, sync2's READ_ONLY_OPTIMAL, PRESENT_SRC.
        for layout in [0, 8, 1_000_314_000, 1_000_001_002] {
            assert_eq!(copy_layout(layout), None, "layout {layout}");
        }
    }

    #[test]
    fn a_frame_of_another_gpu_is_declined_for_good() {
        let mut other = GPU;
        other.driver_uuid = [9; 16];
        let refused = judge(
            &frame(VK_FORMAT_B8G8R8A8_UNORM, ExternalHandle::placeholder()),
            &other,
        )
        .expect_err("another driver");
        assert!(
            matches!(&refused, SharedPresent::Declined { retry: false, reason } if reason.contains("another GPU")),
            "{refused:?}"
        );
    }

    #[test]
    fn a_placeholder_handle_is_declined_for_now() {
        let refused = judge(
            &frame(VK_FORMAT_B8G8R8A8_UNORM, ExternalHandle::placeholder()),
            &GPU,
        )
        .expect_err("nothing to import");
        assert!(matches!(
            refused,
            SharedPresent::Declined { retry: true, .. }
        ));
    }

    #[test]
    fn a_well_formed_frame_passes_and_everything_malformed_is_declined_for_now() {
        let Some(good) = live_frame() else {
            eprintln!("skipping: this host has no OS handles to share");
            return;
        };
        assert_eq!(judge(&good, &GPU), Ok(()));
        let mut rgba = good.clone();
        let mut image = SharedScanoutImage {
            serial: SharedScanoutImage::next_serial(),
            resource_id: 31,
            handle: ExternalHandle::placeholder(),
            handle_type: HANDLE_TYPE_OPAQUE_WIN32,
            allocation_size: 0,
            memory_type_index: 0,
            device_uuid: [1; 16],
            driver_uuid: [2; 16],
            info: good.image.info.clone(),
        };
        image.info.format = 37; // R8G8B8A8_UNORM
        rgba.image = Arc::new(image);
        assert!(judge(&rgba, &GPU).is_err());
        let small = PresenterDevice {
            max_dimension: 1024,
            ..GPU
        };
        assert!(judge(&good, &small).is_err(), "too big for the device");
        let mut outside = good.clone();
        outside.visible = rect(1, 0, 1920, 1080);
        assert!(judge(&outside, &GPU).is_err(), "visible past the image");
        let mut damage = good.clone();
        damage.damage = rect(1900, 0, 21, 1);
        assert!(
            judge(&damage, &GPU).is_err(),
            "damage past the visible region"
        );
        let mut empty = good.clone();
        empty.damage = rect(0, 0, 0, 5);
        assert!(judge(&empty, &GPU).is_err(), "empty damage");
        for release in [
            ImageRelease {
                layout: 0,
                family: QUEUE_FAMILY_FOREIGN,
            },
            ImageRelease {
                layout: LAYOUT_TRANSFER_SRC,
                family: 0,
            },
        ] {
            let mut bad = good.clone();
            bad.release = release;
            assert!(
                matches!(
                    judge(&bad, &GPU),
                    Err(SharedPresent::Declined { retry: true, .. })
                ),
                "{release:?}"
            );
        }
        let mut external = good;
        external.release.family = QUEUE_FAMILY_EXTERNAL;
        assert_eq!(judge(&external, &GPU), Ok(()));
    }

    #[test]
    fn only_a_current_texture_takes_the_damage_alone() {
        let mut f = frame(VK_FORMAT_B8G8R8A8_UNORM, ExternalHandle::placeholder());
        assert_eq!(
            copy_region(&f, true),
            CopyRegion {
                src: rect(100, 200, 300, 40),
                dst: (100, 200)
            }
        );
        assert_eq!(
            copy_region(&f, false),
            CopyRegion {
                src: rect(0, 0, 1920, 1080),
                dst: (0, 0)
            }
        );
        // A panned scanout: the visible region's origin is the texture's.
        f.visible = rect(64, 32, 1280, 720);
        f.damage = rect(64, 40, 10, 10);
        assert_eq!(
            copy_region(&f, true),
            CopyRegion {
                src: rect(64, 40, 10, 10),
                dst: (0, 8)
            }
        );
        assert_eq!(copy_region(&f, false).dst, (0, 0));
    }

    struct Fake;

    impl SharedPresenter for Fake {
        fn present(
            &self,
            _frame: &SharedScanoutFrame,
            _lease: SharedScanoutLease,
        ) -> SharedPresent {
            SharedPresent::Presented
        }
        fn forget(&self, _resource_id: Option<u32>) {}
        fn deactivate(&self) {}
        fn current(&self) -> Option<SharedTexture> {
            None
        }
        fn shown(&self) -> bool {
            false
        }
        fn read_back(&self) -> Result<(u32, u32, Vec<u8>), DisplayError> {
            Ok((1, 1, vec![0; 4]))
        }
        fn stats(&self) -> SharedStats {
            SharedStats::default()
        }
    }

    #[test]
    fn the_slot_is_shared_by_every_clone() {
        let slot = SharedSlot::default();
        let other = slot.clone();
        assert!(other.get().is_none());
        slot.set(Some(Arc::new(Fake)));
        assert!(other.get().is_some());
        assert!(format!("{other:?}").contains("true"));
        other.set(None);
        assert!(slot.get().is_none());
    }
}
