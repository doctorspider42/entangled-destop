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
//! verified cache the ISOs use.
//!
//! # What is trusted, and how strongly
//!
//! The pin is [`guest/bootstrap-kernel/pinned.toml`](../../../guest/bootstrap-kernel/pinned.toml),
//! **compiled into this binary** by `include_str!`. It names the release tag,
//! the asset file names and a **SHA-256 per asset**. A download that does not
//! hash to the pinned digest is deleted, not used.
//!
//! That anchor is worth being precise about, because it is easy to overstate:
//!
//! * the digest lives in *our source tree*, reviewable in `git log` and shipped
//!   inside the executable the user already decided to run. It is **not** a
//!   checksum file fetched from beside the artifact, which would give an
//!   attacker who can serve the artifact the checksum too;
//! * it is **not** a signature. There is no key here and nothing to revoke.
//!   Whoever can land a commit can change the pin, and whoever can publish a
//!   release under the pinned tag *before* the pin is written can choose what
//!   the pin then records. Tags are immutable once assets are attached, which is
//!   what makes an already-pinned release safe to re-fetch forever;
//! * TLS to `github.com` is the transport, so the download is at least not
//!   attacker-modifiable in flight even before the digest check.
//!
//! In short: as strong as the git history of this repository, and no stronger.
//! The Debian media path next door is stronger — pinned OpenPGP keys, signature
//! before digest — because Debian signs its media and we do not yet sign ours.
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
//! them, and `docs/user-guide.md` § "What the guest artifacts are".

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use debian_media::{DigestAlgo, Manifest, Transport, UreqTransport};
use serde::Deserialize;

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

/// Nothing here is remotely this big; a server that streams forever is a bug or
/// an attack, and either way it must not fill the disk. The kernel is ~13 MiB
/// and the initramfs ~200 KiB.
const MAX_ASSET_LEN: u64 = 256 * 1024 * 1024;

/// The compiled-in pin. Its digests are what a download must hash to.
const PINNED_TOML: &str = include_str!("../../../guest/bootstrap-kernel/pinned.toml");

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

#[derive(Debug, Clone, Deserialize)]
pub struct PinnedAsset {
    pub name: String,
    /// Lower-case hex SHA-256 of the asset, 64 characters.
    pub sha256: String,
    /// Size in bytes, so the download can be announced and a wildly wrong
    /// response rejected before it is hashed.
    pub bytes: u64,
}

impl Pinned {
    fn asset(&self, name: &str) -> Result<&PinnedAsset, String> {
        self.assets
            .iter()
            .find(|a| a.name == name)
            .ok_or_else(|| format!("{PIN_PATH} names no asset '{name}'"))
    }

    /// The download URL for one asset.
    fn url(&self, asset: &PinnedAsset) -> String {
        format!(
            "{}/{}/{}",
            base_url(self).trim_end_matches('/'),
            self.tag,
            asset.name
        )
    }
}

/// Where the pin lives, for messages that ask a human to look at it.
const PIN_PATH: &str = "guest/bootstrap-kernel/pinned.toml";

/// Reads the compiled-in pin.
///
/// A parse failure is a build-time mistake rather than a user's, so it says
/// which file to fix; `the_compiled_in_pin_parses_and_names_both_assets` keeps it
/// from ever reaching a release.
pub fn pinned() -> Result<Pinned, String> {
    let pin: Pinned = toml::from_str(PINNED_TOML)
        .map_err(|e| format!("{PIN_PATH} is not a valid guest-artifact pin: {e}"))?;
    for asset in &pin.assets {
        if asset.sha256.len() != DigestAlgo::Sha256.hex_len()
            || !asset.sha256.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(format!(
                "{PIN_PATH}: asset '{}' has no usable sha256 (got {:?}) — a pin without a \
                 digest would download an unverified kernel",
                asset.name, asset.sha256
            ));
        }
    }
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
        .join(sanitize(&pin.tag)))
}

