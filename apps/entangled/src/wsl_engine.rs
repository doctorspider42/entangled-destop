//! `entangled wsl install-engine` — put the Linux engine into a WSL
//! distribution, from a script.
//!
//! # Why a subcommand exists at all
//!
//! The manager has done this from Settings since the WSL backend shipped, and
//! it works. What it cannot do is happen *before* anyone opens the manager,
//! and the person who most needs it is the one who has just run the Windows
//! installer and does not yet know that "WSL (KVM)" is where the KVM backend
//! and 3D acceleration live. So the installer offers it as an optional task
//! (`installer/entangled.iss`), and an Inno Setup script cannot read a digest
//! compiled into a GUI binary — it can only run a program and read an exit
//! code. This is that program.
//!
//! Both surfaces call [`control_api::wsl_engine::install_blocking`]; the only
//! thing this module adds is a command line, a deadline, and the exit-code
//! table below.
//!
//! # Exit codes are the contract
//!
//! `installer/entangled.iss` turns each of these into one sentence on the
//! finished page, so they are as much a public interface as the flags are.
//! **Nothing here may fail the installation** — the installer ignores the code
//! for the purpose of succeeding and only uses it to choose what to say — but
//! a user who ticked the box is owed an honest account of what happened.
//!
//! | Code | Meaning |
//! |---|---|
//! | 0 | installed, or an engine was already there |
//! | 1 | something else went wrong (the message says what) |
//! | 2 | this machine has no usable WSL |
//! | 3 | WSL has no distribution by that name |
//! | 4 | the distribution is there; the engine is not usable and could not be replaced |
//! | 5 | the download failed — no network, or the release has no such asset |
//! | 6 | what arrived did not match the digest built into this program |
//! | 7 | this build carries no pinned digest (every developer build) |
//! | 8 | it took longer than `--timeout` |
//! | 9 | not a Windows host — there is no `wsl.exe` to talk to |
//!
//! # The deadline
//!
//! Every step here can block for a long time and two of them can block
//! forever: a WSL distribution that has never been started boots on the first
//! `wsl -d X` (tens of seconds, sometimes minutes), and a stalled TCP read has
//! no timeout of its own. An installer that waits on either is a hang with no
//! message, which is the one outcome this whole feature is meant to remove. So
//! the work runs on a thread and the command abandons it at `--timeout`,
//! printing what it was waiting for. Abandoning is safe: the install script
//! copies to a temporary name and renames over the target, so a late finish
//! either lands completely or not at all.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use clap::{Args, Subcommand};
use control_api::wsl::{self, EngineFault, Fault};
use control_api::wsl_engine::{self, EngineError, Outcome, Pin};
use debian_media::UreqTransport;

/// SHA-256 of the Linux engine published beside this build, stamped in by the
/// release pipeline (build.rs). Empty in every other build.
const PINNED_SHA256: &str = env!("ENTANGLED_LINUX_ENGINE_SHA256");

/// How long the whole command may take before it gives up and says so.
const DEFAULT_TIMEOUT_SECS: u64 = 900;

#[derive(Subcommand)]
pub enum WslCommand {
    /// Download the Linux engine published with this version and install it
    /// into a WSL distribution.
    ///
    /// This is what the Windows installer's optional "Linux engine in WSL"
    /// task runs, and what the manager's Settings ▸ Install the Linux engine
    /// button does. Safe to run again: an engine that is already there and
    /// runs is reported and left alone.
    InstallEngine(InstallEngineArgs),
}

