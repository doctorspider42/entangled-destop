//! Streaming digests (backlog MVP-608).
//!
//! Debian publishes SHA-512 checksums for CD images and SHA-256 checksums for
//! the archive-side installer images, so both widths are supported. Artifacts
//! are hashed while they stream to disk — the netinst ISO is ~700 MB and is
//! never held in memory.

use sha2::{Digest, Sha256, Sha512};

/// Digest algorithm used by a Debian checksum file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestAlgo {
    Sha256,
    Sha512,
}

impl DigestAlgo {
    /// Length of the lower-case hex encoding of this digest.
    pub const fn hex_len(self) -> usize {
        match self {
            DigestAlgo::Sha256 => 64,
            DigestAlgo::Sha512 => 128,
        }
    }

    /// Infers the algorithm from the length of a hex digest, e.g. when reading a
    /// digest back out of a provenance manifest.
    pub const fn from_hex_len(len: usize) -> Option<Self> {
        match len {
            64 => Some(DigestAlgo::Sha256),
            128 => Some(DigestAlgo::Sha512),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            DigestAlgo::Sha256 => "sha256",
            DigestAlgo::Sha512 => "sha512",
        }
    }

    /// Starts an incremental hash.
    pub fn hasher(self) -> Hasher {
        match self {
            DigestAlgo::Sha256 => Hasher::Sha256(Sha256::new()),
            DigestAlgo::Sha512 => Hasher::Sha512(Sha512::new()),
        }
    }

    /// One-shot digest of an in-memory buffer (checksum files, `Release`).
    pub fn hex_of(self, data: &[u8]) -> String {
        let mut h = self.hasher();
        h.update(data);
        h.finish_hex()
    }
}

/// Incremental hasher over either supported width.
#[derive(Debug, Clone)]
pub enum Hasher {
    Sha256(Sha256),
    Sha512(Sha512),
}

impl Hasher {
    pub fn update(&mut self, data: &[u8]) {
        match self {
            Hasher::Sha256(h) => h.update(data),
            Hasher::Sha512(h) => h.update(data),
        }
    }

    /// Consumes the hasher and returns the lower-case hex digest.
    pub fn finish_hex(self) -> String {
        match self {
            Hasher::Sha256(h) => hex::encode(h.finalize()),
            Hasher::Sha512(h) => hex::encode(h.finalize()),
        }
    }
}

/// Constant-time-ish comparison of two hex digests, case-insensitive.
///
/// Digests are public values, so this is about correctness (case and length),
/// not about timing side channels.
pub fn digests_match(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.eq_ignore_ascii_case(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_answer_tests() {
        assert_eq!(
            DigestAlgo::Sha256.hex_of(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            DigestAlgo::Sha512.hex_of(b"abc"),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
             2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
                .replace(char::is_whitespace, "")
        );
    }

    #[test]
    fn incremental_matches_one_shot() {
        let mut h = DigestAlgo::Sha512.hasher();
        h.update(b"ab");
        h.update(b"c");
        assert_eq!(h.finish_hex(), DigestAlgo::Sha512.hex_of(b"abc"));
    }

    #[test]
    fn hex_lengths_are_declared_correctly() {
        assert_eq!(
            DigestAlgo::Sha256.hex_of(b"").len(),
            DigestAlgo::Sha256.hex_len()
        );
        assert_eq!(
            DigestAlgo::Sha512.hex_of(b"").len(),
            DigestAlgo::Sha512.hex_len()
        );
    }

    #[test]
    fn digest_comparison_ignores_case_but_not_length() {
        assert!(digests_match("ABcd", "abCD"));
        assert!(!digests_match("abcd", "abcde"));
    }
}
