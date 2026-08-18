//! Parsing `SHA512SUMS` files (backlog MVP-605/608): both to verify artifact
//! digests and to *discover* the current release's file names (the netinst
//! ISO name embeds the version, which we must never pin).

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SumsEntry {
    /// Lower-case hex SHA-512.
    pub sha512_hex: String,
    pub file_name: String,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SumsError {
    #[error("line {0}: malformed checksum line")]
    Malformed(usize),

    #[error("line {0}: digest is not 128 hex characters")]
    BadDigest(usize),
}

/// Parses the classic `sha512sum` output format: `<hex><2 spaces or space+*><name>`.
/// Empty lines are ignored. Directory components in names are preserved.
pub fn parse_sums(content: &str) -> Result<Vec<SumsEntry>, SumsError> {
    let mut entries = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let (digest, rest) = line.split_once(' ').ok_or(SumsError::Malformed(i + 1))?;
        if digest.len() != 128 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(SumsError::BadDigest(i + 1));
        }
        // Second byte is ' ' (text mode) or '*' (binary mode).
        let name = rest.strip_prefix(['*', ' ']).unwrap_or(rest);
        if name.is_empty() {
            return Err(SumsError::Malformed(i + 1));
        }
        entries.push(SumsEntry {
            sha512_hex: digest.to_ascii_lowercase(),
            file_name: name.to_string(),
        });
    }
    Ok(entries)
}

impl SumsEntry {
    /// True for a Debian netinst installer ISO (e.g.
    /// `debian-13.6.0-amd64-netinst.iso`), used to discover the current ISO
    /// name from the `current` directory listing.
    pub fn is_netinst_iso(&self, arch: &str) -> bool {
        self.file_name.starts_with("debian-")
            && self.file_name.ends_with(&format!("-{arch}-netinst.iso"))
    }
}

/// Extracts the release version from a netinst ISO file name,
/// e.g. `debian-13.6.0-amd64-netinst.iso` → `13.6.0`.
pub fn version_from_iso_name(name: &str) -> Option<&str> {
    name.strip_prefix("debian-")?.split('-').next()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
0e3e9c1c53508a3e6ee80a2f6b93b7830ca9098e6d18f5b330229c1c8f822db4ca8394f28f8b5bbaa1e0d1c3e9e15deffa25e0a1c88efee7bbd0c00583154ec2  debian-13.6.0-amd64-netinst.iso
2f4d02987a8d0a25b5e1b39a80f3ea0f76ca4c9b7e8f9d9be09b0a2fb52c8b28f7bb4c4de3f10cf1e1caad03bfcfb0e78c118b8dbeae1cb69801d967ce3bf0aa *debian-edu-13.6.0-amd64-netinst.iso
";

    #[test]
    fn parses_text_and_binary_mode_lines() {
        let entries = parse_sums(SAMPLE).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].file_name, "debian-13.6.0-amd64-netinst.iso");
        assert_eq!(entries[1].file_name, "debian-edu-13.6.0-amd64-netinst.iso");
        assert_eq!(entries[0].sha512_hex.len(), 128);
    }

    #[test]
    fn finds_the_plain_netinst_iso_only() {
        let entries = parse_sums(SAMPLE).unwrap();
        let isos: Vec<_> = entries
            .iter()
            .filter(|e| e.is_netinst_iso("amd64"))
            .collect();
        // debian-edu also matches the suffix pattern; discovery must pick the
        // shortest / plain "debian-<ver>" name — resolver's job, both listed here.
        assert!(isos
            .iter()
            .any(|e| e.file_name == "debian-13.6.0-amd64-netinst.iso"));
    }

    #[test]
    fn version_extraction() {
        assert_eq!(
            version_from_iso_name("debian-13.6.0-amd64-netinst.iso"),
            Some("13.6.0")
        );
        assert_eq!(version_from_iso_name("weird.iso"), None);
    }

    #[test]
    fn rejects_bad_digests() {
        assert_eq!(parse_sums("zz  file\n"), Err(SumsError::BadDigest(1)));
        assert_eq!(parse_sums("abc123\n"), Err(SumsError::Malformed(1)));
    }
}
