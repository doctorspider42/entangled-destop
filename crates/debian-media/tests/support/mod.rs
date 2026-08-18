//! Shared offline test scaffolding for the Debian downloader (EPIC 6).
//!
//! Nothing in here touches the network. Two building blocks:
//!
//! * [`Fixture`] — an in-memory [`Transport`] serving a map of URL → bytes, with
//!   knobs for the failure modes the acceptance criteria care about (dropped
//!   connection, server that ignores `Range`, 404, oversized body).
//! * [`TestKey`] — a throwaway OpenPGP key generated in-process, so signature
//!   accept *and* reject paths can be exercised without shipping a private key.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Mutex;

use debian_media::{
    Arch, DigestAlgo, Download, InstallerVariant, Keyring, MediaKind, MediaSource, PinnedKey,
    Transport, TransportError, TrustRoot,
};
use pgp::composed::{DetachedSignature, KeyType, SecretKeyParamsBuilder, SignedSecretKey};
use pgp::crypto::hash::HashAlgorithm;
use pgp::types::{KeyDetails, Password};

// ---------------------------------------------------------------- transport

/// One recorded request, so tests can prove *what* was fetched and in which
/// order — the trust-chain ordering is an assertion about requests, not just
/// about return values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub url: String,
    /// `None` for a whole-file GET, `Some(offset)` for a ranged GET.
    pub range_from: Option<u64>,
}

#[derive(Default)]
pub struct Fixture {
    files: HashMap<String, Vec<u8>>,
    /// URLs that must answer 404.
    missing: Vec<String>,
    /// Pretend the server does not understand `Range`: always answer 200 with
    /// the whole body.
    pub ignore_range: bool,
    /// Cut every streamed body after this many bytes, simulating a connection
    /// that dropped mid-transfer.
    pub cut_at: Option<usize>,
    /// Do not send `Content-Length`/`Content-Range`, so the caller cannot tell a
    /// short read from a complete one.
    pub hide_length: bool,
    log: Mutex<Vec<Request>>,
}

impl Fixture {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(&mut self, url: impl Into<String>, body: impl Into<Vec<u8>>) -> &mut Self {
        self.files.insert(url.into(), body.into());
        self
    }

    pub fn remove(&mut self, url: &str) -> &mut Self {
        self.files.remove(url);
        self.missing.push(url.to_string());
        self
    }

    pub fn requests(&self) -> Vec<Request> {
        self.log.lock().expect("log mutex").clone()
    }

    pub fn requested_urls(&self) -> Vec<String> {
        self.requests().into_iter().map(|r| r.url).collect()
    }

    pub fn clear_log(&self) {
        self.log.lock().expect("log mutex").clear();
    }

    fn record(&self, url: &str, range_from: Option<u64>) {
        self.log.lock().expect("log mutex").push(Request {
            url: url.to_string(),
            range_from,
        });
    }

    fn body(&self, url: &str) -> Result<&Vec<u8>, TransportError> {
        self.files.get(url).ok_or(TransportError::Status(404))
    }
}

impl Transport for Fixture {
    fn get_all(&self, url: &str, limit: u64) -> Result<Vec<u8>, TransportError> {
        self.record(url, None);
        let body = self.body(url)?;
        if body.len() as u64 > limit {
            return Err(TransportError::TooLarge { limit });
        }
        Ok(body.clone())
    }

    fn get_range(&self, url: &str, offset: u64) -> Result<Download, TransportError> {
        self.record(url, (offset > 0).then_some(offset));
        let body = self.body(url)?;
        let total = body.len() as u64;

        let (start, resumed) = if offset == 0 || self.ignore_range {
            (0u64, false)
        } else if offset >= total {
            return Err(TransportError::Status(416));
        } else {
            (offset, true)
        };

        let mut slice = body[start as usize..].to_vec();
        if let Some(cut) = self.cut_at {
            slice.truncate(cut);
        }
        Ok(Download {
            resumed,
            total_len: (!self.hide_length).then_some(total),
            body: Box::new(Cursor::new(slice)),
        })
    }
}

// ------------------------------------------------------------------ keys

/// A throwaway OpenPGP certificate for signing test fixtures.
pub struct TestKey {
    secret: SignedSecretKey,
    pub fingerprint: String,
    pub armored_public: String,
}

impl TestKey {
    pub fn generate(user_id: &str) -> Self {
        let mut rng = rand::thread_rng();
        let params = SecretKeyParamsBuilder::default()
            // Ed25519 keeps key generation instant even in a debug build.
            .key_type(KeyType::Ed25519Legacy)
            .can_certify(true)
            .can_sign(true)
            .primary_user_id(user_id.to_string())
            .build()
            .expect("key params");
        let secret = params
            .generate(&mut rng)
            .expect("generate key")
            .sign(&mut rng, &Password::empty())
            .expect("self-sign key");
        let public = secret.signed_public_key();
        let fingerprint = hex::encode(public.fingerprint().as_bytes());
        let armored_public = public
            .to_armored_string(None.into())
            .expect("armor public key");
        Self {
            secret,
            fingerprint,
            armored_public,
        }
    }

