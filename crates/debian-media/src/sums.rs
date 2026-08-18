//! Parsing `SHA512SUMS` / `SHA256SUMS` files (backlog MVP-605/608): both to
//! verify artifact digests and to *discover* the current release's file names
//! (the netinst ISO name embeds the version, which we must never pin).

use thiserror::Error;

use crate::digest::DigestAlgo;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SumsEntry {
    /// Lower-case hex digest.
    pub sha512_hex: String,
    pub file_name: String,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SumsError {
    #[error("line {0}: malformed checksum line")]
    Malformed(usize),

    #[error("line {0}: digest is not {1} hex characters")]
    BadDigest(usize, usize),
}

/// Parses the classic `sha512sum` output format: `<hex><2 spaces or space+*><name>`.
/// Empty lines are ignored. Directory components in names are preserved.
pub fn parse_sums(content: &str) -> Result<Vec<SumsEntry>, SumsError> {
    parse_sums_with(content, DigestAlgo::Sha512)
}

/// Like [`parse_sums`] but for any supported digest width. Debian's archive-side
/// installer directory publishes SHA-256 rather than SHA-512, so both are
/// needed; the expected width is passed in explicitly so a shorter (weaker)
/// digest can never be silently accepted where a longer one was expected.
pub fn parse_sums_with(content: &str, algo: DigestAlgo) -> Result<Vec<SumsEntry>, SumsError> {
    let want = algo.hex_len();
    let mut entries = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let (digest, rest) = line.split_once(' ').ok_or(SumsError::Malformed(i + 1))?;
        if digest.len() != want || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(SumsError::BadDigest(i + 1, want));
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
    ///
    /// **Loose** on purpose: `debian-edu-…-netinst.iso` and
    /// `debian-mac-…-netinst.iso` also match. Use
    /// [`is_plain_netinst_iso`](Self::is_plain_netinst_iso) to pick the image
    /// the MVP actually wants.
    pub fn is_netinst_iso(&self, arch: &str) -> bool {
        self.file_name.starts_with("debian-")
            && self.file_name.ends_with(&format!("-{arch}-netinst.iso"))
    }

    /// True only for the *plain* netinst image: `debian-<version>-<arch>-netinst.iso`
    /// where `<version>` is purely numeric-and-dots. This is what rejects the
    /// `debian-edu-*` and `debian-mac-*` flavors that share the suffix.
    pub fn is_plain_netinst_iso(&self, arch: &str) -> bool {
        if !self.is_netinst_iso(arch) {
            return false;
        }
        match version_from_iso_name(&self.file_name) {
            Some(version) => {
                is_version_like(version) && self.file_name == plain_netinst_name(version, arch)
            }
            None => false,
        }
    }

    /// The name with a leading `./` removed. Debian's archive-side
    /// `SHA256SUMS` prefixes every path with `./`, the CD-side `SHA512SUMS`
    /// does not.
    pub fn normalized_name(&self) -> &str {
        self.file_name.strip_prefix("./").unwrap_or(&self.file_name)
    }
}

fn plain_netinst_name(version: &str, arch: &str) -> String {
    format!("debian-{version}-{arch}-netinst.iso")
}

fn is_version_like(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && s.bytes().any(|b| b.is_ascii_digit())
}

/// Extracts the release version from a netinst ISO file name,
/// e.g. `debian-13.6.0-amd64-netinst.iso` → `13.6.0`.
pub fn version_from_iso_name(name: &str) -> Option<&str> {
    name.strip_prefix("debian-")?.split('-').next()
}

/// Finds the digest entry for `name`, tolerating the `./` prefix on either side.
pub fn find_entry<'a>(entries: &'a [SumsEntry], name: &str) -> Option<&'a SumsEntry> {
    let wanted = name.strip_prefix("./").unwrap_or(name);
    entries.iter().find(|e| e.normalized_name() == wanted)
}

/// Discovers the current plain netinst ISO among the entries of a `SHA512SUMS`
/// file (MVP-604/605). When several plain images are listed — which should not
/// happen in the `current` directory but is cheap to defend against — the
/// highest version wins.
pub fn find_plain_netinst_iso<'a>(entries: &'a [SumsEntry], arch: &str) -> Option<&'a SumsEntry> {
    entries
        .iter()
        .filter(|e| e.is_plain_netinst_iso(arch))
        .max_by(|a, b| {
            let va = version_from_iso_name(&a.file_name).unwrap_or_default();
            let vb = version_from_iso_name(&b.file_name).unwrap_or_default();
            compare_versions(va, vb)
        })
}

