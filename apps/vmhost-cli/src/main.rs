//! `vmhost` — the CLI control surface for the VMHost VMM.

mod disk;
mod doctor;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "vmhost", version, about = "A small VMM for Linux x86-64 hosts")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Download and verify installer media (Debian stable).
    Fetch(FetchArgs),
    /// Manage virtual disks.
    #[command(subcommand)]
    Disk(DiskCommand),
    /// Boot the installer against a target disk.
    Install(InstallArgs),
    /// Run a VM from a TOML configuration file.
    Run { config: PathBuf },
    /// Check host prerequisites (KVM, capabilities, graphics backend).
    Doctor,
}

#[derive(Args)]
struct FetchArgs {
    /// Distribution to fetch (only "debian").
    distro: String,
    #[arg(long, default_value = "stable")]
    channel: String,
    #[arg(long, default_value = "amd64")]
    arch: String,
    #[arg(long, default_value = "gtk-netboot")]
    variant: String,
}

#[derive(Subcommand)]
enum DiskCommand {
    /// Create an empty sparse RAW disk image.
    Create {
        path: PathBuf,
        /// Size such as 32G, 512M or plain bytes.
        #[arg(long)]
        size: String,
    },
}

#[derive(Args)]
struct InstallArgs {
    /// Distribution to install (only "debian").
    distro: String,
    #[arg(long)]
    disk: PathBuf,
    #[arg(long, default_value = "gtk-netboot")]
    variant: String,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    match cli.command {
        Command::Disk(DiskCommand::Create { path, size }) => {
            let bytes = disk::parse_size(&size).map_err(|e| e.to_string())?;
            disk::create_raw(&path, bytes).map_err(|e| e.to_string())?;
            println!("created {} ({} bytes, sparse)", path.display(), bytes);
            Ok(())
        }
        Command::Doctor => doctor::run(),
        Command::Fetch(args) => Err(format!(
            "fetch {} --channel {} --variant {} is not implemented yet (backlog EPIC 6)",
            args.distro, args.channel, args.variant
        )),
        Command::Install(args) => Err(format!(
            "install {} --disk {} is not implemented yet (backlog EPIC 10)",
            args.distro,
            args.disk.display()
        )),
        Command::Run { config } => {
            let text = std::fs::read_to_string(&config)
                .map_err(|e| format!("cannot read {}: {e}", config.display()))?;
            let cfg = control_api::VmConfig::from_toml(&text).map_err(|e| e.to_string())?;
            Err(format!(
                "config '{}' is valid, but running VMs is not implemented yet (backlog EPIC 1/2)",
                cfg.name
            ))
        }
    }
}
