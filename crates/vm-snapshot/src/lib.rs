//! `vm-snapshot` — writing a running VM to a file, and putting it back.
//!
//! The container ([`format`]) is deliberately dull: a magic, a version, a run
//! of digested sections and an index. What is interesting is everything it
//! refuses. A snapshot outlives the build that wrote it and travels between
//! machines, so this crate treats it as **untrusted input** — every length is
//! checked before anything is allocated, every section is digested, and every
//! way the machine underneath could have changed since the snapshot was taken
//! is a named error rather than a guest that misbehaves an hour later
//! ([`meta`]).
//!
//! # Where the state comes from
//!
//! Nothing below this crate depends on it. The state lives with whatever owns
//! it — the neutral CPU state in `vmm_core::hv`, the transport state in
//! `virtio_core`, the machine's devices in `machine_x86` — and this crate is
//! the one place that knows how any of it is spelled on disk. That is what
//! keeps the hypervisor seam free of a serialization format, and what makes
//! "the encoding changed" a one-file review.
//!
//! # Layout
//!
//! | Module | Owns |
//! |---|---|
//! | [`codec`] | bounds-checked little-endian primitives |
//! | [`format`] | header, section index, digests, the host/version refusals |
//! | [`meta`] | the VM's shape and the disk fingerprints |
//! | [`memory`] | guest RAM, zero pages skipped |
//! | [`cpu`] | one vCPU's architectural state |
//! | [`devices`] | virtio transports and the machine's own devices |
//! | [`vm`] | the whole-VM save and restore, and [`vm::inspect`] for a GUI |

pub mod codec;
pub mod cpu;
pub mod devices;
mod error;
pub mod format;
pub mod memory;
pub mod meta;
pub mod vm;

pub use error::{Result, SnapshotError};
pub use format::{
    Arch, HostKind, SectionEntry, SectionKind, SnapshotReader, SnapshotWriter, MAGIC, VERSION,
};
pub use memory::{Codec, MemoryStats, SaveOptions, Unreported};
pub use meta::{DeviceSlot, FileFingerprint, FileRole, MachineShape, Metadata};
pub use vm::{inspect, SnapshotInfo, SnapshotSummary};

/// Conventional extension for a snapshot file.
pub const EXTENSION: &str = "esnap";

/// The build that wrote a snapshot, for the metadata section.
pub fn writer_id() -> String {
    format!("entangled {}", env!("CARGO_PKG_VERSION"))
}
