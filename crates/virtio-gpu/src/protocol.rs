//! virtio-gpu control protocol: wire layout, parsing and encoding
//! (VirtIO spec 1.2, section 5.7 — backlog MVP-801…810).
//!
//! Pure logic: no guest memory, no display, no transport. Everything here runs
//! on any host OS, which is where the bulk of the untrusted-input checking is
//! unit-tested.
//!
//! # Why explicit `from_le_bytes` instead of `repr(C)` casts
//!
//! Every structure below has a fixed little-endian wire layout whose length is
//! asserted at compile time (`*_LEN`, checked against the spec in the tests).
//! The Rust types are plain structs with hand-written `parse`/`to_bytes`
//! functions rather than `repr(C)` types reinterpreted from guest bytes: a cast
//! would need `unsafe` (or a pod-cast dependency), would depend on the host's
//! padding rules for `struct virtio_gpu_ctrl_hdr`'s `ring_idx` + `padding[3]`
//! tail, and would give a *reference* into a guest-controlled buffer. Copying
//! 24 bytes per command costs nothing measurable next to a framebuffer
//! transfer, and it keeps this module free of `unsafe`.
//!
//! Every `parse` is total: it returns `None` for a buffer that is too short and
//! never indexes out of bounds, never panics.

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

    /// `VIRTIO_GPU_CMD_GET_EDID` — needs `VIRTIO_GPU_F_EDID`, which the MVP
    /// does not offer (MVP-811, P1). Listed so the dispatcher can log it by
    /// name when a curious driver tries anyway.
    pub const GET_EDID: u32 = 0x010a;

    /// Cursor-queue commands (MVP-812, P1). The cursor queue is drained but
    /// the commands are not acted upon yet.
    pub const UPDATE_CURSOR: u32 = 0x0300;
    pub const MOVE_CURSOR: u32 = 0x0301;
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

/// `VIRTIO_GPU_FLAG_FENCE`: the request carries a fence the device must signal.
pub const FLAG_FENCE: u32 = 1 << 0;
/// `VIRTIO_GPU_FLAG_INFO_RING_IDX`: `ring_idx` in the header is meaningful.
pub const FLAG_INFO_RING_IDX: u32 = 1 << 1;

/// Length of `struct virtio_gpu_ctrl_hdr`.
pub const CTRL_HDR_LEN: usize = 24;
/// Length of `struct virtio_gpu_rect`.
pub const RECT_LEN: usize = 16;
/// Length of `struct virtio_gpu_display_one`.
pub const DISPLAY_ONE_LEN: usize = RECT_LEN + 8;
/// `VIRTIO_GPU_MAX_SCANOUTS`: the pmodes array is always this long on the wire.
pub const MAX_SCANOUTS: usize = 16;
/// Length of the `struct virtio_gpu_resp_display_info` body (after the header).
pub const DISPLAY_INFO_BODY_LEN: usize = MAX_SCANOUTS * DISPLAY_ONE_LEN;
/// Length of `struct virtio_gpu_mem_entry`.
pub const MEM_ENTRY_LEN: usize = 16;
/// Length of `struct virtio_gpu_config`.
pub const CONFIG_LEN: usize = 16;

// Compile-time layout gate: a typo in the constants above would silently
// mis-parse every guest command.
const _: () = {
    assert!(CTRL_HDR_LEN == 24);
    assert!(RECT_LEN == 16);
    assert!(DISPLAY_ONE_LEN == 24);
    assert!(DISPLAY_INFO_BODY_LEN == 384);
    assert!(CtrlHdr::LEN + DISPLAY_INFO_BODY_LEN == 408);
    assert!(ResourceCreate2d::LEN == 40);
    assert!(ResourceUnref::LEN == 32);
    assert!(SetScanout::LEN == 48);
    assert!(ResourceFlush::LEN == 48);
    assert!(TransferToHost2d::LEN == 56);
    assert!(AttachBacking::LEN == 32);
};

// ------------------------------------------------------------ byte helpers

