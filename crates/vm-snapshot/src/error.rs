//! Every way reading a snapshot can go wrong, as a typed refusal.
//!
//! A snapshot file is **untrusted input**. It is host-side data rather than
//! guest-side, but it outlives the build that wrote it, it can be copied
//! between machines, truncated by a full disk, or handed over by someone else —
//! so every failure here is a value, never a panic and never an out-of-bounds
//! read. The parser allocates nothing on a length it has not first checked
//! against the bytes actually present.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("snapshot I/O failed while {what}: {source}")]
    Io {
        what: &'static str,
        #[source]
        source: std::io::Error,
    },

    #[error("not an Entangled snapshot: the file does not start with the {expected:?} magic")]
    NotASnapshot { expected: &'static str },

    #[error(
        "snapshot format version {found} cannot be restored by this build (it writes and reads \
         version {expected})"
    )]
    UnsupportedVersion { found: u32, expected: u32 },

    #[error("snapshot header carries unknown flags {flags:#x}; it was written by a newer build")]
    UnknownFlags { flags: u32 },

    #[error(
        "snapshot was taken on {snapshot} and this is {host}: a saved CPU carries that \
         hypervisor's own interrupt-controller and extended-state blobs, which the other one \
         cannot load"
    )]
    ForeignHost { snapshot: String, host: String },

    #[error("snapshot was taken on {snapshot} and this machine is {host}")]
    ForeignArch { snapshot: String, host: String },

    #[error("snapshot is truncated: {what} needs {need} more bytes, {have} are left")]
    Truncated {
        what: &'static str,
        need: u64,
        have: u64,
    },

    #[error("{what} has {left} bytes of trailing data this build does not understand")]
    TrailingBytes { what: &'static str, left: usize },

    #[error("{what} carries the invalid value {value:#x}")]
    BadValue { what: &'static str, value: u64 },

    #[error("{what} claims {value} entries, more than the {max} this build accepts")]
    TooLarge {
        what: &'static str,
        value: u64,
        max: u64,
    },

    #[error("{what} is not valid UTF-8")]
    NotUtf8 { what: &'static str },

    #[error(
        "snapshot section {section} is corrupt: its contents do not match the recorded digest"
    )]
    Corrupt { section: String },

    #[error("snapshot is missing the {0} section")]
    MissingSection(&'static str),

    #[error("snapshot carries two {section} sections for instance {instance}")]
    DuplicateSection { section: String, instance: u32 },

    #[error(
        "snapshot carries a section of kind {kind} this build does not know; restoring it would \
         silently drop state the guest is expecting"
    )]
    UnknownSection { kind: u32 },

    #[error(
        "snapshot section {section} is at version {found}, this build reads version {expected}"
    )]
    SectionVersion {
        section: &'static str,
        found: u32,
        expected: u32,
    },

    #[error(
        "the VM has changed since the snapshot was taken: {field} was {snapshot}, is {current}"
    )]
    Mismatch {
        field: String,
        snapshot: String,
        current: String,
    },

    #[error(
        "disk {path} has changed since the snapshot was taken ({what}: was {snapshot}, is \
         {current}); restoring onto it would corrupt the guest's filesystem"
    )]
    DiskChanged {
        path: String,
        what: &'static str,
        snapshot: String,
        current: String,
    },

    #[error("cannot restore: {0}")]
    Restore(String),
}

impl SnapshotError {
    pub(crate) fn io(what: &'static str) -> impl FnOnce(std::io::Error) -> Self {
        move |source| SnapshotError::Io { what, source }
    }
}

pub type Result<T> = std::result::Result<T, SnapshotError>;
