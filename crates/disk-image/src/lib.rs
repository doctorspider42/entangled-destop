//! `disk-image` — everything the product knows about RAW disk images, shared
//! between the `entangled` CLI and the manager GUI (which links this code
//! rather than shelling out for read-only views).
//!
//! * [`layout`] — MBR/GPT/ext4 parsing. **Everything a partition table says is
//!   untrusted guest data**; see the module docs for the defensive rules.
//! * [`ops`] — filesystem-level operations: size parsing, sparse creation,
//!   grow-only resize, apparent-vs-allocated size, free space, hole punching
//!   and zeroing (the host half of virtio-blk DISCARD / WRITE_ZEROES), the
//!   `.nvram` sidecar convention.
//! * [`inspect`] — one [`DiskReport`] combining the two, with JSON output for
//!   `entangled disk inspect --json`.
//! * [`refs`] — which VM profiles reference a disk (the `disk rm` guard).
//! * [`relocate`] — sparse-preserving, verified move of an image (and its
//!   sidecar) to another directory or drive.
//!
//! Portable by design (ADR-0002): the platform-specific parts (allocated
//! size, sparse marking, allocated-range enumeration, free space) live behind
//! small `cfg` functions with graceful fallbacks; all parsing and policy is
//! plain std.

pub mod inspect;
pub mod layout;
pub mod ops;
pub mod refs;
pub mod relocate;

pub use inspect::{inspect_disk, DiskReport, PartitionReport, TableKind};
pub use layout::{
    find_installed_root, find_uefi_install, has_protective_mbr, parse_mbr, read_partition_table,
    DiskFsError, GptPartition, Guid, InstalledRoot, Partition, PartitionTable, UefiInstall,
    ESP_TYPE, LINUX_FS_TYPE, LINUX_ROOT_X64_TYPE, SECTOR,
};
pub use ops::{
    allocated_bytes, create_raw, disk_space, existing_nvram_sidecar, format_bytes, mark_sparse,
    nvram_sidecar_path, parse_size, punch_hole, resize_raw, write_zeroes, DiskError, PunchOutcome,
    ResizeOutcome,
};
pub use refs::{default_scan_dirs, find_references, remove_disk, ProfileRef, RemoveError};
pub use relocate::{move_disk, MoveError, MoveOutcome};

/// Shared synthetic-image builders for the tests of several modules.
#[cfg(test)]
pub(crate) mod fixtures;
