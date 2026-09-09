//! The UEFI firmware — where `CLOUDHV.fd` comes from on a host that cannot
//! build it.
//!
//! Every UEFI guest this project can install (Ubuntu Server, Ubuntu Desktop,
//! Fedora) boots through EDK2's `OvmfPkg/CloudHv` build, entered at its PVH
//! entry point ([ADR-0003](../../../docs/adr/0003-uefi-firmware.md)). It is
//! built by `guest/firmware/build-cloudhv.sh`, which is a 2.5-minute EDK2 build
//! on **Linux** and does not exist on Windows at all.
//!
//! Until this module, that was the whole story, and the consequence was a
//! shipped product that could not do the thing its README advertises: someone
//! who installed Entangled Desktop from the Windows installer opened the
//! create-machine wizard, chose Ubuntu, and was told to "copy
//! artifacts/firmware/CLOUDHV.fd in from a Linux checkout or a release" — of
//! which they had neither. Accurate, and useless.
//!
//! # Where it comes from now, in order
//!
//! 1. **`--firmware`**, or the `firmware` key of the profile being run — two
//!    halves that behave differently on purpose ([`resolve`] versus
//!    [`resolve_profile`]). A path you *typed* is obeyed or named in the error;
//!    a `--firmware` silently replaced by something else is how an afternoon
//!    disappears. A path a *profile* carries is a starting point: it names the
//!    firmware of the computer that created the machine, so when it is not
//!    here the lookup continues at 2 and logs which one it used instead.
//! 2. **`ENTANGLED_FIRMWARE_DIR`** — a directory holding `CLOUDHV.fd`. The
//!    escape hatch for a developer who just rebuilt EDK2 and means *that* one.
//! 3. **The install directory**: `CLOUDHV.fd` under `artifacts/firmware/`
//!    *beside the `entangled` executable*. This is what the Windows installer
//!    ships (`installer/entangled.iss`), which is what makes a fresh machine
//!    work with no download at all. It is per-machine and read-only, exactly
//!    like the binaries next to it.
//! 4. **The verified cache**, filled by `entangled fetch firmware`: the same
//!    pinned-digest download the bootstrap kernel uses ([`crate::artifact`]).
//!    This is the route for a source checkout on a host that cannot build EDK2.
//! 5. **This checkout**: `artifacts/firmware/CLOUDHV.fd` relative to the
//!    working directory, where `guest/firmware/build-cloudhv.sh` leaves it and
//!    where every profile this project has ever generated points.
//!
//! The install directory is deliberately ahead of the cache and the checkout:
//! the copy that shipped with this executable is the one that matches it, and
//! it is the only one an ordinary user has. A developer whose locally built
//! firmware must win says so with `ENTANGLED_FIRMWARE_DIR` or `--firmware`.
//!
//! # Licence
//!
//! EDK2 is **BSD-2-Clause-Patent** — permissive, with none of the source
//! obligation that keeps the GPL-2.0 bootstrap kernel *out* of the installer.
//! That is why a 4 MiB firmware can be shipped inside a setup executable and a
//! 13 MiB kernel cannot. `cargo deny` never sees either: it reads our Cargo
//! graph, not our release assets, so the attribution is carried by hand in
//! `THIRD-PARTY-NOTICES.txt` (shown on the installer's licence page and
//! installed beside the binaries) and in `about.hbs`, which puts it in the
//! generated `THIRD_PARTY_LICENSES.html`.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::artifact::{self, Hint, PinnedAsset};
pub use crate::artifact::{FetchOptions, FetchedAsset};

/// The firmware file name, in the release, the cache and every checkout.
pub const FIRMWARE_FILE: &str = "CLOUDHV.fd";

/// `artifacts/firmware`, spelled the host's way — the directory name under a
/// checkout's root *and* under the install directory. One place, because the
/// installer deliberately reuses the layout the binary already searched rather
/// than inventing a second one.
///
/// A function rather than a `&str` constant: joining a slash-separated constant
/// onto a Windows directory works, but it prints as
/// `...\debugrtifacts/firmware\CLOUDHV.fd`, and a path a user is asked to
/// look at should not have two kinds of separator in it.
fn firmware_subdir() -> PathBuf {
    Path::new("artifacts").join("firmware")
}

