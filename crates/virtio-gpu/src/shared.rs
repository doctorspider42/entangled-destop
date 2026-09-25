//! Presenting a renderer's scanout image **without reading it back**
//! (ADR-0004, "zero-copy presentation of the GPU-composited desktop").
//!
//! A GPU compositor in the guest (Mutter on Zink over Venus) flips between
//! scanout buffers that are **handle blobs**: exportable device-local memory
//! on the host GPU, holding the canonical image stage S1 made of a LINEAR
//! dma-buf. The copy path reads each flipped frame back to host memory
//! (the renderer's scanout device, stage S2b), the device pushes the bytes
//! into the display's CPU mirror, and the window uploads them into a texture
//! again — three copies through the CPU for pixels that started on the GPU the
//! window draws with.
//!
//! This module is the portable half of the other path: the device hands the
//! display **the image itself** — an owned duplicate of the memory's OS
//! handle, everything an import of it must match, and how the guest released
//! it — and the display, on the same GPU, imports it into its own device and
//! takes the frame from there on the GPU.
//!
//! ```text
//!  RESOURCE_FLUSH of a handle blob
//!    device ── Renderer3d::begin_shared_scanout ──▶ SharedScanoutLease
//!           │   (the payload claimed against guest GPU work, `executor::writes`)
//!           └─ ScanoutSink::present_shared(SharedScanoutFrame) ──▶ SharedPresent
//!                 Presented  → shown; the lease is dropped (claim complete)
//!                 Declined   → the copy path serves this flush
//!                 Failed     → the flush fails in band; the window keeps its frame
//! ```
//!
//! Nothing here is an OS type except [`ExternalHandle`]'s payload, which exists
//! only on Windows (an NT handle, `OPAQUE_WIN32`) — the only host whose
//! renderer makes handle blobs today. On every other host a
//! [`SharedScanoutImage`] is never made, [`crate::Renderer3d::begin_shared_scanout`]
//! answers `None`, and the copy path is the only path.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::protocol::Rect;

/// `VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT`: the handle type of every
/// [`SharedScanoutImage`] this crate makes.
pub const HANDLE_TYPE_OPAQUE_WIN32: u32 = 0x2;

/// `VK_QUEUE_FAMILY_FOREIGN_EXT`: the family a guest compositor on Zink
/// releases its scanout buffer to at the end of every batch.
pub const QUEUE_FAMILY_FOREIGN: u32 = !2;

/// `VK_QUEUE_FAMILY_EXTERNAL`.
pub const QUEUE_FAMILY_EXTERNAL: u32 = !1;

/// The environment variable that picks the presentation path of a renderer's
/// scanout — the A/B switch of the zero-copy measurement, and the escape hatch
/// if a host's import misbehaves: `copy` (read every flip back through the
/// renderer, as before) or `shared` (the default: hand the display the image).
pub const SCANOUT_PATH_ENV: &str = "ENTANGLED_SCANOUT_PATH";

/// How a renderer's scanout reaches the window ([`SCANOUT_PATH_ENV`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ScanoutPath {
    /// Hand the display the shared image when the host can; the copy path
    /// otherwise.
    #[default]
    Shared,
    /// Always read the frame back through the renderer.
    Copy,
}

