//! The guest bootstrap artifacts — where they come from on a host that cannot
//! build them.
//!
//! `entangled install debian` runs the Debian installer on **this project's own
//! kernel**, not on d-i's: Debian builds `virtio_mmio` without
//! `CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES`, so their installer kernel cannot see a
//! single one of our devices. The installed system then boots through the same
//! kernel plus a small initramfs that mounts `/dev/vda` and `switch_root`s into
//! it. Two files, `vmlinuz` and `initrd.img`, and until now the only way to get
//! them was `bash guest/bootstrap-kernel/build.sh` — a Linux kernel build, which
//! does not cross-build. That made `install debian` impossible on Windows, and
//! "go find a Linux machine" is not a product.
//!
//! So the release pipeline builds them once on Linux and publishes them as
//! assets of a GitHub Release, and this module fetches them into the same
//! verified cache the ISOs use. The pin, the digest check, the cache and the
//! private-repository token route all live in [`crate::artifact`], which the
//! firmware ([`crate::firmware`]) shares — read that module for what the pin is
//! worth as a trust anchor.
//!
//! # Licence — read this before adding an asset
//!
//! `vmlinuz` is a compiled Linux kernel: **GPL-2.0-only**, and *distributing* it
//! is what triggers the source obligation, regardless of it being guest-side
//! content. `cargo deny` does not see it — that gate reads our Cargo graph, not
//! our release assets. GPLv2 §3 is satisfied by the release itself: the same
//! release carries the exact upstream tarball, its `.config` and the build
//! script ("equivalent access to copy the source code from the same place").
//! See `.github/workflows/guest-artifacts.yml`, which will not publish without
//! them, and `docs/user-guide.md` § "The guest bootstrap artifacts".
//!
//! This is also why the kernel is **not** in the Windows installer while the
//! UEFI firmware is: shipping a GPL binary inside a setup executable drags the
//! source obligation onto every copy of that installer, and EDK2's
//! BSD-2-Clause-Patent has no such condition.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::artifact::{self, Hint, PinnedAsset};

pub use crate::artifact::{FetchOptions, FetchedAsset};

/// File name of the kernel asset, in the release and in the cache.
pub const KERNEL_FILE: &str = "vmlinuz";
/// File name of the initramfs asset.
pub const INITRD_FILE: &str = "initrd.img";

/// Where a checkout that built its own artifacts keeps them, relative to the
/// working directory — the path `install`, `doctor` and every generated profile
/// have always used.
pub const CHECKOUT_DIR: &str = "artifacts/bootstrap";

/// Override the release location: a mirror, or a directory served over HTTP for
/// an offline test. The pinned digests are enforced against whatever it serves,
/// so this relocates the download without weakening it.
const BASE_URL_ENV: &str = "ENTANGLED_BOOTSTRAP_BASE_URL";

/// Point at a directory that already holds `vmlinuz` and `initrd.img` — a
/// mounted Linux checkout, a shared drive. Used verbatim and *not* digest
/// checked, exactly like `artifacts/bootstrap/` in a checkout: both are "the
/// operator put these here on purpose".
const DIR_ENV: &str = "ENTANGLED_BOOTSTRAP_DIR";

/// The compiled-in pin. Its digests are what a download must hash to.
const PINNED_TOML: &str = include_str!("../../../guest/bootstrap-kernel/pinned.toml");

/// Where the pin lives, for messages that ask a human to look at it.
const PIN_PATH: &str = "guest/bootstrap-kernel/pinned.toml";

/// The workflow that publishes the release this pin names.
const WORKFLOW: &str = ".github/workflows/guest-artifacts.yml";

// ---------------------------------------------------------------------------
// The pin
// ---------------------------------------------------------------------------

