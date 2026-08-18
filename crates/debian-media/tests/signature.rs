//! OpenPGP verification: accept, reject and fingerprint pinning (MVP-607).
//!
//! Runs entirely offline. The accept/reject pairs use a key generated in-process
//! so no private key is committed; the pinning tests use the real key files under
//! `crates/debian-media/keys/`.

mod support;

use debian_media::{
    verify_detached, MediaError, DEBIAN_ARCHIVE, DEBIAN_ARCHIVE_FINGERPRINTS, DEBIAN_CD,
    DEBIAN_CD_FINGERPRINTS,
};
use support::TestKey;

const SUMS: &[u8] = b"aa  debian-13.6.0-amd64-netinst.iso\n";

#[test]
fn a_good_signature_from_a_pinned_key_is_accepted() {
    let key = TestKey::generate("VMHost test <test@example.invalid>");
    let keyring = key.keyring("test");
    let signature = key.detached_sign(SUMS);

    let verified = verify_detached(
        keyring,
        SUMS,
        &signature,
        "https://example.invalid/SHA512SUMS",
    )
    .expect("signature verifies");
    assert_eq!(verified.primary_fingerprint, key.fingerprint);
    assert_eq!(verified.signing_fingerprint, key.fingerprint);
}

#[test]
fn a_signature_over_different_data_is_rejected() {
    let key = TestKey::generate("VMHost test <test@example.invalid>");
    let keyring = key.keyring("test");
    let signature = key.detached_sign(SUMS);

    let tampered = b"bb  debian-13.6.0-amd64-netinst.iso\n";
    let err = verify_detached(keyring, tampered, &signature, "u").expect_err("must not verify");
    assert!(matches!(err, MediaError::Signature { .. }), "{err}");
}

#[test]
fn a_signature_from_a_key_outside_the_keyring_is_rejected() {
    let trusted = TestKey::generate("trusted <a@example.invalid>");
    let attacker = TestKey::generate("attacker <b@example.invalid>");
    let keyring = trusted.keyring("test");

    let signature = attacker.detached_sign(SUMS);
    let err = verify_detached(keyring, SUMS, &signature, "u").expect_err("must not verify");
    assert!(matches!(err, MediaError::Signature { .. }), "{err}");
}

#[test]
fn a_flipped_bit_in_the_signature_is_rejected() {
    let key = TestKey::generate("VMHost test <test@example.invalid>");
    let keyring = key.keyring("test");
    let mut signature = key.detached_sign(SUMS);
    // Corrupt a byte in the middle of the base64 payload.
    let mid = signature.len() / 2;
    signature[mid] ^= 0x01;

    let err = verify_detached(keyring, SUMS, &signature, "u").expect_err("must not verify");
    assert!(matches!(err, MediaError::Signature { .. }), "{err}");
}

/// The fingerprint pin is the trust anchor: if a key file no longer matches the
/// hardcoded fingerprint the keyring must become unusable rather than trust the
/// substituted key.
#[test]
fn a_key_that_does_not_match_its_pinned_fingerprint_makes_the_keyring_unusable() {
    let key = TestKey::generate("VMHost test <test@example.invalid>");
    let wrong = "0".repeat(40);
    let keyring = key.keyring_pinning("test", &wrong);
    let signature = key.detached_sign(SUMS);

    let err = verify_detached(keyring, SUMS, &signature, "u").expect_err("must not verify");
    assert!(
        matches!(err, MediaError::Keyring { .. }),
        "expected a keyring error, got {err}"
    );
}

/// MVP-607: the committed Debian key files are the keys we think they are.
#[test]
fn the_real_committed_keys_match_their_pinned_fingerprints() {
    assert_eq!(
        DEBIAN_CD.fingerprints().expect("CD keyring loads"),
        DEBIAN_CD_FINGERPRINTS,
        "crates/debian-media/keys/debian-cd-*.asc changed"
    );
    assert_eq!(
        DEBIAN_ARCHIVE
            .fingerprints()
            .expect("archive keyring loads"),
        DEBIAN_ARCHIVE_FINGERPRINTS,
        "crates/debian-media/keys/debian-{{archive,release}}-*.asc changed"
    );
}

