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
    /// Lower-case hex SHA-512 of the artifact, verified against the signed
    /// SHA512SUMS.
    pub sha512_hex: String,
    /// True only if SHA512SUMS.sign was verified against the Debian keyring
    /// before the digest check. A false here never leaves the downloader —
    /// unverified artifacts are deleted, not persisted.
    pub signature_verified: bool,
}

impl Manifest {
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_through_toml() {
        let m = Manifest {
            url: "https://cdimage.debian.org/debian-cd/current/amd64/iso-cd/debian-13.6.0-amd64-netinst.iso".into(),
            version: "13.6.0".into(),
            fetched_at: "2026-08-18T12:00:00Z".into(),
            sha512_hex: "ab".repeat(64),
            signature_verified: true,
        };
        let s = m.to_toml().unwrap();
        assert_eq!(Manifest::from_toml(&s).unwrap(), m);
    }
}