impl ScanoutPath {
    /// Parses [`SCANOUT_PATH_ENV`]'s value.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "shared" | "zero-copy" | "gpu" => Some(Self::Shared),
            "copy" | "readback" => Some(Self::Copy),
            _ => None,
        }
    }

    /// The path this process was asked for: the variable if it parses, else
    /// the default (a value that does not parse is logged and ignored).
    #[must_use]
    pub fn from_env() -> Self {
        match std::env::var(SCANOUT_PATH_ENV) {
            Ok(value) => Self::parse(&value).unwrap_or_else(|| {
                tracing::warn!(
                    var = SCANOUT_PATH_ENV,
                    %value,
                    "not a scanout path (\"shared\" or \"copy\"); using the default"
                );
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }
}

/// An owned OS handle to exportable memory — on Windows an NT handle,
/// closed when this is dropped.
///
/// It is always a **duplicate** of the renderer's own handle
/// (`DuplicateHandle`), so whoever holds it — the display, importing — lives
/// on its own terms: the renderer may close its handle, free the exporting
/// memory and destroy the guest's context, and an import made from this one
/// (which references the payload itself) is unaffected, as a dma-buf outlives
/// its exporter.
pub struct ExternalHandle {
    #[cfg(windows)]
    handle: Option<std::os::windows::io::OwnedHandle>,
}

impl ExternalHandle {
    /// A handle that names nothing: what a fake host hands out, so a test can
    /// drive the whole lease without a GPU. Any import of it is refused.
    #[must_use]
    pub fn placeholder() -> Self {
        Self {
            #[cfg(windows)]
            handle: None,
        }
    }

    /// Takes ownership of `handle`.
    #[cfg(windows)]
    #[must_use]
    pub fn from_owned(handle: std::os::windows::io::OwnedHandle) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    /// The raw handle, borrowed for an import call (which only reads it), or
    /// `None` for a [`placeholder`](Self::placeholder).
    #[cfg(windows)]
    #[must_use]
    pub fn raw(&self) -> Option<std::os::windows::io::RawHandle> {
        use std::os::windows::io::AsRawHandle;
        self.handle.as_ref().map(AsRawHandle::as_raw_handle)
    }

    /// Whether this names nothing.
    #[must_use]
    pub fn is_placeholder(&self) -> bool {
        #[cfg(windows)]
        {
            self.handle.is_none()
        }
        #[cfg(not(windows))]
        {
            true
        }
    }
}

impl std::fmt::Debug for ExternalHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_placeholder() {
            f.write_str("ExternalHandle(placeholder)")
        } else {
            f.write_str("ExternalHandle(..)")
        }
    }
}

/// Exactly how the shared image was created on the host: the canonical image
/// of stage S1 (`executor::modifier::CanonicalImage`). An importer must create
/// its image from precisely these values over the imported memory — the
/// exporter's, every guest importer's and the renderer's own scanout device's
/// create infos come from the same function, and an opaque import is only
/// defined for an identical image.
///
/// Values are Vulkan's: `format` a `VkFormat`, `flags` `VkImageCreateFlags`,
/// `usage` `VkImageUsageFlags`. The rest is fixed by construction: 2D, one
/// level, one layer, one sample, `OPTIMAL`, `EXCLUSIVE`, created for
/// [`HANDLE_TYPE_OPAQUE_WIN32`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedImageInfo {
    /// `VkFormat`.
    pub format: i32,
    /// `VkImageCreateFlags`.
    pub flags: u32,
    /// The `VkImageFormatListCreateInfo` list, empty for none.
    pub view_formats: Vec<i32>,
    /// `VkImageUsageFlags`.
    pub usage: u32,
    /// `extent.width`.
    pub width: u32,
    /// `extent.height`.
    pub height: u32,
}

/// `VK_FORMAT_B8G8R8A8_UNORM`.
pub const VK_FORMAT_B8G8R8A8_UNORM: i32 = 44;
/// `VK_FORMAT_B8G8R8A8_SRGB`.
pub const VK_FORMAT_B8G8R8A8_SRGB: i32 = 50;

impl SharedImageInfo {
    /// Whether its bytes are B, G, R, A — the scanout's byte order, so a copy
    /// of it needs no swizzle (`B8G8R8A8_UNORM` and `_SRGB` are the same
    /// bytes; a scanout samples nothing).
    #[must_use]
    pub fn is_bgra8(&self) -> bool {
        matches!(
            self.format,
            VK_FORMAT_B8G8R8A8_UNORM | VK_FORMAT_B8G8R8A8_SRGB
        )
    }
}

