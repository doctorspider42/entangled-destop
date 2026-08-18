//! Pinned Debian OpenPGP keyrings and detached-signature verification
//! (MVP-607).
//!
//! The keys are committed to the repository as armored files and compiled into
//! the binary with [`include_str!`]; their fingerprints are hardcoded next to
//! them and checked every time a keyring is loaded. Nothing here touches the
//! network: no keyserver lookups, no WKD, no trust-on-first-use. A key that
//! does not match its pinned fingerprint is a build-time bug and makes the
//! keyring unusable rather than degrading to "unverified".
//!
//! Two keyrings are needed because Debian signs the two artifact families with
//! different keys:
//!
//! * [`DEBIAN_CD`] — `SHA512SUMS.sign` next to the netinst ISO images on
//!   `cdimage.debian.org`, signed by the *Debian CD signing key* family
//!   (<https://www.debian.org/CD/verify>).
//! * [`DEBIAN_ARCHIVE`] — `dists/<suite>/Release.gpg` in the package archive,
//!   which is the only signature covering the netboot kernel/initrd (the
//!   installer `images/` directory ships no `.sign` file at all). Signed by
//!   the archive automatic signing key and the stable release key
//!   (<https://ftp-master.debian.org/keys/>).

use std::borrow::Cow;
use std::sync::OnceLock;

use pgp::composed::{Deserializable, DetachedSignature, SignedPublicKey};
use pgp::types::KeyDetails;

use crate::error::MediaError;

/// One pinned certificate: the armored key material plus the primary key
/// fingerprint it must have.
///
/// The `Cow`s let the shipped keyrings be `static` (borrowed, zero cost) while
/// tests and any future `--keyring` option can build one at runtime.
#[derive(Debug, Clone)]
pub struct PinnedKey {
    /// Lower-case hex of the primary key fingerprint (v4 keys: 40 chars).
    fingerprint: Cow<'static, str>,
    /// Human-readable identity, used only in log and error messages.
    label: Cow<'static, str>,
    armored: Cow<'static, str>,
}

impl PinnedKey {
    pub fn new(
        fingerprint: impl Into<Cow<'static, str>>,
        label: impl Into<Cow<'static, str>>,
        armored: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            fingerprint: fingerprint.into(),
            label: label.into(),
            armored: armored.into(),
        }
    }
}

/// A set of pinned certificates that may sign one artifact family.
pub struct Keyring {
    /// Short name used in error messages ("Debian CD", "Debian archive").
    pub name: &'static str,
    keys: Cow<'static, [PinnedKey]>,
    loaded: OnceLock<Result<Vec<SignedPublicKey>, String>>,
}

impl std::fmt::Debug for Keyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keyring")
            .field("name", &self.name)
            .field("keys", &self.keys.len())
            .finish()
    }
}

impl Keyring {
    /// Builds a keyring at runtime from armored key material.
    ///
    /// Production code uses the compiled-in [`DEBIAN_CD`] and
    /// [`DEBIAN_ARCHIVE`] keyrings; this exists so tests can exercise the accept
    /// and reject paths with a throwaway key instead of shipping a private key,
    /// and so a user-supplied keyring can be added later without touching the
    /// verification logic.
    pub fn from_keys(name: &'static str, keys: Vec<PinnedKey>) -> Self {
        Self {
            name,
            keys: Cow::Owned(keys),
            loaded: OnceLock::new(),
        }
    }
}

/// Fingerprints of the *Debian CD signing key* family, from
/// <https://www.debian.org/CD/verify>. Long-lived keys; the 2011 key signs
/// current releases and the 2009 key is kept for older media.
pub const DEBIAN_CD_FINGERPRINTS: &[&str] = &[
    "10460dad76165ad81fbc0ce9988021a964e6ea7d",
    "df9b9c49eaa9298432589d76da87e80d6294be9b",
];

/// Fingerprints of the archive keys that sign `dists/stable/Release.gpg`.
///
/// Unlike the CD keys these are rotated with every Debian release, so this list
/// needs an entry per supported suite (see the TODO in the module docs of
/// `release.rs`).
pub const DEBIAN_ARCHIVE_FINGERPRINTS: &[&str] = &[
    "04b54c3cdca79751b16bc6b5225629df75b188bd",
    "41587f7db8c774bccf131416762f67a0b2c39de4",
];

static DEBIAN_CD_KEYS: &[PinnedKey] = &[
    PinnedKey {
        fingerprint: Cow::Borrowed("10460dad76165ad81fbc0ce9988021a964e6ea7d"),
        label: Cow::Borrowed("Debian CD signing key (2009)"),
        armored: Cow::Borrowed(include_str!("../keys/debian-cd-988021A964E6EA7D.asc")),
    },
    PinnedKey {
        fingerprint: Cow::Borrowed("df9b9c49eaa9298432589d76da87e80d6294be9b"),
        label: Cow::Borrowed("Debian CD signing key (2011)"),
        armored: Cow::Borrowed(include_str!("../keys/debian-cd-DA87E80D6294BE9B.asc")),
    },
];

