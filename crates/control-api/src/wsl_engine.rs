//! Getting a Linux engine *into* WSL: the pinned download, the digest check,
//! and the copy into the distribution.
//!
//! [`crate::wsl`] answers "is there an engine, and if not why". This module is
//! the other half — the one that fixes it — and it lives here, beside that
//! classification, because **two programs perform it**: `entangled-manager`
//! from its Settings panel, and `entangled wsl install-engine` from the
//! Windows installer's optional task. One of those is a GUI and the other is
//! driven by an Inno Setup script; a second copy of the download-and-verify
//! logic between them would drift within a release, and the thing that would
//! drift is a security check.
//!
//! # What the download trusts
//!
//! One asset, [`ASSET`], published by the **same release run** that built the
//! program doing the downloading, verified against a SHA-256 that the release
//! pipeline stamped into that program at compile time
//! (`ENTANGLED_LINUX_ENGINE_SHA256`, forwarded by each binary's `build.rs` and
//! handed to this module as [`Pin::sha256`]). Bytes that do not hash to it are
//! deleted, not used, and are never handed to WSL.
//!
//! That is a slightly different anchor from the guest kernel's
//! (`guest/bootstrap-kernel/pinned.toml`, a digest committed to the source
//! tree), and deliberately so:
//!
//! * the kernel is built once and pinned by a human in a later commit, because
//!   many releases share one kernel. The Linux engine is *this* release's own
//!   build — pinning it in the tree would mean shipping an installer whose
//!   engine is a version behind, forever;
//! * so the pipeline hashes the Linux binary **before** it builds the Windows
//!   half, and the digest is compiled into the binaries that will download it.
//!   Nobody types it, nobody can point it at a different build, and the
//!   `windows-installer` job that publishes the installer is the job that
//!   publishes the binary the digest describes;
//! * it is still **not a signature**. There is no key and nothing to revoke:
//!   whoever can change the workflow can change what gets hashed. TLS to
//!   github.com is the transport. In short — as strong as the git history of
//!   this repository plus the integrity of one CI run, and no stronger, which
//!   is the same honest ceiling `apps/entangled/src/artifact.rs` documents for
//!   the guest artifacts.
//!
//! A build made outside the pipeline (every developer build) carries no digest
//! at all, and then [`install_block`] says so instead of downloading something
//! it cannot check. Refusing is the whole point: an unverified engine copied
//! into a distribution would be the one thing in the product with no anchor.
//! The manager greys its button out; the installer task degrades to a sentence
//! on the finished page. Neither fabricates a check it cannot make.
//!
//! # Licence
//!
//! The asset is our own binary, Apache-2.0, built from this repository — the
//! question `cargo deny` answers for the crate graph is the same question here
//! and has the same answer. Nothing GPL is downloaded, shipped or linked.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use debian_media::{DigestAlgo, Manifest, Transport, TransportError};

use crate::wsl::{self, Installed, Runner};

/// The release asset holding the Linux build of `entangled`.
pub const ASSET: &str = "entangled-linux-x86_64";

/// Relocate the download (a mirror, or a directory served over HTTP in a test)
/// without weakening it: the compiled-in digest is enforced against whatever it
/// serves.
pub const URL_ENV: &str = "ENTANGLED_LINUX_ENGINE_URL";

/// The repository whose releases carry [`ASSET`].
pub const REPO: &str = "doctorspider42/entangled-destop";

/// The engine is ~20 MiB. A server that streams forever is a bug or an attack,
/// and either way it must not fill the disk.
const MAX_ASSET_LEN: u64 = 256 * 1024 * 1024;

// ---------------------------------------------------------------------------
// What the calling binary knows that this module cannot
// ---------------------------------------------------------------------------

/// The three facts an install needs from the program performing it: which
/// release it belongs to, what digest that program was published with, and
/// where it keeps downloads.
///
/// All three are properties of the *binary*, not of this crate: the manager and
/// the CLI stamp their own version and their own digest at compile time, and
/// they must be able to disagree (a manager upgraded past its CLI is a
/// perfectly ordinary state and must not silently install the wrong engine).
#[derive(Debug, Clone, Copy)]
pub struct Pin<'a> {
    /// `crate::VERSION` of the calling binary — names the release tag and the
    /// cache directory.
    pub version: &'a str,
    /// The pipeline's digest for this version's Linux engine, already
    /// validated with [`valid_pin`]. `None` in a build the pipeline did not
    /// make.
    pub sha256: Option<&'a str>,
    /// Where verified downloads live (`debian_media::cache_root`, or whatever
    /// the caller's own cache rule says).
    pub cache_root: &'a Path,
}