/// A renderer's scanout image, shareable with a presenter on the same GPU.
///
/// Made once per accepted scanout layout of a blob and handed out by `Arc`
/// on every flip; [`Self::serial`] is what an importer caches its import by.
#[derive(Debug)]
pub struct SharedScanoutImage {
    /// Unique among every image of this process ([`Self::next_serial`]): a
    /// new layout, a new blob, a new boot all get a new one, so an importer's
    /// cache can never hand back another image's import.
    pub serial: u64,
    /// The blob.
    pub resource_id: u32,
    /// The memory's handle, a duplicate owned by this value.
    pub handle: ExternalHandle,
    /// [`HANDLE_TYPE_OPAQUE_WIN32`].
    pub handle_type: u32,
    /// The export's `allocationSize` — an opaque import's must be the same.
    pub allocation_size: u64,
    /// The export's `memoryTypeIndex` — an opaque import's must be the same.
    pub memory_type_index: u32,
    /// The exporting physical device's `deviceUUID`: an opaque handle may
    /// only be imported on a device with the same one.
    pub device_uuid: [u8; 16],
    /// And its `driverUUID`, the same rule.
    pub driver_uuid: [u8; 16],
    /// The image to create over it, exactly.
    pub info: SharedImageInfo,
}

static NEXT_SERIAL: AtomicU64 = AtomicU64::new(1);

impl SharedScanoutImage {
    /// A serial no image of this process has had.
    #[must_use]
    pub fn next_serial() -> u64 {
        NEXT_SERIAL.fetch_add(1, Ordering::Relaxed)
    }
}

/// How the guest last released the image out of its instance: the layout it
/// left it in and the family it released it to. A presenter acquires it from
/// exactly this and releases it back the same way, so the guest's next
/// acquire (`oldLayout` = that layout, from that family) is consistent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageRelease {
    /// `newLayout` of the release (`VkImageLayout`).
    pub layout: i32,
    /// `dstQueueFamilyIndex` of the release: [`QUEUE_FAMILY_FOREIGN`] or
    /// [`QUEUE_FAMILY_EXTERNAL`].
    pub family: u32,
}

/// What a renderer grants for one shared present
/// ([`crate::Renderer3d::begin_shared_scanout`]): the image, how it was
/// released, and — held for as long as the lease lives — the renderer's
/// **claim** on the image's payload: every guest GPU submission touching it
/// has finished, and none starts until the lease is dropped. The device drops
/// it once the display's copy has finished.
pub struct SharedScanoutLease {
    /// The image.
    pub image: Arc<SharedScanoutImage>,
    /// The guest's last release of it.
    pub release: ImageRelease,
    done: Option<Box<dyn FnOnce() + Send>>,
}

impl SharedScanoutLease {
    /// A lease whose claim `done` ends.
    pub fn new(
        image: Arc<SharedScanoutImage>,
        release: ImageRelease,
        done: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self {
            image,
            release,
            done: Some(Box::new(done)),
        }
    }

    /// A lease that holds no claim (a renderer whose images nothing else
    /// writes, and tests).
    #[must_use]
    pub fn unclaimed(image: Arc<SharedScanoutImage>, release: ImageRelease) -> Self {
        Self {
            image,
            release,
            done: None,
        }
    }
}

impl Drop for SharedScanoutLease {
    fn drop(&mut self) {
        if let Some(done) = self.done.take() {
            done();
        }
    }
}

impl std::fmt::Debug for SharedScanoutLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedScanoutLease")
            .field("image", &self.image.serial)
            .field("release", &self.release)
            .field("claimed", &self.done.is_some())
            .finish()
    }
}

/// One flip, as the display is handed it ([`crate::ScanoutSink::present_shared`]).
///
/// Coordinates are the image's (the framebuffer's): `visible` is the region
/// the scanout shows — its size is the guest mode — and `damage` the flushed
/// rect, already clipped to `visible`. The display's own copy of the frame is
/// `visible.width × visible.height`, and a pixel at (x, y) of the image lands
/// at (x − visible.x, y − visible.y) of it.
#[derive(Debug, Clone)]
pub struct SharedScanoutFrame {
    /// The image.
    pub image: Arc<SharedScanoutImage>,
    /// How the guest released it.
    pub release: ImageRelease,
    /// What the scanout shows.
    pub visible: Rect,
    /// What changed, inside `visible`.
    pub damage: Rect,
}

