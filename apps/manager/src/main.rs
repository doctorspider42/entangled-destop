#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
//! `entangled-manager` — the native desktop manager for Entangled Desktop VMs
//! (backlog EPIC 16).
//!
//! Release builds on Windows are GUI-subsystem binaries: starting the manager
//! from Explorer, the Start menu or the installer's shortcut must not park a
//! console window behind it. Terminal use still works — see [`console`] for how
//! the standard handles are recovered, and `init_tracing` for where the log
//! goes when there is no terminal to write to. Debug builds keep the console
//! subsystem so `cargo run` behaves the way developers expect.
//!
//! The manager owns no VMM code at all: it discovers VM profiles on disk,
//! reads them through `control-api`, and drives the `entangled` CLI as child
//! processes (`install`, `run`). Everything long-running lives on a worker
//! thread, so the egui frame loop never blocks.

mod app;
mod backend;
mod console;
mod diagnose;
mod discovery;
mod editor;
mod hostcheck;
mod launcher;
mod logo;
mod metrics;
mod mock;
mod picker;
mod process;
mod settings;
mod snapshots;
mod theme;
mod ui;
mod update;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

/// The version stamped into this build: the release pipeline's
/// `ENTANGLED_VERSION` when set, the workspace version otherwise (build.rs).
pub const VERSION: &str = env!("ENTANGLED_VERSION");

#[derive(Parser)]
#[command(
    name = "entangled-manager",
    version = VERSION,
    about = "Entangled Desktop — native VM manager"
)]
struct Cli {
    /// Override the VM directory for this run (does not touch the saved
    /// settings).
    #[arg(long)]
    vm_dir: Option<PathBuf>,
    /// Override the `entangled` binary for this run.
    #[arg(long)]
    entangled: Option<PathBuf>,
    /// Development aid: render a few frames, save a PNG screenshot and exit.
    #[arg(long)]
    screenshot: Option<PathBuf>,
    /// With --screenshot: which surface to capture.
    #[arg(long, default_value = "main")]
    screenshot_view: ScreenshotView,
    /// Run the UI against deterministic in-memory demo data. No VM directory,
    /// metrics sampler or entangled child process is used.
    #[arg(long)]
    mock: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum ScreenshotView {
    Main,
    Wizard,
    Settings,
    Disks,
    /// Saved sessions, and the two confirmations that throw one away — from
    /// the Snapshots list, and from a suspended machine's own card.
    Snapshots,
    SnapshotDelete,
    SnapshotDiscard,
    /// The machine editor, on each of its sections — one surface per section,
    /// because that is how the panels are actually reviewed.
    Editor,
    EditorBoot,
    EditorNetwork,
    EditorStorage,
    Diagnostics,
}

fn main() -> ExitCode {
    // Before clap: a usage error or `--version` must still reach the terminal
    // that started us, even though a GUI-subsystem process begins with none.
    let attach = console::attach_parent();
    init_tracing(attach);

    let cli = Cli::parse();
    match app::launch(app::Startup {
        vm_dir: cli.vm_dir,
        entangled: cli.entangled,
        screenshot: cli.screenshot,
        screenshot_view: cli.screenshot_view,
        mock: cli.mock,
    }) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Sends `tracing` output where a human can find it.
///
/// With a terminal, stderr as before. Without one — the Explorer launch — the
/// lines would vanish, so they go to `manager.log` beside `manager.toml`
/// instead. The file is truncated at startup rather than rotated: it exists to
/// answer "why did the window misbehave just now", and one session's worth is
/// what makes that readable. If the log cannot be opened the manager still
/// starts; losing diagnostics must never cost the user their UI.
fn init_tracing(attach: console::Attach) {
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());

    if attach.has_console() {
        tracing_subscriber::fmt().with_env_filter(filter).init();
        return;
    }

    match log_file() {
        Some(file) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(LogFile(file))
            .init(),
        None => tracing_subscriber::fmt().with_env_filter(filter).init(),
    }
}

fn log_file() -> Option<std::fs::File> {
    let path = settings::config_path().ok()?.with_file_name("manager.log");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok()?;
    }
    std::fs::File::create(path).ok()
}

/// `&File` is `Write`, which is all a `MakeWriter` owes the subscriber; the
/// file's own append offset serialises the writes.
struct LogFile(std::fs::File);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogFile {
    type Writer = &'a std::fs::File;

    fn make_writer(&'a self) -> Self::Writer {
        &self.0
    }
}
