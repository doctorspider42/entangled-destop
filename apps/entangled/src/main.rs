//! `entangled` — the CLI control surface for the Entangled Desktop VMM.

mod disk;
/// MBR and ext4 parsing, used only by `install`, which is Linux-only: on Windows
/// the whole module is dead code and `-D warnings` says so.
#[cfg(target_os = "linux")]
mod diskfs;
mod doctor;
mod fetch;
#[cfg(target_os = "linux")]
mod install;
#[cfg(target_os = "linux")]
mod install_ubuntu;
/// The run path exists wherever a hypervisor backend does (KVM or WHP); any
/// other OS still gets config validation and a typed refusal.
#[cfg(any(target_os = "linux", windows))]
mod run_vm;
/// The cloud-init NoCloud seed builder, used only by `install` (Linux): on
/// Windows the whole module is dead code and `-D warnings` says so.
#[cfg(target_os = "linux")]
mod seed;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

/// The version stamped into this build: the release pipeline's
/// `ENTANGLED_VERSION` when set, the workspace version otherwise (build.rs).
const VERSION: &str = env!("ENTANGLED_VERSION");

#[derive(Parser)]
#[command(
    name = "entangled",
    version = VERSION,
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
        /// Attach an ISO read-only as the last virtio-blk device and let the
        /// firmware boot it (uefi mode only). Overrides the profile's [cdrom].
        #[arg(long)]
        cdrom: Option<PathBuf>,
        /// Debug: write a PNG of the scanout N seconds after the VM starts
        /// (best effort — nothing is written if the VM stops first).
        #[arg(long, value_name = "SECS")]
        screenshot_after: Option<u64>,
        /// Where --screenshot-after writes its PNG. Defaults to
        /// entangled-screenshot-<name>.png in the working directory.
        #[arg(long, requires = "screenshot_after")]
        screenshot: Option<PathBuf>,
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
    /// Distribution to install: "debian" (d-i, direct kernel boot) or "ubuntu"
    /// (live-server ISO through UEFI, unattended autoinstall).
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
    /// Custom preseed file appended to the installer initrd (Debian only).
    #[arg(long, conflicts_with = "auto")]
    pub preseed: Option<PathBuf>,
    /// Custom autoinstall configuration for Ubuntu: a `#cloud-config` document
    /// whose `autoinstall:` key holds subiquity's directives. Placed on the
    /// NoCloud seed volume in place of the built-in profile
    /// (assets/autoinstall/ubuntu-server.yaml).
    #[arg(long, conflicts_with = "preseed")]
    pub autoinstall: Option<PathBuf>,
    /// Installer ISO (Ubuntu only). Defaults to the newest release verified into
    /// the cache by scripts/fetch-ubuntu-iso.sh.
    #[arg(long)]
    pub iso: Option<PathBuf>,
    /// UEFI firmware image (Ubuntu only). Defaults to
    /// artifacts/firmware/CLOUDHV.fd.
    #[arg(long)]
    pub firmware: Option<PathBuf>,
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
        Command::Run {
            config,
            headless,
            cdrom,
            screenshot_after,
            screenshot,
        } => {
            let text = std::fs::read_to_string(&config)
                .map_err(|e| format!("cannot read {}: {e}", config.display()))?;
            let mut cfg = control_api::VmConfig::from_toml(&text).map_err(|e| e.to_string())?;
            if let Some(iso) = cdrom {
                if !iso.is_file() {
                    return Err(format!("--cdrom {}: not a file", iso.display()));
                }
                cfg.set_cdrom(iso).map_err(|e| e.to_string())?;
            }
            #[cfg(any(target_os = "linux", windows))]
            {
                let shot = screenshot_after.map(|secs| run_vm::ScreenshotRequest {
                    after: std::time::Duration::from_secs(secs),
                    path: screenshot.unwrap_or_else(|| {
                        PathBuf::from(format!("entangled-screenshot-{}.png", cfg.name))
                    }),
                });
                run_vm::run(cfg, headless, shot)
            }
            #[cfg(not(any(target_os = "linux", windows)))]
            {
                let _ = (headless, screenshot_after, screenshot);
                Err(format!(
                    "config '{}' is valid, but running VMs requires a Linux host with KVM \
                     or a Windows host with the Windows Hypervisor Platform",
                    cfg.name
                ))
            }
        }
    }
}
