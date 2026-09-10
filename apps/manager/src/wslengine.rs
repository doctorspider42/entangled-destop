//! The Linux engine inside WSL: checking it before a run, and offering to
//! install it when it is not there.
//!
//! Almost nothing happens here any more, and that is the point. The
//! classification lives in [`control_api::wsl`] and the download, the digest
//! check and the copy into the distribution live in
//! [`control_api::wsl_engine`] — both shared with `entangled`, which offers
//! the same install as `entangled wsl install-engine` and is what the Windows
//! installer's optional task runs. This module is the manager's half: the
//! worker threads, the state the UI renders, and the one fact only this binary
//! knows — the digest the release pipeline stamped into *it*.
//!
//! # What the download trusts
//!
//! `ENTANGLED_LINUX_ENGINE_SHA256`, forwarded by `build.rs` from the release
//! workflow, which hashed the Linux binary before it built this one. The whole
//! anchor — what it is worth, and why it is not a signature — is written out
//! in [`control_api::wsl_engine`]. A build the pipeline did not make (every
//! developer build) carries no digest, and then [`install_block`] says so and
//! the button is greyed rather than downloading something it cannot check.

use std::path::PathBuf;
use std::sync::mpsc;

use control_api::wsl::{self, EngineFault, EngineFound, Fault};
use control_api::wsl_engine::{self, Pin};
use debian_media::UreqTransport;

pub use control_api::wsl_engine::Outcome;

/// SHA-256 of the Linux engine for *this* build, stamped in by the release
/// pipeline. Empty in any build the pipeline did not make.
const PINNED_SHA256: &str = env!("ENTANGLED_LINUX_ENGINE_SHA256");

/// The pinned digest, or `None` in a build the release pipeline did not make.
pub fn pinned_sha256() -> Option<&'static str> {
    wsl_engine::valid_pin(PINNED_SHA256)
}

/// Where this binary keeps verified downloads. `None` when the host has no
/// cache directory at all, which is the one state that has to be reported
/// before anything is attempted.
fn cache_root() -> Option<PathBuf> {
    crate::launcher::cache_root()
}

/// The three facts the shared installer needs from this binary: which release
/// it belongs to, the digest it was published with, and where it caches.
fn pin(cache_root: &std::path::Path) -> Pin<'_> {
    Pin {
        version: crate::VERSION,
        sha256: pinned_sha256(),
        cache_root,
    }
}

/// Why this build cannot offer the download, in the words the button's tooltip
/// uses. `None` when it can.
pub fn install_block() -> Option<String> {
    // The cache root is not part of the *pin* question, so a throwaway path is
    // enough here: `install_block` only ever reads the digest.
    wsl_engine::install_block(&pin(std::path::Path::new(".")))
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
    waker: crate::process::Waker,
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

/// Downloads the pinned Linux engine (or reuses the verified cache copy) and
/// installs it into `distro`, on a worker thread.
pub fn spawn_install(
    distro: String,
    waker: crate::process::Waker,
) -> mpsc::Receiver<Result<Outcome, String>> {
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("wsl-engine-install".to_string())
        .spawn(move || {
            let result = install_blocking(&distro);
            if tx.send(result).is_ok() {
                waker();
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "cannot start the WSL engine install");
    }
    rx
}

/// The shared install, with this binary's pin, cache and quiet runner.
fn install_blocking(distro: &str) -> Result<Outcome, String> {
    let root = cache_root()
        .ok_or_else(|| "no cache directory (set ENTANGLED_CACHE or HOME)".to_string())?;
    wsl_engine::install_blocking(&UreqTransport::new(), &pin(&root), distro, &run_quiet)
        .map_err(|e| e.to_string())
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

    /// A build the pipeline did not make must refuse to download rather than
    /// fetch something it cannot check — and say why, in a sentence with a way
    /// out in it. (The refusal itself is tested in `control-api`; what is
    /// asserted here is that *this* binary's digest reaches it.)
    #[test]
    fn a_developer_build_greys_the_install_button() {
        match pinned_sha256() {
            // A release build: the pin exists, and this test's premise does not.
            Some(pin) => assert!(install_block().is_none(), "pinned {pin} but still blocked"),
            None => {
                let block = install_block().expect("no pin, so no download");
                assert!(block.contains("Settings"), "{block}");
                assert!(block.contains(crate::VERSION), "{block}");
            }
        }
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
    fn the_asset_url_is_this_version_and_ends_in_the_asset() {
        let url = wsl_engine::asset_url(crate::VERSION);
        assert!(url.contains(crate::VERSION), "{url}");
        assert!(url.ends_with(wsl_engine::ASSET), "{url}");
    }
}
