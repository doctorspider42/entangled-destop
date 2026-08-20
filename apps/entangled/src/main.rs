//! `entangled` — the CLI control surface for the Entangled Desktop VMM.

mod disk;
mod doctor;
mod fetch;
/// `install` and `run` exist wherever a hypervisor backend does (KVM or WHP);
/// any other OS still gets config validation and a typed refusal. The installer
/// itself is portable — it drives the same `run_vm` both hosts share, and every
/// file it produces (preseed cpio, NoCloud seed, profile) is logic over bytes.
#[cfg(any(target_os = "linux", windows))]
mod install;
#[cfg(any(target_os = "linux", windows))]
mod install_ubuntu;
mod paths;
#[cfg(any(target_os = "linux", windows))]
mod run_vm;
#[cfg(any(target_os = "linux", windows))]
mod seed;

/// The isolated-renderer helper (ADR-0004 GPU-012). Linux-only, like the
/// renderer it hosts.
#[cfg(target_os = "linux")]
mod gpu_renderer {
    /// Loads virglrenderer and serves the renderer protocol on stdin until the
    /// VMM hangs up.
    ///
    /// Failing here is how a `virgl = true` profile finds out it cannot have
    /// 3D: the client turns this message into the `entangled run` error, so
    /// nothing ever silently falls back to software GL (ADR-0004 §7).
    pub fn serve() -> Result<(), String> {
        let renderer = virtio_gpu::virgl::VirglRenderer::load()
            .map_err(|e| format!("cannot start the 3D renderer: {e}"))?;
        virtio_gpu::remote::serve_stdin(Box::new(renderer))
            .map_err(|e| format!("the 3D renderer session ended badly: {e}"))
    }
}

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

/// The version stamped into this build: the release pipeline's
/// `ENTANGLED_VERSION` when set, the workspace version otherwise (build.rs).
const VERSION: &str = env!("ENTANGLED_VERSION");

/// `install --network`'s default: a host TAP on Linux (which is what every
/// existing script and profile expects), the in-process user-mode NAT on
/// Windows, which has no TAP and no GPL-free driver that could give it one
/// (ADR-0002). Declared here rather than in `install` because clap needs it on
/// every host, including one with no hypervisor backend at all.
pub const DEFAULT_NETWORK: &str = if cfg!(target_os = "linux") {
    "tap"
} else {
    "usernet"
};

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

/// One of these is built once per process, by `Cli::parse`, and then matched on
/// and dropped. `InstallArgs` is the biggest by some way (twelve flags, most of
/// them owned strings and paths) and Windows clippy notices the spread —
/// boxing it is the usual fix, but clap's derive cannot take `Box<T>` as a
/// variant field, and a heap allocation per process start would buy nothing
/// anyway. Same reasoning as `entangled_manager::app::Modal`.
#[allow(clippy::large_enum_variant)]
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
        /// Read lifecycle commands from stdin, one per line: `pause`,
        /// `resume`, `reset`, `type <text>`, `status` (ADR-0005).
        ///
        /// For a program driving this VM — `entangled-manager` uses it for the
        /// Pause and Restart buttons. The window's Ctrl+Alt+P and Ctrl+Alt+R
        /// do the same thing for a person, and need no flag.
        #[arg(long)]
        control_stdin: bool,
    },
    /// Check host prerequisites (KVM, capabilities, graphics backend).
    Doctor,
    /// Serve the 3D renderer protocol on stdin (ADR-0004 GPU-012).
    ///
    /// Not for interactive use: `entangled run` starts this on itself so that
    /// virglrenderer — and the host GL driver it loads — lives in a process
    /// whose crash degrades the VM to 2D instead of killing it. Hidden,
    /// because a human typing it gets a process that reads a binary protocol
    /// from a terminal.
    #[command(hide = true)]
    GpuRenderer,
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
    /// Report sizes (apparent vs on-disk), partition table, filesystems and
    /// the .nvram sidecar of a disk image.
    Inspect {
        path: PathBuf,
        /// Machine-readable JSON instead of the human rendering.
        #[arg(long)]
        json: bool,
    },
    /// Grow a disk image (sparse). Shrinking is refused — it would destroy
    /// guest data at the end of the image.
    Resize {
        path: PathBuf,
        /// New size such as 48G; must be at least the current size.
        #[arg(long)]
        size: String,
    },
    /// Remove a disk image and its .nvram sidecar. Refuses while a VM profile
    /// (next to the disk, or in the manager's VM directory) references it.
    Rm {
        path: PathBuf,
        /// Remove even while profiles still reference the disk.
        #[arg(long)]
        force: bool,
    },
    /// Move a disk image (and its .nvram sidecar) to another directory or
    /// drive: sparse-preserving, verified before the source is deleted, and
    /// referencing profiles are updated.
    Move {
        path: PathBuf,
        /// Destination directory (created if missing).
        #[arg(long)]
        to: PathBuf,
    },
}

