//! The `vmhost fetch` flow against fixture transports (EPIC 6 acceptance
//! criteria), all offline.
//!
//! Covered here:
//!
//! * happy path for both trust chains (detached `SHA512SUMS.sign`, archive
//!   `Release.gpg` → `SHA256SUMS`);
//! * the ordering property — a bad signature must abort *before* the artifact is
//!   requested;
//! * bad digest, truncated/garbled checksum file, unlisted artifact;
//! * resume after an interrupted transfer, including a server that ignores
//!   `Range` and a partial longer than the resource;
//! * failure cleanup vs. resume: verification failures purge, interruptions do
//!   not;
//! * cache re-use with zero network access, and the manifest contents.

mod support;

use debian_media::{
    manifest_path, partial_path, DigestAlgo, FetchOptions, FetchStatus, Fetcher, Manifest,
    MediaCache, MediaError, Provenance,
};
use support::{sums_file, Fixture, TempDir, TestKey, TestSource};

const BASE: &str = "https://mirror.invalid/media";
const VARIANT: debian_media::InstallerVariant = debian_media::InstallerVariant::GtkNetboot;

fn kernel_bytes() -> Vec<u8> {
    (0..40_000u32).map(|i| (i % 251) as u8).collect()
}

fn initrd_bytes() -> Vec<u8> {
    (0..17_000u32).map(|i| (i % 97) as u8).collect()
}

/// Builds a detached-sums fixture: signed `SHA512SUMS` plus the two artifacts.
fn detached_fixture(key: &TestKey) -> Fixture {
    let kernel = kernel_bytes();
    let initrd = initrd_bytes();
    let sums = sums_file(
        DigestAlgo::Sha512,
        &[
            ("netboot/linux", &kernel),
            ("netboot/initrd.gz", &initrd),
            // A decoy that must never be picked up.
            ("netboot/other/linux", b"junk"),
        ],
    );
    let mut fixture = Fixture::new();
    fixture
        .put(format!("{BASE}/SHA512SUMS"), sums.clone())
        .put(
            format!("{BASE}/SHA512SUMS.sign"),
            key.detached_sign(sums.as_bytes()),
        )
        .put(format!("{BASE}/netboot/linux"), kernel)
        .put(format!("{BASE}/netboot/initrd.gz"), initrd);
    fixture
}

/// The signed netboot index and the `Release` that commits to it, for `version`.
fn archive_index_and_release(version: &str) -> (String, String) {
    let index = sums_file(
        DigestAlgo::Sha256,
        &[
            ("./netboot/linux", &kernel_bytes()),
            ("./netboot/initrd.gz", &initrd_bytes()),
        ],
    );
    let release = format!(
        "Origin: Debian\nSuite: stable\nVersion: {version}\nCodename: trixie\nSHA256:\n {} {:>8} images/SHA256SUMS\n",
        DigestAlgo::Sha256.hex_of(index.as_bytes()),
        index.len()
    );
    (index, release)
}

/// The netboot chain: `Release` (signed) → `images/SHA256SUMS` → artifacts.
fn archive_fixture(key: &TestKey) -> Fixture {
    archive_fixture_for(key, "13.6")
}

fn archive_fixture_for(key: &TestKey, version: &str) -> Fixture {
    let (index, release) = archive_index_and_release(version);
    let mut fixture = Fixture::new();
    fixture
        .put(format!("{BASE}/Release"), release.clone())
        .put(
            format!("{BASE}/Release.gpg"),
            key.detached_sign(release.as_bytes()),
        )
        .put(format!("{BASE}/images/SHA256SUMS"), index)
        .put(format!("{BASE}/images/netboot/linux"), kernel_bytes())
        .put(format!("{BASE}/images/netboot/initrd.gz"), initrd_bytes());
    fixture
}

// ------------------------------------------------------------- happy paths

