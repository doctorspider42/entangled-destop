//! Fuzzes the Debian metadata parsers (backlog MVP-1402, EPIC 6).
//!
//! `SHA512SUMS`/`SHA256SUMS` and the archive `Release` file are parsed *before*
//! their signature has been used for anything, and in the netboot flow the ISO
//! name and release version are *derived* from them. Both parsers therefore see
//! bytes straight off the network and must never panic, hang or slice a
//! multi-byte character in half.
//!
//! The same input is fed to the helpers that run on top of the parse result
//! (netinst-ISO discovery, version extraction, entry lookup), because those do
//! the string surgery where an off-by-one would live.

#![no_main]

use debian_media::{
    find_entry, find_plain_netinst_iso, parse_sums, parse_sums_with, version_from_iso_name,
    DigestAlgo, Release,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Non-UTF-8 input is rejected before parsing in the real code path (the HTTP
    // layer hands over a String), so fuzz the parsers with text.
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };

    for algo in [DigestAlgo::Sha512, DigestAlgo::Sha256] {
        if let Ok(entries) = parse_sums_with(text, algo) {
            // Every accepted digest must be exactly the expected width and pure
            // lower-case hex: the digest is what the whole trust chain rests on.
            for entry in &entries {
                assert_eq!(
                    entry.sha512_hex.len(),
                    algo.hex_len(),
                    "accepted a {}-character digest for {algo:?}",
                    entry.sha512_hex.len()
                );
                assert!(
                    entry
                        .sha512_hex
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                    "accepted a non-hex digest: {:?}",
                    entry.sha512_hex
                );
                assert!(!entry.file_name.is_empty(), "accepted an empty file name");
                // Name helpers must not panic on arbitrary names.
                let _ = entry.normalized_name();
                let _ = entry.is_netinst_iso("amd64");
                let _ = entry.is_plain_netinst_iso("amd64");
                let _ = version_from_iso_name(&entry.file_name);
                let _ = find_entry(&entries, &entry.file_name);
            }
            // Discovery must only ever return an entry that claims to be one.
            if let Some(iso) = find_plain_netinst_iso(&entries, "amd64") {
                assert!(iso.is_plain_netinst_iso("amd64"));
            }
            let _ = find_plain_netinst_iso(&entries, "");
        }
    }
    // The default-algorithm entry point, in case its wrapper ever diverges.
    let _ = parse_sums(text);

    if let Ok(release) = Release::parse(text) {
        // Look-ups on a parsed Release must be total: any path, including the
        // empty one, either resolves or errors.
        let _ = release.index_digest("main/installer-amd64/current/images/SHA256SUMS");
        let _ = release.index_digest("");
        // Field values are echoed into provenance manifests and drive netboot
        // version discovery; `Some("")` must never get that far.
        for field in [&release.suite, &release.codename, &release.version] {
            if let Some(value) = field {
                assert!(!value.is_empty(), "parsed an empty Release field");
            }
        }
        assert!(
            release.suite.is_some() || release.codename.is_some(),
            "a Release with neither Suite nor Codename must be rejected"
        );
    }
});
