//! `vmhost` — the CLI control surface for the VMHost VMM.

mod disk;
mod doctor;
mod fetch;
#[cfg(target_os = "linux")]
mod run_vm;

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
pub struct FetchArgs {
    /// Distribution to fetch (only "debian").
    distro: String,
    #[arg(long, default_value = "stable")]
    channel: String,
    #[arg(long, default_value = "amd64")]
    arch: String,
    /// One of text-netboot, gtk-netboot, netinst-iso.
    #[arg(long, default_value = "gtk-netboot")]
    variant: String,
    /// Re-check the signed checksum file even when the cache already holds a
    /// verified copy (this is how a new point release is picked up).
    #[arg(long)]
    refresh: bool,
    /// Never access the network: use the verified cache or fail.
    #[arg(long, conflicts_with = "refresh")]
    offline: bool,
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
        Command::Fetch(args) => fetch::run(&args),
        Command::Install(args) => Err(format!(
            "install {} --disk {} is not implemented yet (backlog EPIC 10)",
            args.distro,
            args.disk.display()
        )),
        Command::Run { config } => {
            let text = std::fs::read_to_string(&config)
                .map_err(|e| format!("cannot read {}: {e}", config.display()))?;
            let cfg = control_api::VmConfig::from_toml(&text).map_err(|e| e.to_string())?;
            #[cfg(target_os = "linux")]
            {
                run_vm::run(cfg)
            }
            #[cfg(not(target_os = "linux"))]
            {
                Err(format!(
                    "config '{}' is valid, but running VMs requires a Linux host with KVM \
                     (on Windows use WSL2)",
                    cfg.name
                ))
            }
        }
    }
}