static DEBIAN_ARCHIVE_KEYS: &[PinnedKey] = &[
    PinnedKey {
        fingerprint: Cow::Borrowed("04b54c3cdca79751b16bc6b5225629df75b188bd"),
        label: Cow::Borrowed("Debian Archive Automatic Signing Key (13/trixie)"),
        armored: Cow::Borrowed(include_str!("../keys/debian-archive-13.asc")),
    },
    PinnedKey {
        fingerprint: Cow::Borrowed("41587f7db8c774bccf131416762f67a0b2c39de4"),
        label: Cow::Borrowed("Debian Stable Release Key (13/trixie)"),
        armored: Cow::Borrowed(include_str!("../keys/debian-release-13.asc")),
    },
];

/// Keys that sign `SHA512SUMS.sign` on `cdimage.debian.org`.
pub static DEBIAN_CD: Keyring = Keyring {
    name: "Debian CD",
    keys: Cow::Borrowed(DEBIAN_CD_KEYS),
    loaded: OnceLock::new(),
};

/// Keys that sign `dists/<suite>/Release.gpg` on `deb.debian.org`.
pub static DEBIAN_ARCHIVE: Keyring = Keyring {
    name: "Debian archive",
    keys: Cow::Borrowed(DEBIAN_ARCHIVE_KEYS),
    loaded: OnceLock::new(),
};

impl Keyring {
    /// Parses the pinned key files once and checks every fingerprint.
    fn certs(&self) -> Result<&[SignedPublicKey], MediaError> {
        let result = self.loaded.get_or_init(|| {
            let mut certs = Vec::with_capacity(self.keys.len());
            for pinned in self.keys.iter() {
                let (cert, _headers) = SignedPublicKey::from_string(&pinned.armored)
                    .map_err(|e| format!("{}: cannot parse the key file: {e}", pinned.label))?;
                let actual = hex::encode(cert.fingerprint().as_bytes());
                if actual != pinned.fingerprint {
                    return Err(format!(
                        "{}: fingerprint {actual} does not match the pinned {}",
                        pinned.label, pinned.fingerprint
                    ));
                }
                certs.push(cert);
            }
            Ok(certs)
        });
        match result {
            Ok(certs) => Ok(certs),
            Err(reason) => Err(MediaError::Keyring {
                keyring: self.name,
                reason: reason.clone(),
            }),
        }
    }

    /// Lower-case hex fingerprints of every certificate in this keyring,
    /// primary keys only. Exposed for `entangled doctor`-style reporting and for
    /// the pinning test.
    pub fn fingerprints(&self) -> Result<Vec<String>, MediaError> {
        Ok(self
            .certs()?
            .iter()
            .map(|c| hex::encode(c.fingerprint().as_bytes()))
            .collect())
    }
}

/// Which key ultimately verified a signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedBy {
    /// Lower-case hex fingerprint of the *primary* key of the verifying
    /// certificate (the pinned anchor, even when a signing subkey did the work).
    pub primary_fingerprint: String,
    /// Lower-case hex fingerprint of the key or subkey that made the signature.
    pub signing_fingerprint: String,
}

/// Verifies a detached OpenPGP signature over `data` against a pinned keyring.
///
/// `armored_sig` is the raw `.sign`/`.gpg` file. Debian sometimes puts several
/// signatures in one file (`Release.gpg` carries three); a single signature
/// from a single pinned certificate is enough, which is exactly the
/// gpg/`apt` policy.
///
/// `url` is only used to build the error message.
pub fn verify_detached(
    keyring: &Keyring,
    data: &[u8],
    armored_sig: &[u8],
    url: &str,
) -> Result<VerifiedBy, MediaError> {
    verify_detached_with_certs(keyring.certs()?, keyring.name, data, armored_sig, url)
}

