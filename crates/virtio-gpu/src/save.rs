//! What a suspended virtio-gpu writes down, and what it can put back
//! ([ADR-0006](../../../docs/adr/0006-suspend-restore.md)).
//!
//! # The 2D half comes back; the 3D half cannot
//!
//! A 2D resource is a host BGRA buffer plus the list of **guest** pages the
//! driver attached to it. The pages are guest memory, which the snapshot
//! already carries in full — so the host buffer does not have to be saved at
//! all. Recording each resource's identity, geometry and backing list, and
//! then re-running the transfer the guest would have run, rebuilds it exactly.
//! That is the difference between a resumed desktop and a black window, and it
//! costs a few hundred bytes per resource instead of eight megabytes.
//!
//! A 3D resource is a texture inside the host's GL driver, reached through
//! virglrenderer, and there is no interface — in virglrenderer or in GL — that
//! hands its contents back in a form another process could reload. Neither can
//! a rendering *context*: it is a live command-stream state machine. So the 3D
//! half is not saved. A guest that had 3D contexts open is told its device
//! needs a reset when it resumes, which is the same signal GPU-012 already
//! sends when the isolated renderer crashes (ADR-0004) — and the same recovery
//! path, one the guest's driver stack has already been observed to take.
//!
//! # Wire format
//!
//! Written in the same style as [`crate::protocol`]: explicit little-endian
//! fields, every read bounds-checked, no `repr(C)` casts and no `unsafe`. The
//! bytes come out of a file, so they are treated exactly like a guest command.
//!
//! ```text
//!   version           u32   = GPU_STATE_VERSION
//!   events_read       u32
//!   live_3d_contexts  u32   how many the guest had open (0 on a 2D device)
//!   live_blobs        u32   how many blob resources it held (VEN-2001)
//!   resource_count    u32
//!   per resource: id u32 | format u32 | width u32 | height u32
//!                 backing_count u32 | (addr u64, length u32) *
//!   scanout_present   u32   0 or 1
//!   scanout: resource_id u32 | x u32 | y u32 | w u32 | h u32
//!            source u32 (0 = 2D, 1 = 3D, 2 = blob) | stride u32 | offset u32
//! ```

use crate::protocol::{MemEntry, Rect};

/// Version of the virtio-gpu device blob.
///
/// **2** since Venus phase 1 (VEN-2001): the scanout's source became a
/// three-way choice and blob resources joined the table. A version-1 snapshot
/// describes a device that could not have had either, but its scanout record
/// is a different shape, so it is refused by number rather than misread — and
/// the *section* version around it (`vm_snapshot::devices::VIRTIO_VERSION`)
/// refuses it one layer earlier, with a message that names both versions.
pub const GPU_STATE_VERSION: u32 = 2;

/// Refuses a blob that claims more resources than the device could ever hold,
/// before anything is allocated. [`crate::resource::MAX_RESOURCES`] is the real
/// bound; this is the parse-time one and they are the same number.
const MAX_RESOURCES: u32 = crate::resource::MAX_RESOURCES as u32;

/// Same, for one resource's backing list.
const MAX_BACKING: u32 = crate::resource::MAX_BACKING_ENTRIES;

/// Bytes one backing entry costs **here**: a `u64` address and a `u32` length.
///
/// Deliberately not [`MemEntry::LEN`], which is 16 — the *guest's* wire format
/// pads each entry to a 16-byte boundary, and this one does not. Using the wire
/// size as the count check's minimum made the parser demand a third more bytes
/// than the writer had produced, and refused every resource with a scattered
/// backing list. It survived the small-guest test because that guest's
/// framebuffer happened to be one contiguous entry; an installed Ubuntu's
/// 1280x800 scanout has two thousand.
const SAVED_ENTRY_BYTES: usize = 8 + 4;

/// One 2D resource, as a snapshot records it: everything except the pixels,
/// which are re-derived from the guest pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedResource {
    pub id: u32,
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub backing: Vec<MemEntry>,
}