/// Where `guest/firmware/build-cloudhv.sh` leaves it, relative to the working
/// directory: the path `install`, `doctor` and every generated profile have
/// always used.
pub const CHECKOUT_PATH: &str = "artifacts/firmware/CLOUDHV.fd";

/// Override the release location: a mirror, or a directory served over HTTP for
/// an offline test. The pinned digest is enforced against whatever it serves,
/// so this relocates the download without weakening it.
const BASE_URL_ENV: &str = "ENTANGLED_FIRMWARE_BASE_URL";

/// Point at a directory that already holds `CLOUDHV.fd`. Used verbatim and
/// *not* digest checked, exactly like `artifacts/firmware/` in a checkout: both
/// are "the operator put this here on purpose".
const DIR_ENV: &str = "ENTANGLED_FIRMWARE_DIR";

/// The compiled-in pin. Its digest is what a download must hash to.
const PINNED_TOML: &str = include_str!("../../../guest/firmware/pinned.toml");

/// Where the pin lives, for messages that ask a human to look at it.
const PIN_PATH: &str = "guest/firmware/pinned.toml";

/// The workflow that publishes the release this pin names.
const WORKFLOW: &str = ".github/workflows/firmware.yml";

/// The spelling `entangled fetch` accepts. Also what every failure hint prints,
/// so there is exactly one command to copy.
pub const FETCH_TARGET: &str = "firmware";

// ---------------------------------------------------------------------------
// The pin
// ---------------------------------------------------------------------------

/// One published firmware build, named by an immutable release tag.
#[derive(Debug, Clone, Deserialize)]
pub struct Pinned {
    /// The GitHub Release tag holding the asset. Immutable: a new build is a
    /// new tag, so an old `entangled` keeps fetching exactly what it was
    /// reviewed against.
    pub tag: String,
    /// The upstream EDK2 tag this was built from, for the record and for the
    /// line that tells a user what they are downloading.
    pub edk2_tag: String,
    /// `https://github.com/<owner>/<repo>/releases/download` — the tag and file
    /// name are appended.
    pub base_url: String,
    /// Where the corresponding source is published. EDK2 imposes no source
    /// obligation, but naming where a 4 MiB opaque binary came from is the
    /// minimum a person auditing this deserves.
    pub source: String,
    pub assets: Vec<PinnedAsset>,
}

impl Pinned {
    fn asset(&self) -> Result<&PinnedAsset, String> {
        self.assets
            .iter()
            .find(|a| a.name == FIRMWARE_FILE)
            .ok_or_else(|| format!("{PIN_PATH} names no asset '{FIRMWARE_FILE}'"))
    }

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

/// Reads the compiled-in pin. A parse failure is a build-time mistake rather
/// than a user's, so it says which file to fix.
pub fn pinned() -> Result<Pinned, String> {
    let pin: Pinned = toml::from_str(PINNED_TOML)
        .map_err(|e| format!("{PIN_PATH} is not a valid firmware pin: {e}"))?;
    artifact::validate_digests(PIN_PATH, &pin.assets)?;
    Ok(pin)
}

fn base_url(pin: &Pinned) -> String {
    std::env::var(BASE_URL_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| pin.base_url.clone())
}

/// The cache directory for one pinned release: `<cache>/firmware/<tag>`.
pub fn cache_dir(pin: &Pinned) -> Result<PathBuf, String> {
    Ok(crate::paths::cache_root()?
        .join("firmware")
        .join(artifact::sanitize(&pin.tag)))
}

// ---------------------------------------------------------------------------
// Finding it
// ---------------------------------------------------------------------------

/// How a firmware image was found. Reported by `doctor` and logged by `run`,
/// because "which of the five" is the first question when the wrong one boots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// `--firmware`, or the profile's own `firmware` key.
    Explicit,
    /// A directory named by `ENTANGLED_FIRMWARE_DIR`.
    Directory,
    /// `artifacts/firmware/` beside this executable — what the installer ships.
    Install,
    /// The verified cache, digest-checked against the pin on the way out.
    Cache,
    /// `artifacts/firmware/` under the working directory: a checkout that ran
    /// the build script.
    Checkout,
}