/// A compiled-in digest, or `None` when the string is absent or malformed.
///
/// The check is not decoration: `env!` of an unset variable yields `""`, and a
/// half-typed digest must be treated as no digest at all rather than as a
/// comparison that can never match.
pub fn valid_pin(raw: &str) -> Option<&str> {
    let pin = raw.trim();
    (pin.len() == DigestAlgo::Sha256.hex_len() && pin.bytes().all(|b| b.is_ascii_hexdigit()))
        .then_some(pin)
}

/// Why this build cannot offer the download, in the words the manager's tooltip
/// and the installer's finished page both use. `None` when it can.
pub fn install_block(pin: &Pin<'_>) -> Option<String> {
    pin.sha256.is_none().then(|| {
        format!(
            "This build was not made by the release pipeline, so it carries no verified \
             digest for the Linux engine (version {}). Build the Linux engine yourself and \
             put its path in the manager's Settings ▸ Linux engine — nothing here will \
             download a binary it cannot check.",
            pin.version
        )
    })
}

/// Where the asset is downloaded from.
pub fn asset_url(version: &str) -> String {
    if let Ok(url) = std::env::var(URL_ENV) {
        if !url.trim().is_empty() {
            return url.trim().to_string();
        }
    }
    format!("https://github.com/{REPO}/releases/download/v{version}/{ASSET}")
}

/// Where the verified download is kept: `<cache>/engine/<version>/<asset>`, per
/// version, because two builds of the same version do not exist but two
/// versions certainly do.
pub fn cache_path(cache_root: &Path, version: &str) -> PathBuf {
    cache_root
        .join("engine")
        .join(sanitize(version))
        .join(ASSET)
}

/// A version string reduced to something safe to join onto a path. It comes
/// from our own build, but it is still text that becomes a directory name, and
/// `..` in it would escape the cache.
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

// ---------------------------------------------------------------------------
// What can go wrong
// ---------------------------------------------------------------------------

/// A refused install. Every variant carries the whole sentence a user reads;
/// the variant itself exists so that a *program* — `entangled wsl
/// install-engine`, and through it the Windows installer — can turn each one
/// into its own exit code and its own line on the finished page.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// This build carries no pinned digest (every developer build).
    #[error("{0}")]
    NoPin(String),
    /// The cache directory could not be found or written.
    #[error("{0}")]
    Cache(String),
    /// The asset could not be fetched: no network, or no such release.
    #[error("{0}")]
    Download(String),
    /// Bytes arrived and did not match the pin. Nothing unverified is kept.
    #[error("{0}")]
    Digest(String),
    /// The verified file is somewhere WSL cannot see (a UNC path).
    #[error("{0}")]
    Unreachable(String),
    /// `wsl.exe` itself could not be started.
    #[error("{0}")]
    Wsl(String),
    /// The distribution ran and the copy did not land.
    #[error("{0}")]
    Install(String),
}

impl EngineError {
    /// A stable machine-readable tag, for `--json` and for the exit-code table
    /// in `apps/entangled/src/wsl_engine.rs`.
    pub const fn kind(&self) -> &'static str {
        match self {
            EngineError::NoPin(_) => "no-pinned-digest",
            EngineError::Cache(_) => "cache-unusable",
            EngineError::Download(_) => "download-failed",
            EngineError::Digest(_) => "digest-mismatch",
            EngineError::Unreachable(_) => "unreachable-download",
            EngineError::Wsl(_) => "wsl-unavailable",
            EngineError::Install(_) => "install-failed",
        }
    }
}

// ---------------------------------------------------------------------------
// The install
// ---------------------------------------------------------------------------

/// What a successful install produced, for the toast, for the sentence the CLI
/// prints, and for the setting the manager then writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub installed: Installed,
    /// The asset was already in the verified cache; no network was used.
    pub cached: bool,
    /// The distribution recomputed the digest of the copy it now holds and it
    /// matched. `false` only means the distribution had no `sha256sum` — the
    /// bytes were still verified on this side before they were handed over.
    pub rechecked: bool,
}

impl Outcome {
    pub fn summary(&self) -> String {
        let version = self
            .installed
            .version
            .as_deref()
            .unwrap_or("version unknown");
        let path = if self.installed.on_path {
            ", and on the distribution's PATH"
        } else {
            ""
        };
        format!(
            "Linux engine {version} installed at {}{path}",
            self.installed.path
        )
    }

