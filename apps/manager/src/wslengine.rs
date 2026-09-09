//! The Linux engine inside WSL: checking it before a run, and offering to
//! install it when it is not there.
//!
//! The classification lives in [`control_api::wsl`] (shared with `entangled
//! doctor`); this module is the manager's half of it — the worker threads, the
//! download, and the state the UI renders.
//!
//! # What the download trusts
//!
//! One asset, `entangled-linux-x86_64`, published by the **same release run**
//! that built this manager, and verified against a SHA-256 that the release
//! pipeline stamped into this binary at compile time
//! (`ENTANGLED_LINUX_ENGINE_SHA256`, forwarded by `build.rs`). Bytes that do not
//! hash to it are deleted, not used, and are never handed to WSL.
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
//!   half, and the digest is compiled into the manager that will download it.
//!   Nobody types it, nobody can point it at a different build, and the
//!   `windows-installer` job that publishes the installer is the job that
//!   publishes the binary the digest describes;
//! * it is still **not a signature**. There is no key and nothing to revoke:
//!   whoever can change the workflow can change what gets hashed. TLS to
//!   github.com is the transport. In short — as strong as the git history of
//!   this repository plus the integrity of one CI run, and no stronger, which
//!   is the same honest ceiling `bootstrap.rs` documents for the kernel.
//!
//! A build made outside the pipeline (every developer build) carries no digest
//! at all, and then the install button says so instead of downloading something
//! it cannot check. Refusing is the whole point: an unverified engine copied
//! into a distribution would be the one thing in the product with no anchor.
//!
//! # Licence
//!
//! The asset is our own binary, Apache-2.0, built from this repository — the
//! question `cargo deny` answers for the crate graph is the same question here
//! and has the same answer. Nothing GPL is downloaded, shipped or linked.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use control_api::wsl::{self, EngineFault, EngineFound, Fault, Installed};
use debian_media::{DigestAlgo, Manifest, Transport, UreqTransport};

use crate::process::Waker;

/// The release asset holding the Linux build of `entangled`.
pub const ASSET: &str = "entangled-linux-x86_64";

/// SHA-256 of that asset for *this* build, stamped in by the release pipeline.
/// Empty in any build the pipeline did not make.
const PINNED_SHA256: &str = env!("ENTANGLED_LINUX_ENGINE_SHA256");

/// Relocate the download (a mirror, or a directory served over HTTP in a test)
/// without weakening it: the compiled-in digest is enforced against whatever it
/// serves.
const URL_ENV: &str = "ENTANGLED_LINUX_ENGINE_URL";

/// The engine is ~20 MiB. A server that streams forever is a bug or an attack,
/// and either way it must not fill the disk.
const MAX_ASSET_LEN: u64 = 256 * 1024 * 1024;

/// Where the verified download is kept: `<cache>/engine/<version>/<asset>`, per
/// version, because two builds of the same version do not exist but two
/// versions certainly do.
fn cache_path() -> Result<PathBuf, String> {
    let root = crate::launcher::cache_root()
        .ok_or_else(|| "no cache directory (set ENTANGLED_CACHE or HOME)".to_string())?;
    Ok(root
        .join("engine")
        .join(sanitize(crate::VERSION))
        .join(ASSET))
}

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

/// The pinned digest, or `None` in a build the release pipeline did not make.
pub fn pinned_sha256() -> Option<&'static str> {
    let pin = PINNED_SHA256.trim();
    (pin.len() == DigestAlgo::Sha256.hex_len() && pin.bytes().all(|b| b.is_ascii_hexdigit()))
        .then_some(pin)
}

/// Where the asset is downloaded from.
pub fn asset_url() -> String {
    if let Ok(url) = std::env::var(URL_ENV) {
        if !url.trim().is_empty() {
            return url.trim().to_string();
        }
    }
    format!(
        "https://github.com/{}/releases/download/v{}/{ASSET}",
        crate::update::REPO,
        crate::VERSION
    )
}