/// A detached-`SHA512SUMS` root states no version, and fixed netboot-style names
/// carry none either. Rather than invent a cache directory name, the fetch must
/// refuse — the alternative would be a hardcoded release number.
#[test]
fn a_root_without_a_discoverable_version_is_refused() {
    let key = TestKey::generate("cd <cd@example.invalid>");
    let tmp = TempDir::new("fetch-noversion");
    let fetcher = Fetcher::new(
        TestSource::detached(BASE, key.keyring("test CD"), DigestAlgo::Sha512),
        detached_fixture(&key),
        MediaCache::with_root(tmp.path()),
        debian_media::Arch::Amd64,
    );

    let err = fetcher.fetch(VARIANT).expect_err("no version discoverable");
    assert!(matches!(err, MediaError::UnknownVersion { .. }), "{err}");
    // The signature was still verified first.
    let urls = fetcher.transport().requested_urls();
    assert!(
        urls.contains(&format!("{BASE}/SHA512SUMS.sign")),
        "{urls:?}"
    );
    assert!(!urls.iter().any(|u| u.contains("netboot/")), "{urls:?}");
}

#[test]
fn archive_release_chain_downloads_verifies_and_writes_manifests() {
    let key = TestKey::generate("archive <a@example.invalid>");
    let tmp = TempDir::new("fetch-archive");
    let fetcher = Fetcher::new(
        TestSource::archive(BASE, key.keyring("test archive")),
        archive_fixture(&key),
        MediaCache::with_root(tmp.path()),
        debian_media::Arch::Amd64,
    );

    let report = fetcher.fetch(VARIANT).expect("fetch succeeds");
    // MVP-605: the version comes out of the signed Release file.
    assert_eq!(report.version, "13.6");
    assert!(report.provenance.is_verified_online());
    assert_eq!(report.artifacts.len(), 2);

    for artifact in &report.artifacts {
        assert_eq!(artifact.status, FetchStatus::Downloaded);
        assert!(artifact.path.is_file(), "{:?} missing", artifact.path);
        assert!(artifact.manifest_path.is_file());
        // No `.part` survives a successful fetch.
        assert!(!partial_path(&artifact.path).exists());

        // MVP-610: exact URL, discovered version, RFC 3339 UTC time, digest.
        let m = &artifact.manifest;
        assert!(m.signature_verified);
        assert_eq!(m.version, "13.6");
        assert!(m.url.starts_with(BASE), "{}", m.url);
        assert_eq!(m.sha512_hex.len(), DigestAlgo::Sha256.hex_len());
        assert!(
            m.fetched_at.ends_with('Z') && m.fetched_at.len() == 20,
            "{}",
            m.fetched_at
        );
        assert_eq!(m.signed_by.as_deref(), Some(key.fingerprint.as_str()));

        // And it round-trips through TOML on disk.
        let text = std::fs::read_to_string(&artifact.manifest_path).unwrap();
        assert_eq!(&Manifest::from_toml(&text).unwrap(), m);

        // The bytes on disk really are the artifact.
        let on_disk = std::fs::read(&artifact.path).unwrap();
        assert_eq!(DigestAlgo::Sha256.hex_of(&on_disk), m.sha512_hex);
    }

    // Both variants of the netboot tree publish a file called `linux`, so the
    // cache path must be variant-scoped.
    let kernel = &report.artifacts[0].path;
    assert!(
        kernel
            .to_string_lossy()
            .replace('\\', "/")
            .contains("13.6/amd64-gtk-netboot/"),
        "{kernel:?}"
    );
}

#[test]
fn netinst_iso_name_and_version_are_discovered_from_the_signed_sums() {
    let key = TestKey::generate("cd <cd@example.invalid>");
    let iso = b"pretend this is a 700 MB image".to_vec();
    let sums = sums_file(
        DigestAlgo::Sha512,
        &[
            ("debian-edu-13.6.0-amd64-netinst.iso", b"edu decoy"),
            ("debian-mac-13.6.0-amd64-netinst.iso", b"mac decoy"),
            ("debian-13.6.0-amd64-netinst.iso", &iso),
            ("debian-13.6.0-amd64-DVD-1.iso", b"dvd decoy"),
        ],
    );
    let mut fixture = Fixture::new();
    fixture
        .put(format!("{BASE}/SHA512SUMS"), sums.clone())
        .put(
            format!("{BASE}/SHA512SUMS.sign"),
            key.detached_sign(sums.as_bytes()),
        )
        .put(
            format!("{BASE}/debian-13.6.0-amd64-netinst.iso"),
            iso.clone(),
        );

    let tmp = TempDir::new("fetch-iso");
    let fetcher = Fetcher::new(
        TestSource::detached_iso(BASE, key.keyring("test CD")),
        fixture,
        MediaCache::with_root(tmp.path()),
        debian_media::Arch::Amd64,
    );

    let report = fetcher.fetch(VARIANT).expect("fetch succeeds");
    assert_eq!(report.version, "13.6.0");
    assert_eq!(report.artifacts.len(), 1);
    let artifact = &report.artifacts[0];
    assert!(artifact
        .path
        .to_string_lossy()
        .ends_with("debian-13.6.0-amd64-netinst.iso"));
    assert_eq!(std::fs::read(&artifact.path).unwrap(), iso);

    // The decoys were never requested.
    let urls = fetcher.transport().requested_urls();
    assert!(
        !urls
            .iter()
            .any(|u| u.contains("edu") || u.contains("mac") || u.contains("DVD")),
        "{urls:?}"
    );
}