    /// The summary plus the two caveats worth saying out loud. This is the
    /// whole sentence the CLI prints and the manager toasts, so that the two
    /// surfaces cannot describe the same install differently.
    pub fn sentence(&self) -> String {
        let mut message = self.summary();
        if self.cached {
            message.push_str(" (from the verified download already on disk)");
        }
        if !self.rechecked {
            message.push_str(
                " — the distribution has no sha256sum, so the copy inside it could not be \
                 re-checked; the download itself was verified",
            );
        }
        message
    }
}

/// Downloads the pinned Linux engine (or reuses the verified cache copy) and
/// installs it into `distro`.
///
/// Blocking, and against an injectable transport and process runner, so both
/// the accept and the refuse paths are testable with no network and no WSL.
/// The manager calls it on a worker thread; the CLI calls it on a thread it
/// abandons if a deadline expires (a wedged `wsl.exe` must not hang an
/// installer).
pub fn install_blocking(
    transport: &dyn Transport,
    pin: &Pin<'_>,
    distro: &str,
    run: Runner<'_>,
) -> Result<Outcome, EngineError> {
    if let Some(block) = install_block(pin) {
        return Err(EngineError::NoPin(block));
    }
    let expected = pin
        .sha256
        .ok_or_else(|| EngineError::NoPin("no pinned digest".to_string()))?
        .to_ascii_lowercase();
    let path = cache_path(pin.cache_root, pin.version);
    let cached = fetch_verified(transport, pin, &path, &expected)?;

    // The distribution has to reach the file, and a cache under a UNC path (a
    // roaming profile on a share) is the one place it cannot. Say that here
    // rather than letting `cp` fail inside a shell script.
    let source = wsl::to_wsl_path(&path).map_err(|e| {
        EngineError::Unreachable(format!(
            "the verified download is at {}, which WSL cannot see: {e}",
            path.display()
        ))
    })?;

    let out = run(&wsl::install_args(distro, &source))
        .map_err(|e| EngineError::Wsl(format!("cannot start {}: {e}", wsl::WSL_PROGRAM)))?;
    let text = out.text();
    if !out.ok() {
        return Err(EngineError::Install(format!(
            "installing the engine into {distro} failed: {}",
            wsl::translate(&text).unwrap_or_else(|| first_line(&text))
        )));
    }
    let installed = wsl::parse_installed(&text).ok_or_else(|| {
        EngineError::Install(format!(
            "{distro} accepted the copy but did not report where it went: {}",
            first_line(&text)
        ))
    })?;

    // The last link of the chain: the bytes inside the distribution are the
    // bytes that were verified out here. A distribution with no `sha256sum`
    // cannot answer, which is reported rather than assumed away.
    let rechecked = match &installed.sha256 {
        Some(found) if *found != expected => {
            return Err(EngineError::Digest(format!(
                "the engine copied into {distro} does not match what was downloaded \
                 (expected sha256 {expected}, the distribution computed {found}). Nothing \
                 unverified is left in use: remove {} and try again",
                installed.path
            )))
        }
        Some(_) => true,
        None => false,
    };

    Ok(Outcome {
        installed,
        cached,
        rechecked,
    })
}

