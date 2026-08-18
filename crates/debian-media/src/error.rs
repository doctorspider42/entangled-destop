//! Typed errors for the Debian media downloader (EPIC 6).

use std::path::PathBuf;

use thiserror::Error;

use crate::http::TransportError;
use crate::release::ReleaseError;
use crate::sums::SumsError;

/// Everything that can go wrong between "user typed `vmhost fetch`" and
/// "verified artifact plus manifest sits in the cache".
#[derive(Debug, Error)]
pub enum MediaError {
    #[error("cannot fetch {url}: {source}")]
    Transport {
        url: String,
        #[source]
        source: TransportError,
    },

    #[error("{url}: {source}")]
    Sums {
        url: String,
        #[source]
        source: SumsError,
    },

    #[error("{url}: {source}")]
    Release {
        url: String,
        #[source]
        source: ReleaseError,
    },

    /// The OpenPGP trust chain failed. Nothing downstream of this is trusted:
    /// a matching digest with an unverified checksum file still counts as a
    /// failure (MVP-607).
    #[error("OpenPGP verification of {url} failed against the pinned {keyring} keyring: {reason}")]
    Signature {
        url: String,
        keyring: &'static str,
        reason: String,
    },

    /// A pinned key file failed to load, or its fingerprint did not match the
    /// fingerprint hardcoded next to it. Always a bug in this crate, never
    /// something a remote server can trigger.
    #[error("pinned keyring {keyring} is unusable: {reason}")]
    Keyring {
        keyring: &'static str,
        reason: String,
    },

    #[error("{file} is not listed in the signed checksum file {sums_url}")]
    NotListed { file: String, sums_url: String },

    #[error("{url} is not valid UTF-8 and cannot be a Debian control file")]
    NotUtf8 { url: String },

    #[error(
        "{algo} mismatch for {url}: signed sums say {expected}, downloaded data hashes to {actual}"
    )]
    DigestMismatch {
        url: String,
        algo: &'static str,
        expected: String,
        actual: String,
    },

    /// The transfer ended before the server-announced length. The partial file
    /// is deliberately **kept** so the next run resumes it (MVP-609); only
    /// verification failures purge the cache.
    #[error("{url} was interrupted after {received} of {expected} bytes; rerun to resume")]
    Interrupted {
        url: String,
        received: u64,
        expected: u64,
    },

    #[error("nothing usable in the cache for {variant} ({arch}) — rerun without --offline")]
    NothingCached {
        variant: &'static str,
        arch: &'static str,
    },

    #[error("no plain debian-<version>-{arch}-netinst.iso found in {sums_url}")]
    NoNetinstIso { arch: String, sums_url: String },

    #[error("cannot determine the Debian release version from {source_desc}")]
    UnknownVersion { source_desc: String },

    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot locate a cache directory: set XDG_CACHE_HOME or HOME")]
    NoCacheDir,

    #[error("cannot serialize the provenance manifest: {0}")]
    ManifestWrite(#[from] toml::ser::Error),

    /// The server sent more bytes than the signed checksum file accounts for,
    /// or an unbounded control file exceeded its sanity limit.
    #[error("{url} is larger than the accepted limit of {limit} bytes")]
    TooLarge { url: String, limit: u64 },

    #[error("unsupported distribution {0:?}: only \"debian\" is available")]
    UnknownDistro(String),

    #[error("unsupported channel {0:?}: only \"stable\" is available")]
    UnknownChannel(String),

    #[error("unsupported architecture {0:?}: only \"amd64\" is available")]
    UnknownArch(String),

    #[error("unsupported variant {0:?}: expected text-netboot, gtk-netboot or netinst-iso")]
    UnknownVariant(String),
}

impl MediaError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        MediaError::Io {
            path: path.into(),
            source,
        }
    }

    /// Whether this failure means "the bytes in the cache are not trustworthy",
    /// in which case the partial artifact must be deleted (backlog acceptance
    /// criterion), as opposed to "the transfer did not finish", where the
    /// partial is the thing that makes the next run resumable.
    pub fn invalidates_cache(&self) -> bool {
        match self {
            MediaError::DigestMismatch { .. }
            | MediaError::Signature { .. }
            | MediaError::Sums { .. }
            | MediaError::Release { .. }
            | MediaError::NotUtf8 { .. }
            | MediaError::NotListed { .. }
            | MediaError::TooLarge { .. }
            | MediaError::ManifestWrite(_) => true,

            MediaError::Interrupted { .. }
            | MediaError::Transport { .. }
            | MediaError::Io { .. }
            | MediaError::Keyring { .. }
            | MediaError::NoCacheDir
            | MediaError::NothingCached { .. }
            | MediaError::NoNetinstIso { .. }
            | MediaError::UnknownVersion { .. }
            | MediaError::UnknownDistro(_)
            | MediaError::UnknownChannel(_)
            | MediaError::UnknownArch(_)
            | MediaError::UnknownVariant(_) => false,
        }
    }
}
