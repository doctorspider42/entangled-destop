//! virtio-gpu 2D device (backlog EPIC 8).
//!
//! Current state: control protocol constants and the resource/scanout model
//! types. Command processing arrives with the mmio transport; VirGL/Rutabaga
//! 3D is a separate post-MVP milestone (GPU-0xx).

/// 2D control commands we implement in the MVP (`VIRTIO_GPU_CMD_*`).
pub mod cmd {
    pub const GET_DISPLAY_INFO: u32 = 0x0100;
    pub const RESOURCE_CREATE_2D: u32 = 0x0101;
    pub const RESOURCE_UNREF: u32 = 0x0102;
    pub const SET_SCANOUT: u32 = 0x0103;
    pub const RESOURCE_FLUSH: u32 = 0x0104;
    pub const TRANSFER_TO_HOST_2D: u32 = 0x0105;
    pub const RESOURCE_ATTACH_BACKING: u32 = 0x0106;
    pub const RESOURCE_DETACH_BACKING: u32 = 0x0107;
}

/// Response types (`VIRTIO_GPU_RESP_*`).
pub mod resp {
    pub const OK_NODATA: u32 = 0x1100;
    pub const OK_DISPLAY_INFO: u32 = 0x1101;
    pub const ERR_UNSPEC: u32 = 0x1200;
    pub const ERR_OUT_OF_MEMORY: u32 = 0x1201;
    pub const ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
    pub const ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
    pub const ERR_INVALID_PARAMETER: u32 = 0x1205;
}

/// The only pixel format the MVP guarantees (MVP-809); what Linux fbdev and
/// Weston's DRM backend pick by default.
pub const FORMAT_B8G8R8A8_UNORM: u32 = 2;

/// Bytes per pixel for [`FORMAT_B8G8R8A8_UNORM`].
pub const BYTES_PER_PIXEL: u32 = 4;

/// Upper bound on a single 2D resource, sized for 4K with headroom; blocks a
/// guest from requesting absurd host allocations.
pub const MAX_RESOURCE_PIXELS: u64 = 4096 * 2304;

/// A rectangle in resource coordinates (used by transfer/flush dirty rects).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    /// True when the rect lies fully inside a `w`×`h` resource; guards every
    /// guest-supplied rect before any host copy (acceptance: "VM cannot force
    /// copies outside its memory").
    pub fn fits_within(&self, w: u32, h: u32) -> bool {
        let x_end = u64::from(self.x) + u64::from(self.width);
        let y_end = u64::from(self.y) + u64::from(self.height);
        self.width > 0 && self.height > 0 && x_end <= u64::from(w) && y_end <= u64::from(h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_bounds() {
        let full = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        assert!(full.fits_within(1920, 1080));
        let off = Rect {
            x: 1,
            y: 0,
            width: 1920,
            height: 1080,
        };
        assert!(!off.fits_within(1920, 1080));
        let empty = Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 10,
        };
        assert!(!empty.fits_within(1920, 1080));
        // u32 overflow must not wrap into "fits".
        let evil = Rect {
            x: u32::MAX,
            y: 0,
            width: 2,
            height: 2,
        };
        assert!(!evil.fits_within(1920, 1080));
    }
}