/// Little-endian `u32` at `at`. Zero when the slice is too short — every caller
/// has already length-checked the buffer, this is the no-panic fallback.
fn le32(bytes: &[u8], at: usize) -> u32 {
    match bytes
        .get(at..at + 4)
        .and_then(|s| <[u8; 4]>::try_from(s).ok())
    {
        Some(raw) => u32::from_le_bytes(raw),
        None => 0,
    }
}

/// Little-endian `u64` at `at`; see [`le32`].
fn le64(bytes: &[u8], at: usize) -> u64 {
    match bytes
        .get(at..at + 8)
        .and_then(|s| <[u8; 8]>::try_from(s).ok())
    {
        Some(raw) => u64::from_le_bytes(raw),
        None => 0,
    }
}

fn put32(out: &mut [u8], at: usize, value: u32) {
    if let Some(slot) = out.get_mut(at..at + 4) {
        slot.copy_from_slice(&value.to_le_bytes());
    }
}

fn put64(out: &mut [u8], at: usize, value: u64) {
    if let Some(slot) = out.get_mut(at..at + 8) {
        slot.copy_from_slice(&value.to_le_bytes());
    }
}

// -------------------------------------------------------------------- rect

/// A rectangle in resource coordinates (`struct virtio_gpu_rect`), also used
/// for transfer/flush dirty rects and for the scanout region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    /// Wire length of `struct virtio_gpu_rect`.
    pub const LEN: usize = RECT_LEN;

    /// Parses a rect at `at`. `None` when the buffer is too short.
    pub fn parse(bytes: &[u8], at: usize) -> Option<Self> {
        if bytes.len() < at.checked_add(Self::LEN)? {
            return None;
        }
        Some(Self {
            x: le32(bytes, at),
            y: le32(bytes, at + 4),
            width: le32(bytes, at + 8),
            height: le32(bytes, at + 12),
        })
    }

    /// Writes the rect at `at`; short buffers are left untouched.
    pub fn encode(&self, out: &mut [u8], at: usize) {
        put32(out, at, self.x);
        put32(out, at + 4, self.y);
        put32(out, at + 8, self.width);
        put32(out, at + 12, self.height);
    }

    /// True when the rect lies fully inside a `w`×`h` resource; guards every
    /// guest-supplied rect before any host copy (acceptance: "VM cannot force
    /// copies outside its memory").
    pub fn fits_within(&self, w: u32, h: u32) -> bool {
        let x_end = u64::from(self.x) + u64::from(self.width);
        let y_end = u64::from(self.y) + u64::from(self.height);
        self.width > 0 && self.height > 0 && x_end <= u64::from(w) && y_end <= u64::from(h)
    }

    /// Number of pixels covered, as a `u64` so nothing overflows.
    pub fn pixels(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    /// The overlap of two rects, or `None` when they do not overlap.
    ///
    /// Used to clip a guest flush rect to the region the scanout actually
    /// shows, so a flush that reaches outside the visible area updates the
    /// visible part instead of being rejected (that is what Linux' fbdev
    /// deferred-io flushes do at mode-change time).
    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        // Right/bottom edges as u64: x + width cannot overflow that way.
        let right = (u64::from(self.x) + u64::from(self.width))
            .min(u64::from(other.x) + u64::from(other.width));
        let bottom = (u64::from(self.y) + u64::from(self.height))
            .min(u64::from(other.y) + u64::from(other.height));
        if right <= u64::from(x) || bottom <= u64::from(y) {
            return None;
        }
        Some(Rect {
            x,
            y,
            // Both differences are positive and bounded by the inputs' u32
            // extents, so the casts cannot truncate.
            width: (right - u64::from(x)) as u32,
            height: (bottom - u64::from(y)) as u32,
        })
    }
}

// ------------------------------------------------------------------ header