#[derive(Args)]
pub struct InstallEngineArgs {
    /// Which WSL distribution — the name `wsl --list` shows.
    #[arg(long, default_value = wsl::DEFAULT_DISTRO)]
    pub distro: String,
    /// One line of output and nothing else: what happened, or why it did not.
    #[arg(long)]
    pub quiet: bool,
    /// One JSON object on stdout instead of prose. Implies --quiet.
    #[arg(long)]
    pub json: bool,
    /// Install even when a working engine is already there.
    #[arg(long)]
    pub force: bool,
    /// Give up after this many seconds (0 disables the deadline).
    #[arg(long, value_name = "SECS", default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub timeout: u64,
    /// Append the one-line result to this file as well.
    ///
    /// For the installer, which runs this hidden and has nowhere to show
    /// stdout. Defaults to `<cache>/engine/install-engine.log` under --quiet
    /// and to nothing otherwise; `--log -` turns it off.
    #[arg(long, value_name = "FILE")]
    pub log: Option<PathBuf>,
}

/// Every way this command can end, with the exit code the installer reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Installed,
    AlreadyPresent,
    Failed,
    NoWsl,
    NoDistro,
    EngineUnusable,
    DownloadFailed,
    DigestMismatch,
    NoPinnedDigest,
    TimedOut,
    NotWindows,
}

impl Status {
    pub const fn code(self) -> u8 {
        match self {
            Status::Installed | Status::AlreadyPresent => 0,
            Status::Failed => 1,
            Status::NoWsl => 2,
            Status::NoDistro => 3,
            Status::EngineUnusable => 4,
            Status::DownloadFailed => 5,
            Status::DigestMismatch => 6,
            Status::NoPinnedDigest => 7,
            Status::TimedOut => 8,
            Status::NotWindows => 9,
        }
    }

    /// The machine-readable tag `--json` reports. Stable: scripts key off it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Status::Installed => "installed",
            Status::AlreadyPresent => "already-present",
            Status::Failed => "failed",
            Status::NoWsl => "no-wsl",
            Status::NoDistro => "no-distro",
            Status::EngineUnusable => "engine-unusable",
            Status::DownloadFailed => "download-failed",
            Status::DigestMismatch => "digest-mismatch",
            Status::NoPinnedDigest => "no-pinned-digest",
            Status::TimedOut => "timed-out",
            Status::NotWindows => "not-windows",
        }
    }

    pub const fn ok(self) -> bool {
        self.code() == 0
    }
}

/// What the command did, in the shape both the prose and the JSON render from.
#[derive(Debug, Clone)]
pub struct Report {
    pub status: Status,
    pub distro: String,
    pub message: String,
    /// The absolute Linux path of the engine, when there is one.
    pub path: Option<String>,
    pub version: Option<String>,
    /// The download came from the verified cache; no network was used.
    pub cached: bool,
}

impl Report {
    fn new(status: Status, distro: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            distro: distro.to_string(),
            message: message.into(),
            path: None,
            version: None,
            cached: false,
        }
    }

    fn json(&self) -> String {
        serde_json::json!({
            "ok": self.status.ok(),
            "code": self.status.code(),
            "status": self.status.as_str(),
            "distro": self.distro,
            "path": self.path,
            "version": self.version,
            "cached": self.cached,
            "message": self.message,
        })
        .to_string()
    }

    /// The single line every surface prints: the log file, `--quiet` stdout,
    /// and the last line of the chatty rendering.
    fn line(&self) -> String {
        match self.status.ok() {
            true => self.message.clone(),
            false => format!("the Linux engine was not installed: {}", self.message),
        }
    }
}

/// `entangled wsl <command>`.
///
/// Returns an [`ExitCode`] rather than a `Result` because the code *is* the
/// answer here: `main` hands this one straight back (see its comment).
pub fn run(command: WslCommand) -> ExitCode {
    match command {
        WslCommand::InstallEngine(args) => {
            let report = install_engine(&args);
            emit(&report, &args);
            ExitCode::from(report.status.code())
        }
    }
}

/// Prints the report the way the flags asked for, and appends it to the log.
fn emit(report: &Report, args: &InstallEngineArgs) {
    if args.json {
        println!("{}", report.json());
    } else if report.status.ok() {
        println!("{}", report.line());
    } else {
        eprintln!("{}", report.line());
    }
    if let Some(path) = log_path(args) {
        append_log(&path, report);
    }
}