// ---------------------------------------------------- ordering / rejection

/// MVP-607: a correct digest with an unverified checksum file is a FAILURE, and
/// the failure happens before a single artifact byte is requested.
#[test]
fn a_bad_signature_aborts_before_any_artifact_is_requested() {
    let trusted = TestKey::generate("trusted <a@example.invalid>");
    let attacker = TestKey::generate("attacker <b@example.invalid>");

    // The Release and the sums file are perfectly correct — only the signature
    // comes from the wrong key.
    let (_, release) = archive_index_and_release("13.6");
    let mut fixture = archive_fixture(&trusted);
    fixture.put(
        format!("{BASE}/Release.gpg"),
        attacker.detached_sign(release.as_bytes()),
    );

    let tmp = TempDir::new("fetch-badsig");
    let fetcher = Fetcher::new(
        TestSource::archive(BASE, trusted.keyring("test archive")),
        fixture,
        MediaCache::with_root(tmp.path()),
        debian_media::Arch::Amd64,
    );

    let err = fetcher.fetch(VARIANT).expect_err("must not verify");
    assert!(matches!(err, MediaError::Signature { .. }), "{err}");

    let urls = fetcher.transport().requested_urls();
    assert_eq!(
        urls,
        vec![format!("{BASE}/Release"), format!("{BASE}/Release.gpg")],
        "nothing beyond the trust root may be fetched"
    );
    assert!(
        !urls
            .iter()
            .any(|u| u.contains("SHA256SUMS") || u.contains("netboot")),
        "{urls:?}"
    );
}

/// The archive chain's second hop: the checksum index must match the digest the
/// signed `Release` commits to.
#[test]
fn a_tampered_checksum_index_is_rejected_by_the_signed_release() {
    let key = TestKey::generate("archive <a@example.invalid>");
    let (index, _) = archive_index_and_release("13.6");
    let mut tampered = index.into_bytes();
    // Same length, different content — so the size check cannot be what catches it.
    let last = tampered.len() - 2;
    tampered[last] = if tampered[last] == b'a' { b'b' } else { b'a' };
    let mut fixture = archive_fixture(&key);
    fixture.put(format!("{BASE}/images/SHA256SUMS"), tampered);

    let tmp = TempDir::new("fetch-tampered-index");
    let fetcher = Fetcher::new(
        TestSource::archive(BASE, key.keyring("test archive")),
        fixture,
        MediaCache::with_root(tmp.path()),
        debian_media::Arch::Amd64,
    );

    let err = fetcher.fetch(VARIANT).expect_err("must not verify");
    match err {
        MediaError::DigestMismatch { algo, url, .. } => {
            assert_eq!(algo, "sha256");
            assert!(url.ends_with("SHA256SUMS"), "{url}");
        }
        other => panic!("unexpected error {other}"),
    }
    assert!(
        !fetcher
            .transport()
            .requested_urls()
            .iter()
            .any(|u| u.contains("netboot")),
        "artifacts must not be fetched once the index is untrusted"
    );
}