/// One published set of guest artifacts, named by an immutable release tag.
#[derive(Debug, Clone, Deserialize)]
pub struct Pinned {
    /// The GitHub Release tag holding the assets. Immutable: a new build is a
    /// new tag, so an old `entangled` keeps fetching exactly what it was
    /// reviewed against.
    pub tag: String,
    /// Upstream kernel version the `vmlinuz` was built from, for the record and
    /// for the message that tells a user what they are downloading.
    pub kernel_version: String,
    /// `https://github.com/<owner>/<repo>/releases/download` — the tag and file
    /// name are appended.
    pub base_url: String,
    /// Where the corresponding source for `vmlinuz` is published, so the GPLv2
    /// obligation has an address a human can read out.
    pub source: String,
    pub assets: Vec<PinnedAsset>,
}

impl Pinned {
    fn asset(&self, name: &str) -> Result<&PinnedAsset, String> {
        self.assets
            .iter()
            .find(|a| a.name == name)
            .ok_or_else(|| format!("{PIN_PATH} names no asset '{name}'"))
    }

    /// The release description [`crate::artifact`] fetches against.
    fn release<'a>(&'a self, base_url: &'a str, version: &'a str) -> artifact::Release<'a> {
        artifact::Release {
            pin_path: PIN_PATH,
            tag: &self.tag,
            base_url,
            version,
            assets: &self.assets,
            hint: Hint {
                workflow: WORKFLOW,
                base_url_env: BASE_URL_ENV,
                dir_env: DIR_ENV,
            },
        }
    }

    /// The plain browser download URL for one asset. Only the tests need it
    /// spelled out; the fetch path builds it inside [`crate::artifact`].
    #[cfg(test)]
    fn url(&self, asset: &PinnedAsset) -> String {
        self.release(&base_url(self), "").url(asset)
    }
}

/// Reads the compiled-in pin.
///
/// A parse failure is a build-time mistake rather than a user's, so it says
/// which file to fix; `the_compiled_in_pin_parses_and_names_both_assets` keeps it
/// from ever reaching a release.
pub fn pinned() -> Result<Pinned, String> {
    let pin: Pinned = toml::from_str(PINNED_TOML)
        .map_err(|e| format!("{PIN_PATH} is not a valid guest-artifact pin: {e}"))?;
    artifact::validate_digests(PIN_PATH, &pin.assets)?;
    Ok(pin)
}

fn base_url(pin: &Pinned) -> String {
    std::env::var(BASE_URL_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| pin.base_url.clone())
}

// ---------------------------------------------------------------------------
// Finding them
// ---------------------------------------------------------------------------

/// How a located pair of artifacts was found — which is what decides whether
/// the paths written into a generated profile may stay relative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// `artifacts/bootstrap/` under the working directory: a Linux checkout that
    /// ran the build script. Relative, and deliberately left that way — every
    /// profile this project has ever written names it relatively.
    Checkout,
    /// A directory named by `ENTANGLED_BOOTSTRAP_DIR`.
    Directory,
    /// The verified cache, digest-checked against the pin on the way out.
    Cache,
}

impl Origin {
    pub const fn as_str(self) -> &'static str {
        match self {
            Origin::Checkout => "this checkout",
            Origin::Directory => "ENTANGLED_BOOTSTRAP_DIR",
            Origin::Cache => "the verified cache",
        }
    }
}

/// A usable kernel + initramfs pair.
#[derive(Debug, Clone)]
pub struct Artifacts {
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub origin: Origin,
}

/// The cache directory for one pinned release: `<cache>/bootstrap/<tag>`.
///
/// Per tag, not per version: two builds of the same kernel version differ, and
/// serving one where the other was pinned is the collision the media cache's
/// `<arch>-<variant>` component exists to avoid.
pub fn cache_dir(pin: &Pinned) -> Result<PathBuf, String> {
    Ok(crate::paths::cache_root()?
        .join("bootstrap")
        .join(artifact::sanitize(&pin.tag)))
}