/// Where the one-line result is appended, if anywhere: `--log FILE`, `--log -`
/// for nowhere, and under `--quiet`/`--json` a default beside the download
/// cache — because that is the mode the installer uses, and it has nowhere to
/// show stdout.
fn log_path(args: &InstallEngineArgs) -> Option<PathBuf> {
    match &args.log {
        Some(path) if path.as_os_str() == "-" => None,
        Some(path) => Some(path.clone()),
        None if args.quiet || args.json => Some(
            crate::paths::cache_root()
                .ok()?
                .join("engine")
                .join("install-engine.log"),
        ),
        None => None,
    }
}

fn append_log(path: &std::path::Path, report: &Report) {
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let line = format!(
        "{} entangled {} wsl install-engine --distro {}: [{}] {}\n",
        debian_media::now_utc(),
        crate::VERSION,
        report.distro,
        report.status.as_str(),
        report.message
    );
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

// ---------------------------------------------------------------------------
// The work
// ---------------------------------------------------------------------------

/// Runs the install with a deadline over the whole thing.
fn install_engine(args: &InstallEngineArgs) -> Report {
    let distro = args.distro.trim();
    let distro = if distro.is_empty() {
        wsl::DEFAULT_DISTRO
    } else {
        distro
    };

    if !cfg!(windows) {
        return Report::new(
            Status::NotWindows,
            distro,
            "this command talks to wsl.exe, which only exists on Windows. On Linux the \
             engine is the program you just ran — build or install it in the usual way."
                .to_string(),
        );
    }

    if !args.quiet && !args.json {
        println!(
            "Asking WSL about '{distro}'. A distribution that has not been started yet has \
             to boot first, which can take a minute."
        );
    }

    let (tx, rx) = mpsc::channel();
    let owned = InstallEngineArgs {
        distro: distro.to_string(),
        quiet: args.quiet,
        json: args.json,
        force: args.force,
        timeout: args.timeout,
        log: None,
    };
    let spawned = std::thread::Builder::new()
        .name("wsl-install-engine".to_string())
        .spawn(move || {
            let report = install_engine_blocking(&owned);
            let _ = tx.send(report);
        });
    if let Err(e) = spawned {
        return Report::new(
            Status::Failed,
            distro,
            format!("cannot start a worker thread: {e}"),
        );
    }

    if args.timeout == 0 {
        return rx.recv().unwrap_or_else(|_| {
            Report::new(Status::Failed, distro, "the worker thread died".to_string())
        });
    }
    match rx.recv_timeout(Duration::from_secs(args.timeout)) {
        Ok(report) => report,
        Err(mpsc::RecvTimeoutError::Timeout) => Report::new(
            Status::TimedOut,
            distro,
            format!(
                "nothing finished within {} seconds. WSL may still be starting '{distro}', or \
                 the download may be stalled. Nothing half-written is left behind — try again, \
                 or do it from the manager (Settings ▸ Install the Linux engine), which shows \
                 progress.",
                args.timeout
            ),
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Report::new(Status::Failed, distro, "the worker thread died".to_string())
        }
    }
}

/// Probe, then install if that is what is missing.
fn install_engine_blocking(args: &InstallEngineArgs) -> Report {
    let distro = args.distro.as_str();

    match wsl::probe(distro, None) {
        Ok(found) if !args.force => {
            let mut report = Report::new(
                Status::AlreadyPresent,
                distro,
                format!(
                    "{distro} already has a working Entangled engine: {}",
                    found.summary()
                ),
            );
            report.path = found.path.clone();
            report.version = found.version.clone();
            return report;
        }
        Ok(_) => {}
        Err(fault) => {
            if let Some(report) = refusal(distro, &fault) {
                return report;
            }
        }
    }

    let root = match crate::paths::cache_root() {
        Ok(root) => root,
        Err(e) => return Report::new(Status::Failed, distro, e),
    };
    let pin = Pin {
        version: crate::VERSION,
        sha256: wsl_engine::valid_pin(PINNED_SHA256),
        cache_root: &root,
    };

    if !args.quiet && !args.json {
        println!(
            "Installing the Linux engine {} into {distro} (from {}).",
            crate::VERSION,
            wsl_engine::asset_url(crate::VERSION)
        );
    }

    match wsl_engine::install_blocking(&UreqTransport::new(), &pin, distro, &wsl::run_wsl) {
        Ok(outcome) => installed(distro, &outcome),
        Err(error) => Report::new(status_of(&error), distro, error.to_string()),
    }
}

/// A probe fault that is not "there is simply no engine yet" ends the command:
/// there is nowhere to install to, or nothing an install would fix.
fn refusal(distro: &str, fault: &EngineFault) -> Option<Report> {
    let status = match fault.fault {
        // The one fault an install is the answer to.
        Fault::NoEngine => return None,
        Fault::NoWsl => Status::NoWsl,
        Fault::NoDistro => Status::NoDistro,
        Fault::EngineFailed => Status::EngineUnusable,
    };
    Some(Report::new(status, distro, fault.sentence()))
}

fn installed(distro: &str, outcome: &Outcome) -> Report {
    let mut report = Report::new(Status::Installed, distro, outcome.sentence());
    report.path = Some(outcome.installed.path.clone());
    report.version = outcome.installed.version.clone();
    report.cached = outcome.cached;
    report
}

/// Each shared install error keeps its own exit code, so the installer can say
/// "there is no network" and "that build has no digest" differently.
fn status_of(error: &EngineError) -> Status {
    match error {
        EngineError::NoPin(_) => Status::NoPinnedDigest,
        EngineError::Download(_) => Status::DownloadFailed,
        EngineError::Digest(_) => Status::DigestMismatch,
        EngineError::Wsl(_) => Status::NoWsl,
        EngineError::Install(_) | EngineError::Unreachable(_) => Status::EngineUnusable,
        EngineError::Cache(_) => Status::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exit codes are read by `installer/entangled.iss`, which cannot be
    /// compiled against them. Changing one silently is how the installer ends
    /// up telling a user the opposite of what happened.
    #[test]
    fn the_exit_codes_are_the_documented_table() {
        for (status, code, tag) in [
            (Status::Installed, 0, "installed"),
            (Status::AlreadyPresent, 0, "already-present"),
            (Status::Failed, 1, "failed"),
            (Status::NoWsl, 2, "no-wsl"),
            (Status::NoDistro, 3, "no-distro"),
            (Status::EngineUnusable, 4, "engine-unusable"),
            (Status::DownloadFailed, 5, "download-failed"),
            (Status::DigestMismatch, 6, "digest-mismatch"),
            (Status::NoPinnedDigest, 7, "no-pinned-digest"),
            (Status::TimedOut, 8, "timed-out"),
            (Status::NotWindows, 9, "not-windows"),
        ] {
            assert_eq!(status.code(), code, "{tag}");
            assert_eq!(status.as_str(), tag);
            assert_eq!(status.ok(), code == 0, "{tag}");
        }
    }

    /// Every shared install failure must reach a code of its own — a fallback
    /// that swallowed one into "something went wrong" would take the fix out
    /// of the installer's sentence.
    #[test]
    fn every_install_error_keeps_its_own_code() {
        let cases = [
            (EngineError::NoPin(String::new()), Status::NoPinnedDigest),
            (EngineError::Download(String::new()), Status::DownloadFailed),
            (EngineError::Digest(String::new()), Status::DigestMismatch),
            (EngineError::Wsl(String::new()), Status::NoWsl),
            (EngineError::Install(String::new()), Status::EngineUnusable),
            (
                EngineError::Unreachable(String::new()),
                Status::EngineUnusable,
            ),
            (EngineError::Cache(String::new()), Status::Failed),
        ];
        for (error, expected) in cases {
            assert_eq!(status_of(&error), expected, "{}", error.kind());
        }
    }

    /// A probe fault decides whether an install is even worth attempting, and
    /// only one of the four is.
    #[test]
    fn only_a_missing_engine_leads_to_an_install() {
        let fault = |fault| EngineFault {
            fault,
            what: "what".into(),
            fix: "fix".into(),
        };
        assert!(refusal("Ubuntu", &fault(Fault::NoEngine)).is_none());
        for (kind, expected) in [
            (Fault::NoWsl, Status::NoWsl),
            (Fault::NoDistro, Status::NoDistro),
            (Fault::EngineFailed, Status::EngineUnusable),
        ] {
            let report = refusal("Ubuntu", &fault(kind)).expect("refused");
            assert_eq!(report.status, expected);
            assert!(report.message.contains("what — fix"), "{}", report.message);
        }
    }

    /// `--json` is an interface: the installer does not use it, but a script
    /// that does must find the same keys every release.
    #[test]
    fn the_json_report_carries_the_whole_answer() {
        let mut report = Report::new(Status::Installed, "Ubuntu", "engine installed");
        report.path = Some("/home/spider/.local/bin/entangled".into());
        report.version = Some("0.2.137".into());
        let json: serde_json::Value = serde_json::from_str(&report.json()).expect("valid JSON");
        assert_eq!(json["ok"], true);
        assert_eq!(json["code"], 0);
        assert_eq!(json["status"], "installed");
        assert_eq!(json["distro"], "Ubuntu");
        assert_eq!(json["path"], "/home/spider/.local/bin/entangled");
        assert_eq!(json["version"], "0.2.137");
        assert_eq!(json["cached"], false);

        let failed = Report::new(Status::NoWsl, "Ubuntu", "no WSL here");
        let json: serde_json::Value = serde_json::from_str(&failed.json()).expect("valid JSON");
        assert_eq!(json["ok"], false);
        assert_eq!(json["code"], 2);
        assert!(json["path"].is_null());
        // The prose line says what happened *and* that nothing was installed.
        assert!(
            failed.line().contains("was not installed"),
            "{}",
            failed.line()
        );
    }

    /// The default log only appears in the mode the installer uses, and
    /// `--log -` is how a script says "nowhere".
    #[test]
    fn the_log_file_is_opt_in_except_when_quiet() {
        let args = |quiet: bool, log: Option<&str>| InstallEngineArgs {
            distro: "Ubuntu".into(),
            quiet,
            json: false,
            force: false,
            timeout: 1,
            log: log.map(PathBuf::from),
        };
        assert!(log_path(&args(false, None)).is_none());
        assert_eq!(
            log_path(&args(false, Some("C:/tmp/x.log"))),
            Some(PathBuf::from("C:/tmp/x.log"))
        );
        assert!(log_path(&args(true, Some("-"))).is_none());
        // Under --quiet with no --log it is the cache default, when this host
        // has a cache at all.
        if crate::paths::cache_root().is_ok() {
            let path = log_path(&args(true, None)).expect("a default log");
            assert!(path.ends_with("engine/install-engine.log"), "{path:?}");
        }
    }

    /// On a Linux host the command refuses immediately and says why — there is
    /// no `wsl.exe` to talk to, and the engine is the program being run.
    #[cfg(not(windows))]
    #[test]
    fn a_linux_host_is_told_the_command_is_not_for_it() {
        let report = install_engine(&InstallEngineArgs {
            distro: String::new(),
            quiet: true,
            json: false,
            force: false,
            timeout: 5,
            log: Some(PathBuf::from("-")),
        });
        assert_eq!(report.status, Status::NotWindows);
        assert_eq!(report.status.code(), 9);
        assert_eq!(report.distro, wsl::DEFAULT_DISTRO);
        assert!(report.message.contains("wsl.exe"), "{}", report.message);
    }
}