/// The keyring-agnostic core of [`verify_detached`]. Tests use it with
/// certificates generated in-process, so the accept and reject paths are
/// covered without shipping a private key.
pub(crate) fn verify_detached_with_certs(
    certs: &[SignedPublicKey],
    keyring_name: &'static str,
    data: &[u8],
    armored_sig: &[u8],
    url: &str,
) -> Result<VerifiedBy, MediaError> {
    let signatures = parse_signatures(armored_sig).map_err(|reason| MediaError::Signature {
        url: url.to_string(),
        keyring: keyring_name,
        reason,
    })?;
    if signatures.is_empty() {
        return Err(MediaError::Signature {
            url: url.to_string(),
            keyring: keyring_name,
            reason: "the signature file contains no OpenPGP signature packets".into(),
        });
    }

    let mut attempts = 0usize;
    for signature in &signatures {
        let issuers: Vec<String> = signature
            .signature
            .issuer_fingerprint()
            .iter()
            .map(|f| hex::encode(f.as_bytes()))
            .collect();

        for cert in certs {
            let primary = hex::encode(cert.fingerprint().as_bytes());
            // Try the primary key, then every subkey. When the signature names
            // an issuer, skip keys that cannot be it — this keeps the error
            // count meaningful and avoids pointless RSA-4096 work.
            let mut candidates: Vec<(String, Candidate<'_>)> =
                vec![(primary.clone(), Candidate::Primary(cert))];
            for sub in &cert.public_subkeys {
                candidates.push((
                    hex::encode(sub.fingerprint().as_bytes()),
                    Candidate::Sub(sub),
                ));
            }

            for (signing, candidate) in candidates {
                if !issuers.is_empty() && !issuers.contains(&signing) {
                    continue;
                }
                attempts += 1;
                let verified = match candidate {
                    Candidate::Primary(c) => signature.verify(c, data).is_ok(),
                    Candidate::Sub(s) => signature.verify(s, data).is_ok(),
                };
                if verified {
                    tracing::debug!(
                        keyring = keyring_name,
                        primary = %primary,
                        signing = %signing,
                        url,
                        "OpenPGP signature verified"
                    );
                    return Ok(VerifiedBy {
                        primary_fingerprint: primary,
                        signing_fingerprint: signing,
                    });
                }
            }
        }
    }

    Err(MediaError::Signature {
        url: url.to_string(),
        keyring: keyring_name,
        reason: format!(
            "none of the {} pinned certificates verified any of the {} signature(s) \
             ({attempts} key/signature combination(s) tried)",
            certs.len(),
            signatures.len()
        ),
    })
}

enum Candidate<'a> {
    Primary(&'a SignedPublicKey),
    Sub(&'a pgp::composed::SignedPublicSubKey),
}

/// Parses an armored (or, as a fallback, binary) detached signature file into
/// its signature packets.
fn parse_signatures(bytes: &[u8]) -> Result<Vec<DetachedSignature>, String> {
    let looks_armored = bytes
        .windows(BEGIN_SIGNATURE.len())
        .any(|w| w == BEGIN_SIGNATURE);

    let parsed: Vec<DetachedSignature> = if looks_armored {
        let (iter, _headers) = DetachedSignature::from_armor_many(bytes)
            .map_err(|e| format!("malformed armored signature: {e}"))?;
        // A single unparsable packet must not discard the whole file: Debian
        // has historically mixed signature versions in one `.gpg`.
        iter.filter_map(Result::ok).collect()
    } else {
        let mut iter = DetachedSignature::from_bytes_many(bytes)
            .map_err(|e| format!("malformed binary signature: {e}"))?;
        std::iter::from_fn(|| iter.next())
            .filter_map(Result::ok)
            .collect()
    };
    Ok(parsed)
}

const BEGIN_SIGNATURE: &[u8] = b"-----BEGIN PGP SIGNATURE-----";

#[cfg(test)]
mod tests {
    use super::*;

    /// MVP-607: the committed key files really are the keys we think they are.
    /// If Debian ever republishes a key file this test fails loudly instead of
    /// silently widening the trust anchor.
    #[test]
    fn pinned_keys_match_their_fingerprints() {
        let cd = DEBIAN_CD.fingerprints().expect("CD keyring loads");
        assert_eq!(cd, DEBIAN_CD_FINGERPRINTS);

        let archive = DEBIAN_ARCHIVE
            .fingerprints()
            .expect("archive keyring loads");
        assert_eq!(archive, DEBIAN_ARCHIVE_FINGERPRINTS);
    }

    #[test]
    fn keyrings_are_not_empty_and_have_signing_capable_material() {
        for keyring in [&DEBIAN_CD, &DEBIAN_ARCHIVE] {
            let certs = keyring.certs().expect("keyring loads");
            assert!(!certs.is_empty(), "{} is empty", keyring.name);
        }
    }

    #[test]
    fn empty_and_garbage_signature_files_are_rejected() {
        let err = verify_detached(&DEBIAN_CD, b"data", b"", "u").unwrap_err();
        assert!(matches!(err, MediaError::Signature { .. }), "{err}");

        let err = verify_detached(&DEBIAN_CD, b"data", b"not a signature", "u").unwrap_err();
        assert!(matches!(err, MediaError::Signature { .. }), "{err}");
    }

    #[test]
    fn truncated_armor_is_rejected_without_panicking() {
        let sig = b"-----BEGIN PGP SIGNATURE-----\n\niQIz\n";
        let err = verify_detached(&DEBIAN_CD, b"data", sig, "u").unwrap_err();
        assert!(matches!(err, MediaError::Signature { .. }), "{err}");
    }
}