/// Where the artifacts are on this host, if anywhere.
///
/// In order: an explicit directory, this checkout, the verified cache. The
/// checkout comes before the cache because a developer who just rebuilt the
/// kernel means *that* one, and finding a stale download instead is the kind of
/// surprise that costs an afternoon.
pub fn locate() -> Option<Artifacts> {
    if let Some(dir) = std::env::var_os(DIR_ENV).map(PathBuf::from) {
        if let Some(found) = pair_in(&dir, Origin::Directory) {
            return Some(found);
        }
    }
    if let Some(found) = pair_in(Path::new(CHECKOUT_DIR), Origin::Checkout) {
        return Some(found);
    }
    let pin = pinned().ok()?;
    let dir = cache_dir(&pin).ok()?;
    let found = pair_in(&dir, Origin::Cache)?;
    // The cache is the one origin nobody hand-placed, so it is the one that gets
    // re-checked: a truncated file from a killed process, or a cache directory
    // left over from a build whose pin has since moved on, must not silently
    // become the kernel a guest boots.
    for (path, name) in [(&found.kernel, KERNEL_FILE), (&found.initrd, INITRD_FILE)] {
        let asset = pin.asset(name).ok()?;
        if artifact::digest_of(path).ok()? != asset.sha256.to_ascii_lowercase() {
            return None;
        }
    }
    Some(found)
}

fn pair_in(dir: &Path, origin: Origin) -> Option<Artifacts> {
    let kernel = dir.join(KERNEL_FILE);
    let initrd = dir.join(INITRD_FILE);
    (kernel.is_file() && initrd.is_file()).then_some(Artifacts {
        kernel,
        initrd,
        origin,
    })
}

/// What to tell someone who has neither. One sentence per way out, in the order
/// they should try them on *this* host.
pub fn missing_hint() -> String {
    let mut hint = String::from("run `entangled fetch bootstrap-kernel` (~13 MiB, SHA-256 pinned)");
    if cfg!(target_os = "linux") {
        // Both scripts, not just the kernel one: half a pair is not a pair, and
        // `build.sh` alone leaves a host that still cannot install Debian.
        hint.push_str(
            ", or build them here with `bash guest/bootstrap-kernel/build.sh` and \
             `bash scripts/build-bootstrap-initramfs.sh`",
        );
    } else {
        hint.push_str(
            ", or copy an artifacts/bootstrap/ directory in from a Linux checkout (the kernel \
             build is Linux-only and does not cross-build)",
        );
    }
    hint
}

// ---------------------------------------------------------------------------
// Fetching them
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct FetchReport {
    pub tag: String,
    pub kernel_version: String,
    pub source: String,
    pub dir: PathBuf,
    pub assets: Vec<FetchedAsset>,
}

/// Downloads (or re-verifies) the pinned guest artifacts into the cache.
pub fn fetch(options: FetchOptions) -> Result<FetchReport, String> {
    let transport = debian_media::UreqTransport::new();
    fetch_with(&transport, options)
}

/// The body, against any `Transport` — which is how the accept and reject paths
/// are tested without a network.
pub fn fetch_with(
    transport: &dyn debian_media::Transport,
    options: FetchOptions,
) -> Result<FetchReport, String> {
    let pin = pinned()?;
    let dir = cache_dir(&pin)?;
    fetch_into(transport, &pin, &dir, options)
}