impl Origin {
    pub const fn as_str(self) -> &'static str {
        match self {
            Origin::Explicit => "the path you gave",
            Origin::Directory => "ENTANGLED_FIRMWARE_DIR",
            Origin::Install => "this installation",
            Origin::Cache => "the verified cache",
            Origin::Checkout => "this checkout",
        }
    }
}

/// A usable firmware image.
#[derive(Debug, Clone)]
pub struct Firmware {
    pub path: PathBuf,
    pub origin: Origin,
}

/// `artifacts/firmware/` beside the running executable — the per-machine,
/// read-only copy the Windows installer lays down next to `entangled.exe`.
///
/// `None` when the executable's own path cannot be read, which is not a state
/// worth failing over: every other origin still applies.
pub fn install_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join(firmware_subdir()))
}

/// Where the firmware is on this host, if anywhere — steps 2 to 5 of the order
/// in the module documentation. Step 1 is the caller's business, because only
/// the caller knows whether a path was given.
pub fn locate() -> Option<Firmware> {
    locate_among(&candidate_dirs())
}

/// The directories [`locate`] tries, in order. Split out from the search itself
/// so the *order* can be asserted without depending on what this machine
/// happens to have downloaded.
fn candidate_dirs() -> Vec<(PathBuf, Origin)> {
    let mut dirs = Vec::with_capacity(4);
    if let Some(dir) = std::env::var_os(DIR_ENV)
        .map(PathBuf::from)
        .filter(|d| !d.as_os_str().is_empty())
    {
        dirs.push((dir, Origin::Directory));
    }
    if let Some(dir) = install_dir() {
        dirs.push((dir, Origin::Install));
    }
    if let Ok(dir) = pinned().and_then(|pin| cache_dir(&pin)) {
        dirs.push((dir, Origin::Cache));
    }
    dirs.push((firmware_subdir(), Origin::Checkout));
    dirs
}

/// The first candidate directory holding a `CLOUDHV.fd` — with one exception:
/// the cache is the one origin nobody hand-placed, so it is the one that gets
/// re-checked against the pin. A truncated file from a killed process, or a
/// cache directory left over from a build whose pin has since moved on, must
/// not silently become the firmware a guest boots.
fn locate_among(candidates: &[(PathBuf, Origin)]) -> Option<Firmware> {
    for (dir, origin) in candidates {
        let Some(found) = image_in(dir, *origin) else {
            continue;
        };
        if *origin == Origin::Cache && !matches_pin(&found.path) {
            continue;
        }
        return Some(found);
    }
    None
}

fn matches_pin(path: &Path) -> bool {
    let Ok(pin) = pinned() else { return false };
    let Ok(asset) = pin.asset() else { return false };
    artifact::digest_of(path).is_ok_and(|found| found == asset.sha256.to_ascii_lowercase())
}

fn image_in(dir: &Path, origin: Origin) -> Option<Firmware> {
    let path = dir.join(FIRMWARE_FILE);
    path.is_file().then_some(Firmware { path, origin })
}

/// The whole lookup, from an optional explicit path.
///
/// An explicit path that does not exist is an error naming *that* path: a
/// `--firmware` silently replaced by something else is how you spend an hour
/// wondering why your edited build did nothing.
pub fn resolve(explicit: Option<&Path>) -> Result<Firmware, String> {
    if let Some(path) = explicit {
        return if path.is_file() {
            Ok(Firmware {
                path: path.to_path_buf(),
                origin: Origin::Explicit,
            })
        } else {
            Err(format!(
                "the firmware you named, {}, is not there.\n{}",
                path.display(),
                missing_message()
            ))
        };
    }
    locate().ok_or_else(missing_message)
}

/// The same, for a path that came out of a **profile** rather than a command
/// line.
///
/// A profile's `firmware` key is usually the relative `artifacts/firmware/…`
/// that `install` wrote on the machine that created it, so on a fresh install —
/// or simply from another working directory — it points at nothing. Falling
/// through to the ordinary lookup is what makes such a profile portable; the
/// substitution is logged, because a machine booting a firmware other than the
/// one its profile names is worth one line in the transcript.
pub fn resolve_profile(configured: &Path) -> Result<Firmware, String> {
    if configured.is_file() {
        return Ok(Firmware {
            path: configured.to_path_buf(),
            origin: Origin::Explicit,
        });
    }
    let found = locate().ok_or_else(|| {
        format!(
            "this machine boots via UEFI and its profile names {}, which is not there.\n{}",
            configured.display(),
            missing_message()
        )
    })?;
    tracing::info!(
        configured = %configured.display(),
        using = %found.path.display(),
        origin = found.origin.as_str(),
        "the profile's firmware path does not exist; using the one this host has"
    );
    Ok(found)
}