/// `struct virtio_gpu_ctrl_hdr`, the first 24 bytes of every request and every
/// response on both queues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CtrlHdr {
    /// `VIRTIO_GPU_CMD_*` on the way in, `VIRTIO_GPU_RESP_*` on the way out.
    pub kind: u32,
    pub flags: u32,
    pub fence_id: u64,
    pub ctx_id: u32,
    pub ring_idx: u8,
}

impl CtrlHdr {
    /// Wire length of `struct virtio_gpu_ctrl_hdr`.
    pub const LEN: usize = CTRL_HDR_LEN;

    /// Parses the header from the start of `bytes`.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::LEN {
            return None;
        }
        Some(Self {
            kind: le32(bytes, 0),
            flags: le32(bytes, 4),
            fence_id: le64(bytes, 8),
            ctx_id: le32(bytes, 16),
            ring_idx: bytes.get(20).copied().unwrap_or(0),
        })
    }

    /// Serialises the header. The three padding bytes are always zero.
    pub fn to_bytes(self) -> [u8; CTRL_HDR_LEN] {
        let mut out = [0u8; CTRL_HDR_LEN];
        put32(&mut out, 0, self.kind);
        put32(&mut out, 4, self.flags);
        put64(&mut out, 8, self.fence_id);
        put32(&mut out, 16, self.ctx_id);
        out[20] = self.ring_idx;
        out
    }

    /// True when the request asks for a fence.
    pub fn wants_fence(&self) -> bool {
        self.flags & FLAG_FENCE != 0
    }

    /// The response header for a request with this header.
    ///
    /// Fencing is trivially immediate here: the device completes every command
    /// synchronously before it writes the response, so a request that carries
    /// `VIRTIO_GPU_FLAG_FENCE` is already "past the fence" when the driver sees
    /// the reply. Per spec the device then sets the flag in the response and
    /// echoes `fence_id` (plus `ctx_id`, and `ring_idx` when the request also
    /// set `VIRTIO_GPU_FLAG_INFO_RING_IDX`). Unfenced requests get a zeroed
    /// fence.
    pub fn response(&self, resp_type: u32) -> Self {
        if !self.wants_fence() {
            return Self {
                kind: resp_type,
                ..Self::default()
            };
        }
        let mut flags = FLAG_FENCE;
        let mut ring_idx = 0;
        if self.flags & FLAG_INFO_RING_IDX != 0 {
            flags |= FLAG_INFO_RING_IDX;
            ring_idx = self.ring_idx;
        }
        Self {
            kind: resp_type,
            flags,
            fence_id: self.fence_id,
            ctx_id: self.ctx_id,
            ring_idx,
        }
    }
}

// ----------------------------------------------------------------- commands

/// `struct virtio_gpu_resource_create_2d`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceCreate2d {
    pub resource_id: u32,
    pub format: u32,
    pub width: u32,
    pub height: u32,
}

impl ResourceCreate2d {
    /// Wire length including the header.
    pub const LEN: usize = CTRL_HDR_LEN + 16;

    /// Parses the command from a full command buffer (header included).
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::LEN {
            return None;
        }
        Some(Self {
            resource_id: le32(bytes, CTRL_HDR_LEN),
            format: le32(bytes, CTRL_HDR_LEN + 4),
            width: le32(bytes, CTRL_HDR_LEN + 8),
            height: le32(bytes, CTRL_HDR_LEN + 12),
        })
    }
}

/// `struct virtio_gpu_resource_unref` — also the layout of
/// `struct virtio_gpu_resource_detach_backing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceUnref {
    pub resource_id: u32,
}

impl ResourceUnref {
    /// Wire length including the header (`resource_id` + `padding`).
    pub const LEN: usize = CTRL_HDR_LEN + 8;

    /// Parses the command from a full command buffer (header included).
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::LEN {
            return None;
        }
        Some(Self {
            resource_id: le32(bytes, CTRL_HDR_LEN),
        })
    }
}

/// `struct virtio_gpu_set_scanout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetScanout {
    pub rect: Rect,
    pub scanout_id: u32,
    pub resource_id: u32,
}

