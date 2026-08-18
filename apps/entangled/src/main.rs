//! `entangled` — the CLI control surface for the Entangled Desktop VMM.

mod disk;
mod diskfs;
mod doctor;
mod fetch;
#[cfg(target_os = "linux")]
mod install;
#[cfg(target_os = "linux")]
mod run_vm;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "entangled",
    version,
    about = "Entangled Desktop — a small VMM for Linux x86-64 hosts"
)]
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
    Run {
        config: PathBuf,
        /// Do not open a window; the VM still runs with an off-screen
        /// scanout (serial console remains on stdout).
        #[arg(long)]
        headless: bool,
    },
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
pub struct InstallArgs {
    /// Distribution to install (only "debian").
    pub distro: String,
    /// Target RAW disk image; created if missing.
    #[arg(long)]
    pub disk: PathBuf,
    #[arg(long, default_value = "gtk-netboot")]
    pub variant: String,
    /// Fully automated installation with the built-in Weston test profile
    /// (assets/preseed/auto-weston.cfg).
    #[arg(long)]
    pub auto: bool,
    /// Custom preseed file appended to the installer initrd.
    #[arg(long, conflicts_with = "auto")]
    pub preseed: Option<PathBuf>,
    /// Size for a newly created disk (e.g. 16G).
    #[arg(long, default_value = "16G")]
    pub size: String,
    /// Installer VM memory in MiB.
    #[arg(long, default_value_t = 1536)]
    pub memory_mib: u64,
    /// Host TAP interface (see scripts/setup-tap.sh).
    #[arg(long, default_value = "entangled0")]
    pub interface: String,
    /// VM/profile name (defaults to the disk file stem).
    #[arg(long)]
    pub name: Option<String>,
    /// Run without a window (serial console only).
    #[arg(long)]
    pub headless: bool,
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
        Command::Install(args) => {
            #[cfg(target_os = "linux")]
            {
                install::run(&args)
            }
            #[cfg(not(target_os = "linux"))]
            {
                Err(format!(
                    "install {} --disk {} requires a Linux host with KVM (on Windows use WSL2)",
                    args.distro,
                    args.disk.display()
                ))
            }
        }
        Command::Run { config, headless } => {
            let text = std::fs::read_to_string(&config)
                .map_err(|e| format!("cannot read {}: {e}", config.display()))?;
            let cfg = control_api::VmConfig::from_toml(&text).map_err(|e| e.to_string())?;
            #[cfg(target_os = "linux")]
            {
                run_vm::run(cfg, headless)
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = headless;
                Err(format!(
                    "config '{}' is valid, but running VMs requires a Linux host with KVM \
                     (on Windows use WSL2)",
                    cfg.name
                ))
            }
        }
    }
}