/// Makes sure the cache holds a copy matching the pin. Returns `true` when it
/// already did and nothing was downloaded.
pub fn fetch_verified(
    transport: &dyn Transport,
    pin: &Pin<'_>,
    path: &Path,
    expected: &str,
) -> Result<bool, EngineError> {
    if path.is_file() && digest_of(path).is_ok_and(|found| found == expected) {
        return Ok(true);
    }
    let dir = path
        .parent()
        .ok_or_else(|| EngineError::Cache(format!("{} has no parent directory", path.display())))?;
    std::fs::create_dir_all(dir)
        .map_err(|e| EngineError::Cache(format!("cannot create {}: {e}", dir.display())))?;

    let url = asset_url(pin.version);
    let partial = debian_media::partial_path(path);
    let _ = std::fs::remove_file(&partial);
    tracing::info!(url, "downloading the Linux engine");

    let download = transport.get_range(&url, 0).map_err(|e| {
        EngineError::Download(format!(
            "cannot download {url}: {e}{}",
            not_published_hint(pin, &e)
        ))
    })?;
    let mut hasher = DigestAlgo::Sha256.hasher();
    let mut file = std::fs::File::create(&partial)
        .map_err(|e| EngineError::Cache(format!("cannot create {}: {e}", partial.display())))?;
    let mut reader = download.body.take(MAX_ASSET_LEN + 1);
    let mut buf = vec![0u8; 64 * 1024];
    let mut written: u64 = 0;
    loop {
        let read = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                let _ = std::fs::remove_file(&partial);
                return Err(EngineError::Download(format!(
                    "download of {url} failed: {e}"
                )));
            }
        };
        written += read as u64;
        if written > MAX_ASSET_LEN {
            let _ = std::fs::remove_file(&partial);
            return Err(EngineError::Download(format!(
                "{url} is larger than this build will accept"
            )));
        }
        hasher.update(&buf[..read]);
        if let Err(e) = file.write_all(&buf[..read]) {
            let _ = std::fs::remove_file(&partial);
            return Err(EngineError::Cache(format!(
                "cannot write {}: {e}",
                partial.display()
            )));
        }
    }
    if let Err(e) = file.flush() {
        let _ = std::fs::remove_file(&partial);
        return Err(EngineError::Cache(format!(
            "cannot write {}: {e}",
            partial.display()
        )));
    }
    drop(file);

    let found = hasher.finish_hex();
    if found != expected {
        let _ = std::fs::remove_file(&partial);
        return Err(EngineError::Digest(format!(
            "{url} does not match the digest this build was published with: expected sha256 \
             {expected}, got {found} ({written} bytes). The file has been deleted; nothing \
             unverified is kept"
        )));
    }
    std::fs::rename(&partial, path).map_err(|e| {
        let _ = std::fs::remove_file(&partial);
        EngineError::Cache(format!(
            "cannot move the verified download into {}: {e}",
            path.display()
        ))
    })?;

    // The same provenance note the media cache writes beside every artifact.
    // `signature_verified` is false and that is not an oversight: there is no
    // signature over this, only a digest compiled into the program that
    // downloaded it.
    let manifest = Manifest {
        url: url.clone(),
        version: pin.version.to_string(),
        fetched_at: debian_media::now_utc(),
        sha512_hex: expected.to_string(),
        signature_verified: false,
        signed_by: None,
        keyring: Some("sha256 stamped into this build by the release pipeline".to_string()),
    };
    if let Ok(text) = manifest.to_toml() {
        let manifest_path = debian_media::manifest_path(path);
        if let Err(e) = std::fs::write(&manifest_path, text) {
            tracing::warn!(error = %e, path = %manifest_path.display(), "cannot write the provenance note");
        }
    }
    Ok(false)
}