/// The same, against an explicit pin and directory. Split out so tests can pin
/// digests of bytes they made up — the compiled-in pin describes a 13 MiB kernel
/// no fixture can produce.
pub fn fetch_into(
    transport: &dyn debian_media::Transport,
    pin: &Pinned,
    dir: &Path,
    options: FetchOptions,
) -> Result<FetchReport, String> {
    let url = base_url(pin);
    let version = format!("{} ({})", pin.kernel_version, pin.tag);
    let assets = artifact::fetch_into(transport, &pin.release(&url, &version), dir, options)?;
    Ok(FetchReport {
        tag: pin.tag.clone(),
        kernel_version: pin.kernel_version.clone(),
        source: pin.source.clone(),
        dir: dir.to_path_buf(),
        assets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::AssetStatus;
    use debian_media::{DigestAlgo, Download, Manifest, Transport, TransportError};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A transport that serves bytes from a map and logs what was asked for.
    /// Everything above the socket is exercised through it, so the accept path,
    /// the reject path and `--offline` are all covered with no network.
    struct Fixture {
        files: HashMap<String, Vec<u8>>,
        asked: Mutex<Vec<String>>,
    }

    impl Fixture {
        fn new(files: &[(String, Vec<u8>)]) -> Self {
            Self {
                files: files.iter().cloned().collect(),
                asked: Mutex::new(Vec::new()),
            }
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().expect("fixture log").clone()
        }
    }

    impl Transport for Fixture {
        fn get_all(&self, url: &str, _limit: u64) -> Result<Vec<u8>, TransportError> {
            self.files
                .get(url)
                .cloned()
                .ok_or(TransportError::Status(404))
        }

        fn get_range(&self, url: &str, _offset: u64) -> Result<Download, TransportError> {
            self.asked
                .lock()
                .expect("fixture log")
                .push(url.to_string());
            let body = self
                .files
                .get(url)
                .cloned()
                .ok_or(TransportError::Status(404))?;
            Ok(Download {
                resumed: false,
                total_len: Some(body.len() as u64),
                body: Box::new(std::io::Cursor::new(body)),
            })
        }
    }

    /// A pin over made-up bytes: the compiled-in one describes a 13 MiB kernel no
    /// fixture can produce, so the flow is tested against a pin of the same shape.
    fn fake_pin(kernel: &[u8], initrd: &[u8]) -> Pinned {
        Pinned {
            tag: "guest-artifacts-test-1".into(),
            kernel_version: "6.12.9".into(),
            base_url: "https://example.invalid/releases/download".into(),
            source: "https://example.invalid/releases/tag/guest-artifacts-test-1".into(),
            assets: vec![
                PinnedAsset {
                    name: KERNEL_FILE.into(),
                    sha256: DigestAlgo::Sha256.hex_of(kernel),
                    bytes: kernel.len() as u64,
                },
                PinnedAsset {
                    name: INITRD_FILE.into(),
                    sha256: DigestAlgo::Sha256.hex_of(initrd),
                    bytes: initrd.len() as u64,
                },
            ],
        }
    }

    fn serving(pin: &Pinned, kernel: &[u8], initrd: &[u8]) -> Fixture {
        Fixture::new(&[
            (pin.url(pin.asset(KERNEL_FILE).unwrap()), kernel.to_vec()),
            (pin.url(pin.asset(INITRD_FILE).unwrap()), initrd.to_vec()),
        ])
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("entangled-bootstrap-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// The pin is compiled in, so a malformed one is a broken build rather than a
    /// broken host — which makes this test the only thing between a typo and a
    /// release that can fetch nothing.
    #[test]
    fn the_compiled_in_pin_parses_and_names_both_assets() {
        let pin = pinned().expect("the compiled-in pin parses");
        assert!(!pin.tag.is_empty());
        assert!(
            pin.base_url.starts_with("https://"),
            "the pin must name an https origin, got {:?}",
            pin.base_url
        );
        assert!(
            pin.source.starts_with("https://"),
            "the GPLv2 source address must be a real URL: {:?}",
            pin.source
        );
        for name in [KERNEL_FILE, INITRD_FILE] {
            let asset = pin.asset(name).expect("asset present");
            assert_eq!(asset.sha256.len(), 64, "{name}");
            assert!(asset.bytes > 0, "{name} has no size");
        }
        // The URL it builds is the GitHub release-download shape, tag included.
        let url = pin.url(pin.asset(KERNEL_FILE).unwrap());
        assert!(
            url.ends_with(&format!("/{}/{KERNEL_FILE}", pin.tag)),
            "{url}"
        );
    }

    /// The happy path, end to end over a fixture: both assets downloaded, both
    /// hashed, both manifested — and a second run touches the network not at all.
    #[test]
    fn matching_bytes_are_kept_manifested_and_then_served_from_the_cache() {
        let (kernel, initrd) = (
            b"a pretend bzImage".as_slice(),
            b"a pretend cpio".as_slice(),
        );
        let pin = fake_pin(kernel, initrd);
        let dir = temp_dir("ok");

        let fixture = serving(&pin, kernel, initrd);
        let report = fetch_into(&fixture, &pin, &dir, FetchOptions::default()).expect("fetch");
        assert_eq!(report.assets.len(), 2);
        assert!(report
            .assets
            .iter()
            .all(|a| a.status == AssetStatus::Downloaded));
        assert_eq!(std::fs::read(dir.join(KERNEL_FILE)).unwrap(), kernel);
        assert_eq!(std::fs::read(dir.join(INITRD_FILE)).unwrap(), initrd);
        assert_eq!(fixture.asked().len(), 2);

        // The provenance manifest, and in particular its honesty: there is no
        // signature over these artifacts, so the field that would claim one says
        // false and the keyring field says where the trust actually comes from.
        let manifest_path = debian_media::manifest_path(&dir.join(KERNEL_FILE));
        let manifest =
            Manifest::from_toml(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
        assert!(
            !manifest.signature_verified,
            "nothing signs these; a manifest that says otherwise is a lie"
        );
        assert_eq!(manifest.sha512_hex, DigestAlgo::Sha256.hex_of(kernel));
        assert!(manifest.keyring.unwrap().contains(PIN_PATH));
        assert!(manifest.url.contains(&pin.tag));

        // Second run: cache hit, no request at all — the same "fetch twice is one
        // download" rule the Debian media path holds to.
        let again = serving(&pin, kernel, initrd);
        let report = fetch_into(&again, &pin, &dir, FetchOptions::default()).expect("re-fetch");
        assert!(report
            .assets
            .iter()
            .all(|a| a.status == AssetStatus::Cached));
        assert!(again.asked().is_empty(), "a cache hit reached the network");

        // ...unless asked to refresh, which is what re-validates a mirror.
        let refreshed = serving(&pin, kernel, initrd);
        let report = fetch_into(
            &refreshed,
            &pin,
            &dir,
            FetchOptions {
                refresh: true,
                ..Default::default()
            },
        )
        .expect("refresh");
        assert!(report
            .assets
            .iter()
            .all(|a| a.status == AssetStatus::Downloaded));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Bytes that do not match the pin are refused, deleted, and named — and the
    /// partial goes with them. This is the assertion that a compromised or simply
    /// wrong mirror cannot put a kernel in front of a guest.
    #[test]
    fn bytes_that_miss_the_pinned_digest_are_deleted_not_used() {
        let (kernel, initrd) = (
            b"a pretend bzImage".as_slice(),
            b"a pretend cpio".as_slice(),
        );
        let pin = fake_pin(kernel, initrd);
        let dir = temp_dir("tampered");

        let fixture = serving(&pin, b"a DIFFERENT bzImage", initrd);
        let err = fetch_into(&fixture, &pin, &dir, FetchOptions::default())
            .expect_err("bytes that do not match the pin must be refused");
        assert!(err.contains("does not match the digest pinned"), "{err}");
        assert!(err.contains("expected sha256"), "{err}");
        assert!(err.contains(PIN_PATH), "{err}");

        assert!(
            !dir.join(KERNEL_FILE).exists(),
            "a rejected download was kept"
        );
        assert!(
            !debian_media::partial_path(&dir.join(KERNEL_FILE)).exists(),
            "a rejected .part was kept"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cache entry that has been truncated or replaced since it was written is
    /// re-downloaded rather than trusted: the digest is re-checked on every hit.
    #[test]
    fn a_corrupt_cache_entry_is_re_fetched() {
        let (kernel, initrd) = (
            b"a pretend bzImage".as_slice(),
            b"a pretend cpio".as_slice(),
        );
        let pin = fake_pin(kernel, initrd);
        let dir = temp_dir("corrupt");
        std::fs::write(dir.join(KERNEL_FILE), b"tampered in place").unwrap();
        std::fs::write(dir.join(INITRD_FILE), initrd).unwrap();

        let fixture = serving(&pin, kernel, initrd);
        let report = fetch_into(&fixture, &pin, &dir, FetchOptions::default()).expect("fetch");
        let kernel_asset = report
            .assets
            .iter()
            .find(|a| a.name == KERNEL_FILE)
            .unwrap();
        assert_eq!(kernel_asset.status, AssetStatus::Downloaded);
        // ...and the untouched one was not re-downloaded.
        let initrd_asset = report
            .assets
            .iter()
            .find(|a| a.name == INITRD_FILE)
            .unwrap();
        assert_eq!(initrd_asset.status, AssetStatus::Cached);
        assert_eq!(std::fs::read(dir.join(KERNEL_FILE)).unwrap(), kernel);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--offline` with an empty cache refuses by name and reaches no network.
    #[test]
    fn offline_with_nothing_cached_refuses_by_name() {
        let (kernel, initrd) = (b"k".as_slice(), b"i".as_slice());
        let pin = fake_pin(kernel, initrd);
        let dir = temp_dir("offline");
        let fixture = serving(&pin, kernel, initrd);
        let err = fetch_into(
            &fixture,
            &pin,
            &dir,
            FetchOptions {
                offline: true,
                ..Default::default()
            },
        )
        .expect_err("offline and empty");
        assert!(err.contains("--offline"), "{err}");
        assert!(err.contains(KERNEL_FILE), "{err}");
        assert!(fixture.asked().is_empty(), "--offline reached the network");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A release that has not been published yet answers 404, and a bare
    /// "HTTP status 404" is not an answer: the message must say which of the
    /// causes it is and what to do instead — including the one that is true of
    /// this repository today, that it is private.
    #[test]
    fn a_missing_release_asset_explains_itself() {
        let (kernel, initrd) = (b"k".as_slice(), b"i".as_slice());
        let mut pin = fake_pin(kernel, initrd);
        // A github.com base URL, because the private-repository sentence is the
        // one a user hits and it is only true of GitHub.
        pin.base_url =
            "https://github.com/doctorspider42/entangled-destop/releases/download".into();
        let dir = temp_dir("missing");
        let empty = Fixture::new(&[]);
        let err =
            fetch_into(&empty, &pin, &dir, FetchOptions::default()).expect_err("nothing served");
        assert!(err.contains("404"), "{err}");
        assert!(err.contains(WORKFLOW), "{err}");
        assert!(err.contains(BASE_URL_ENV), "{err}");
        assert!(err.contains(DIR_ENV), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory with only half the pair is not a pair — a kernel with no
    /// initramfs boots to a panic, which is a worse failure than "not found".
    #[test]
    fn half_a_pair_is_not_found() {
        let dir = temp_dir("half");
        std::fs::write(dir.join(KERNEL_FILE), b"only the kernel").unwrap();
        assert!(pair_in(&dir, Origin::Directory).is_none());
        std::fs::write(dir.join(INITRD_FILE), b"and now the initrd").unwrap();
        let found = pair_in(&dir, Origin::Directory).expect("both halves");
        assert_eq!(found.origin, Origin::Directory);
        assert_eq!(found.kernel, dir.join(KERNEL_FILE));
        assert_eq!(found.initrd, dir.join(INITRD_FILE));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The hint names the way out that exists on *this* host, and always names
    /// the fetch — which is the whole point of the feature.
    #[test]
    fn the_missing_hint_fits_this_host() {
        let hint = missing_hint();
        assert!(hint.contains("entangled fetch bootstrap-kernel"), "{hint}");
        if cfg!(target_os = "linux") {
            assert!(hint.contains("build.sh"), "{hint}");
        } else {
            assert!(hint.contains("does not cross-build"), "{hint}");
        }
    }
}