/// Which half of the device owned the scanout's pixels, as a snapshot records
/// it.
///
/// A copy of `device::ScanoutSource`'s shape rather than the type itself: that
/// one is private to the device and free to change, this one is the wire format
/// and is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavedScanoutSource {
    /// The host 2D table. The only one a restore can rebuild.
    TwoD,
    /// The 3D renderer.
    ThreeD,
    /// A blob resource (VEN-2001), with the layout `SET_SCANOUT_BLOB` gave it.
    Blob { stride: u32, offset: u32 },
}

impl SavedScanoutSource {
    /// True when the pixels live somewhere a restore cannot reach.
    pub fn is_host_owned(self) -> bool {
        !matches!(self, SavedScanoutSource::TwoD)
    }

    const fn code(self) -> u32 {
        match self {
            SavedScanoutSource::TwoD => 0,
            SavedScanoutSource::ThreeD => 1,
            SavedScanoutSource::Blob { .. } => 2,
        }
    }
}

/// The scanout binding, if one was live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SavedScanout {
    pub resource_id: u32,
    pub rect: Rect,
    pub source: SavedScanoutSource,
}

/// Everything virtio-gpu carries across a suspend.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GpuState {
    pub events_read: u32,
    /// How many 3D rendering contexts the guest had open. Non-zero means the
    /// restored device must tell the driver to start again.
    pub live_3d_contexts: u32,
    /// How many blob resources the guest held (VEN-2001). Same consequence, for
    /// the same reason: a blob is a host-visible mapping into a window this
    /// process no longer owns, and a `HOST3D` one's bytes never left the
    /// renderer at all. Counted rather than described, because there is nothing
    /// useful to describe — only the driver can make them again.
    pub live_blobs: u32,
    pub resources: Vec<SavedResource>,
    pub scanout: Option<SavedScanout>,
}

/// Why a saved GPU blob could not be read.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GpuStateError {
    #[error("virtio-gpu state is at version {found}, this build reads version {expected}")]
    Version { found: u32, expected: u32 },

    #[error("virtio-gpu state is truncated: {what} needs {need} bytes, {have} are left")]
    Truncated {
        what: &'static str,
        need: usize,
        have: usize,
    },

    #[error("virtio-gpu state claims {value} {what}, more than the {max} this build accepts")]
    TooMany {
        what: &'static str,
        value: u32,
        max: u32,
    },

    #[error("virtio-gpu state has {0} bytes of trailing data this build does not understand")]
    Trailing(usize),

    #[error("virtio-gpu state carries the invalid value {value} for {what}")]
    BadValue { what: &'static str, value: u32 },
}

/// A bounds-checked forward reader, in the style [`crate::protocol`] uses for
/// guest commands — because this input deserves the same treatment.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn left(&self) -> usize {
        self.bytes.len().saturating_sub(self.at)
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, GpuStateError> {
        let raw = self
            .bytes
            .get(self.at..self.at.saturating_add(4))
            .and_then(|s| <[u8; 4]>::try_from(s).ok())
            .ok_or(GpuStateError::Truncated {
                what,
                need: 4,
                have: self.left(),
            })?;
        self.at += 4;
        Ok(u32::from_le_bytes(raw))
    }

    fn u64(&mut self, what: &'static str) -> Result<u64, GpuStateError> {
        let raw = self
            .bytes
            .get(self.at..self.at.saturating_add(8))
            .and_then(|s| <[u8; 8]>::try_from(s).ok())
            .ok_or(GpuStateError::Truncated {
                what,
                need: 8,
                have: self.left(),
            })?;
        self.at += 8;
        Ok(u64::from_le_bytes(raw))
    }

    fn bool(&mut self, what: &'static str) -> Result<bool, GpuStateError> {
        match self.u32(what)? {
            0 => Ok(false),
            1 => Ok(true),
            value => Err(GpuStateError::BadValue { what, value }),
        }
    }

    /// A count, refused above `max` and above what the remaining bytes could
    /// possibly hold — the rule that keeps a hostile blob from steering an
    /// allocation.
    fn count(
        &mut self,
        what: &'static str,
        max: u32,
        element_bytes: usize,
    ) -> Result<u32, GpuStateError> {
        let value = self.u32(what)?;
        if value > max {
            return Err(GpuStateError::TooMany { what, value, max });
        }
        let need = (value as usize).saturating_mul(element_bytes);
        if need > self.left() {
            return Err(GpuStateError::Truncated {
                what,
                need,
                have: self.left(),
            });
        }
        Ok(value)
    }
}