impl SetScanout {
    /// Wire length including the header.
    pub const LEN: usize = CTRL_HDR_LEN + RECT_LEN + 8;

    /// Parses the command from a full command buffer (header included).
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::LEN {
            return None;
        }
        Some(Self {
            rect: Rect::parse(bytes, CTRL_HDR_LEN)?,
            scanout_id: le32(bytes, CTRL_HDR_LEN + RECT_LEN),
            resource_id: le32(bytes, CTRL_HDR_LEN + RECT_LEN + 4),
        })
    }
}

/// `struct virtio_gpu_resource_flush`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceFlush {
    pub rect: Rect,
    pub resource_id: u32,
}

impl ResourceFlush {
    /// Wire length including the header.
    pub const LEN: usize = CTRL_HDR_LEN + RECT_LEN + 8;

    /// Parses the command from a full command buffer (header included).
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::LEN {
            return None;
        }
        Some(Self {
            rect: Rect::parse(bytes, CTRL_HDR_LEN)?,
            resource_id: le32(bytes, CTRL_HDR_LEN + RECT_LEN),
        })
    }
}

/// `struct virtio_gpu_transfer_to_host_2d`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferToHost2d {
    pub rect: Rect,
    /// Byte offset of the rect's first pixel inside the backing store.
    pub offset: u64,
    pub resource_id: u32,
}

impl TransferToHost2d {
    /// Wire length including the header.
    pub const LEN: usize = CTRL_HDR_LEN + RECT_LEN + 16;

    /// Parses the command from a full command buffer (header included).
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::LEN {
            return None;
        }
        Some(Self {
            rect: Rect::parse(bytes, CTRL_HDR_LEN)?,
            offset: le64(bytes, CTRL_HDR_LEN + RECT_LEN),
            resource_id: le32(bytes, CTRL_HDR_LEN + RECT_LEN + 8),
        })
    }
}

/// Fixed part of `struct virtio_gpu_resource_attach_backing`; the
/// `nr_entries` [`MemEntry`] items follow it in the same chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachBacking {
    pub resource_id: u32,
    /// Guest-supplied entry count — validated before anything is allocated.
    pub nr_entries: u32,
}

impl AttachBacking {
    /// Wire length of the fixed part, header included.
    pub const LEN: usize = CTRL_HDR_LEN + 8;

    /// Parses the fixed part from a full command buffer (header included).
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::LEN {
            return None;
        }
        Some(Self {
            resource_id: le32(bytes, CTRL_HDR_LEN),
            nr_entries: le32(bytes, CTRL_HDR_LEN + 4),
        })
    }

    /// Byte length of a command carrying `nr_entries` entries, or `None` on
    /// overflow.
    pub fn total_len(nr_entries: u32) -> Option<usize> {
        usize::try_from(nr_entries)
            .ok()?
            .checked_mul(MEM_ENTRY_LEN)?
            .checked_add(Self::LEN)
    }
}

/// One `struct virtio_gpu_mem_entry` of a resource's backing store.
///
/// `addr` is a *guest physical address the guest chose*: it is never trusted,
/// never used to index host memory directly, and only ever read through the
/// checked `vm-memory` API at transfer time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemEntry {
    pub addr: u64,
    pub length: u32,
}

impl MemEntry {
    /// Wire length of one entry.
    pub const LEN: usize = MEM_ENTRY_LEN;

    /// Parses entry `index` of an attach-backing command buffer.
    pub fn parse_at(bytes: &[u8], index: u32) -> Option<Self> {
        let at = usize::try_from(index)
            .ok()?
            .checked_mul(Self::LEN)?
            .checked_add(AttachBacking::LEN)?;
        if bytes.len() < at.checked_add(Self::LEN)? {
            return None;
        }
        Some(Self {
            addr: le64(bytes, at),
            length: le32(bytes, at + 8),
        })
    }
}

// ---------------------------------------------------------------- responses

/// One entry of the `GET_DISPLAY_INFO` reply (`struct
/// virtio_gpu_display_one`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DisplayOne {
    pub rect: Rect,
    pub enabled: bool,
    pub flags: u32,
}