#[test]
fn a_wrong_artifact_digest_fails_and_purges_the_cache() {
    let key = TestKey::generate("archive <a@example.invalid>");
    let mut fixture = archive_fixture(&key);
    // Serve a kernel that does not match the signed digest.
    fixture.put(
        format!("{BASE}/images/netboot/linux"),
        vec![0xAAu8; kernel_bytes().len()],
    );

    let tmp = TempDir::new("fetch-baddigest");
    let cache = MediaCache::with_root(tmp.path());
    let expected_path = cache.artifact_path("13.6", debian_media::Arch::Amd64, VARIANT, "linux");
    let fetcher = Fetcher::new(
        TestSource::archive(BASE, key.keyring("test archive")),
        fixture,
        cache,
        debian_media::Arch::Amd64,
    );

    let err = fetcher.fetch(VARIANT).expect_err("must not verify");
    assert!(matches!(err, MediaError::DigestMismatch { .. }), "{err}");
    assert!(err.invalidates_cache());

    // Acceptance criterion: nothing is left behind in the active cache.
    assert!(!expected_path.exists(), "{expected_path:?} survived");
    assert!(!partial_path(&expected_path).exists());
    assert!(!manifest_path(&expected_path).exists());
}

#[test]
fn a_garbled_checksum_file_is_rejected() {
    let key = TestKey::generate("cd <cd@example.invalid>");
    // Signed, but truncated mid-digest: the signature is valid over the garbage.
    let sums = "0123456789abcdef  netboot/linux\n";
    let mut fixture = Fixture::new();
    fixture.put(format!("{BASE}/SHA512SUMS"), sums).put(
        format!("{BASE}/SHA512SUMS.sign"),
        key.detached_sign(sums.as_bytes()),
    );

    let tmp = TempDir::new("fetch-garbled");
    let fetcher = Fetcher::new(
        TestSource::detached(BASE, key.keyring("test CD"), DigestAlgo::Sha512),
        fixture,
        MediaCache::with_root(tmp.path()),
        debian_media::Arch::Amd64,
    );

    let err = fetcher.fetch(VARIANT).expect_err("must not parse");
    assert!(matches!(err, MediaError::Sums { .. }), "{err}");
}

#[test]
fn an_artifact_missing_from_the_signed_sums_is_refused() {
    let key = TestKey::generate("archive <a@example.invalid>");
    let mut fixture = archive_fixture(&key);
    // A signed index that only lists the kernel.
    let index = sums_file(DigestAlgo::Sha256, &[("./netboot/linux", &kernel_bytes())]);
    let release = format!(
        "Suite: stable\nVersion: 13.6\nSHA256:\n {} {} images/SHA256SUMS\n",
        DigestAlgo::Sha256.hex_of(index.as_bytes()),
        index.len()
    );
    fixture
        .put(format!("{BASE}/images/SHA256SUMS"), index)
        .put(format!("{BASE}/Release"), release.clone())
        .put(
            format!("{BASE}/Release.gpg"),
            key.detached_sign(release.as_bytes()),
        );

    let tmp = TempDir::new("fetch-unlisted");
    let fetcher = Fetcher::new(
        TestSource::archive(BASE, key.keyring("test archive")),
        fixture,
        MediaCache::with_root(tmp.path()),
        debian_media::Arch::Amd64,
    );

    let err = fetcher.fetch(VARIANT).expect_err("initrd is unlisted");
    match err {
        MediaError::NotListed { file, .. } => assert_eq!(file, "netboot/initrd.gz"),
        other => panic!("unexpected error {other}"),
    }
}

#[test]
fn a_missing_signature_file_is_a_transport_error_not_a_silent_pass() {
    let key = TestKey::generate("archive <a@example.invalid>");
    let mut fixture = archive_fixture(&key);
    fixture.remove(&format!("{BASE}/Release.gpg"));

    let tmp = TempDir::new("fetch-nosig");
    let fetcher = Fetcher::new(
        TestSource::archive(BASE, key.keyring("test archive")),
        fixture,
        MediaCache::with_root(tmp.path()),
        debian_media::Arch::Amd64,
    );

    let err = fetcher.fetch(VARIANT).expect_err("no signature, no trust");
    assert!(matches!(err, MediaError::Transport { .. }), "{err}");
}

// ------------------------------------------------------------------ resume