fn put32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

impl GpuState {
    /// Encodes the blob.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.resources.len() * 64);
        put32(&mut out, GPU_STATE_VERSION);
        put32(&mut out, self.events_read);
        put32(&mut out, self.live_3d_contexts);
        put32(&mut out, self.live_blobs);
        put32(&mut out, self.resources.len() as u32);
        for resource in &self.resources {
            put32(&mut out, resource.id);
            put32(&mut out, resource.format);
            put32(&mut out, resource.width);
            put32(&mut out, resource.height);
            put32(&mut out, resource.backing.len() as u32);
            for entry in &resource.backing {
                put64(&mut out, entry.addr);
                put32(&mut out, entry.length);
            }
        }
        match &self.scanout {
            Some(scanout) => {
                put32(&mut out, 1);
                put32(&mut out, scanout.resource_id);
                put32(&mut out, scanout.rect.x);
                put32(&mut out, scanout.rect.y);
                put32(&mut out, scanout.rect.width);
                put32(&mut out, scanout.rect.height);
                put32(&mut out, scanout.source.code());
                let (stride, offset) = match scanout.source {
                    SavedScanoutSource::Blob { stride, offset } => (stride, offset),
                    _ => (0, 0),
                };
                put32(&mut out, stride);
                put32(&mut out, offset);
            }
            None => put32(&mut out, 0),
        }
        out
    }

    /// Decodes the blob. Every failure is a value; nothing here panics or
    /// allocates on an unchecked length.
    pub fn decode(bytes: &[u8]) -> Result<Self, GpuStateError> {
        let mut r = Cursor::new(bytes);
        let version = r.u32("version")?;
        if version != GPU_STATE_VERSION {
            return Err(GpuStateError::Version {
                found: version,
                expected: GPU_STATE_VERSION,
            });
        }
        let events_read = r.u32("events_read")?;
        let live_3d_contexts = r.u32("3d contexts")?;
        let live_blobs = r.u32("blob resources")?;
        // 20 bytes is the smallest a resource record can be (four fields plus a
        // zero backing count).
        let count = r.count("resources", MAX_RESOURCES, 20)?;
        let mut resources = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let id = r.u32("resource id")?;
            let format = r.u32("resource format")?;
            let width = r.u32("resource width")?;
            let height = r.u32("resource height")?;
            let entries = r.count("backing entries", MAX_BACKING, SAVED_ENTRY_BYTES)?;
            let mut backing = Vec::with_capacity(entries as usize);
            for _ in 0..entries {
                backing.push(MemEntry {
                    addr: r.u64("backing address")?,
                    length: r.u32("backing length")?,
                });
            }
            resources.push(SavedResource {
                id,
                format,
                width,
                height,
                backing,
            });
        }
        let scanout = if r.bool("scanout present")? {
            let resource_id = r.u32("scanout resource")?;
            let rect = Rect {
                x: r.u32("scanout x")?,
                y: r.u32("scanout y")?,
                width: r.u32("scanout width")?,
                height: r.u32("scanout height")?,
            };
            let code = r.u32("scanout source")?;
            let stride = r.u32("scanout stride")?;
            let offset = r.u32("scanout offset")?;
            let source = match code {
                0 | 1 if stride != 0 || offset != 0 => {
                    // A non-blob source has no layout, so a non-zero one means
                    // the two halves of the codec disagree — and the encoding
                    // would then have two spellings for one state.
                    return Err(GpuStateError::BadValue {
                        what: "scanout layout on a non-blob source",
                        value: stride | offset,
                    });
                }
                0 => SavedScanoutSource::TwoD,
                1 => SavedScanoutSource::ThreeD,
                2 => SavedScanoutSource::Blob { stride, offset },
                value => {
                    return Err(GpuStateError::BadValue {
                        what: "scanout source",
                        value,
                    })
                }
            };
            Some(SavedScanout {
                resource_id,
                rect,
                source,
            })
        } else {
            None
        };
        if r.left() != 0 {
            return Err(GpuStateError::Trailing(r.left()));
        }
        Ok(Self {
            events_read,
            live_3d_contexts,
            live_blobs,
            resources,
            scanout,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> GpuState {
        GpuState {
            events_read: 0,
            live_3d_contexts: 2,
            live_blobs: 0,
            resources: vec![
                SavedResource {
                    id: 1,
                    format: 2,
                    width: 1920,
                    height: 1080,
                    backing: vec![
                        MemEntry {
                            addr: 0x1000,
                            length: 4096,
                        },
                        MemEntry {
                            addr: 0x8000,
                            length: 8192,
                        },
                    ],
                },
                SavedResource {
                    id: 7,
                    format: 2,
                    width: 64,
                    height: 64,
                    backing: Vec::new(),
                },
            ],
            scanout: Some(SavedScanout {
                resource_id: 1,
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
                source: SavedScanoutSource::TwoD,
            }),
        }
    }

    /// The bug an installed Ubuntu found and the small guest did not: a
    /// resource whose backing is *scattered* over many pages, which is what a
    /// real framebuffer looks like. The count check's minimum element size has
    /// to be the size this module writes, not the guest's padded wire size.
    #[test]
    fn a_scattered_backing_list_round_trips() {
        let backing: Vec<MemEntry> = (0..2048)
            .map(|i| MemEntry {
                addr: 0x1_0000 + i * 4096,
                length: 4096,
            })
            .collect();
        let state = GpuState {
            events_read: 0,
            live_3d_contexts: 0,
            live_blobs: 0,
            resources: vec![SavedResource {
                id: 1,
                format: 2,
                width: 1280,
                height: 800,
                backing,
            }],
            scanout: Some(SavedScanout {
                resource_id: 1,
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 1280,
                    height: 800,
                },
                source: SavedScanoutSource::TwoD,
            }),
        };
        let bytes = state.encode();
        assert_eq!(GpuState::decode(&bytes).unwrap(), state);
        // And the encoder and the parser's own minimum agree, which is the
        // invariant that broke.
        // header (5 u32) + one resource record (5 u32) + its backing + the
        // present flag and the eight-word scanout record.
        assert_eq!(
            bytes.len(),
            20 + 20 + 2048 * SAVED_ENTRY_BYTES + 36,
            "the encoder and SAVED_ENTRY_BYTES disagree"
        );
    }

    #[test]
    fn the_state_round_trips() {
        let state = sample();
        assert_eq!(GpuState::decode(&state.encode()).unwrap(), state);
    }

    #[test]
    fn a_state_without_a_scanout_round_trips() {
        let state = GpuState {
            scanout: None,
            ..sample()
        };
        assert_eq!(GpuState::decode(&state.encode()).unwrap(), state);
    }

    /// Every prefix of a valid blob is an error, never a panic and never a
    /// half-built state.
    #[test]
    fn every_truncation_is_a_typed_error() {
        let bytes = sample().encode();
        for cut in 0..bytes.len() {
            let _ = GpuState::decode(&bytes[..cut]).unwrap_err();
        }
    }

    #[test]
    fn trailing_bytes_are_refused() {
        let mut bytes = sample().encode();
        bytes.push(0);
        assert!(matches!(
            GpuState::decode(&bytes).unwrap_err(),
            GpuStateError::Trailing(1)
        ));
    }

    #[test]
    fn a_wrong_version_is_refused_by_number() {
        let mut bytes = sample().encode();
        bytes[0..4].copy_from_slice(&99u32.to_le_bytes());
        assert!(matches!(
            GpuState::decode(&bytes).unwrap_err(),
            GpuStateError::Version { found: 99, .. }
        ));
    }

    /// The rule the cursor exists for: an absurd count costs four bytes of
    /// work, not an allocation.
    #[test]
    fn an_absurd_resource_count_is_refused_without_allocating() {
        let mut bytes = header(u32::MAX);
        assert!(matches!(
            GpuState::decode(&bytes).unwrap_err(),
            GpuStateError::TooMany { .. }
        ));

        // And one inside the format bound but past the end of the input.
        bytes = header(60);
        assert!(matches!(
            GpuState::decode(&bytes).unwrap_err(),
            GpuStateError::Truncated { .. }
        ));
    }

    #[test]
    fn an_absurd_backing_count_is_refused_too() {
        let mut bytes = header(1);
        for value in [1u32, 2, 8, 8] {
            put32(&mut bytes, value);
        }
        put32(&mut bytes, u32::MAX);
        assert!(matches!(
            GpuState::decode(&bytes).unwrap_err(),
            GpuStateError::TooMany { .. }
        ));
    }

    #[test]
    fn a_non_boolean_scanout_flag_is_refused() {
        let mut bytes = header(0);
        put32(&mut bytes, 7);
        assert!(matches!(
            GpuState::decode(&bytes).unwrap_err(),
            GpuStateError::BadValue { value: 7, .. }
        ));
    }

    /// The scanout source is a three-way enum now (VEN-2001), and every value
    /// outside it is a refusal — as is a layout on a source that has none,
    /// which would give the encoding two spellings for one state.
    #[test]
    fn an_unknown_scanout_source_is_refused() {
        let scanout = |code: u32, stride: u32, offset: u32| {
            let mut bytes = header(0);
            put32(&mut bytes, 1); // present
            for value in [1u32, 0, 0, 64, 64] {
                put32(&mut bytes, value);
            }
            put32(&mut bytes, code);
            put32(&mut bytes, stride);
            put32(&mut bytes, offset);
            bytes
        };
        assert!(matches!(
            GpuState::decode(&scanout(9, 0, 0)).unwrap_err(),
            GpuStateError::BadValue { value: 9, .. }
        ));
        // A 2D or 3D scanout carrying a blob layout.
        for code in [0u32, 1] {
            assert!(matches!(
                GpuState::decode(&scanout(code, 256, 0)).unwrap_err(),
                GpuStateError::BadValue { .. }
            ));
        }
        // A real blob scanout is fine.
        let state = GpuState::decode(&scanout(2, 256, 4096)).unwrap();
        assert_eq!(
            state.scanout.unwrap().source,
            SavedScanoutSource::Blob {
                stride: 256,
                offset: 4096
            }
        );
    }

    /// Blob resources and 3D contexts are both counted, and both survive the
    /// round trip — they are what tells a restored device to ask its driver to
    /// start again.
    #[test]
    fn the_host_side_counts_round_trip() {
        let state = GpuState {
            live_3d_contexts: 3,
            live_blobs: 7,
            scanout: Some(SavedScanout {
                resource_id: 4,
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 800,
                    height: 600,
                },
                source: SavedScanoutSource::Blob {
                    stride: 3200,
                    offset: 0,
                },
            }),
            ..sample()
        };
        let back = GpuState::decode(&state.encode()).unwrap();
        assert_eq!(back, state);
        assert!(back.scanout.unwrap().source.is_host_owned());
    }

    /// A version-1 blob — everything written before Venus phase 1 — is refused
    /// by number rather than misread against the wider record.
    #[test]
    fn a_version_1_blob_is_refused_by_number() {
        let mut bytes = sample().encode();
        bytes[0..4].copy_from_slice(&1u32.to_le_bytes());
        assert!(matches!(
            GpuState::decode(&bytes).unwrap_err(),
            GpuStateError::Version {
                found: 1,
                expected: GPU_STATE_VERSION
            }
        ));
    }

    /// The fixed prefix every hand-built test blob starts with, ending in a
    /// resource count.
    fn header(resources: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        put32(&mut bytes, GPU_STATE_VERSION);
        put32(&mut bytes, 0); // events_read
        put32(&mut bytes, 0); // live 3D contexts
        put32(&mut bytes, 0); // live blobs
        put32(&mut bytes, resources);
        bytes
    }
}