    /// An armored detached signature over `data`, as Debian's `.sign`/`.gpg`
    /// files carry.
    pub fn detached_sign(&self, data: &[u8]) -> Vec<u8> {
        let rng = rand::thread_rng();
        DetachedSignature::sign_binary_data(
            rng,
            &self.secret.primary_key,
            &Password::empty(),
            HashAlgorithm::Sha512,
            data,
        )
        .expect("sign")
        .to_armored_bytes(None.into())
        .expect("armor signature")
    }

    /// A keyring pinning this key, ready to hand to a [`TestSource`].
    ///
    /// Leaked on purpose: [`TrustRoot`] holds `&'static Keyring` because the real
    /// keyrings are compiled in. One leak per test process is irrelevant.
    pub fn keyring(&self, name: &'static str) -> &'static Keyring {
        self.keyring_pinning(name, &self.fingerprint)
    }

    /// Same, but pinning an arbitrary fingerprint — used to prove the pin is
    /// actually enforced.
    pub fn keyring_pinning(&self, name: &'static str, fingerprint: &str) -> &'static Keyring {
        Box::leak(Box::new(Keyring::from_keys(
            name,
            vec![PinnedKey::new(
                fingerprint.to_string(),
                "test key".to_string(),
                self.armored_public.clone(),
            )],
        )))
    }
}

// ----------------------------------------------------------------- source

/// A [`MediaSource`] pointing at fixture URLs with an injectable keyring, so the
/// full fetch flow (resume, cleanup, cache, manifests) runs offline.
pub struct TestSource {
    pub base: String,
    pub arch: Arch,
    pub root: TrustRoot,
    pub artifacts: Vec<MediaKind>,
    /// Path of each artifact inside the signed digest list. Empty for the ISO,
    /// whose name must be discovered.
    pub paths: HashMap<&'static str, String>,
}

fn kind_key(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Kernel => "kernel",
        MediaKind::Initrd => "initrd",
        MediaKind::Iso => "iso",
        MediaKind::Sha512Sums => "sums",
        MediaKind::Sha512SumsSignature => "sig",
    }
}

impl TestSource {
    /// A cdimage-style source: `<base>/SHA512SUMS` + `.sign`, netboot-like
    /// artifact names.
    pub fn detached(base: &str, keyring: &'static Keyring, algo: DigestAlgo) -> Self {
        Self {
            base: base.to_string(),
            arch: Arch::Amd64,
            root: TrustRoot::DetachedSums {
                dir_url: base.to_string(),
                sums_file: "SHA512SUMS",
                signature_file: "SHA512SUMS.sign",
                algo,
                keyring,
            },
            artifacts: vec![MediaKind::Kernel, MediaKind::Initrd],
            paths: HashMap::from([
                ("kernel", "netboot/linux".to_string()),
                ("initrd", "netboot/initrd.gz".to_string()),
            ]),
        }
    }

    /// A cdimage-style source whose single artifact is a discovered netinst ISO.
    pub fn detached_iso(base: &str, keyring: &'static Keyring) -> Self {
        let mut s = Self::detached(base, keyring, DigestAlgo::Sha512);
        s.artifacts = vec![MediaKind::Iso];
        s.paths.clear();
        s
    }

    /// An archive-style source: `<base>/Release` + `Release.gpg` committing to
    /// `images/SHA256SUMS`.
    pub fn archive(base: &str, keyring: &'static Keyring) -> Self {
        Self {
            base: base.to_string(),
            arch: Arch::Amd64,
            root: TrustRoot::ArchiveRelease {
                dists_url: base.to_string(),
                index_path: "images/SHA256SUMS".to_string(),
                index_base_url: format!("{base}/images"),
                algo: DigestAlgo::Sha256,
                keyring,
            },
            artifacts: vec![MediaKind::Kernel, MediaKind::Initrd],
            paths: HashMap::from([
                ("kernel", "netboot/linux".to_string()),
                ("initrd", "netboot/initrd.gz".to_string()),
            ]),
        }
    }
}

impl MediaSource for TestSource {
    fn directory_url(&self, _variant: InstallerVariant, _kind: MediaKind) -> String {
        self.base.clone()
    }

    fn file_name(&self, _variant: InstallerVariant, kind: MediaKind) -> Option<&'static str> {
        match kind {
            MediaKind::Kernel => Some("linux"),
            MediaKind::Initrd => Some("initrd.gz"),
            _ => None,
        }
    }

    fn trust_root(&self, _variant: InstallerVariant) -> TrustRoot {
        self.root.clone()
    }

    fn sums_path(&self, _variant: InstallerVariant, kind: MediaKind) -> Option<String> {
        self.paths.get(kind_key(kind)).cloned()
    }

    fn artifacts(&self, _variant: InstallerVariant) -> &'static [MediaKind] {
        // `MediaSource` returns a static slice (the real source's lists are
        // compile-time constants); tests only need the two shapes.
        if self.artifacts == [MediaKind::Iso] {
            &[MediaKind::Iso]
        } else {
            &[MediaKind::Kernel, MediaKind::Initrd]
        }
    }
}

// ------------------------------------------------------------- misc helpers

/// A scratch directory that removes itself on drop.
pub struct TempDir {
    path: std::path::PathBuf,
}

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let unique = format!(
            "vmhost-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Renders a `sha512sum`-style checksum file from (name, content) pairs.
pub fn sums_file(algo: DigestAlgo, files: &[(&str, &[u8])]) -> String {
    files
        .iter()
        .map(|(name, body)| format!("{}  {name}\n", algo.hex_of(body)))
        .collect()
}