/// Compares dotted numeric versions component-wise (`13.10.0` > `13.9.0`),
/// which a plain string comparison gets wrong.
pub(crate) fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let mut left = a.split('.');
    let mut right = b.split('.');
    loop {
        match (left.next(), right.next()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (a, b) => {
                let a = a.unwrap_or("0").parse::<u64>().unwrap_or(0);
                let b = b.unwrap_or("0").parse::<u64>().unwrap_or(0);
                match a.cmp(&b) {
                    std::cmp::Ordering::Equal => continue,
                    other => return other,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha512(seed: u8) -> String {
        format!("{seed:02x}").repeat(64)
    }

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
        assert_eq!(parse_sums("zz  file\n"), Err(SumsError::BadDigest(1, 128)));
        assert_eq!(parse_sums("abc123\n"), Err(SumsError::Malformed(1)));
    }

    /// MVP-604/605: the real `current/amd64/iso-cd/SHA512SUMS` lists edu and mac
    /// flavors alongside the plain image, and the DVD directory adds more
    /// decoys. Discovery must land on the plain netinst image.
    #[test]
    fn discovery_picks_the_plain_image_among_decoys() {
        let content = format!(
            "{}  debian-edu-13.6.0-amd64-netinst.iso\n\
             {}  debian-mac-13.6.0-amd64-netinst.iso\n\
             {}  debian-13.6.0-amd64-netinst.iso\n\
             {}  debian-13.6.0-amd64-DVD-1.iso\n\
             {}  debian-live-13.6.0-amd64-gnome.iso\n\
             {}  debian-13.6.0-arm64-netinst.iso\n\
             {} *debian-testing-amd64-netinst.iso\n",
            sha512(1),
            sha512(2),
            sha512(3),
            sha512(4),
            sha512(5),
            sha512(6),
            sha512(7),
        );
        let entries = parse_sums(&content).unwrap();
        let found = find_plain_netinst_iso(&entries, "amd64").expect("plain image found");
        assert_eq!(found.file_name, "debian-13.6.0-amd64-netinst.iso");
        assert_eq!(found.sha512_hex, sha512(3));
        assert_eq!(
            version_from_iso_name(&found.file_name),
            Some("13.6.0"),
            "version is discovered, never pinned"
        );
    }

    #[test]
    fn discovery_returns_none_when_only_flavors_are_present() {
        let content = format!(
            "{}  debian-edu-13.6.0-amd64-netinst.iso\n{}  debian-mac-13.6.0-amd64-netinst.iso\n",
            sha512(1),
            sha512(2)
        );
        let entries = parse_sums(&content).unwrap();
        assert!(find_plain_netinst_iso(&entries, "amd64").is_none());
    }

    #[test]
    fn discovery_prefers_the_highest_version_numerically() {
        let content = format!(
            "{}  debian-13.9.0-amd64-netinst.iso\n{}  debian-13.10.0-amd64-netinst.iso\n",
            sha512(1),
            sha512(2)
        );
        let entries = parse_sums(&content).unwrap();
        let found = find_plain_netinst_iso(&entries, "amd64").unwrap();
        assert_eq!(found.file_name, "debian-13.10.0-amd64-netinst.iso");
    }

    #[test]
    fn sha256_sums_parse_with_the_narrower_width() {
        let content = "\
c8b67f68fb34d3bc91935564255b8f3404199f44fab227672e4861a62434dad5  ./netboot/debian-installer/amd64/initrd.gz
e7667ff961fcf0f872e2618a930454a6362ce58995f386431f60a5169c85f41a  ./netboot/debian-installer/amd64/linux
";
        let entries = parse_sums_with(content, DigestAlgo::Sha256).unwrap();
        assert_eq!(entries.len(), 2);
        let linux = find_entry(&entries, "netboot/debian-installer/amd64/linux").unwrap();
        assert_eq!(
            linux.sha512_hex,
            "e7667ff961fcf0f872e2618a930454a6362ce58995f386431f60a5169c85f41a"
        );
        // The `./` prefix is accepted on the lookup side as well.
        assert!(find_entry(&entries, "./netboot/debian-installer/amd64/linux").is_some());
        assert!(find_entry(&entries, "netboot/gtk/debian-installer/amd64/linux").is_none());
    }

    /// A SHA-256 digest must never satisfy a SHA-512 expectation: the parser
    /// rejects the whole file rather than accepting a weaker digest.
    #[test]
    fn a_sha256_file_is_rejected_when_sha512_is_expected() {
        let content = "c8b67f68fb34d3bc91935564255b8f3404199f44fab227672e4861a62434dad5  x\n";
        assert_eq!(parse_sums(content), Err(SumsError::BadDigest(1, 128)));
    }

    #[test]
    fn version_comparison_is_component_wise() {
        use std::cmp::Ordering;
        assert_eq!(compare_versions("13.10.0", "13.9.0"), Ordering::Greater);
        assert_eq!(compare_versions("13.6", "13.6.0"), Ordering::Equal);
        assert_eq!(compare_versions("12.0.0", "13.0.0"), Ordering::Less);
    }
}