#[test]
fn an_interrupted_transfer_keeps_its_partial_and_the_next_run_resumes_it() {
    let key = TestKey::generate("archive <a@example.invalid>");
    let tmp = TempDir::new("fetch-resume");
    let cache = MediaCache::with_root(tmp.path());
    let kernel_path = cache.artifact_path("13.6", debian_media::Arch::Amd64, VARIANT, "linux");

    // Pass 1: the connection drops after 10 000 bytes.
    let mut fixture = archive_fixture(&key);
    fixture.cut_at = Some(10_000);
    let fetcher = Fetcher::new(
        TestSource::archive(BASE, key.keyring("test archive")),
        fixture,
        MediaCache::with_root(tmp.path()),
        debian_media::Arch::Amd64,
    );
    let err = fetcher.fetch(VARIANT).expect_err("transfer was cut short");
    match &err {
        MediaError::Interrupted {
            received, expected, ..
        } => {
            assert_eq!(*received, 10_000);
            assert_eq!(*expected, kernel_bytes().len() as u64);
        }
        other => panic!("unexpected error {other}"),
    }
    // An interruption must NOT purge: the partial is what makes resume possible.
    assert!(!err.invalidates_cache());
    let partial = partial_path(&kernel_path);
    assert_eq!(
        std::fs::metadata(&partial).expect("partial kept").len(),
        10_000
    );
    assert!(!kernel_path.exists(), "nothing may look complete yet");

    // Pass 2: a healthy server. The kernel resumes from 10 000, the initrd is a
    // fresh download.
    let fetcher = Fetcher::new(
        TestSource::archive(BASE, key.keyring("test archive")),
        archive_fixture(&key),
        cache,
        debian_media::Arch::Amd64,
    );
    let report = fetcher.fetch(VARIANT).expect("resumed fetch succeeds");
    let kernel = &report.artifacts[0];
    assert_eq!(kernel.status, FetchStatus::Resumed);
    assert_eq!(report.artifacts[1].status, FetchStatus::Downloaded);
    assert_eq!(std::fs::read(&kernel.path).unwrap(), kernel_bytes());
    assert!(!partial.exists());

    // The ranged request really did start where the partial ended, i.e. the
    // already-present prefix was re-hashed rather than re-downloaded.
    let ranged = fetcher
        .transport()
        .requests()
        .into_iter()
        .find(|r| r.url.ends_with("netboot/linux"))
        .expect("kernel was requested");
    assert_eq!(ranged.range_from, Some(10_000));
}

#[test]
fn a_server_that_ignores_range_restarts_the_download_correctly() {
    let key = TestKey::generate("archive <a@example.invalid>");
    let tmp = TempDir::new("fetch-norange");
    let cache = MediaCache::with_root(tmp.path());
    let kernel_path = cache.artifact_path("13.6", debian_media::Arch::Amd64, VARIANT, "linux");

    let mut cut = archive_fixture(&key);
    cut.cut_at = Some(5_000);
    let fetcher = Fetcher::new(
        TestSource::archive(BASE, key.keyring("test archive")),
        cut,
        MediaCache::with_root(tmp.path()),
        debian_media::Arch::Amd64,
    );
    fetcher.fetch(VARIANT).expect_err("cut short");
    assert_eq!(
        std::fs::metadata(partial_path(&kernel_path)).unwrap().len(),
        5_000
    );

    let mut plain = archive_fixture(&key);
    plain.ignore_range = true;
    let fetcher = Fetcher::new(
        TestSource::archive(BASE, key.keyring("test archive")),
        plain,
        cache,
        debian_media::Arch::Amd64,
    );
    let report = fetcher.fetch(VARIANT).expect("fetch succeeds anyway");
    // The partial could not be continued, so this is a full download, not a resume.
    assert_eq!(report.artifacts[0].status, FetchStatus::Downloaded);
    assert_eq!(
        std::fs::read(&report.artifacts[0].path).unwrap(),
        kernel_bytes()
    );
}

#[test]
fn a_partial_longer_than_the_resource_is_discarded() {
    let key = TestKey::generate("archive <a@example.invalid>");
    let tmp = TempDir::new("fetch-overlong");
    let cache = MediaCache::with_root(tmp.path());
    let kernel_path = cache.artifact_path("13.6", debian_media::Arch::Amd64, VARIANT, "linux");
    std::fs::create_dir_all(kernel_path.parent().unwrap()).unwrap();
    // A stale `.part` bigger than the real file — a 416 or a restart, never a
    // corrupt "success".
    std::fs::write(
        partial_path(&kernel_path),
        vec![0u8; kernel_bytes().len() + 500],
    )
    .unwrap();

    let fetcher = Fetcher::new(
        TestSource::archive(BASE, key.keyring("test archive")),
        archive_fixture(&key),
        cache,
        debian_media::Arch::Amd64,
    );
    let report = fetcher.fetch(VARIANT).expect("fetch recovers");
    assert_eq!(report.artifacts[0].status, FetchStatus::Downloaded);
    assert_eq!(
        std::fs::read(&report.artifacts[0].path).unwrap(),
        kernel_bytes()
    );
}