/// A tag or file name reduced to something safe to join onto a path. The pin is
/// ours, but it is still text that becomes a path, and `..` in it would escape
/// the cache.
fn sanitize(component: &str) -> String {
    component
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .replace("..", "__")
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
        if digest_of(path).ok()? != asset.sha256.to_ascii_lowercase() {
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
        hint.push_str(", or build it here with `bash guest/bootstrap-kernel/build.sh`");
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

/// What one asset did during a fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetStatus {
    /// Already in the cache and hashing to the pinned digest.
    Cached,
    Downloaded,
}

impl AssetStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            AssetStatus::Cached => "cached",
            AssetStatus::Downloaded => "downloaded",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FetchedAsset {
    pub name: String,
    pub path: PathBuf,
    pub url: String,
    pub sha256: String,
    pub status: AssetStatus,
}

#[derive(Debug, Clone)]
pub struct FetchReport {
    pub tag: String,
    pub kernel_version: String,
    pub source: String,
    pub dir: PathBuf,
    pub assets: Vec<FetchedAsset>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FetchOptions {
    /// Re-download even when the cached copy already matches the pin.
    pub refresh: bool,
    /// Never touch the network: use the cache or fail.
    pub offline: bool,
}

/// Downloads (or re-verifies) the pinned guest artifacts into the cache.
pub fn fetch(options: FetchOptions) -> Result<FetchReport, String> {
    let transport = UreqTransport::new();
    fetch_with(&transport, options)
}

/// The body, against any [`Transport`] — which is how the accept and reject
/// paths are tested without a network.
pub fn fetch_with(transport: &dyn Transport, options: FetchOptions) -> Result<FetchReport, String> {
    let pin = pinned()?;
    let dir = cache_dir(&pin)?;
    fetch_into(transport, &pin, &dir, options)
}

/// The same, against an explicit pin and directory. Split out so tests can pin
/// digests of bytes they made up — the compiled-in pin describes a 13 MiB kernel
/// no fixture can produce.
pub fn fetch_into(
    transport: &dyn Transport,
    pin: &Pinned,
    dir: &Path,
    options: FetchOptions,
) -> Result<FetchReport, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    let mut assets = Vec::new();
    for name in [KERNEL_FILE, INITRD_FILE] {
        let asset = pin.asset(name)?;
        let expected = asset.sha256.to_ascii_lowercase();
        let path = dir.join(sanitize(name));
        let url = pin.url(asset);

        let cached = !options.refresh
            && path.is_file()
            && digest_of(&path).is_ok_and(|found| found == expected);
        let status = if cached {
            AssetStatus::Cached
        } else {
            if options.offline {
                return Err(format!(
                    "--offline, and {} is not in the cache (or does not match the pinned \
                     digest): {}",
                    name,
                    path.display()
                ));
            }
            download_verified(transport, &url, &path, asset, &expected)?;
            AssetStatus::Downloaded
        };

        // The provenance note the whole project writes beside a verified
        // artifact. `signature_verified` is *false* and that is not a bug: there
        // is no signature over these, only a digest pinned in our source. A
        // manifest that claimed otherwise would be the lie this field exists to
        // prevent.
        let manifest = Manifest {
            url: url.clone(),
            version: format!("{} ({})", pin.kernel_version, pin.tag),
            fetched_at: debian_media::now_utc(),
            sha512_hex: expected.clone(),
            signature_verified: false,
            signed_by: None,
            keyring: Some(format!("pinned sha256 in {PIN_PATH}")),
        };
        let manifest_path = debian_media::manifest_path(&path);
        let text = manifest
            .to_toml()
            .map_err(|e| format!("cannot render the provenance manifest: {e}"))?;
        std::fs::write(&manifest_path, text)
            .map_err(|e| format!("cannot write {}: {e}", manifest_path.display()))?;

        assets.push(FetchedAsset {
            name: name.to_string(),
            path,
            url,
            sha256: expected,
            status,
        });
    }

    Ok(FetchReport {
        tag: pin.tag.clone(),
        kernel_version: pin.kernel_version.clone(),
        source: pin.source.clone(),
        dir: dir.to_path_buf(),
        assets,
    })
}

/// Streams one asset to `<path>.part`, hashing as it goes, and renames it into
/// place only once the digest matches the pin.
///
/// No resume: these are small enough that restarting is cheaper than the
/// bookkeeping, and unlike a 2.9 GiB ISO an interrupted 13 MiB download is not
/// worth keeping. A failed verification deletes the partial — the same rule
/// `debian-media` follows, for the same reason.
fn download_verified(
    transport: &dyn Transport,
    url: &str,
    path: &Path,
    asset: &PinnedAsset,
    expected: &str,
) -> Result<(), String> {
    let partial = debian_media::partial_path(path);
    let _ = std::fs::remove_file(&partial);

    tracing::info!(url, bytes = asset.bytes, "downloading guest artifact");
    let download = transport
        .get_range(url, 0)
        .map_err(|e| format!("cannot download {url}: {e}{}", not_published_hint(&e)))?;

    let mut hasher = DigestAlgo::Sha256.hasher();
    let mut file = std::fs::File::create(&partial)
        .map_err(|e| format!("cannot create {}: {e}", partial.display()))?;
    let mut reader = download.body.take(MAX_ASSET_LEN + 1);
    let mut buf = vec![0u8; 64 * 1024];
    let mut written: u64 = 0;
    loop {
        let read = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                let _ = std::fs::remove_file(&partial);
                return Err(format!("download of {url} failed: {e}"));
            }
        };
        written += read as u64;
        if written > MAX_ASSET_LEN {
            let _ = std::fs::remove_file(&partial);
            return Err(format!(
                "{url} is larger than the {MAX_ASSET_LEN} byte limit for a guest artifact"
            ));
        }
        hasher.update(&buf[..read]);
        if let Err(e) = file.write_all(&buf[..read]) {
            let _ = std::fs::remove_file(&partial);
            return Err(format!("cannot write {}: {e}", partial.display()));
        }
    }
    if let Err(e) = file.flush() {
        let _ = std::fs::remove_file(&partial);
        return Err(format!("cannot write {}: {e}", partial.display()));
    }
    drop(file);

    let found = hasher.finish_hex();
    if found != expected {
        let _ = std::fs::remove_file(&partial);
        return Err(format!(
            "{url} does not match the digest pinned in {PIN_PATH}: expected sha256 \
             {expected}, got {found} ({written} bytes). The file has been deleted; nothing \
             unverified is kept"
        ));
    }
    std::fs::rename(&partial, path).map_err(|e| {
        let _ = std::fs::remove_file(&partial);
        format!(
            "cannot move the verified download into {}: {e}",
            path.display()
        )
    })
}