/// A 404 here has one likely cause, so say it instead of leaving "HTTP status
/// 404" to be interpreted.
fn not_published_hint(pin: &Pin<'_>, error: &TransportError) -> String {
    match error {
        TransportError::Status(404) => format!(
            "\n  release v{} carries no {ASSET} asset. Either this build was made from a \
             branch that never published one, or {URL_ENV} points somewhere that does not \
             serve it. Build the Linux engine yourself and name it in the manager's \
             Settings ▸ Linux engine instead.",
            pin.version
        ),
        _ => String::new(),
    }
}

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

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no output")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use debian_media::{Download, TransportError};
    use std::collections::HashMap;

    struct Serve(HashMap<String, Vec<u8>>);

    impl Transport for Serve {
        fn get_all(&self, url: &str, _limit: u64) -> Result<Vec<u8>, TransportError> {
            self.0.get(url).cloned().ok_or(TransportError::Status(404))
        }

        fn get_range(&self, url: &str, _offset: u64) -> Result<Download, TransportError> {
            let body = self
                .0
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

    fn pin<'a>(root: &'a Path, sha: Option<&'a str>) -> Pin<'a> {
        Pin {
            version: "0.2.137",
            sha256: sha,
            cache_root: root,
        }
    }

    #[test]
    fn only_a_full_hex_digest_counts_as_a_pin() {
        assert_eq!(valid_pin(&"a1".repeat(32)), Some("a1".repeat(32).as_str()));
        assert_eq!(valid_pin(""), None);
        assert_eq!(valid_pin("   "), None);
        assert_eq!(valid_pin(&"z".repeat(64)), None);
        assert_eq!(valid_pin(&"ab".repeat(20)), None);
    }

    /// A build the pipeline did not make must refuse to download rather than
    /// fetch something it cannot check — and say why, in a sentence with a way
    /// out in it.
    #[test]
    fn a_build_without_a_pin_refuses_to_download() {
        let root = std::env::temp_dir();
        let unpinned = pin(&root, None);
        let block = install_block(&unpinned).expect("no pin, so no download");
        assert!(block.contains("Settings"), "{block}");
        assert!(block.contains("0.2.137"), "{block}");

        let serve = Serve(HashMap::new());
        let error = install_blocking(&serve, &unpinned, "Ubuntu", &|_| Ok(wsl::Ran::default()))
            .expect_err("must not download");
        assert_eq!(error.kind(), "no-pinned-digest");
        assert!(error.to_string().contains("verified digest"), "{error}");

        assert!(install_block(&pin(&root, Some(&"a1".repeat(32)))).is_none());
    }

    /// The download half, driven end to end against a fake server: a body that
    /// hashes wrong is deleted rather than installed.
    #[test]
    fn a_download_that_does_not_match_the_pin_is_deleted() {
        let dir = std::env::temp_dir().join(format!("entangled-wsl-engine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let good = b"the real engine".to_vec();
        let expected = DigestAlgo::Sha256.hex_of(&good);
        let pinned = pin(&dir, Some(&expected));
        let path = cache_path(&dir, pinned.version);
        let url = asset_url(pinned.version);

        let wrong = Serve(
            [(url.clone(), b"something else".to_vec())]
                .into_iter()
                .collect(),
        );
        let error = fetch_verified(&wrong, &pinned, &path, &expected).expect_err("mismatch");
        assert_eq!(error.kind(), "digest-mismatch");
        assert!(error.to_string().contains("does not match the digest"));
        assert!(!path.exists(), "an unverified download must not be kept");

        // No server at all is the "no network" shape, and it must not be
        // mistaken for a digest problem.
        let empty = Serve(HashMap::new());
        assert_eq!(
            fetch_verified(&empty, &pinned, &path, &expected)
                .expect_err("nothing served")
                .kind(),
            "download-failed"
        );

        let right = Serve([(url, good)].into_iter().collect());
        assert!(!fetch_verified(&right, &pinned, &path, &expected).expect("accepted"));
        assert!(path.is_file());
        // Second time round it is a cache hit and nothing is served at all.
        let empty = Serve(HashMap::new());
        assert!(fetch_verified(&empty, &pinned, &path, &expected).expect("cached"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The version becomes a directory name, so it is sanitised: a pin is ours,
    /// but `..` in one would still walk out of the cache.
    #[test]
    fn the_cache_path_is_per_version_and_sanitised() {
        let root = Path::new("/cache");
        assert!(cache_path(root, "0.2.137")
            .to_string_lossy()
            .replace('\\', "/")
            .ends_with("/cache/engine/0.2.137/entangled-linux-x86_64"));
        let hostile = cache_path(root, "../../etc").to_string_lossy().to_string();
        assert!(!hostile.contains(".."), "{hostile}");
    }

    #[test]
    fn the_asset_url_is_this_version_and_is_overridable() {
        let url = asset_url("0.2.137");
        assert!(url.contains("0.2.137"), "{url}");
        assert!(url.ends_with(ASSET), "{url}");
        assert!(url.starts_with("https://github.com/"), "{url}");
    }

    /// A 404 is the shape a wrong or unpublished version takes, and it must
    /// name the version rather than leave "404" to be interpreted.
    #[test]
    fn a_404_says_which_release_has_no_engine() {
        let root = std::env::temp_dir();
        let hint = not_published_hint(&pin(&root, None), &TransportError::Status(404));
        assert!(hint.contains("v0.2.137"), "{hint}");
        assert!(hint.contains(URL_ENV), "{hint}");
        assert!(not_published_hint(&pin(&root, None), &TransportError::Status(500)).is_empty());
    }

    /// The whole sentence a successful install produces — both surfaces print
    /// this one, so its caveats are asserted once here rather than twice there.
    #[test]
    fn the_outcome_sentence_carries_both_caveats() {
        let outcome = Outcome {
            installed: Installed {
                path: "/home/spider/.local/bin/entangled".into(),
                version: Some("0.2.137".into()),
                sha256: None,
                on_path: false,
            },
            cached: true,
            rechecked: false,
        };
        let sentence = outcome.sentence();
        assert!(sentence.contains("0.2.137"), "{sentence}");
        assert!(sentence.contains("already on disk"), "{sentence}");
        assert!(sentence.contains("no sha256sum"), "{sentence}");

        let clean = Outcome {
            cached: false,
            rechecked: true,
            ..outcome
        };
        assert_eq!(clean.sentence(), clean.summary());
    }
}