/// What the display did with a [`SharedScanoutFrame`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SharedPresent {
    /// Taken: the display's copy of the frame is complete on the GPU, and the
    /// image may be handed back to the guest.
    Presented,
    /// Not taken — serve this flush through the copy path. With `retry` the
    /// display may take a later one (its GPU is not up yet, this image would
    /// not import); without, it never will (no GPU of its own, another GPU
    /// than the renderer's), and the device stops asking.
    Declined {
        /// Why, for the log.
        reason: String,
        /// Whether asking again later may succeed.
        retry: bool,
    },
    /// Tried and failed (the GPU did not finish in time, the device was
    /// lost): the flush fails in band and the window keeps its last frame.
    Failed(String),
}

impl SharedPresent {
    /// A permanent refusal.
    pub fn never(reason: impl Into<String>) -> Self {
        Self::Declined {
            reason: reason.into(),
            retry: false,
        }
    }

    /// A refusal for now.
    pub fn not_now(reason: impl Into<String>) -> Self {
        Self::Declined {
            reason: reason.into(),
            retry: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn image() -> Arc<SharedScanoutImage> {
        Arc::new(SharedScanoutImage {
            serial: SharedScanoutImage::next_serial(),
            resource_id: 7,
            handle: ExternalHandle::placeholder(),
            handle_type: HANDLE_TYPE_OPAQUE_WIN32,
            allocation_size: 0x80_0000,
            memory_type_index: 1,
            device_uuid: [1; 16],
            driver_uuid: [2; 16],
            info: SharedImageInfo {
                format: VK_FORMAT_B8G8R8A8_UNORM,
                flags: 0x8,
                view_formats: vec![VK_FORMAT_B8G8R8A8_UNORM, VK_FORMAT_B8G8R8A8_SRGB],
                usage: 0x1f,
                width: 1920,
                height: 1080,
            },
        })
    }

    #[test]
    fn the_scanout_path_parses_both_spellings_and_defaults_to_shared() {
        assert_eq!(ScanoutPath::parse("copy"), Some(ScanoutPath::Copy));
        assert_eq!(ScanoutPath::parse(" Readback "), Some(ScanoutPath::Copy));
        assert_eq!(ScanoutPath::parse("shared"), Some(ScanoutPath::Shared));
        assert_eq!(ScanoutPath::parse("zero-copy"), Some(ScanoutPath::Shared));
        assert_eq!(ScanoutPath::parse("sometimes"), None);
        assert_eq!(ScanoutPath::default(), ScanoutPath::Shared);
    }

    #[test]
    fn a_lease_ends_its_claim_exactly_once_when_dropped() {
        let ended = Arc::new(AtomicBool::new(false));
        let lease = {
            let ended = Arc::clone(&ended);
            SharedScanoutLease::new(
                image(),
                ImageRelease {
                    layout: 6,
                    family: QUEUE_FAMILY_FOREIGN,
                },
                move || assert!(!ended.swap(true, Ordering::SeqCst), "ended twice"),
            )
        };
        assert!(!ended.load(Ordering::SeqCst));
        assert!(format!("{lease:?}").contains("claimed: true"));
        drop(lease);
        assert!(ended.load(Ordering::SeqCst));
        drop(SharedScanoutLease::unclaimed(
            image(),
            ImageRelease {
                layout: 6,
                family: QUEUE_FAMILY_FOREIGN,
            },
        ));
    }

    #[test]
    fn serials_are_never_reused_and_placeholders_name_nothing() {
        let (a, b) = (image(), image());
        assert_ne!(a.serial, b.serial);
        assert!(a.handle.is_placeholder());
        assert_eq!(format!("{:?}", a.handle), "ExternalHandle(placeholder)");
        assert!(a.info.is_bgra8());
        let rgba = SharedImageInfo {
            format: 37,
            ..a.info.clone()
        };
        assert!(!rgba.is_bgra8());
    }

    #[test]
    fn the_foreign_and_external_families_are_vulkans() {
        assert_eq!(QUEUE_FAMILY_FOREIGN, 0xffff_fffd);
        assert_eq!(QUEUE_FAMILY_EXTERNAL, 0xffff_fffe);
        assert!(matches!(
            SharedPresent::never("x"),
            SharedPresent::Declined { retry: false, .. }
        ));
        assert!(matches!(
            SharedPresent::not_now("x"),
            SharedPresent::Declined { retry: true, .. }
        ));
    }
}