/// Without a server-announced length a short transfer is indistinguishable from
/// a complete one, so it surfaces as a digest mismatch — and then the partial
/// *is* discarded, because the bytes cannot be trusted.
#[test]
fn a_short_transfer_without_content_length_fails_the_digest_check_and_purges() {
    let key = TestKey::generate("archive <a@example.invalid>");
    let mut fixture = archive_fixture(&key);
    fixture.cut_at = Some(1_000);
    fixture.hide_length = true;

    let tmp = TempDir::new("fetch-nolen");
    let cache = MediaCache::with_root(tmp.path());
    let kernel_path = cache.artifact_path("13.6", debian_media::Arch::Amd64, VARIANT, "linux");
    let fetcher = Fetcher::new(
        TestSource::archive(BASE, key.keyring("test archive")),
        fixture,
        cache,
        debian_media::Arch::Amd64,
    );

    let err = fetcher.fetch(VARIANT).expect_err("digest cannot match");
    assert!(matches!(err, MediaError::DigestMismatch { .. }), "{err}");
    assert!(!partial_path(&kernel_path).exists());
    assert!(!kernel_path.exists());
}

// ------------------------------------------------------------------- cache

/// MVP-609 / backlog: `vmhost fetch` twice means one download.
#[test]
fn a_second_run_is_served_from_the_cache_with_zero_network_access() {
    let key = TestKey::generate("archive <a@example.invalid>");
    let tmp = TempDir::new("fetch-cache");

    let fetcher = Fetcher::new(
        TestSource::archive(BASE, key.keyring("test archive")),
        archive_fixture(&key),
        MediaCache::with_root(tmp.path()),
        debian_media::Arch::Amd64,
    );
    let first = fetcher.fetch(VARIANT).expect("first fetch");
    assert!(first
        .artifacts
        .iter()
        .all(|a| a.status == FetchStatus::Downloaded));

    // A brand new fetcher over the same cache, so nothing is memoised in RAM.
    let fetcher = Fetcher::new(
        TestSource::archive(BASE, key.keyring("test archive")),
        archive_fixture(&key),
        MediaCache::with_root(tmp.path()),
        debian_media::Arch::Amd64,
    );
    let second = fetcher.fetch(VARIANT).expect("second fetch");
    assert_eq!(second.version, "13.6");
    assert!(matches!(second.provenance, Provenance::Cache));
    assert!(second
        .artifacts
        .iter()
        .all(|a| a.status == FetchStatus::Cached));
    assert!(
        fetcher.transport().requested_urls().is_empty(),
        "a verified cache hit must not touch the network: {:?}",
        fetcher.transport().requested_urls()
    );

    // `--refresh` deliberately goes back to the signed root.
    let fetcher = Fetcher::new(
        TestSource::archive(BASE, key.keyring("test archive")),
        archive_fixture(&key),
        MediaCache::with_root(tmp.path()),
        debian_media::Arch::Amd64,
    );
    let refreshed = fetcher
        .fetch_with(
            VARIANT,
            FetchOptions {
                refresh: true,
                offline: false,
            },
        )
        .expect("refresh");
    assert!(refreshed.provenance.is_verified_online());
    assert!(refreshed
        .artifacts
        .iter()
        .all(|a| a.status == FetchStatus::Cached));
    let urls = fetcher.transport().requested_urls();
    assert!(urls.contains(&format!("{BASE}/Release")), "{urls:?}");
    assert!(
        !urls.iter().any(|u| u.contains("netboot")),
        "the artifacts themselves must not be re-downloaded: {urls:?}"
    );
}