#[derive(Args)]
pub struct InstallArgs {
    /// Distribution to install: "debian" (d-i, direct kernel boot) or "ubuntu"
    /// (live-server ISO through UEFI, unattended autoinstall).
    pub distro: String,
    /// Target RAW disk image; created if missing. Defaults to
    /// <vm dir>/<name>.raw, where the VM directory is the manager's
    /// (~/entangled-vms, %USERPROFILE%\entangled-vms on Windows, or whatever
    /// its manager.toml says) so both surfaces list the same machines.
    #[arg(long)]
    pub disk: Option<PathBuf>,
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
    /// Host TAP interface (see scripts/setup-tap.sh), for --network tap.
    #[arg(long, default_value = "entangled0")]
    pub interface: String,
    /// Installer network: "tap" (a host interface, Linux only), "usernet"
    /// (user-mode NAT inside this process — no host setup, no administrator) or
    /// "none" (offline; Ubuntu installs offline anyway). Defaults to tap on
    /// Linux and usernet on Windows, which has no TAP.
    #[arg(long, default_value = DEFAULT_NETWORK)]
    pub network: String,
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
        Command::Disk(command) => match command {
            DiskCommand::Create { path, size } => disk::create(&path, &size),
            DiskCommand::Inspect { path, json } => disk::inspect(&path, json),
            DiskCommand::Resize { path, size } => disk::resize(&path, &size),
            DiskCommand::Rm { path, force } => disk::rm(&path, force),
            DiskCommand::Move { path, to } => disk::mv(&path, &to),
        },
        Command::Doctor => doctor::run(),
        Command::GpuRenderer => {
            #[cfg(target_os = "linux")]
            {
                gpu_renderer::serve()
            }
            #[cfg(not(target_os = "linux"))]
            {
                Err("the isolated 3D renderer is Linux-only for now (ADR-0004 §6)".to_string())
            }
        }
        Command::Fetch(args) => fetch::run(&args),
        Command::Install(args) => {
            #[cfg(any(target_os = "linux", windows))]
            {
                install::run(&args)
            }
            #[cfg(not(any(target_os = "linux", windows)))]
            {
                Err(format!(
                    "install {} needs a Linux host with KVM or a Windows host with the \
                     Windows Hypervisor Platform",
                    args.distro
                ))
            }
        }
        Command::Run {
            config,
            headless,
            cdrom,
            screenshot_after,
            screenshot,
            control_stdin,
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
                run_vm::run_with(cfg, headless, None, shot, control_stdin).map(|_| ())
            }
            #[cfg(not(any(target_os = "linux", windows)))]
            {
                let _ = (headless, screenshot_after, screenshot, control_stdin);
                Err(format!(
                    "config '{}' is valid, but running VMs requires a Linux host with KVM \
                     or a Windows host with the Windows Hypervisor Platform",
                    cfg.name
                ))
            }
        }
    }
}