/// Why this build cannot offer the download, in the words the button's tooltip
/// uses. `None` when it can.
pub fn install_block() -> Option<String> {
    pinned_sha256().is_none().then(|| {
        format!(
            "This build of the manager was not made by the release pipeline, so it carries no \
             verified digest for the Linux engine (version {}). Build the Linux engine \
             yourself and put its path in Settings ▸ Linux engine — the manager will not \
             download a binary it cannot check.",
            crate::VERSION
        )
    })
}

// ---------------------------------------------------------------------------
// The state the UI renders
// ---------------------------------------------------------------------------

/// The pre-flight's answer for the WSL backend.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Status {
    /// Nothing asked yet — a fresh window, or the settings just changed.
    #[default]
    Unknown,
    Checking,
    Ready(EngineFound),
    Failed(EngineFault),
}

impl Status {
    /// True only when a machine may actually be started on this backend. An
    /// unfinished check is not a yes.
    pub fn usable(&self) -> bool {
        matches!(self, Status::Ready(_))
    }

    /// The one line the wizard, the settings panel and the cards show. `None`
    /// while nothing is known yet.
    pub fn line(&self) -> Option<String> {
        match self {
            Status::Unknown => None,
            Status::Checking => Some("checking the WSL engine…".to_string()),
            Status::Ready(found) => Some(found.summary()),
            Status::Failed(fault) => Some(fault.what.clone()),
        }
    }

    /// Whether "Install the Linux engine" makes sense right now.
    pub fn installable(&self) -> bool {
        matches!(self, Status::Failed(fault) if fault.fault.installable())
    }

    /// The refusal a launch shows, complete with its fix.
    pub fn refusal(&self) -> Option<String> {
        match self {
            Status::Failed(fault) => Some(fault.sentence()),
            _ => None,
        }
    }
}

/// The identity of a check: re-probing is only necessary when one of these
/// changes, and a settings save that touched neither must not restart it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub distro: String,
    pub engine: Option<String>,
}

impl Target {
    pub fn new(distro: &str, engine: Option<&str>) -> Self {
        Self {
            distro: distro.trim().to_string(),
            engine: engine
                .map(str::trim)
                .filter(|e| !e.is_empty())
                .map(str::to_string),
        }
    }
}

// ---------------------------------------------------------------------------
// Probing, off the frame loop
// ---------------------------------------------------------------------------

/// `wsl.exe`, spawned the way this GUI spawns everything: with no console
/// window flashing over it.
fn run_quiet(args: &[String]) -> std::io::Result<wsl::Ran> {
    let mut command = std::process::Command::new(wsl::WSL_PROGRAM);
    command.args(args);
    crate::process::quiet_command(&mut command);
    let out = command.output()?;
    Ok(wsl::Ran {
        code: out.status.code(),
        stdout: out.stdout,
        stderr: out.stderr,
    })
}

/// Runs the three-question pre-flight on a worker thread.
///
/// It can take seconds — a cold distribution has to boot — which is exactly why
/// it never happens on the frame loop.
pub fn spawn_probe(
    target: Target,
    waker: Waker,
) -> mpsc::Receiver<Result<EngineFound, EngineFault>> {
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("wsl-engine-check".to_string())
        .spawn(move || {
            let answer = wsl::probe_with(&target.distro, target.engine.as_deref(), &run_quiet);
            if tx.send(answer).is_ok() {
                waker();
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "cannot start the WSL engine check");
    }
    rx
}

// ---------------------------------------------------------------------------
// Installing
// ---------------------------------------------------------------------------

/// What a successful install produced, for the toast and for the setting the
/// manager then writes.
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
}

