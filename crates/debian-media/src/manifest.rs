//! Provenance manifest written next to every verified artifact (MVP-610).
//! An artifact without a manifest is treated as unverified and re-fetched.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    /// Exact URL the artifact was downloaded from.
    pub url: String,
    /// Debian release version discovered at fetch time (e.g. "13.6.0").
    pub version: String,
    /// RFC 3339 UTC timestamp of the completed, verified download.
    pub fetched_at: String,
    /// Lower-case hex digest of the artifact, verified against the signed
    /// checksum file. SHA-512 for CD media, SHA-256 for archive-side installer
    /// images — the length identifies which.
    pub sha512_hex: String,
    /// True only if the checksum file's OpenPGP signature was verified against
    /// a pinned Debian keyring *before* the digest check. A false here never
    /// leaves the downloader — unverified artifacts are deleted, not persisted.
    pub signature_verified: bool,
    /// Lower-case hex fingerprint of the key that signed the trust root, and the
    /// keyring it came from. Optional so manifests written by older versions
    /// still load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyring: Option<String>,
}

impl Manifest {
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// The digest algorithm implied by [`Self::sha512_hex`].
    pub fn algo(&self) -> Option<crate::digest::DigestAlgo> {
        crate::digest::DigestAlgo::from_hex_len(self.sha512_hex.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::DigestAlgo;

    fn sample() -> Manifest {
        Manifest {
            url: "https://cdimage.debian.org/debian-cd/current/amd64/iso-cd/debian-13.6.0-amd64-netinst.iso".into(),
            version: "13.6.0".into(),
            fetched_at: "2026-08-18T12:00:00Z".into(),
            sha512_hex: "ab".repeat(64),
            signature_verified: true,
            signed_by: Some("df9b9c49eaa9298432589d76da87e80d6294be9b".into()),
            keyring: Some("Debian CD".into()),
        }
    }

    #[test]
    fn roundtrips_through_toml() {
        let m = sample();
        let s = m.to_toml().unwrap();
        assert_eq!(Manifest::from_toml(&s).unwrap(), m);
    }

    #[test]
    fn toml_records_the_fields_the_backlog_asks_for() {
        let s = sample().to_toml().unwrap();
        for needle in [
            "url = ",
            "version = \"13.6.0\"",
            "fetched_at = \"2026-08-18T12:00:00Z\"",
            "sha512_hex = ",
            "signature_verified = true",
        ] {
            assert!(s.contains(needle), "{needle:?} missing from:\n{s}");
        }
    }

    /// Manifests without the optional provenance fields must still load: they
    /// were written before those fields existed.
    #[test]
    fn older_manifests_without_optional_fields_still_load() {
        let text = "\
url = \"https://example.invalid/linux\"
version = \"13.6\"
fetched_at = \"2026-08-18T12:00:00Z\"
sha512_hex = \"abcd\"
signature_verified = true
";
        let m = Manifest::from_toml(text).unwrap();
        assert_eq!(m.signed_by, None);
        assert_eq!(m.keyring, None);
        assert_eq!(m.algo(), None, "a 4-char digest is not a known algorithm");
    }

    #[test]
    fn digest_algorithm_is_inferred_from_the_digest_length() {
        let mut m = sample();
        assert_eq!(m.algo(), Some(DigestAlgo::Sha512));
        m.sha512_hex = "ab".repeat(32);
        assert_eq!(m.algo(), Some(DigestAlgo::Sha256));
    }
}