/// A 404 here has exactly one likely cause and one exact fix, so say it rather
/// than leaving "HTTP status 404" to be interpreted.
fn not_published_hint(error: &debian_media::TransportError) -> String {
    match error {
        debian_media::TransportError::Status(404) => format!(
            "\n  the release tag pinned in {PIN_PATH} has no such asset. Either the \
             guest-artifacts workflow has not published it yet (see \
             .github/workflows/guest-artifacts.yml), or {BASE_URL_ENV} points somewhere \
             that does not serve it. Until it is published, put a `vmlinuz` and an \
             `initrd.img` built by `guest/bootstrap-kernel/build.sh` in a directory and \
             name it with {DIR_ENV}"
        ),
        _ => String::new(),
    }
}

/// SHA-256 of a file on disk, streamed.
fn digest_of(path: &Path) -> Result<String, String> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut hasher = DigestAlgo::Sha256.hasher();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buf)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hasher.finish_hex())
}

#[cfg(test)]
mod tests {
    use super::*;
    use debian_media::{Download, TransportError};
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

    /// A pin whose digest is missing or malformed is refused outright — the
    /// alternative is downloading a kernel and hoping. `pinned()` reads a
    /// compiled-in file that cannot be swapped, so the rule it applies is
    /// asserted here over the same predicate.
    #[test]
    fn a_pin_without_a_usable_digest_would_be_refused() {
        for bad in ["", &"z".repeat(64), &"ab".repeat(10)] {
            let usable = bad.len() == DigestAlgo::Sha256.hex_len()
                && bad.bytes().all(|b| b.is_ascii_hexdigit());
            assert!(!usable, "{bad:?} must not count as a usable digest");
        }
        let good = "d4".repeat(32);
        assert!(
            good.len() == DigestAlgo::Sha256.hex_len()
                && good.bytes().all(|b| b.is_ascii_hexdigit())
        );
    }

    /// `..` in a tag must not walk out of the cache directory. The pin is ours,
    /// but it is still text that becomes a path.
    #[test]
    fn path_components_from_the_pin_are_sanitised() {
        assert_eq!(
            sanitize("guest-artifacts-6.12.9-1"),
            "guest-artifacts-6.12.9-1"
        );
        for hostile in ["../../etc", "..\\..\\windows", "a/b", "c:evil"] {
            let safe = sanitize(hostile);
            assert!(
                !safe.contains('/') && !safe.contains('\\') && !safe.contains(".."),
                "{hostile:?} sanitised to {safe:?}"
            );
        }
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
    /// "HTTP status 404" is not an answer: the message must say which of the two
    /// causes it is and what to do instead.
    #[test]
    fn a_missing_release_asset_explains_itself() {
        let (kernel, initrd) = (b"k".as_slice(), b"i".as_slice());
        let pin = fake_pin(kernel, initrd);
        let dir = temp_dir("missing");
        let empty = Fixture::new(&[]);
        let err =
            fetch_into(&empty, &pin, &dir, FetchOptions::default()).expect_err("nothing served");
        assert!(err.contains("404"), "{err}");
        assert!(err.contains("guest-artifacts.yml"), "{err}");
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