/// The fingerprints are v4 OpenPGP fingerprints in lower-case hex; a typo that
/// upper-cases or truncates one would silently stop matching.
#[test]
fn pinned_fingerprint_constants_are_well_formed() {
    for fpr in DEBIAN_CD_FINGERPRINTS
        .iter()
        .chain(DEBIAN_ARCHIVE_FINGERPRINTS)
    {
        assert_eq!(fpr.len(), 40, "{fpr}");
        assert!(
            fpr.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "{fpr} must be lower-case hex"
        );
    }
}

/// The CD half of the trust chain, verified offline against real Debian data.
///
/// `tests/fixtures/cdimage-SHA512SUMS{,.sign}` are the genuine files from
/// `cdimage.debian.org/debian-cd/current/amd64/iso-cd/` as of Debian 13.6.0.
/// The signature over those exact bytes stays valid forever, so this pins down
/// the whole CD-side metadata path — rpgp accepting a real Debian CD signature,
/// checksum parsing, and picking the plain netinst image out of the edu/mac
/// decoys — with no network access at all.
#[test]
fn the_real_cdimage_sums_verify_and_resolve_offline() {
    let sums = include_bytes!("fixtures/cdimage-SHA512SUMS");
    let signature = include_bytes!("fixtures/cdimage-SHA512SUMS.sign");

    let verified = verify_detached(
        &DEBIAN_CD,
        sums,
        signature,
        "https://cdimage.debian.org/debian-cd/current/amd64/iso-cd/SHA512SUMS",
    )
    .expect("a real Debian CD signature must verify against the pinned keyring");
    assert!(
        DEBIAN_CD_FINGERPRINTS.contains(&verified.primary_fingerprint.as_str()),
        "verified by an unexpected key {}",
        verified.primary_fingerprint
    );

    // Only now may the digests be used (order is the security property).
    let entries = debian_media::parse_sums(std::str::from_utf8(sums).unwrap()).expect("parses");
    let iso = debian_media::find_plain_netinst_iso(&entries, "amd64").expect("plain netinst found");
    assert_eq!(iso.file_name, "debian-13.6.0-amd64-netinst.iso");
    assert_eq!(
        debian_media::version_from_iso_name(&iso.file_name),
        Some("13.6.0")
    );
    // The edu and mac flavors are present in this very file — that is the trap.
    assert!(entries
        .iter()
        .any(|e| e.file_name.starts_with("debian-edu-")));
    assert!(entries
        .iter()
        .any(|e| e.file_name.starts_with("debian-mac-")));
}

/// Tampering with a single byte of a real Debian checksum file must break its
/// real signature.
#[test]
fn a_tampered_real_cdimage_sums_file_is_rejected() {
    let mut sums = include_bytes!("fixtures/cdimage-SHA512SUMS").to_vec();
    let signature = include_bytes!("fixtures/cdimage-SHA512SUMS.sign");
    sums[0] = if sums[0] == b'a' { b'b' } else { b'a' };

    let err = verify_detached(&DEBIAN_CD, &sums, signature, "u").expect_err("must not verify");
    assert!(matches!(err, MediaError::Signature { .. }), "{err}");
}

/// A signature file with no OpenPGP packets, or with garbage, must fail cleanly
/// against the real keyring — never panic, never succeed.
#[test]
fn malformed_signature_files_fail_against_the_real_keyring() {
    for bytes in [
        b"".as_slice(),
        b"hello".as_slice(),
        b"-----BEGIN PGP SIGNATURE-----\n\nnot base64 at all\n-----END PGP SIGNATURE-----\n",
        &[0xffu8; 64],
    ] {
        let err = verify_detached(&DEBIAN_CD, SUMS, bytes, "u").expect_err("must not verify");
        assert!(
            matches!(err, MediaError::Signature { .. }),
            "unexpected error {err}"
        );
    }
}