#[test]
fn a_corrupted_cached_artifact_is_re_downloaded() {
    let key = TestKey::generate("archive <a@example.invalid>");
    let tmp = TempDir::new("fetch-corrupt");
    let fetcher = Fetcher::new(
        TestSource::archive(BASE, key.keyring("test archive")),
        archive_fixture(&key),
        MediaCache::with_root(tmp.path()),
        debian_media::Arch::Amd64,
    );
    let first = fetcher.fetch(VARIANT).expect("first fetch");
    let kernel = first.artifacts[0].path.clone();

    // Bit rot (or tampering) after the fact: the manifest still says "verified"
    // but the file no longer hashes to the recorded digest.
    std::fs::write(&kernel, vec![0x00u8; kernel_bytes().len()]).unwrap();

    let fetcher = Fetcher::new(
        TestSource::archive(BASE, key.keyring("test archive")),
        archive_fixture(&key),
        MediaCache::with_root(tmp.path()),
        debian_media::Arch::Amd64,
    );
    let second = fetcher
        .fetch(VARIANT)
        .expect("second fetch repairs the cache");
    assert_eq!(second.artifacts[0].status, FetchStatus::Downloaded);
    assert_eq!(std::fs::read(&kernel).unwrap(), kernel_bytes());
}

#[test]
fn an_artifact_without_a_manifest_is_never_treated_as_verified() {
    let key = TestKey::generate("archive <a@example.invalid>");
    let tmp = TempDir::new("fetch-nomanifest");
    let fetcher = Fetcher::new(
        TestSource::archive(BASE, key.keyring("test archive")),
        archive_fixture(&key),
        MediaCache::with_root(tmp.path()),
        debian_media::Arch::Amd64,
    );
    let first = fetcher.fetch(VARIANT).expect("first fetch");
    let kernel = first.artifacts[0].path.clone();
    std::fs::remove_file(manifest_path(&kernel)).unwrap();

    let fetcher = Fetcher::new(
        TestSource::archive(BASE, key.keyring("test archive")),
        archive_fixture(&key),
        MediaCache::with_root(tmp.path()),
        debian_media::Arch::Amd64,
    );
    let second = fetcher.fetch(VARIANT).expect("second fetch");
    assert_eq!(second.artifacts[0].status, FetchStatus::Downloaded);
    assert!(manifest_path(&kernel).is_file());
}

#[test]
fn offline_mode_fails_loudly_when_the_cache_is_empty() {
    let key = TestKey::generate("archive <a@example.invalid>");
    let tmp = TempDir::new("fetch-offline");
    let fetcher = Fetcher::new(
        TestSource::archive(BASE, key.keyring("test archive")),
        archive_fixture(&key),
        MediaCache::with_root(tmp.path()),
        debian_media::Arch::Amd64,
    );

    let err = fetcher
        .fetch_with(
            VARIANT,
            FetchOptions {
                refresh: false,
                offline: true,
            },
        )
        .expect_err("nothing cached");
    assert!(matches!(err, MediaError::NothingCached { .. }), "{err}");
    assert!(fetcher.transport().requested_urls().is_empty());
}

#[test]
fn cache_selects_the_highest_verified_version() {
    let key = TestKey::generate("archive <a@example.invalid>");
    let tmp = TempDir::new("fetch-versions");

    // Seed 13.9 first, then 13.10, then ask offline: 13.10 must win even though
    // "13.10" sorts before "13.9" as a string.
    for version in ["13.9", "13.10"] {
        let fetcher = Fetcher::new(
            TestSource::archive(BASE, key.keyring("test archive")),
            archive_fixture_for(&key, version),
            MediaCache::with_root(tmp.path()),
            debian_media::Arch::Amd64,
        );
        fetcher
            .fetch_with(
                VARIANT,
                FetchOptions {
                    refresh: true,
                    offline: false,
                },
            )
            .expect("seed the cache");
    }

    let fetcher = Fetcher::new(
        TestSource::archive(BASE, key.keyring("test archive")),
        archive_fixture(&key),
        MediaCache::with_root(tmp.path()),
        debian_media::Arch::Amd64,
    );
    let report = fetcher
        .fetch_with(
            VARIANT,
            FetchOptions {
                refresh: false,
                offline: true,
            },
        )
        .expect("offline hit");
    assert_eq!(report.version, "13.10");
    assert!(fetcher.transport().requested_urls().is_empty());
}