/// What to tell someone who has none — in the order they should try it, and in
/// terms of what *they* can do rather than what a developer with a Linux
/// checkout could.
pub fn missing_message() -> String {
    let mut message = format!(
        "No UEFI firmware ({FIRMWARE_FILE}) on this host, and a machine that starts via UEFI \
         cannot boot without it.\n  \
         Entangled Desktop can download it: run `entangled fetch {FETCH_TARGET}` (4 MiB, \
         checked against a SHA-256 pinned in this build).\n  \
         It normally ships with Entangled Desktop, so reinstalling also puts it back"
    );
    match install_dir() {
        Some(dir) => message.push_str(&format!(" — at {}.", dir.join(FIRMWARE_FILE).display())),
        None => message.push('.'),
    }
    if cfg!(target_os = "linux") {
        message.push_str(&format!(
            "\n  On Linux you can also build it: `bash guest/firmware/build-cloudhv.sh` \
             (~2.5 min), which writes {CHECKOUT_PATH}."
        ));
    }
    message.push_str(&format!(
        "\n  Or pass --firmware, or point {DIR_ENV} at a directory holding {FIRMWARE_FILE}."
    ));
    message
}

// ---------------------------------------------------------------------------
// Fetching it
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct FetchReport {
    pub tag: String,
    pub edk2_tag: String,
    pub source: String,
    pub dir: PathBuf,
    pub asset: FetchedAsset,
}

/// Downloads (or re-verifies) the pinned firmware into the cache.
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

