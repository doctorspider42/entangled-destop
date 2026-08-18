//! Debian media handling (backlog EPIC 6): resolving what to download from
//! the `stable`/`current` channels, verifying it (OpenPGP over the signed
//! checksum root, then the artifact's own digest) and recording provenance
//! manifests.
//!
//! Nothing here pins a release number: the current version is discovered from
//! channel metadata at fetch time (backlog section 4).
//!
//! # Trust chain
//!
//! ```text
//! pinned OpenPGP keys (crates/debian-media/keys, fingerprints in keyring.rs)
//!         │
//!         ├─ netinst ISO : SHA512SUMS.sign → SHA512SUMS → ISO
//!         └─ netboot     : Release.gpg     → Release → SHA256SUMS → linux/initrd.gz
//!                 │
//!                 └→ provenance Manifest written next to the artifact
//! ```
//!
//! The signature is always verified **before** any digest from the checksum
//! file is used, and the manifest is written **after** both checks pass. See
//! [`fetch`] for the exact ordering and the failure-cleanup rules.
//!
//! # Portability
//!
//! This crate builds and tests on any host OS: it contains no `cfg(target_os)`
//! gates and its only platform interaction is the filesystem cache and a
//! rustls-based HTTP client.

mod cache;
mod digest;
mod error;
mod fetch;
mod http;
mod keyring;
mod manifest;
mod release;
mod rfc3339;
mod source;
mod sums;

pub use cache::{manifest_path, partial_path, purge, MediaCache, MANIFEST_SUFFIX, PARTIAL_SUFFIX};
pub use digest::{DigestAlgo, Hasher};
pub use error::MediaError;
pub use fetch::{
    FetchOptions, FetchReport, FetchStatus, FetchedArtifact, Fetcher, Provenance, MAX_ARTIFACT_LEN,
};
pub use http::{Download, Transport, TransportError, UreqTransport, MAX_CONTROL_FILE_LEN};
pub use keyring::{
    verify_detached, Keyring, PinnedKey, VerifiedBy, DEBIAN_ARCHIVE, DEBIAN_ARCHIVE_FINGERPRINTS,
    DEBIAN_CD, DEBIAN_CD_FINGERPRINTS,
};
pub use manifest::Manifest;
pub use release::{Release, ReleaseError};
pub use rfc3339::{format_unix_utc, now_utc};
pub use source::{Arch, DebianStableSource, InstallerVariant, MediaKind, MediaSource, TrustRoot};
pub use sums::{
    find_entry, find_plain_netinst_iso, parse_sums, parse_sums_with, version_from_iso_name,
    SumsEntry, SumsError,
};

/// Convenience entry point for `entangled fetch debian` (MVP-1201).
///
/// Validates the user-facing strings, resolves the cache location from the
/// environment and runs the full verified fetch against the real network.
pub fn fetch_debian(
    distro: &str,
    channel: &str,
    arch: &str,
    variant: &str,
    options: FetchOptions,
) -> Result<FetchReport, MediaError> {
    if !distro.eq_ignore_ascii_case("debian") {
        return Err(MediaError::UnknownDistro(distro.to_string()));
    }
    // `stable` is the only channel the MVP resolves; `current` is the same thing
    // spelled the way cdimage.debian.org spells it.
    if !matches!(channel, "stable" | "current") {
        return Err(MediaError::UnknownChannel(channel.to_string()));
    }
    let arch: Arch = arch
        .parse()
        .map_err(|_| MediaError::UnknownArch(arch.to_string()))?;
    let variant: InstallerVariant = variant
        .parse()
        .map_err(|_| MediaError::UnknownVariant(variant.to_string()))?;

    let fetcher = Fetcher::new(
        DebianStableSource::new(arch),
        UreqTransport::new(),
        MediaCache::discover()?,
        arch,
    );
    fetcher.fetch_with(variant, options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_arguments_are_validated_before_any_network_access() {
        let opts = FetchOptions::default();
        assert!(matches!(
            fetch_debian("ubuntu", "stable", "amd64", "gtk-netboot", opts),
            Err(MediaError::UnknownDistro(_))
        ));
        assert!(matches!(
            fetch_debian("debian", "sid", "amd64", "gtk-netboot", opts),
            Err(MediaError::UnknownChannel(_))
        ));
        assert!(matches!(
            fetch_debian("debian", "stable", "riscv64", "gtk-netboot", opts),
            Err(MediaError::UnknownArch(_))
        ));
        assert!(matches!(
            fetch_debian("debian", "stable", "amd64", "dvd", opts),
            Err(MediaError::UnknownVariant(_))
        ));
    }
}
