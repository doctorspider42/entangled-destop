//! `entangled-manager` — the native desktop manager for Entangled Desktop VMs
//! (backlog EPIC 16).
//!
//! The manager owns no VMM code at all: it discovers VM profiles on disk,
//! reads them through `control-api`, and drives the `entangled` CLI as child
//! processes (`install`, `run`). Everything long-running lives on a worker
//! thread, so the egui frame loop never blocks.

mod app;
mod diagnose;
mod discovery;
mod editor;
mod launcher;
mod logo;
mod metrics;
mod mock;
mod process;
mod settings;
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
    Editor,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

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