/// The same, against an explicit pin and directory, so tests can pin digests of
/// bytes they made up — the compiled-in pin describes a 4 MiB EDK2 build no
/// fixture can produce.
pub fn fetch_into(
    transport: &dyn debian_media::Transport,
    pin: &Pinned,
    dir: &Path,
    options: FetchOptions,
) -> Result<FetchReport, String> {
    let url = base_url(pin);
    let version = format!("{} ({})", pin.edk2_tag, pin.tag);
    let mut assets = artifact::fetch_into(transport, &pin.release(&url, &version), dir, options)?;
    let asset = assets
        .pop()
        .ok_or_else(|| format!("{PIN_PATH} names no assets at all"))?;
    Ok(FetchReport {
        tag: pin.tag.clone(),
        edk2_tag: pin.edk2_tag.clone(),
        source: pin.source.clone(),
        dir: dir.to_path_buf(),
        asset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::AssetStatus;
    use debian_media::{DigestAlgo, Download, Manifest, Transport, TransportError};
    use std::collections::HashMap;
    use std::sync::Mutex;

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

    fn fake_pin(image: &[u8]) -> Pinned {
        Pinned {
            tag: "firmware-edk2-test-1".into(),
            edk2_tag: "edk2-stable202602".into(),
            base_url: "https://example.invalid/releases/download".into(),
            source: "https://example.invalid/releases/tag/firmware-edk2-test-1".into(),
            assets: vec![PinnedAsset {
                name: FIRMWARE_FILE.into(),
                sha256: DigestAlgo::Sha256.hex_of(image),
                bytes: image.len() as u64,
            }],
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("entangled-firmware-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// The pin is compiled in, so a malformed one is a broken build rather than
    /// a broken host — the only thing between a typo and a release that can
    /// fetch nothing.
    #[test]
    fn the_compiled_in_pin_parses_and_names_the_firmware() {
        let pin = pinned().expect("the compiled-in pin parses");
        assert!(!pin.tag.is_empty());
        assert!(
            pin.base_url.starts_with("https://"),
            "the pin must name an https origin, got {:?}",
            pin.base_url
        );
        assert!(pin.source.starts_with("https://"), "{:?}", pin.source);
        assert!(
            pin.edk2_tag.starts_with("edk2-"),
            "the upstream tag identifies what was built: {:?}",
            pin.edk2_tag
        );
        let asset = pin.asset().expect("the firmware asset");
        assert_eq!(asset.sha256.len(), 64);
        // CloudHvX64's flash image is exactly 4 MiB and the machine's pflash
        // window assumes it (machine_x86::layout::PFLASH_BASE, ADR-0003). A pin
        // that says otherwise is a pin for a different firmware.
        assert_eq!(asset.bytes, 4 * 1024 * 1024, "CLOUDHV.fd is a 4 MiB image");
        let url = pin.url(asset);
        assert!(
            url.ends_with(&format!("/{}/{FIRMWARE_FILE}", pin.tag)),
            "{url}"
        );
    }

    /// The happy path over a fixture: downloaded, hashed, manifested — and a
    /// second run touches the network not at all.
    #[test]
    fn matching_bytes_are_kept_manifested_and_then_served_from_the_cache() {
        let image = b"a pretend PVH ELF".as_slice();
        let pin = fake_pin(image);
        let dir = temp_dir("ok");
        let files = [(pin.url(pin.asset().unwrap()), image.to_vec())];

        let fixture = Fixture::new(&files);
        let report = fetch_into(&fixture, &pin, &dir, FetchOptions::default()).expect("fetch");
        assert_eq!(report.asset.status, AssetStatus::Downloaded);
        assert_eq!(std::fs::read(dir.join(FIRMWARE_FILE)).unwrap(), image);

        let manifest_path = debian_media::manifest_path(&dir.join(FIRMWARE_FILE));
        let manifest =
            Manifest::from_toml(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
        assert!(
            !manifest.signature_verified,
            "nothing signs the firmware; a manifest that says otherwise is a lie"
        );
        assert!(manifest.keyring.unwrap().contains(PIN_PATH));
        assert!(manifest.version.contains("edk2-stable202602"));

        let again = Fixture::new(&files);
        let report = fetch_into(&again, &pin, &dir, FetchOptions::default()).expect("re-fetch");
        assert_eq!(report.asset.status, AssetStatus::Cached);
        assert!(again.asked().is_empty(), "a cache hit reached the network");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Bytes that do not match the pin are refused, deleted and named. A
    /// firmware is the most privileged thing a guest runs; an unverified one
    /// must never reach the disk it is loaded from.
    #[test]
    fn bytes_that_miss_the_pinned_digest_are_deleted_not_used() {
        let pin = fake_pin(b"a pretend PVH ELF");
        let dir = temp_dir("tampered");
        let fixture = Fixture::new(&[(
            pin.url(pin.asset().unwrap()),
            b"a DIFFERENT firmware".to_vec(),
        )]);
        let err = fetch_into(&fixture, &pin, &dir, FetchOptions::default())
            .expect_err("bytes that do not match the pin must be refused");
        assert!(err.contains("does not match the digest pinned"), "{err}");
        assert!(err.contains(PIN_PATH), "{err}");
        assert!(
            !dir.join(FIRMWARE_FILE).exists(),
            "a rejected download was kept"
        );
        assert!(
            !debian_media::partial_path(&dir.join(FIRMWARE_FILE)).exists(),
            "a rejected .part was kept"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--offline` with an empty cache refuses by name and reaches no network.
    #[test]
    fn offline_with_nothing_cached_refuses_by_name() {
        let pin = fake_pin(b"f");
        let dir = temp_dir("offline");
        let fixture = Fixture::new(&[(pin.url(pin.asset().unwrap()), b"f".to_vec())]);
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
        assert!(err.contains(FIRMWARE_FILE), "{err}");
        assert!(fixture.asked().is_empty(), "--offline reached the network");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The message a user sees. This is the deliverable: the old one told a
    /// person with no Linux checkout to go and get one. Every clause here is
    /// something they can actually do.
    #[test]
    fn the_missing_message_is_addressed_to_a_user_not_a_developer() {
        let message = missing_message();
        assert!(
            message.contains("entangled fetch firmware"),
            "the download must be named: {message}"
        );
        assert!(
            message.contains("reinstalling"),
            "the installer copy must be named: {message}"
        );
        assert!(message.contains(FIRMWARE_FILE), "{message}");
        assert!(message.contains(DIR_ENV), "{message}");
        assert!(
            !message.contains("copy artifacts/firmware/CLOUDHV.fd in from a Linux checkout"),
            "the old dead end came back: {message}"
        );
        if cfg!(target_os = "linux") {
            assert!(message.contains("build-cloudhv.sh"), "{message}");
        } else {
            assert!(
                !message.contains("build-cloudhv.sh"),
                "a Windows user cannot run the build script: {message}"
            );
        }
    }

    /// An explicit path that is not there is an error naming *that* path — a
    /// `--firmware` silently replaced by something else is a debugging trap.
    #[test]
    fn an_explicit_path_is_never_silently_replaced() {
        let dir = temp_dir("explicit");
        let missing = dir.join("nowhere.fd");
        let err = resolve(Some(&missing)).expect_err("a named path that is absent must fail");
        assert!(err.contains("nowhere.fd"), "{err}");
        assert!(err.contains("entangled fetch firmware"), "{err}");

        let present = dir.join(FIRMWARE_FILE);
        std::fs::write(&present, b"pretend firmware").unwrap();
        let found = resolve(Some(&present)).expect("an explicit path that exists");
        assert_eq!(found.origin, Origin::Explicit);
        assert_eq!(found.path, present);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The search order, and the one substitution that makes an installed
    /// machine portable.
    ///
    /// Fabricated directories rather than the host's real ones, and one test
    /// rather than four: the order is the deliverable, and `locate()` on a
    /// developer's machine depends on whether *they* have ever run the fetch.
    /// (The environment variable is touched here, so everything that reads it
    /// is folded into this one test — cargo runs tests in threads of one
    /// process, and two of these racing is exactly the flake you would expect.)
    #[test]
    fn the_search_order_is_install_then_cache_then_checkout() {
        let root = temp_dir("order");
        let dirs: Vec<PathBuf> = ["given", "install", "cache", "checkout"]
            .iter()
            .map(|name| {
                let dir = root.join(name);
                std::fs::create_dir_all(&dir).unwrap();
                dir
            })
            .collect();
        let candidates = |from: usize| -> Vec<(PathBuf, Origin)> {
            [
                Origin::Directory,
                Origin::Install,
                Origin::Cache,
                Origin::Checkout,
            ]
            .iter()
            .enumerate()
            .skip(from)
            .map(|(i, origin)| (dirs[i].clone(), *origin))
            .collect()
        };

        // Nothing anywhere is None, not a panic and not a wrong file.
        assert!(locate_among(&[]).is_none());
        assert!(locate_among(&candidates(0)).is_none());

        // Last resort first: the checkout answers when it is all there is.
        std::fs::write(dirs[3].join(FIRMWARE_FILE), b"checkout").unwrap();
        assert_eq!(
            locate_among(&candidates(0)).unwrap().origin,
            Origin::Checkout
        );

        // A cached copy that does not match the compiled-in pin is skipped
        // rather than booted — the checkout still wins.
        std::fs::write(dirs[2].join(FIRMWARE_FILE), b"a stale download").unwrap();
        assert_eq!(
            locate_among(&candidates(0)).unwrap().origin,
            Origin::Checkout,
            "an unverified cache entry must not outrank anything"
        );

        // The installer's copy outranks both, which is the whole point: it is
        // the one an ordinary user has and the one that matches this binary.
        std::fs::write(dirs[1].join(FIRMWARE_FILE), b"installed").unwrap();
        assert_eq!(
            locate_among(&candidates(0)).unwrap().origin,
            Origin::Install
        );

        // ...and an explicit directory outranks even that.
        std::fs::write(dirs[0].join(FIRMWARE_FILE), b"given").unwrap();
        let found = locate_among(&candidates(0)).unwrap();
        assert_eq!(found.origin, Origin::Directory);
        assert_eq!(found.path, dirs[0].join(FIRMWARE_FILE));

        // The environment variable really is the first candidate, and the
        // install directory really is `artifacts/firmware/` beside this
        // executable — the same relative layout a checkout uses, which is why
        // the installer could adopt it instead of moving anyone's files.
        let _dir_env = EnvGuard::set(DIR_ENV, dirs[0].as_os_str());
        let listed = candidate_dirs();
        assert_eq!(listed[0], (dirs[0].clone(), Origin::Directory));
        let exe = std::env::current_exe().expect("test binary path");
        assert_eq!(
            listed[1],
            (
                exe.parent().unwrap().join(firmware_subdir()),
                Origin::Install
            )
        );
        assert_eq!(listed.last().unwrap().1, Origin::Checkout);
        assert_eq!(listed.last().unwrap().0, firmware_subdir());
        // Spelled the host's way throughout: no mixed separators in a path a
        // user is told to look at.
        assert!(
            cfg!(not(windows)) || !listed[1].0.to_string_lossy().contains("artifacts/firmware"),
            "{}",
            listed[1].0.display()
        );

        // A profile written on another machine names a path that is not here.
        // Falling through to the host's own copy is what makes it portable.
        let elsewhere = root.join("theirs/artifacts/firmware/CLOUDHV.fd");
        let found = resolve_profile(&elsewhere).expect("the host has one");
        assert_eq!(found.origin, Origin::Directory);
        assert_eq!(found.path, dirs[0].join(FIRMWARE_FILE));

        // An existing profile path still wins over everything.
        std::fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
        std::fs::write(&elsewhere, b"theirs").unwrap();
        let found = resolve_profile(&elsewhere).expect("the profile's own path");
        assert_eq!(found.origin, Origin::Explicit);
        assert_eq!(found.path, elsewhere);

        drop(_dir_env);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The Windows installer must carry the firmware, and must refuse to build
    /// without one.
    ///
    /// This module's whole search order rests on it — step 3, "the install
    /// directory", is only ever populated by `installer/entangled.iss` — and
    /// the bug being fixed here is precisely a release that shipped without it.
    /// So the packaging invariant is asserted in the code that depends on it,
    /// against the compiled-in text of the script itself.
    #[test]
    fn the_windows_installer_ships_the_firmware_and_will_not_build_without_it() {
        const ISS: &str = include_str!("../../../installer/entangled.iss");
        let entry = ISS
            .lines()
            .find(|line| line.starts_with("Source:") && line.contains("\\CLOUDHV.fd\";"))
            .expect("installer/entangled.iss must ship CLOUDHV.fd");
        assert!(
            entry.contains(r#"DestDir: "{app}\artifacts\firmware""#),
            "the firmware must land where this module looks for it — \
             artifacts\\firmware beside the executable: {entry}"
        );
        assert!(
            !entry.contains("skipifsourcedoesntexist"),
            "a compile with no firmware must fail, not quietly ship a setup that \
             cannot create a UEFI machine: {entry}"
        );
        // And the licence page shows what is redistributed, not only what we
        // wrote (THIRD-PARTY-NOTICES.txt, appended at compile time).
        assert!(
            ISS.contains("THIRD-PARTY-NOTICES.txt"),
            "the installer must show and install the third-party notices"
        );
    }

    /// EDK2 is BSD-2-Clause-Patent — permissive, but attribution is a
    /// condition, and this is the first non-Rust binary the project ships, so
    /// `cargo about` cannot carry it. The notice is compiled in here so it
    /// cannot be deleted without a test failing.
    #[test]
    fn the_redistributed_firmware_is_attributed() {
        const NOTICES: &str = include_str!("../../../THIRD-PARTY-NOTICES.txt");
        for needle in [
            "TianoCore EDK2",
            "BSD-2-Clause-Patent",
            FIRMWARE_FILE,
            "github.com/tianocore/edk2",
            // The disclaimer the licence requires to travel with the binary.
            "THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS",
        ] {
            assert!(
                NOTICES.contains(needle),
                "THIRD-PARTY-NOTICES.txt no longer mentions {needle:?}"
            );
        }
        // The upstream tag the pin names must be the one the notice describes;
        // bumping EDK2 without re-reading the attribution is the drift this
        // catches.
        let pin = pinned().expect("the compiled-in pin parses");
        assert!(
            NOTICES.contains(&pin.edk2_tag),
            "the notices describe a different EDK2 than the pin ({})",
            pin.edk2_tag
        );
    }

    /// One environment variable, set or cleared for the length of a test and
    /// put back afterwards.
    struct EnvGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &std::ffi::OsStr) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}
