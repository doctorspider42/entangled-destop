//! The archive-side trust root: `dists/<suite>/Release` (MVP-605/606/607).
//!
//! # Why this exists
//!
//! The backlog assumes every artifact directory ships `SHA512SUMS` plus
//! `SHA512SUMS.sign`. That holds for the CD images on `cdimage.debian.org`, but
//! **not** for the netboot kernel/initrd: the installer directory
//! `dists/stable/main/installer-<arch>/current/images/` only publishes
//! `MD5SUMS` and `SHA256SUMS`, with no detached signature anywhere. The only
//! signature covering those files is the archive `Release` file, exactly as
//! `apt` uses it:
//!
//! ```text
//! Release.gpg  --(pinned archive keyring)-->  Release
//!              --(SHA256 entry)-->  images/SHA256SUMS
//!              --(SHA256 entry)-->  netboot/.../linux
//! ```
//!
//! So the netboot path gains one hop compared to the CD path but keeps the same
//! property: no byte reaches the cache unless a pinned OpenPGP key signed
//! something that transitively commits to its digest.
//!
//! The `Release` file also carries `Version:` (e.g. `13.6`) and `Codename:`,
//! which is how the netboot flow discovers the release version without pinning
//! it anywhere in code.
//!
//! TODO(post-MVP): archive keys rotate per Debian release, so
//! `keyring::DEBIAN_ARCHIVE` needs a new pinned key each time a new stable ships
//! (the CD keys are long-lived and do not have this problem). Track the
//! `debian-archive-keyring` package and add the next suite's key before it
//! becomes `stable`.

use thiserror::Error;

