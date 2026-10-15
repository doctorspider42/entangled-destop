//! Authentic Canonical metadata, fetched from the immutable Noble 20260926
//! cloud-image directory. All tests are offline; no guest binary is committed.
use debian_media::{verify_detached, UBUNTU_CLOUD, UBUNTU_CLOUD_FINGERPRINT};
mod support;
use support::{Fixture, TempDir};

const SUMS: &[u8] = include_bytes!("fixtures/ubuntu-cloud/SHA256SUMS");
const SIGNATURE: &[u8] = include_bytes!("fixtures/ubuntu-cloud/SHA256SUMS.gpg");
const URL: &str = "https://cloud-images.ubuntu.com/noble/20260926/SHA256SUMS";

#[test]
fn canonical_metadata_verifies_against_the_independently_pinned_cloud_key() {
    let verified = verify_detached(&UBUNTU_CLOUD, SUMS, SIGNATURE, URL).unwrap();
    assert_eq!(verified.primary_fingerprint, UBUNTU_CLOUD_FINGERPRINT);
    let entries = debian_media::parse_sums_with(
        std::str::from_utf8(SUMS).unwrap(),
        debian_media::DigestAlgo::Sha256,
    )
    .unwrap();
    let image = entries
        .iter()
        .find(|entry| entry.file_name == "noble-server-cloudimg-arm64.tar.gz")
        .unwrap();
    assert_eq!(
        image.sha512_hex,
        "1800eb56b6b839a08c020551e16fbd8c95b2772452ecce3737a9ad25fe594126"
    );
}

#[test]
fn changing_the_signed_checksum_payload_is_rejected() {
    let mut changed = SUMS.to_vec();
    changed[0] ^= 1;
    assert!(verify_detached(&UBUNTU_CLOUD, &changed, SIGNATURE, URL).is_err());
}

#[test]
fn the_iso_signing_key_is_not_a_cloud_image_trust_root() {
    let key = debian_media::PinnedKey::new(
        "843938df228d22f7b3742bc0d94aa3f0efe21092",
        "Ubuntu CD",
        include_str!("../../../scripts/keys/ubuntu-cd-D94AA3F0EFE21092.asc"),
    );
    let keyring = debian_media::Keyring::from_keys("Ubuntu CD", vec![key]);
    assert!(verify_detached(&keyring, SUMS, SIGNATURE, URL).is_err());
}

#[test]
fn cloud_fetch_rejects_bad_metadata_before_requesting_the_archive_and_purges_it() {
    let directory = TempDir::new("ubuntu-cloud-bad-signature");
    let mut fixture = Fixture::new();
    let mut changed = SUMS.to_vec();
    changed[0] ^= 1;
    fixture
        .put(URL, changed)
        .put(format!("{URL}.gpg"), SIGNATURE.to_vec());
    let archive = directory.path().join(debian_media::UBUNTU_CLOUD_ARCHIVE);
    std::fs::write(&archive, b"untrusted cached data").unwrap();
    std::fs::write(debian_media::partial_path(&archive), b"partial").unwrap();
    let result = debian_media::fetch_ubuntu_cloud_with(
        &fixture,
        directory.path(),
        debian_media::FetchOptions::default(),
    );
    assert!(result.is_err());
    assert_eq!(
        fixture.requested_urls(),
        vec![URL.to_string(), format!("{URL}.gpg")]
    );
    assert!(!archive.exists());
    assert!(!debian_media::partial_path(&archive).exists());
}

#[test]
fn offline_cloud_fetch_does_not_contact_the_transport() {
    let directory = TempDir::new("ubuntu-cloud-offline");
    let fixture = Fixture::new();
    let result = debian_media::fetch_ubuntu_cloud_with(
        &fixture,
        directory.path(),
        debian_media::FetchOptions {
            offline: true,
            refresh: false,
        },
    );
    assert!(result.is_err());
    assert!(fixture.requests().is_empty());
}

#[test]
fn an_offline_corrupt_archive_is_removed_after_authenticated_digest_mismatch() {
    let directory = TempDir::new("ubuntu-cloud-corrupt-archive");
    std::fs::write(directory.path().join("SHA256SUMS"), SUMS).unwrap();
    std::fs::write(directory.path().join("SHA256SUMS.gpg"), SIGNATURE).unwrap();
    let archive = directory.path().join(debian_media::UBUNTU_CLOUD_ARCHIVE);
    std::fs::write(&archive, b"corrupt image").unwrap();
    let fixture = Fixture::new();
    let result = debian_media::fetch_ubuntu_cloud_with(
        &fixture,
        directory.path(),
        debian_media::FetchOptions {
            offline: true,
            refresh: false,
        },
    );
    assert!(matches!(
        result,
        Err(debian_media::MediaError::DigestMismatch { .. })
    ));
    assert!(!archive.exists());
    assert!(fixture.requests().is_empty());
}