/// Encodes the body of `struct virtio_gpu_resp_display_info`: the fixed
/// 16-entry pmodes array, with `modes` at the front and zeroes (disabled
/// scanouts) after it. Extra modes beyond [`MAX_SCANOUTS`] are ignored.
pub fn display_info_body(modes: &[DisplayOne]) -> [u8; DISPLAY_INFO_BODY_LEN] {
    let mut out = [0u8; DISPLAY_INFO_BODY_LEN];
    for (index, mode) in modes.iter().take(MAX_SCANOUTS).enumerate() {
        let at = index * DISPLAY_ONE_LEN;
        mode.rect.encode(&mut out, at);
        put32(&mut out, at + RECT_LEN, u32::from(mode.enabled));
        put32(&mut out, at + RECT_LEN + 4, mode.flags);
    }
    out
}

/// Encodes `struct virtio_gpu_config` (MVP-801): `events_read`,
/// `events_clear`, `num_scanouts`, `num_capsets`.
///
/// `events_clear` always reads back as zero — it is a write-to-clear register.
pub fn config_bytes(events_read: u32, num_scanouts: u32, num_capsets: u32) -> [u8; CONFIG_LEN] {
    let mut out = [0u8; CONFIG_LEN];
    put32(&mut out, 0, events_read);
    put32(&mut out, 4, 0);
    put32(&mut out, 8, num_scanouts);
    put32(&mut out, 12, num_capsets);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sizes straight from the spec's `struct` definitions (section 5.7.6).
    #[test]
    fn wire_lengths_match_the_spec() {
        assert_eq!(CtrlHdr::LEN, 24);
        assert_eq!(Rect::LEN, 16);
        assert_eq!(DISPLAY_ONE_LEN, 24);
        assert_eq!(CtrlHdr::LEN + DISPLAY_INFO_BODY_LEN, 408);
        assert_eq!(ResourceCreate2d::LEN, 40);
        assert_eq!(ResourceUnref::LEN, 32);
        assert_eq!(SetScanout::LEN, 48);
        assert_eq!(ResourceFlush::LEN, 48);
        assert_eq!(TransferToHost2d::LEN, 56);
        assert_eq!(AttachBacking::LEN, 32);
        assert_eq!(MemEntry::LEN, 16);
        assert_eq!(CONFIG_LEN, 16);
        assert_eq!(AttachBacking::total_len(3), Some(32 + 48));
        // A count no chain could ever carry: huge on a 64-bit host, `None` on a
        // 32-bit one. What matters is that the multiplication never wraps into
        // a small, plausible-looking length.
        assert!(AttachBacking::total_len(u32::MAX).is_none_or(|len| len > u32::MAX as usize));
    }

    #[test]
    fn header_round_trips_including_the_ring_index() {
        let hdr = CtrlHdr {
            kind: cmd::RESOURCE_FLUSH,
            flags: FLAG_FENCE | FLAG_INFO_RING_IDX,
            fence_id: 0x0102_0304_0506_0708,
            ctx_id: 0xdead_beef,
            ring_idx: 3,
        };
        let bytes = hdr.to_bytes();
        assert_eq!(bytes.len(), CtrlHdr::LEN);
        // Field offsets on the wire.
        assert_eq!(&bytes[0..4], &cmd::RESOURCE_FLUSH.to_le_bytes());
        assert_eq!(&bytes[8..16], &0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(&bytes[16..20], &0xdead_beefu32.to_le_bytes());
        assert_eq!(bytes[20], 3);
        assert_eq!(&bytes[21..24], &[0, 0, 0], "padding must be zero");
        assert_eq!(CtrlHdr::parse(&bytes), Some(hdr));
    }

    #[test]
    fn truncated_buffers_parse_to_none_instead_of_panicking() {
        let full = [0u8; 64];
        for len in 0..CtrlHdr::LEN {
            assert_eq!(CtrlHdr::parse(&full[..len]), None, "len {len}");
        }
        assert!(CtrlHdr::parse(&full[..CtrlHdr::LEN]).is_some());
        assert_eq!(ResourceCreate2d::parse(&full[..39]), None);
        assert!(ResourceCreate2d::parse(&full[..40]).is_some());
        assert_eq!(SetScanout::parse(&full[..47]), None);
        assert_eq!(ResourceFlush::parse(&full[..47]), None);
        assert_eq!(TransferToHost2d::parse(&full[..55]), None);
        assert_eq!(AttachBacking::parse(&full[..31]), None);
        assert_eq!(MemEntry::parse_at(&full[..47], 0), None);
        assert!(MemEntry::parse_at(&full[..48], 0).is_some());
        assert_eq!(MemEntry::parse_at(&full[..48], 1), None);
        assert_eq!(Rect::parse(&full, usize::MAX), None);
        assert_eq!(MemEntry::parse_at(&full, u32::MAX), None);
    }

    #[test]
    fn commands_parse_field_by_field() {
        let mut buf = vec![0u8; 64];
        let hdr = CtrlHdr {
            kind: cmd::TRANSFER_TO_HOST_2D,
            ..CtrlHdr::default()
        };
        buf[..CtrlHdr::LEN].copy_from_slice(&hdr.to_bytes());
        let rect = Rect {
            x: 7,
            y: 9,
            width: 640,
            height: 480,
        };
        rect.encode(&mut buf, CtrlHdr::LEN);
        put64(&mut buf, CtrlHdr::LEN + RECT_LEN, 0x1_0000);
        put32(&mut buf, CtrlHdr::LEN + RECT_LEN + 8, 42);

        let cmd = TransferToHost2d::parse(&buf).expect("parses");
        assert_eq!(cmd.rect, rect);
        assert_eq!(cmd.offset, 0x1_0000);
        assert_eq!(cmd.resource_id, 42);
        assert_eq!(CtrlHdr::parse(&buf).map(|h| h.kind), Some(0x0105));
    }

    #[test]
    fn attach_backing_entries_parse_in_order() {
        let mut buf = vec![0u8; AttachBacking::total_len(2).expect("fits")];
        put32(&mut buf, CtrlHdr::LEN, 5);
        put32(&mut buf, CtrlHdr::LEN + 4, 2);
        put64(&mut buf, AttachBacking::LEN, 0x4000);
        put32(&mut buf, AttachBacking::LEN + 8, 0x1000);
        put64(&mut buf, AttachBacking::LEN + MemEntry::LEN, 0x9000);
        put32(&mut buf, AttachBacking::LEN + MemEntry::LEN + 8, 0x800);

        let fixed = AttachBacking::parse(&buf).expect("parses");
        assert_eq!((fixed.resource_id, fixed.nr_entries), (5, 2));
        assert_eq!(
            MemEntry::parse_at(&buf, 0),
            Some(MemEntry {
                addr: 0x4000,
                length: 0x1000
            })
        );
        assert_eq!(
            MemEntry::parse_at(&buf, 1),
            Some(MemEntry {
                addr: 0x9000,
                length: 0x800
            })
        );
        assert_eq!(MemEntry::parse_at(&buf, 2), None, "past the entry array");
    }

    #[test]
    fn fenced_requests_get_an_echoed_fence_in_the_response() {
        let plain = CtrlHdr {
            kind: cmd::RESOURCE_FLUSH,
            fence_id: 0x99,
            ctx_id: 7,
            ring_idx: 2,
            flags: 0,
        };
        let resp = plain.response(resp::OK_NODATA);
        assert_eq!(resp.kind, resp::OK_NODATA);
        assert_eq!(resp.flags, 0);
        assert_eq!(resp.fence_id, 0, "no fence requested, no fence returned");
        assert_eq!(resp.ctx_id, 0);
        assert_eq!(resp.ring_idx, 0);

        let fenced = CtrlHdr {
            flags: FLAG_FENCE,
            ..plain
        };
        let resp = fenced.response(resp::OK_NODATA);
        assert_eq!(resp.flags, FLAG_FENCE);
        assert_eq!(resp.fence_id, 0x99);
        assert_eq!(resp.ctx_id, 7);
        assert_eq!(resp.ring_idx, 0, "ring_idx is only echoed when announced");

        let ringed = CtrlHdr {
            flags: FLAG_FENCE | FLAG_INFO_RING_IDX,
            ..plain
        };
        let resp = ringed.response(resp::ERR_UNSPEC);
        assert_eq!(resp.flags, FLAG_FENCE | FLAG_INFO_RING_IDX);
        assert_eq!(resp.ring_idx, 2);
        assert_eq!(resp.kind, resp::ERR_UNSPEC);
    }

    #[test]
    fn rect_bounds() {
        let full = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        assert!(full.fits_within(1920, 1080));
        assert_eq!(full.pixels(), 1920 * 1080);
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
        let evil = Rect {
            x: 0,
            y: u32::MAX - 1,
            width: 4,
            height: 4,
        };
        assert!(!evil.fits_within(1920, 1080));
    }

    #[test]
    fn rect_intersection_clips_and_rejects_disjoint_rects() {
        let scanout = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 50,
        };
        let inside = Rect {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        };
        assert_eq!(scanout.intersect(&inside), Some(inside));
        let overhang = Rect {
            x: 90,
            y: 40,
            width: 40,
            height: 40,
        };
        assert_eq!(
            scanout.intersect(&overhang),
            Some(Rect {
                x: 90,
                y: 40,
                width: 10,
                height: 10
            })
        );
        let outside = Rect {
            x: 100,
            y: 0,
            width: 10,
            height: 10,
        };
        assert_eq!(scanout.intersect(&outside), None);
        // Overflowing extents clip instead of wrapping.
        let evil = Rect {
            x: u32::MAX - 1,
            y: 0,
            width: u32::MAX,
            height: u32::MAX,
        };
        assert_eq!(scanout.intersect(&evil), None);
        assert_eq!(evil.intersect(&scanout), None);
    }

    #[test]
    fn display_info_body_marks_only_the_given_scanouts() {
        let body = display_info_body(&[DisplayOne {
            rect: Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            enabled: true,
            flags: 0,
        }]);
        assert_eq!(body.len(), DISPLAY_INFO_BODY_LEN);
        assert_eq!(le32(&body, 0), 0);
        assert_eq!(le32(&body, 8), 1920);
        assert_eq!(le32(&body, 12), 1080);
        assert_eq!(le32(&body, 16), 1, "scanout 0 enabled");
        assert_eq!(le32(&body, 20), 0, "no flags");
        // Every other pmode is zeroed, i.e. disabled.
        assert!(body[DISPLAY_ONE_LEN..].iter().all(|b| *b == 0));

        // More modes than the wire array holds are ignored, not overflowed.
        let many = vec![DisplayOne::default(); MAX_SCANOUTS + 4];
        assert_eq!(display_info_body(&many).len(), DISPLAY_INFO_BODY_LEN);
    }

    #[test]
    fn config_layout() {
        let cfg = config_bytes(0x3, 1, 0);
        assert_eq!(le32(&cfg, 0), 3, "events_read");
        assert_eq!(le32(&cfg, 4), 0, "events_clear always reads zero");
        assert_eq!(le32(&cfg, 8), 1, "num_scanouts");
        assert_eq!(le32(&cfg, 12), 0, "num_capsets");
    }

    #[test]
    fn byte_helpers_never_read_out_of_bounds() {
        let short = [1u8, 2, 3];
        assert_eq!(le32(&short, 0), 0);
        assert_eq!(le64(&short, 0), 0);
        let mut out = [0u8; 3];
        put32(&mut out, 0, 0xffff_ffff);
        put64(&mut out, 0, u64::MAX);
        assert_eq!(out, [0, 0, 0], "short writes are dropped, not panics");
    }
}