use crate::digest::DigestAlgo;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReleaseError {
    #[error("not a Debian Release file: no {0} field")]
    MissingField(&'static str),

    #[error("no {0} section")]
    MissingHashSection(&'static str),

    #[error("{0:?} is not listed in the Release file")]
    FileNotListed(String),

    #[error("malformed hash line for {0:?}")]
    MalformedHashLine(String),
}

/// The parts of a Debian archive `Release` file this crate needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// `Suite:` — e.g. `stable`.
    pub suite: Option<String>,
    /// `Codename:` — e.g. `trixie`.
    pub codename: Option<String>,
    /// `Version:` — e.g. `13.6`. Absent in `testing`/`unstable`.
    pub version: Option<String>,
    /// Entries of the `SHA256:` section: (hex digest, size, path).
    entries: Vec<ReleaseEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReleaseEntry {
    sha256_hex: String,
    size: u64,
    path: String,
}

/// The digest algorithm used by the `Release` → index-file hop. Debian's
/// `Release` files always carry a `SHA256:` section (`MD5Sum:` is legacy and
/// deliberately ignored here).
pub const RELEASE_INDEX_ALGO: DigestAlgo = DigestAlgo::Sha256;

impl Release {
    /// Parses the RFC 822-style `Release` file. Unknown fields and hash
    /// sections other than `SHA256:` are ignored.
    ///
    /// The input is untrusted at parse time — the signature is verified over
    /// the same bytes before this is called, but the parser still must not
    /// panic on anything.
    pub fn parse(content: &str) -> Result<Self, ReleaseError> {
        let mut suite = None;
        let mut codename = None;
        let mut version = None;
        let mut entries = Vec::new();
        let mut in_sha256 = false;

        for line in content.lines() {
            // Continuation lines (hash entries) start with a space or tab.
            if line.starts_with([' ', '\t']) {
                if !in_sha256 {
                    continue;
                }
                let mut parts = line.split_whitespace();
                match (parts.next(), parts.next(), parts.next()) {
                    (Some(digest), Some(size), Some(path)) => {
                        if digest.len() != RELEASE_INDEX_ALGO.hex_len()
                            || !digest.bytes().all(|b| b.is_ascii_hexdigit())
                        {
                            return Err(ReleaseError::MalformedHashLine(path.to_string()));
                        }
                        let size = size
                            .parse::<u64>()
                            .map_err(|_| ReleaseError::MalformedHashLine(path.to_string()))?;
                        entries.push(ReleaseEntry {
                            sha256_hex: digest.to_ascii_lowercase(),
                            size,
                            path: path.to_string(),
                        });
                    }
                    _ => return Err(ReleaseError::MalformedHashLine(line.trim().to_string())),
                }
                continue;
            }

            let Some((field, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            in_sha256 = field == "SHA256";
            // An empty value is a *missing* field, not a field whose value is
            // the empty string: `Some("")` would otherwise reach the provenance
            // manifest and the netboot version discovery as a real answer
            // (found by the MVP-1402 release-parser fuzz target).
            if value.is_empty() {
                continue;
            }
            match field {
                "Suite" => suite = Some(value.to_string()),
                "Codename" => codename = Some(value.to_string()),
                "Version" => version = Some(value.to_string()),
                _ => {}
            }
        }

        if entries.is_empty() {
            return Err(ReleaseError::MissingHashSection("SHA256"));
        }
        if suite.is_none() && codename.is_none() {
            return Err(ReleaseError::MissingField("Suite"));
        }
        Ok(Release {
            suite,
            codename,
            version,
            entries,
        })
    }

    /// Signed SHA-256 and size of an index file, by its archive-relative path
    /// (e.g. `main/installer-amd64/current/images/SHA256SUMS`).
    pub fn index_digest(&self, path: &str) -> Result<(&str, u64), ReleaseError> {
        self.entries
            .iter()
            .find(|e| e.path == path)
            .map(|e| (e.sha256_hex.as_str(), e.size))
            .ok_or_else(|| ReleaseError::FileNotListed(path.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
Origin: Debian
Label: Debian
Suite: stable
Version: 13.6
Codename: trixie
Acquire-By-Hash: yes
MD5Sum:
 3f9227b1aa510c20408dc34132c2df9e  1959586 contrib/Contents-all
 126d39239a2a1f7ca1c943baf770ac3a    80699 main/installer-amd64/current/images/SHA256SUMS
SHA256:
 52b5d72836b8a4ea01853ad7baf174e4d59ef7bcc5dd4a6f5c51a1812e815ad8    80699 main/installer-amd64/current/images/SHA256SUMS
 aa5e1b0d2c1f7f4d9b9d4b7f5c3a2e1d0f9e8d7c6b5a4938271605f4e3d2c1b0  1959586 contrib/Contents-all
";

    #[test]
    fn parses_fields_and_the_sha256_section() {
        let r = Release::parse(SAMPLE).unwrap();
        assert_eq!(r.suite.as_deref(), Some("stable"));
        assert_eq!(r.codename.as_deref(), Some("trixie"));
        assert_eq!(r.version.as_deref(), Some("13.6"));
        let (digest, size) = r
            .index_digest("main/installer-amd64/current/images/SHA256SUMS")
            .unwrap();
        assert_eq!(
            digest,
            "52b5d72836b8a4ea01853ad7baf174e4d59ef7bcc5dd4a6f5c51a1812e815ad8"
        );
        assert_eq!(size, 80_699);
    }

    /// The MD5Sum section lists the very same path with a 32-hex digest. If the
    /// parser leaked MD5 entries into the index we would be trusting MD5.
    #[test]
    fn md5_section_is_ignored_entirely() {
        let r = Release::parse(SAMPLE).unwrap();
        let (digest, _) = r
            .index_digest("main/installer-amd64/current/images/SHA256SUMS")
            .unwrap();
        assert_eq!(digest.len(), 64);
        assert_eq!(r.entries.len(), 2, "only the SHA256 section is collected");
    }

    #[test]
    fn unlisted_paths_are_an_error() {
        let r = Release::parse(SAMPLE).unwrap();
        assert_eq!(
            r.index_digest("main/installer-arm64/current/images/SHA256SUMS"),
            Err(ReleaseError::FileNotListed(
                "main/installer-arm64/current/images/SHA256SUMS".into()
            ))
        );
    }

    #[test]
    fn rejects_a_release_without_a_sha256_section() {
        let text = "Suite: stable\nMD5Sum:\n abc 1 x\n";
        assert_eq!(
            Release::parse(text),
            Err(ReleaseError::MissingHashSection("SHA256"))
        );
    }

    #[test]
    fn rejects_a_truncated_hash_line() {
        let text = "Suite: stable\nSHA256:\n 52b5d728 80699 main/x\n";
        assert!(matches!(
            Release::parse(text),
            Err(ReleaseError::MalformedHashLine(_))
        ));
    }

    #[test]
    fn rejects_a_file_that_is_not_a_release_file() {
        assert!(Release::parse("").is_err());
        assert!(Release::parse("hello world").is_err());
    }
}
