//! The one test that talks to the real internet — opt-in, never run in CI.
//!
//! It exercises the full text-netboot chain against `deb.debian.org`:
//! `dists/stable/Release` + `Release.gpg` verified against the pinned Debian
//! archive keyring, then `installer-amd64/current/images/SHA256SUMS` verified
//! against the signed `Release`, then the ~10 MB installer kernel verified
//! against that index, then a provenance manifest.
//!
//! Run it manually:
//!
//! ```text
//! cargo test -p debian-media --test network -- --ignored --nocapture
//! ```
//!
//! It downloads roughly 10 MB into a temporary directory that is removed
//! afterwards, and it will fail whenever the machine has no outbound HTTPS.
//! Because it fetches `stable`, it also breaks by design once Debian rotates to
//! an archive key that is not pinned in `crates/debian-media/keys/` — which is
//! exactly the signal we want.

mod support;

use debian_media::{
    Arch, DebianStableSource, DigestAlgo, FetchStatus, Fetcher, InstallerVariant, MediaCache,
    Provenance, UreqTransport,
};
use support::TempDir;

#[test]
#[ignore = "requires network access to deb.debian.org; run with --ignored"]
fn fetches_and_verifies_the_real_text_netboot_kernel() {
    let tmp = TempDir::new("network");
    let fetcher = Fetcher::new(
        DebianStableSource::new(Arch::Amd64),
        UreqTransport::new(),
        MediaCache::with_root(tmp.path()),
        Arch::Amd64,
    );

    let report = fetcher
        .fetch(InstallerVariant::TextNetboot)
        .expect("real fetch of the text netboot installer");

    println!("resolved Debian {} from signed metadata", report.version);
    match &report.provenance {
        Provenance::Verified {
            sums_url,
            signed_by,
            keyring,
        } => {
            println!("  trust root: {sums_url}");
            println!(
                "  signed by:  {} ({keyring})",
                signed_by.signing_fingerprint
            );
        }
        Provenance::Cache => panic!("a fresh temp cache cannot be a cache hit"),
    }

    // The version is discovered, so only its shape can be asserted.
    assert!(
        report.version.starts_with(char::is_numeric),
        "unexpected version {:?}",
        report.version
    );
    assert_eq!(report.artifacts.len(), 2, "kernel and initrd");

    for artifact in &report.artifacts {
        println!(
            "  {} {} ({})",
            artifact.status.as_str(),
            artifact.path.display(),
            artifact.manifest.sha512_hex
        );
        assert_eq!(artifact.status, FetchStatus::Downloaded);
        assert!(artifact.manifest.signature_verified);
        assert!(artifact.manifest_path.is_file());

        // Independently re-hash what landed on disk.
        let bytes = std::fs::read(&artifact.path).expect("read artifact");
        assert!(!bytes.is_empty());
        assert_eq!(
            DigestAlgo::Sha256.hex_of(&bytes),
            artifact.manifest.sha512_hex,
            "on-disk bytes must match the manifest digest"
        );
    }

    // The kernel really is a Linux bzImage: "HdrS" at offset 0x202.
    let kernel = std::fs::read(&report.artifacts[0].path).expect("read kernel");
    assert!(kernel.len() > 0x210, "kernel is suspiciously small");
    assert_eq!(&kernel[0x202..0x206], b"HdrS", "not a Linux bzImage");

    // A second fetch over the same cache must be a zero-network cache hit.
    let again = fetcher
        .fetch(InstallerVariant::TextNetboot)
        .expect("second fetch");
    assert!(matches!(again.provenance, Provenance::Cache));
    assert!(again
        .artifacts
        .iter()
        .all(|a| a.status == FetchStatus::Cached));
}
