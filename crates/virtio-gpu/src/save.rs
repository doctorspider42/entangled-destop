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
//!   resource_count    u32
//!   per resource: id u32 | format u32 | width u32 | height u32
//!                 backing_count u32 | (addr u64, length u32) *
//!   scanout_present   u32   0 or 1
//!   scanout: resource_id u32 | x u32 | y u32 | w u32 | h u32 | three_d u32
//! ```

use crate::protocol::{MemEntry, Rect};

/// Version of the virtio-gpu device blob.
pub const GPU_STATE_VERSION: u32 = 1;

/// Refuses a blob that claims more resources than the device could ever hold,
/// before anything is allocated. [`crate::resource::MAX_RESOURCES`] is the real
/// bound; this is the parse-time one and they are the same number.
const MAX_RESOURCES: u32 = crate::resource::MAX_RESOURCES as u32;

/// Same, for one resource's backing list.
const MAX_BACKING: u32 = crate::resource::MAX_BACKING_ENTRIES;

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

/// The scanout binding, if one was live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SavedScanout {
    pub resource_id: u32,
    pub rect: Rect,
    pub three_d: bool,
}

/// Everything virtio-gpu carries across a suspend.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GpuState {
    pub events_read: u32,
    /// How many 3D rendering contexts the guest had open. Non-zero means the
    /// restored device must tell the driver to start again.
    pub live_3d_contexts: u32,
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
                put32(&mut out, u32::from(scanout.three_d));
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
        // 20 bytes is the smallest a resource record can be (four fields plus a
        // zero backing count).
        let count = r.count("resources", MAX_RESOURCES, 20)?;
        let mut resources = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let id = r.u32("resource id")?;
            let format = r.u32("resource format")?;
            let width = r.u32("resource width")?;
            let height = r.u32("resource height")?;
            let entries = r.count("backing entries", MAX_BACKING, MemEntry::LEN)?;
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
            Some(SavedScanout {
                resource_id: r.u32("scanout resource")?,
                rect: Rect {
                    x: r.u32("scanout x")?,
                    y: r.u32("scanout y")?,
                    width: r.u32("scanout width")?,
                    height: r.u32("scanout height")?,
                },
                three_d: r.bool("scanout three_d")?,
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
                three_d: false,
            }),
        }
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
        let mut bytes = Vec::new();
        put32(&mut bytes, GPU_STATE_VERSION);
        put32(&mut bytes, 0);
        put32(&mut bytes, 0);
        put32(&mut bytes, u32::MAX);
        assert!(matches!(
            GpuState::decode(&bytes).unwrap_err(),
            GpuStateError::TooMany { .. }
        ));

        // And one inside the format bound but past the end of the input.
        let mut bytes = Vec::new();
        put32(&mut bytes, GPU_STATE_VERSION);
        put32(&mut bytes, 0);
        put32(&mut bytes, 0);
        put32(&mut bytes, 60);
        assert!(matches!(
            GpuState::decode(&bytes).unwrap_err(),
            GpuStateError::Truncated { .. }
        ));
    }

    #[test]
    fn an_absurd_backing_count_is_refused_too() {
        let mut bytes = Vec::new();
        put32(&mut bytes, GPU_STATE_VERSION);
        put32(&mut bytes, 0);
        put32(&mut bytes, 0);
        put32(&mut bytes, 1);
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
        let mut bytes = Vec::new();
        put32(&mut bytes, GPU_STATE_VERSION);
        put32(&mut bytes, 0);
        put32(&mut bytes, 0);
        put32(&mut bytes, 0);
        put32(&mut bytes, 7);
        assert!(matches!(
            GpuState::decode(&bytes).unwrap_err(),
            GpuStateError::BadValue { value: 7, .. }
        ));
    }
}
