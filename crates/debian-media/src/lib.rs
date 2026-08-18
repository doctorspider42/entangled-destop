//! Debian media handling (backlog EPIC 6): resolving what to download from
//! the `stable`/`current` channels, verifying it (OpenPGP over SHA512SUMS,
//! then SHA-512 of the artifact) and recording provenance manifests.
//!
//! Nothing here pins a release number: the current version is discovered from
//! channel metadata at fetch time (backlog section 4).

mod manifest;
mod source;
mod sums;

pub use manifest::Manifest;
pub use source::{Arch, DebianStableSource, InstallerVariant, MediaKind, MediaSource};
pub use sums::{parse_sums, version_from_iso_name, SumsEntry, SumsError};