/// Downloads the pinned Linux engine (or reuses the verified cache copy) and
/// installs it into `distro`, on a worker thread.
pub fn spawn_install(distro: String, waker: Waker) -> mpsc::Receiver<Result<Outcome, String>> {
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("wsl-engine-install".to_string())
        .spawn(move || {
            let result = install_blocking(&UreqTransport::new(), &distro, &run_quiet);
            if tx.send(result).is_ok() {
                waker();
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "cannot start the WSL engine install");
    }
    rx
}

/// The whole install, against an injectable transport and runner so the accept
/// and refuse paths are testable with no network and no WSL.
pub fn install_blocking(
    transport: &dyn Transport,
    distro: &str,
    run: wsl::Runner<'_>,
) -> Result<Outcome, String> {
    if let Some(block) = install_block() {
        return Err(block);
    }
    let expected = pinned_sha256()
        .ok_or("no pinned digest")?
        .to_ascii_lowercase();
    let path = cache_path()?;
    let cached = fetch_verified(transport, &path, &expected)?;

    // The distribution has to reach the file, and a cache under a UNC path (a
    // roaming profile on a share) is the one place it cannot. Say that here
    // rather than letting `cp` fail inside a shell script.
    let source = crate::backend::to_wsl_path(&path).map_err(|e| {
        format!(
            "the verified download is at {}, which WSL cannot see: {e}",
            path.display()
        )
    })?;

    let out = run(&wsl::install_args(distro, &source))
        .map_err(|e| format!("cannot start {}: {e}", wsl::WSL_PROGRAM))?;
    let text = out.text();
    if !out.ok() {
        return Err(format!(
            "installing the engine into {distro} failed: {}",
            wsl::translate(&text).unwrap_or_else(|| first_line(&text))
        ));
    }
    let installed = wsl::parse_installed(&text).ok_or_else(|| {
        format!(
            "{distro} accepted the copy but did not report where it went: {}",
            first_line(&text)
        )
    })?;

    // The last link of the chain: the bytes inside the distribution are the
    // bytes that were verified out here. A distribution with no `sha256sum`
    // cannot answer, which is reported rather than assumed away.
    let rechecked = match &installed.sha256 {
        Some(found) if *found != expected => {
            return Err(format!(
                "the engine copied into {distro} does not match what was downloaded \
                 (expected sha256 {expected}, the distribution computed {found}). Nothing \
                 unverified is left in use: remove {} and try again",
                installed.path
            ))
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
fn fetch_verified(transport: &dyn Transport, path: &Path, expected: &str) -> Result<bool, String> {
    if path.is_file() && digest_of(path).is_ok_and(|found| found == expected) {
        return Ok(true);
    }
    let dir = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    let url = asset_url();
    let partial = debian_media::partial_path(path);
    let _ = std::fs::remove_file(&partial);
    tracing::info!(url, "downloading the Linux engine");

    let download = transport
        .get_range(&url, 0)
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
            return Err(format!("{url} is larger than this build will accept"));
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
            "{url} does not match the digest this build was published with: expected sha256 \
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
    })?;

    // The same provenance note the media cache writes beside every artifact.
    // `signature_verified` is false and that is not an oversight: there is no
    // signature over this, only a digest compiled into the program that
    // downloaded it.
    let manifest = Manifest {
        url: url.clone(),
        version: crate::VERSION.to_string(),
        fetched_at: debian_media::now_utc(),
        sha512_hex: expected.to_string(),
        signature_verified: false,
        signed_by: None,
        keyring: Some("sha256 stamped into this manager by the release pipeline".to_string()),
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
fn not_published_hint(error: &debian_media::TransportError) -> String {
    match error {
        debian_media::TransportError::Status(404) => format!(
            "\n  release v{} carries no {ASSET} asset. Either this build was made from a \
             branch that never published one, or {URL_ENV} points somewhere that does not \
             serve it. Build the Linux engine yourself and name it in Settings ▸ Linux \
             engine instead.",
            crate::VERSION
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

/// The fixture the `--mock` session shows: a distribution with no engine, which
/// is the state this whole feature exists for and the one a screenshot has to
/// be able to show.
pub fn mock_status() -> Status {
    Status::Failed(EngineFault {
        fault: Fault::NoEngine,
        what: "Ubuntu has no Entangled engine ('entangled' is not there)".to_string(),
        fix: "The Windows installer ships no Linux build: let the manager install one \
              (Settings ▸ Linux engine), or point that setting at your own build."
            .to_string(),
    })
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

    /// A build the pipeline did not make must refuse to download rather than
    /// fetch something it cannot check — and say why, in a sentence with a way
    /// out in it.
    #[test]
    fn a_developer_build_refuses_to_download_an_unpinned_engine() {
        if pinned_sha256().is_some() {
            // A release build: the pin exists, and this test's premise does not.
            return;
        }
        let block = install_block().expect("no pin, so no download");
        assert!(block.contains("Settings"), "{block}");
        let serve = Serve(HashMap::new());
        let error = install_blocking(&serve, "Ubuntu", &|_| Ok(wsl::Ran::default()))
            .expect_err("must not download");
        assert!(error.contains("verified digest"), "{error}");
    }

    /// The download half, driven end to end against a fake server: a body that
    /// hashes wrong is deleted rather than installed.
    #[test]
    fn a_download_that_does_not_match_the_pin_is_deleted() {
        let dir = std::env::temp_dir().join(format!("entangled-wslengine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(ASSET);
        let good = b"the real engine".to_vec();
        let expected = DigestAlgo::Sha256.hex_of(&good);
        let url = asset_url();

        let wrong = Serve(
            [(url.clone(), b"something else".to_vec())]
                .into_iter()
                .collect(),
        );
        let error = fetch_verified(&wrong, &path, &expected).expect_err("digest mismatch");
        assert!(error.contains("does not match the digest"), "{error}");
        assert!(!path.exists(), "an unverified download must not be kept");

        let right = Serve([(url, good)].into_iter().collect());
        assert!(!fetch_verified(&right, &path, &expected).expect("accepted"));
        assert!(path.is_file());
        // Second time round it is a cache hit and nothing is served at all.
        let empty = Serve(HashMap::new());
        assert!(fetch_verified(&empty, &path, &expected).expect("cached"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The status is what gates the buttons, so its three answers must not
    /// blur: only `Ready` is usable, and only a missing engine is installable.
    #[test]
    fn the_status_gates_are_exact() {
        assert!(!Status::Unknown.usable());
        assert!(!Status::Checking.usable());
        assert!(!Status::Unknown.installable());

        let missing = mock_status();
        assert!(!missing.usable());
        assert!(missing.installable());
        assert!(missing
            .refusal()
            .expect("refusal")
            .contains("let the manager install one"));

        let broken = Status::Failed(EngineFault {
            fault: Fault::NoDistro,
            what: "WSL has no distribution called 'Fedora'".into(),
            fix: "install one".into(),
        });
        assert!(
            !broken.installable(),
            "there is nowhere to install an engine without a distribution"
        );

        let ready = Status::Ready(EngineFound {
            distro: "Ubuntu".into(),
            command: "entangled".into(),
            path: Some("/home/spider/.local/bin/entangled".into()),
            version: Some("0.2.137".into()),
        });
        assert!(ready.usable());
        assert!(ready.refusal().is_none());
        assert!(ready.line().expect("line").contains("0.2.137"));
    }

    /// A re-check is needed when the distribution or the engine path changes,
    /// and only then — the settings panel saves on every OK.
    #[test]
    fn the_check_target_ignores_whitespace_and_empty_paths() {
        assert_eq!(
            Target::new("Ubuntu", Some("  ")),
            Target::new("Ubuntu", None)
        );
        assert_eq!(
            Target::new(" Ubuntu ", Some(" /opt/e ")),
            Target::new("Ubuntu", Some("/opt/e"))
        );
        assert_ne!(Target::new("Ubuntu", None), Target::new("Debian", None));
    }

    #[test]
    fn the_asset_url_is_this_version_and_is_overridable() {
        let url = asset_url();
        assert!(url.contains(crate::VERSION), "{url}");
        assert!(url.ends_with(ASSET), "{url}");
    }
}
