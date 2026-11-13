//! `entangled install windows`: an unattended Windows 10/11 installation from
//! an untouched Microsoft ISO (ADR-0008 §4).
//!
//! The Windows ISO goes on the AHCI controller as it is, so Windows PE reads
//! it with its in-box driver. Beside it goes a small answer disc this command
//! writes: `Autounattend.xml` at its root, which Setup's implicit search
//! finds, and the virtio-win drivers under `$WinPEDriver$`, which Setup loads
//! into Windows PE — that is what makes the virtio disk visible at all — and
//! installs into the new system. Nothing is remastered and no host tool runs.
//!
//! What it reads off the two ISOs, through `disk_image::optical`:
//!
//! * the edition list of `sources/install.wim` (or `.esd`): the XML in the WIM
//!   header names every image's index, edition and build, so the answer file
//!   can pick one by index and the drivers can match Windows 10 or 11;
//! * the drivers for that version from the virtio-win ISO
//!   (`<driver>/<w10|w11>/amd64`).
//!
//! The run ends when the guest powers off: the answer file's first logon
//! turns hibernation off and shuts down. Then the profile that boots the
//! installed system — the same machine without the two discs — is written.

use std::io::Write;
use std::path::{Path, PathBuf};

use control_api::{
    BootMode, BootSection, DiskSection, DisplayAdapter, DisplaySection, OpticalSection,
    VirtioTransport, VmConfig,
};
use disk_image::optical::{self, iso9660::IsoBuilder, OpticalImage};

use crate::run_vm::{self, Automation};
use crate::{install, InstallArgs};

/// The answer file, compiled in so the command works from anywhere.
const TEMPLATE: &str = include_str!("../../../assets/unattend/windows.xml");
/// The first-logon setup script, run from the answer disc (see the template).
const FIRST_LOGON: &str = include_str!("../../../assets/unattend/first-logon.cmd");
/// The startup task that keeps the 3D function on MSI (see the script).
const GPU3D_MSI: &str = include_str!("../../../assets/unattend/gpu3d-msi.ps1");
/// The logon task that puts Edge's WebGL on Vulkan (see the script).
const GPU3D_EDGE: &str = include_str!("../../../assets/unattend/gpu3d-edge.ps1");
/// Keeps our own render-only driver bound to every instance of its function
/// (ADR-0009).
const OWNGPU_BIND: &str = include_str!("../../../assets/unattend/owngpu-bind.ps1");
const WDDM_ON: &str = include_str!("../../../assets/unattend/wddm-on.cmd");
const WDDM_OFF: &str = include_str!("../../../assets/unattend/wddm-off.cmd");

/// `--gpu3d auto`: download the pinned package instead of naming a folder.
pub const GPU3D_AUTO: &str = "auto";

/// The WIM header's magic.
const WIM_MAGIC: &[u8; 8] = b"MSWIM\0\0\0";
/// Where the WIM header keeps the resource header of its XML data.
const WIM_XML_RESHDR: usize = 0x48;
/// The resource-header flag of a compressed resource.
const WIM_RESHDR_COMPRESSED: u8 = 0x04;
/// Largest WIM XML read. Microsoft's multi-edition ESDs have ~20 KiB.
const MAX_WIM_XML: u64 = 16 << 20;
/// The `<ARCH>` of an x86-64 image.
const WIM_ARCH_AMD64: u32 = 9;
/// First build of Windows 11.
const WINDOWS_11_BUILD: u32 = 22000;

/// Largest single driver file copied off the virtio-win ISO.
const MAX_DRIVER_FILE: u64 = 64 << 20;
/// The virtio-win drivers the answer disc carries, and whether the install
/// cannot work without them: the disk's and the network's are what Setup and
/// the first boot need; the rest make the installed system whole —
/// `viogpudo` among them, which takes the virtio-vga display over from
/// `BasicDisplay` and lets the desktop follow the window.
const DRIVERS: &[(&str, bool)] = &[
    ("viostor", true),
    ("NetKVM", true),
    ("viogpudo", false),
    ("vioinput", false),
    ("Balloon", false),
    ("vioserial", false),
    ("viorng", false),
    // virtio-fs shared folders (`[[share]] virtiofs = true`): the driver is
    // ready, and WinFsp plus virtio-win's virtiofs service make the mount.
    ("viofs", false),
];

/// Smallest disk Windows installs onto (Microsoft says 32 GB for 10 and 64
/// for 11; Setup itself refuses much below 20).
const MIN_DISK_BYTES: u64 = 32 << 30;
/// The disk when `--size` is left at its Linux default. Sparse, so it costs
/// what Windows writes.
const DEFAULT_DISK_BYTES: u64 = 64 << 30;
/// Memory below which Setup refuses (2 GiB for 64-bit).
const MIN_MEMORY_MIB: u64 = 2048;
/// Memory when `--memory-mib` is left at its Linux default.
const DEFAULT_MEMORY_MIB: u64 = 4096;
/// `--size` and `--memory-mib` as clap fills them in when not given.
const CLAP_DEFAULT_SIZE: &str = "16G";
const CLAP_DEFAULT_MEMORY_MIB: u64 = 1536;

/// What the boot loader on a Windows disc prints on the firmware console
/// (which is ttyS0 as much as the screen) before it times out and boots the
/// next device instead.
const BOOT_PROMPT: &str = "Press any key to boot from CD or DVD";

/// One image in `install.wim`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WimImage {
    pub index: u32,
    pub name: String,
    pub edition: String,
    pub build: u32,
    pub arch: u32,
}

impl WimImage {
    fn is_windows_11(&self) -> bool {
        self.build >= WINDOWS_11_BUILD
    }
}

pub fn run(args: &InstallArgs) -> Result<(), String> {
    if args.auto
        || args.preseed.is_some()
        || args.autoinstall.is_some()
        || args.kickstart.is_some()
        || args.venus
        || args.control_stdin
    {
        return Err(
            "install windows is always unattended, from its own answer file: --auto, \
             --preseed, --autoinstall, --kickstart, --venus and --control-stdin do not apply"
                .into(),
        );
    }
    let plan = prepare(args)?;
    tracing::info!(
        vm = %plan.install.name,
        edition = %plan.image.name,
        build = plan.image.build,
        answers = %plan.answers.display(),
        transcript = %plan.transcript.display(),
        "starting Windows Setup; it powers off when done (20-60 min)"
    );
    let mut script = BootKey::default();
    let report = run_vm::run_with(
        plan.install.clone(),
        Some(Automation {
            script: Box::new(move |log| script.step(log)),
            transcript: Some(plan.transcript.clone()),
        }),
        run_vm::RunOptions {
            headless: args.headless,
            ..Default::default()
        },
    )
    .map_err(|e| format!("installer VM failed: {e}"))?;
    if !report.guest_shutdown {
        return Err(format!(
            "Windows did not power off, so the installation did not finish. The disk \
             {} and the answer disc {} are left as they are; the serial transcript is {}",
            plan.install.disks[0].path.display(),
            plan.answers.display(),
            plan.transcript.display(),
        ));
    }
    write_profile(&plan.profile, &plan.installed)?;
    if let Err(error) = std::fs::remove_file(&plan.answers) {
        tracing::warn!(%error, path = %plan.answers.display(), "could not remove the answer disc");
    }
    tracing::info!(
        profile = %plan.profile.display(),
        "Windows is installed; start it with `entangled run {}`",
        plan.profile.display()
    );
    Ok(())
}

/// Everything decided before the VM starts.
struct Plan {
    /// The machine Setup runs on: the installed one plus both discs.
    install: VmConfig,
    /// The machine the profile describes.
    installed: VmConfig,
    image: WimImage,
    profile: PathBuf,
    answers: PathBuf,
    transcript: PathBuf,
}

fn absolute(path: &Path) -> Result<PathBuf, String> {
    std::path::absolute(path).map_err(|e| e.to_string())
}

/// One of the downloads a Windows guest needs, from the verified cache or
/// its publisher, said aloud because the first one is ~840 MiB.
pub(crate) fn media(media: crate::windows_media::Media) -> Result<PathBuf, String> {
    if let Ok(path) = crate::windows_media::cached_path(media) {
        if !path.is_file() {
            eprintln!(
                "downloading {} (verified against the pinned digest)...",
                media.label()
            );
        }
    }
    crate::windows_media::fetch(media, crate::artifact::FetchOptions::default())
        .map_err(|e| format!("cannot get {}: {e}", media.label()))
}

fn prepare(args: &InstallArgs) -> Result<Plan, String> {
    let iso = absolute(args.iso.as_deref().ok_or(
        "install windows requires --iso with a Windows 10 or 11 x64 installation ISO \
         (Microsoft's own, untouched)",
    )?)?;
    // The drivers and WinFsp come from their publishers, verified against
    // the digests compiled in (`guest/windows/pinned.toml`), unless given.
    let drivers = match args.drivers.as_deref() {
        Some(path) => absolute(path)?,
        None => media(crate::windows_media::Media::VirtioWin)?,
    };
    let winfsp = match (args.winfsp.as_deref(), args.no_shared_folders) {
        (Some(path), _) => Some(absolute(path)?),
        (None, true) => None,
        (None, false) => Some(media(crate::windows_media::Media::WinFsp)?),
    };
    // `--gpu3d auto`: the 3D package this build pins (`entangled fetch gpu3d`).
    let gpu3d = match args.gpu3d.as_deref() {
        Some(path) if path == Path::new(GPU3D_AUTO) => {
            Some(media(crate::windows_media::Media::Gpu3d)?)
        }
        Some(path) => Some(absolute(path)?),
        None => None,
    };
    for (what, path) in [("Windows ISO", &iso), ("virtio-win ISO", &drivers)] {
        if !path.is_file() {
            return Err(format!("the {what} {} was not found", path.display()));
        }
    }
    let name = install::vm_name(args);
    if name.is_empty()
        || name.len() > 63
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err("machine name must be 1..63 letters, digits, '-' or '_'".into());
    }
    let target = absolute(&install::target_disk(args)?)?;
    let profile = target.with_file_name(format!("{name}.toml"));
    let nvram = target.with_file_name(format!("{name}.nvram"));
    let answers = target.with_file_name(format!("{name}-answers.iso"));
    let transcript = target.with_file_name(format!("{name}-install.log"));
    for path in [&profile, &nvram] {
        if path.exists() {
            return Err(format!(
                "{} already exists: this machine has been installed, or an install of it \
                 was interrupted. Remove it (and the disk) to start over",
                path.display()
            ));
        }
    }
    let size = if args.size == CLAP_DEFAULT_SIZE {
        DEFAULT_DISK_BYTES
    } else {
        crate::disk::parse_size(&args.size).map_err(|e| e.to_string())?
    };
    if size < MIN_DISK_BYTES {
        return Err(format!(
            "Windows needs a disk of at least {} GiB; --size {} is smaller",
            MIN_DISK_BYTES >> 30,
            args.size
        ));
    }
    let memory_mib = if args.memory_mib == CLAP_DEFAULT_MEMORY_MIB {
        DEFAULT_MEMORY_MIB
    } else if args.memory_mib < MIN_MEMORY_MIB {
        return Err(format!(
            "Windows Setup needs at least {MIN_MEMORY_MIB} MiB; --memory-mib {} is less",
            args.memory_mib
        ));
    } else {
        args.memory_mib
    };

    let (disc, image) = answer_disc(
        &iso,
        &drivers,
        args.edition.as_deref(),
        &computer_name(&name),
        &timezone(),
        args.product_key
            .as_deref()
            .map(product_key)
            .transpose()?
            .as_deref(),
        Extras {
            gpu3d: gpu3d.as_deref(),
            winfsp: winfsp.as_deref(),
        },
    )?;
    let firmware = crate::firmware::resolve(args.firmware.as_deref()).map_err(|e| e.to_string())?;
    tracing::info!(
        firmware = %firmware.path.display(),
        "Windows needs the firmware's framebuffer driver (QemuVideoDxe): a build of \
         guest/firmware/build-cloudhv.sh from the windows-guests branch, or a release that has it"
    );
    let net = install::net_plan(&args.network, &args.interface)?;
    let installed = VmConfig {
        architecture: Default::default(),
        name: name.clone(),
        memory_mib,
        vcpus: install::installed_vcpus_here(args),
        transport: VirtioTransport::Pci,
        boot: BootSection {
            mode: BootMode::Uefi,
            firmware: Some(absolute(&firmware.path)?),
            nvram: Some(nvram),
            ..Default::default()
        },
        disks: vec![DiskSection {
            path: target.clone(),
            writable: true,
        }],
        cdrom: None,
        optical: vec![],
        network: net.machine_section(&name, &target),
        display: DisplaySection {
            // One adapter for the firmware, Setup and the installed system:
            // `BasicDisplay` draws into its VRAM until `viogpudo` binds it.
            adapter: DisplayAdapter::VirtioVga,
            // Either package drives Venus beside the VGA function; our own
            // (ADR-0009) needs its render-only function for that.
            venus: gpu3d.is_some(),
            render_node: gpu3d.as_deref().is_some_and(is_own_gpu_package),
            refresh_hz: install::installed_refresh_hz_here(args),
            ..Default::default()
        },
        // Sound through Windows' in-box HD Audio driver: virtio-win has no
        // released virtio-snd driver.
        sound: control_api::SoundSection {
            enabled: true,
            device: control_api::SoundDevice::Hda,
            ..Default::default()
        },
        gamepad: Default::default(),
        shares: Vec::new(),
        reclaim_memory: false,
        // Windows reads model-specific MSRs this machine does not have and
        // bugchecks on the #GP (ADR-0008 §3).
        ignore_msrs: true,
        // The Hyper-V reference clock: without it every
        // QueryPerformanceCounter is a PM timer port exit (ADR-0008 §2).
        hyperv_enlightenments: true,
        // The clipboard, through entangled-agent (first-logon.cmd).
        guest_agent: true,
    };
    let mut install = installed.clone();
    // Offline: Setup needs nothing from the network, and with it OOBE
    // fetches "zero-day" updates, restarts in the middle of the unattended
    // passes and stops at "Why did my PC restart?" waiting for a click.
    // The installed machine gets its network back.
    install.network = None;
    install.optical = vec![
        OpticalSection { path: iso },
        OpticalSection {
            path: answers.clone(),
        },
    ];
    // Both through the validator a profile on disk would meet.
    let installed =
        VmConfig::from_toml(&toml::to_string_pretty(&installed).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let install =
        VmConfig::from_toml(&toml::to_string_pretty(&install).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;

    std::fs::write(&answers, disc)
        .map_err(|e| format!("cannot write the answer disc {}: {e}", answers.display()))?;
    if !target.exists() {
        crate::disk::create_raw(&target, size).map_err(|e| e.to_string())?;
    }
    Ok(Plan {
        install,
        installed,
        image,
        profile,
        answers,
        transcript,
    })
}

/// Writes `cfg` to `path`, never over an existing file.
fn write_profile(path: &Path, cfg: &VmConfig) -> Result<(), String> {
    let text = toml::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    file.write_all(text.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// What the answer disc carries beyond Windows itself, all optional.
#[derive(Default, Clone, Copy)]
struct Extras<'a> {
    /// The viogpu3d package folder (`--gpu3d`).
    gpu3d: Option<&'a Path>,
    /// WinFsp's installer (`--winfsp`).
    winfsp: Option<&'a Path>,
}

/// The answer disc's bytes and the image it installs.
fn answer_disc(
    windows_iso: &Path,
    drivers_iso: &Path,
    edition: Option<&str>,
    computer_name: &str,
    timezone: &str,
    product_key: Option<&str>,
    extras: Extras<'_>,
) -> Result<(Vec<u8>, WimImage), String> {
    let Extras { gpu3d, winfsp } = extras;
    let mut windows = optical::open(windows_iso).map_err(|e| e.to_string())?;
    let images =
        wim_images(windows.as_mut()).map_err(|e| format!("{}: {e}", windows_iso.display()))?;
    let image = choose_image(&images, edition)?.clone();
    if image.arch != WIM_ARCH_AMD64 {
        return Err(format!(
            "{} is not an x64 image (WIM architecture {}); this machine is x86-64",
            image.name, image.arch
        ));
    }
    let mut disc = IsoBuilder::new("ENTANGLED");
    disc.add_file(
        "Autounattend.xml",
        answer_file(&image, computer_name, timezone, product_key).into_bytes(),
    )
    .map_err(|e| e.to_string())?;
    disc.add_file("entangled/first-logon.cmd", crlf(FIRST_LOGON))
        .map_err(|e| e.to_string())?;
    match guest_agent() {
        Some(agent) => disc
            .add_file("entangled/entangled-agent.exe", agent)
            .map_err(|e| e.to_string())?,
        None => tracing::warn!(
            "no entangled-agent.exe beside this program (or at {AGENT_ENV}); the guest \
             installs without the shared clipboard"
        ),
    }
    if let Some(dir) = gpu3d {
        add_gpu3d(&mut disc, dir)?;
    }
    if let Some(msi) = winfsp {
        disc.add_file("entangled/winfsp.msi", winfsp_installer(msi)?)
            .map_err(|e| e.to_string())?;
    }
    let mut virtio = optical::open(drivers_iso).map_err(|e| e.to_string())?;
    for (path, data) in driver_files(virtio.as_mut(), image.is_windows_11())
        .map_err(|e| format!("{}: {e}", drivers_iso.display()))?
    {
        disc.add_file(&format!("$WinPEDriver$/{path}"), data)
            .map_err(|e| e.to_string())?;
    }
    Ok((disc.build(), image))
}

/// The 3D package `dir` and the scripts that install it, under `entangled/`
/// — where `first-logon.cmd` looks for them, on the answer disc and on the
/// disc [`gpu3d_disc`] makes for an installed guest alike.
fn add_gpu3d(disc: &mut IsoBuilder, dir: &Path) -> Result<(), String> {
    disc.add_file("entangled/gpu3d-msi.ps1", crlf(GPU3D_MSI))
        .map_err(|e| e.to_string())?;
    disc.add_file("entangled/gpu3d-edge.ps1", crlf(GPU3D_EDGE))
        .map_err(|e| e.to_string())?;
    let (at, files) = if is_own_gpu_package(dir) {
        disc.add_file("entangled/owngpu-bind.ps1", crlf(OWNGPU_BIND))
            .map_err(|e| e.to_string())?;
        // Opt-in (ADR-0010): how a guest turns the WDDM driver on and off.
        disc.add_file("entangled/wddm-on.cmd", crlf(WDDM_ON))
            .map_err(|e| e.to_string())?;
        disc.add_file("entangled/wddm-off.cmd", crlf(WDDM_OFF))
            .map_err(|e| e.to_string())?;
        ("owngpu", own_gpu_files(dir)?)
    } else {
        ("gpu3d", gpu3d_files(dir)?)
    };
    for (path, data) in files {
        disc.add_file(&format!("entangled/{at}/{path}"), data)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// `Install 3D.cmd`, at the root of [`gpu3d_disc`]: asks for administrator
/// rights (UAC) when it lacks them, runs the 3D half of `first-logon.cmd` —
/// the rest of that script finds nothing of its own on this disc and does
/// nothing — and restarts Windows, which test-signing and the driver need.
const INSTALL_3D_CMD: &str = r#"@echo off
rem Entangled: adds 3D graphics to this Windows (ADR-0009). Double-click it.
net session >nul 2>&1
if errorlevel 1 (
  powershell -NoProfile -Command "Start-Process -Verb RunAs -FilePath '%~f0'"
  exit /b
)
echo Installing 3D graphics...
call "%~d0\entangled\first-logon.cmd" %~d0
echo.
echo Done. Windows restarts in 20 seconds to finish (it shows "Test Mode" afterwards).
shutdown /r /t 20 /c "Entangled: finishing 3D graphics"
pause
"#;

/// The disc that adds 3D to an **installed** Windows guest (`entangled
/// enable-3d`): the package and scripts the answer disc would have carried,
/// and `Install 3D.cmd` to run them.
pub(crate) fn gpu3d_disc(dir: &Path) -> Result<Vec<u8>, String> {
    let mut disc = IsoBuilder::new(control_api::enable_3d::DISC_LABEL);
    disc.add_file(control_api::enable_3d::WINDOWS_SCRIPT, crlf(INSTALL_3D_CMD))
        .map_err(|e| e.to_string())?;
    disc.add_file("entangled/first-logon.cmd", crlf(FIRST_LOGON))
        .map_err(|e| e.to_string())?;
    add_gpu3d(&mut disc, dir)?;
    Ok(disc.build())
}

/// The 3D package's files, as the answer disc carries them under
/// `entangled/gpu3d/`: the test certificate, the Vulkan runtime installer
/// (renamed `VulkanRT-Installer.exe`) and everything under `Win10/amd64`
/// except debug symbols.
fn gpu3d_files(dir: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let read = |path: &Path| {
        std::fs::read(path).map_err(|e| format!("--gpu3d: cannot read {}: {e}", path.display()))
    };
    let cert = dir.join("VirtIOTestCert.cer");
    let drivers = dir.join("Win10").join("amd64");
    if !cert.is_file() || !drivers.join("viogpu3d.inf").is_file() {
        return Err(format!(
            "--gpu3d: {} is not the viogpu3d package: it needs VirtIOTestCert.cer and \
             Win10\\amd64\\viogpu3d.inf",
            dir.display()
        ));
    }
    let runtime = find_file(dir, |name| {
        name.starts_with("vulkanrt") && name.ends_with(".exe")
    })?
    .ok_or_else(|| {
        format!(
            "--gpu3d: no Vulkan runtime installer (VulkanRT-*.exe from LunarG) in {}",
            dir.display()
        )
    })?;
    let mut files = vec![
        ("VirtIOTestCert.cer".to_owned(), read(&cert)?),
        ("VulkanRT-Installer.exe".to_owned(), read(&runtime)?),
    ];
    let mut pending = vec![(drivers, "Win10/amd64".to_owned())];
    while let Some((at, prefix)) = pending.pop() {
        let entries = std::fs::read_dir(&at)
            .map_err(|e| format!("--gpu3d: cannot list {}: {e}", at.display()))?;
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
                continue;
            };
            if path.is_dir() {
                pending.push((path, format!("{prefix}/{name}")));
            } else if !name.to_ascii_lowercase().ends_with(".pdb") {
                files.push((format!("{prefix}/{name}"), read(&path)?));
            }
        }
    }
    files.sort();
    Ok(files)
}

/// Whether `--gpu3d` names Entangled's own package (ADR-0009) rather than
/// Yttrium's.
pub(crate) fn is_own_gpu_package(dir: &Path) -> bool {
    dir.join("entangled-gpu.inf").is_file()
}

/// The first file in `dir` whose lower-cased name `wanted` accepts.
fn find_file(dir: &Path, wanted: impl Fn(&str) -> bool) -> Result<Option<PathBuf>, String> {
    Ok(std::fs::read_dir(dir)
        .map_err(|e| format!("--gpu3d: cannot list {}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| wanted(&name.to_ascii_lowercase()))
        }))
}

/// Our own package's files (ADR-0009), as the answer disc carries them under
/// `entangled/owngpu/`: the kernel driver, a test-signed build's certificate
/// (renamed `entangled-gpu.cer`, absent for a Microsoft-signed one), the tool that installs it over the Microsoft-signed
/// display driver, Mesa's Venus ICD and the Vulkan runtime installer.
fn own_gpu_files(dir: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let read = |path: &Path| {
        std::fs::read(path).map_err(|e| format!("--gpu3d: cannot read {}: {e}", path.display()))
    };
    let missing = |what: &str| format!("--gpu3d: {} has no {what}", dir.display());
    let runtime = find_file(dir, |name| {
        name.starts_with("vulkanrt") && name.ends_with(".exe")
    })?
    .ok_or_else(|| missing("Vulkan runtime installer (VulkanRT-*.exe from LunarG)"))?;
    let mut files = vec![("VulkanRT-Installer.exe".to_owned(), read(&runtime)?)];
    // A test-signed build comes with its certificate, and the guest runs in
    // test-signing mode; one Microsoft signed (attestation, ADR-0009 phase F)
    // comes without, and installs as any driver does.
    if let Some(cert) = find_file(dir, |name| name.ends_with(".cer"))? {
        files.push(("entangled-gpu.cer".to_owned(), read(&cert)?));
    }
    for name in [
        "entangled-gpu.inf",
        "entangled-gpu.sys",
        "entangled-gpu.cat",
        "gpu-test.exe",
        "vulkan_virtio.dll",
    ] {
        let path = dir.join(name);
        if !path.is_file() {
            return Err(missing(name));
        }
        files.push((name.to_owned(), read(&path)?));
    }
    // Optional: our build of the display-only driver, with a vsync (ADR-0008,
    // the vsync amendment). A package from before it has none of the three
    // and the display stays viogpudo's; one with only some is broken.
    let display: Vec<_> = DISPLAY_DRIVER_FILES
        .iter()
        .map(|name| (name, dir.join(name)))
        .collect();
    if display.iter().any(|(_, path)| path.is_file()) {
        for (name, path) in display {
            if !path.is_file() {
                return Err(missing(name));
            }
            files.push(((*name).to_owned(), read(&path)?));
        }
    }
    // Optional, and opt-in inside the guest: the WDDM driver for the display
    // function and its Direct3D user-mode driver (ADR-0010). All four or none.
    let wddm: Vec<_> = WDDM_DRIVER_FILES
        .iter()
        .map(|name| (name, dir.join(name)))
        .collect();
    if wddm.iter().any(|(_, path)| path.is_file()) {
        for (name, path) in wddm {
            if !path.is_file() {
                return Err(missing(name));
            }
            files.push(((*name).to_owned(), read(&path)?));
        }
        let d3d9: Vec<_> = WDDM_D3D9_FILES
            .iter()
            .map(|name| (name, dir.join(name)))
            .collect();
        if d3d9.iter().any(|(_, path)| path.is_file()) {
            for (name, path) in d3d9 {
                if !path.is_file() {
                    return Err(missing(name));
                }
                files.push(((*name).to_owned(), read(&path)?));
            }
        }
    }
    // Optional: Mesa's OpenGL over Vulkan (Zink) and DXVK's Direct3D 9-11,
    // each a folder of DLLs an application takes from beside itself. The
    // published package (windows-gpu.yml) is flat, a release being a list of
    // files: its DLLs sit beside the driver and go into their folders here.
    for (folder, wanted) in OPTIONAL_GPU_FOLDERS {
        let mut at = dir.join(folder);
        let flat = !at.is_dir();
        if flat {
            if !wanted
                .iter()
                .all(|name| dir.join(flat_name(folder, name)).is_file())
            {
                continue;
            }
            at = dir.to_path_buf();
        }
        for name in *wanted {
            let path = if flat {
                at.join(flat_name(folder, name))
            } else {
                at.join(name)
            };
            if !path.is_file() {
                return Err(format!(
                    "--gpu3d: {} has no {name}; it holds {}",
                    at.display(),
                    wanted.join(", ")
                ));
            }
            files.push((format!("{folder}/{name}"), read(&path)?));
        }
    }
    files.sort();
    Ok(files)
}

/// The display driver in our own package (`guest/windows-gpu/display`): all
/// three or none.
const DISPLAY_DRIVER_FILES: &[&str] = &[
    "entangled-display.inf",
    "entangled-display.sys",
    "entangled-display.cat",
];

/// The WDDM driver for the display function and its Direct3D user-mode driver
/// (`guest/windows-gpu/wddm`, `guest/windows-gpu/mesa/build-d3d10.cmd`): all
/// four or none. Copied into the guest, installed there only on request.
/// The Direct3D 9 driver (`guest/windows-gpu/d3d9`, ADR-0011) rides with
/// them once the INF names it: [`WDDM_D3D9_FILES`].
const WDDM_DRIVER_FILES: &[&str] = &[
    "entangled-wddm.inf",
    "entangled-wddm.sys",
    "entangled-wddm.cat",
    "entangled_d3d10.dll",
];

/// The Direct3D 9 user-mode driver of the WDDM driver, for 64-bit and 32-bit
/// programs, and the 32-bit twin of the Direct3D 10 one: all or none, and
/// only beside the WDDM driver. A package from before ADR-0011 has none, and
/// its INF names none.
const WDDM_D3D9_FILES: &[&str] = &[
    "entangled_d3d9.dll",
    "entangled_d3d9_x86.dll",
    "entangled_d3d10_x86.dll",
];

/// The optional folders of our own package and the DLLs each must hold:
/// `opengl` from `guest/windows-gpu/mesa/build-zink.cmd`, `dxvk` from a DXVK
/// release's `x64` (2.3 and 3.1 verified, ADR-0009), and what a 32-bit
/// program takes: `x86` from `guest/windows-gpu/mesa/build-x86.cmd` (the
/// Venus ICD its Vulkan loader finds) and `dxvk/x86` from the DXVK release's
/// `x32`. A 32-bit game otherwise has no graphics driver in the guest at all.
const OPTIONAL_GPU_FOLDERS: &[(&str, &[&str])] = &[
    ("opengl", &["opengl32.dll", "libgallium_wgl.dll"]),
    (
        "dxvk",
        &["d3d9.dll", "d3d10core.dll", "d3d11.dll", "dxgi.dll"],
    ),
    ("x86", &["vulkan_virtio.dll"]),
    (
        "dxvk/x86",
        &["d3d9.dll", "d3d10core.dll", "d3d11.dll", "dxgi.dll"],
    ),
];

/// The name of an optional folder's DLL in the flat, published package: its
/// own, except that a 32-bit one (a folder ending in `x86`) would collide
/// with its 64-bit twin there and is `<stem>_x86.dll`.
fn flat_name(folder: &str, name: &str) -> String {
    match name.strip_suffix(".dll") {
        Some(stem) if folder.ends_with("x86") => format!("{stem}_x86.dll"),
        _ => name.to_owned(),
    }
}

/// WinFsp's installer for the answer disc: an `.msi` of a sane size.
fn winfsp_installer(msi: &Path) -> Result<Vec<u8>, String> {
    let is_msi = msi
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("msi"));
    if !is_msi {
        return Err(format!(
            "--winfsp: {} is not an .msi (WinFsp's installer, winfsp-*.msi)",
            msi.display()
        ));
    }
    let data =
        std::fs::read(msi).map_err(|e| format!("--winfsp: cannot read {}: {e}", msi.display()))?;
    if data.len() as u64 > MAX_DRIVER_FILE {
        return Err(format!(
            "--winfsp: {} is too large for an installer",
            msi.display()
        ));
    }
    Ok(data)
}

/// Where a guest agent build may be named explicitly.
const AGENT_ENV: &str = "ENTANGLED_WINDOWS_AGENT";
/// Largest agent accepted.
const MAX_AGENT: u64 = 32 << 20;

/// The guest agent's bytes: `$ENTANGLED_WINDOWS_AGENT`, else
/// `entangled-agent.exe` beside this program (where the Windows installer
/// and a `cargo build` both put it).
fn guest_agent() -> Option<Vec<u8>> {
    let path = std::env::var_os(AGENT_ENV)
        .map(std::path::PathBuf::from)
        .or_else(|| {
            let exe = std::env::current_exe().ok()?;
            Some(exe.parent()?.join("entangled-agent.exe"))
        })?;
    let len = std::fs::metadata(&path).ok()?.len();
    if len == 0 || len > MAX_AGENT {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    tracing::info!(path = %path.display(), "the guest agent goes on the answer disc");
    Some(bytes)
}

/// `text` with CRLF line ends, whatever the checkout gave it: `cmd` reads a
/// batch file with LF ends wrongly (labels and `goto` misfire).
fn crlf(text: &str) -> Vec<u8> {
    text.replace("\r\n", "\n")
        .replace('\n', "\r\n")
        .into_bytes()
}

/// The images listed in the Windows ISO's `install.wim` or `install.esd`.
fn wim_images(iso: &mut dyn OpticalImage) -> Result<Vec<WimImage>, String> {
    let wim = match iso.find("sources/install.wim").map_err(|e| e.to_string())? {
        Some(wim) => wim,
        None => iso
            .find("sources/install.esd")
            .map_err(|e| e.to_string())?
            .ok_or("no sources/install.wim or install.esd: not a Windows installation ISO")?,
    };
    let mut header = [0u8; 0x60];
    iso.read_at(&wim.file, 0, &mut header)
        .map_err(|e| e.to_string())?;
    if &header[..8] != WIM_MAGIC {
        return Err("the install image is not a WIM".into());
    }
    let reshdr = &header[WIM_XML_RESHDR..WIM_XML_RESHDR + 24];
    let word = |at: usize| u64::from_le_bytes(reshdr[at..at + 8].try_into().unwrap_or([0; 8]));
    let flags = (word(0) >> 56) as u8;
    let size = word(0) & 0x00ff_ffff_ffff_ffff;
    let offset = word(8);
    if flags & WIM_RESHDR_COMPRESSED != 0 {
        return Err("the install image's XML is compressed, which no WIM tool writes".into());
    }
    if !(2..=MAX_WIM_XML).contains(&size) {
        return Err(format!("the install image's XML is {size} bytes"));
    }
    let mut xml = vec![0u8; size as usize];
    iso.read_at(&wim.file, offset, &mut xml)
        .map_err(|e| e.to_string())?;
    let units: Vec<u16> = xml
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let images = parse_wim_xml(&String::from_utf16_lossy(&units));
    if images.is_empty() {
        return Err("the install image lists no images".into());
    }
    Ok(images)
}

/// The `<IMAGE>` elements of a WIM's XML. A string scan, not a parser: the
/// document is DISM's, flat, and only a handful of leaf elements matter.
fn parse_wim_xml(xml: &str) -> Vec<WimImage> {
    let leaf = |block: &str, tag: &str| -> Option<String> {
        let open = format!("<{tag}>");
        let start = block.find(&open)? + open.len();
        let end = block[start..].find(&format!("</{tag}>"))? + start;
        Some(block[start..end].trim().to_string())
    };
    let mut out = Vec::new();
    for block in xml.split("<IMAGE ").skip(1) {
        let block = block.split("</IMAGE>").next().unwrap_or(block);
        let Some(index) = block
            .strip_prefix("INDEX=\"")
            .and_then(|rest| rest.split('"').next())
            .and_then(|n| n.parse().ok())
        else {
            continue;
        };
        let number = |tag| leaf(block, tag).and_then(|v| v.parse().ok()).unwrap_or(0);
        out.push(WimImage {
            index,
            name: leaf(block, "NAME").unwrap_or_default(),
            edition: leaf(block, "EDITIONID").unwrap_or_default(),
            build: number("BUILD"),
            arch: number("ARCH"),
        });
    }
    out
}

/// The image `wanted` names — an index, an edition id (`Professional`) or a
/// name (`Windows 10 Pro`) — or by default Pro, or the only one.
fn choose_image<'a>(images: &'a [WimImage], wanted: Option<&str>) -> Result<&'a WimImage, String> {
    let found = match wanted {
        Some(wanted) => images.iter().find(|i| {
            wanted.parse() == Ok(i.index)
                || i.edition.eq_ignore_ascii_case(wanted)
                || i.name.eq_ignore_ascii_case(wanted)
        }),
        None if images.len() == 1 => images.first(),
        None => images
            .iter()
            .find(|i| i.edition.eq_ignore_ascii_case("Professional")),
    };
    found.ok_or_else(|| {
        let list: Vec<String> = images
            .iter()
            .map(|i| format!("  {}: {} ({})", i.index, i.name, i.edition))
            .collect();
        format!(
            "{}; pass --edition with one of:\n{}",
            match wanted {
                Some(w) => format!("the ISO has no edition '{w}'"),
                None => "the ISO has no Pro edition to default to".into(),
            },
            list.join("\n")
        )
    })
}

/// The drivers for Windows 10 or 11 on the virtio-win ISO, as paths under
/// `$WinPEDriver$` and their bytes.
fn driver_files(
    iso: &mut dyn OpticalImage,
    windows_11: bool,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    let versions: &[&str] = if windows_11 {
        &["w11", "w10"]
    } else {
        &["w10"]
    };
    let mut out = Vec::new();
    for &(driver, required) in DRIVERS {
        let mut dir = None;
        for version in versions {
            if let Some(found) = iso
                .find(&format!("{driver}/{version}/amd64"))
                .map_err(|e| e.to_string())?
            {
                dir = Some(found);
                break;
            }
        }
        let Some(dir) = dir else {
            if required {
                return Err(format!(
                    "no {driver}/{}/amd64: not a virtio-win ISO, or one too old for this Windows",
                    versions[0]
                ));
            }
            tracing::warn!(
                driver,
                "the virtio-win ISO has no build of this driver; skipped"
            );
            continue;
        };
        for (path, entry) in optical::walk(iso, &dir).map_err(|e| e.to_string())? {
            let data = iso
                .read_file(&entry.file, MAX_DRIVER_FILE)
                .map_err(|e| e.to_string())?;
            out.push((format!("{driver}/{path}"), data));
        }
    }
    Ok(out)
}

/// The answer file for `image`.
fn answer_file(
    image: &WimImage,
    computer_name: &str,
    timezone: &str,
    product_key: Option<&str>,
) -> String {
    // Windows 11 Setup checks for a TPM 2.0 and Secure Boot, neither of which
    // this machine has yet; these are Microsoft's own bypass values.
    let pe_commands = if image.is_windows_11() {
        let keys = ["BypassTPMCheck", "BypassSecureBootCheck", "BypassRAMCheck"];
        let mut block = String::from("      <RunSynchronous>\n");
        for (order, key) in keys.iter().enumerate() {
            block.push_str(&format!(
                "        <RunSynchronousCommand wcm:action=\"add\">\n\
                 \x20         <Order>{}</Order>\n\
                 \x20         <Path>reg add HKLM\\SYSTEM\\Setup\\LabConfig /v {key} /t REG_DWORD /d 1 /f</Path>\n\
                 \x20       </RunSynchronousCommand>\n",
                order + 1
            ));
        }
        block.push_str("      </RunSynchronous>\n");
        block
    } else {
        String::new()
    };
    TEMPLATE
        .replace("@PE_COMMANDS@", &pe_commands)
        .replace("@IMAGE_INDEX@", &image.index.to_string())
        .replace("@COMPUTER_NAME@", computer_name)
        .replace("@TIMEZONE@", &xml_escape(timezone))
        .replace("@PRODUCT_KEY@", product_key.unwrap_or(""))
}

/// A product key as the answer file wants it: five groups of five letters
/// and digits, upper case.
fn product_key(key: &str) -> Result<String, String> {
    let key = key.trim().to_ascii_uppercase();
    let groups: Vec<&str> = key.split('-').collect();
    let valid = groups.len() == 5
        && groups
            .iter()
            .all(|g| g.len() == 5 && g.chars().all(|c| c.is_ascii_alphanumeric()));
    if valid {
        Ok(key)
    } else {
        Err(format!(
            "--product-key {key:?} is not a product key: it is five groups of five \
             letters and digits, XXXXX-XXXXX-XXXXX-XXXXX-XXXXX"
        ))
    }
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A NetBIOS computer name from the VM name: letters, digits and `-`, at most
/// 15, not only digits.
fn computer_name(vm: &str) -> String {
    let name: String = vm
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(15)
        .collect::<String>()
        .trim_matches('-')
        .to_ascii_uppercase();
    if name.is_empty() || name.bytes().all(|b| b.is_ascii_digit()) {
        "ENTANGLED".into()
    } else {
        name
    }
}

/// The Windows time zone the guest gets: `ENTANGLED_WIN_TIMEZONE` when set
/// (a Windows name, such as `Central European Standard Time`), else the
/// host's own, else UTC.
fn timezone() -> String {
    std::env::var("ENTANGLED_WIN_TIMEZONE")
        .ok()
        .filter(|tz| !tz.trim().is_empty())
        .or_else(host_timezone)
        .unwrap_or_else(|| "UTC".into())
}

/// This host's time zone as a Windows time zone name: what `tzutil /g`
/// says on Windows.
#[cfg(windows)]
fn host_timezone() -> Option<String> {
    let out = std::process::Command::new("tzutil")
        .arg("/g")
        .output()
        .ok()?;
    let name = String::from_utf8(out.stdout).ok()?.trim().to_string();
    // A name, not an error message: letters, digits, spaces and the
    // punctuation Windows' names use.
    let plausible = out.status.success()
        && !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " ()+-.".contains(c));
    plausible.then_some(name)
}

/// This host's IANA zone (`/etc/localtime`'s target, or `/etc/timezone`),
/// mapped to Windows' name for it where [`IANA_TO_WINDOWS`] knows it.
#[cfg(not(windows))]
fn host_timezone() -> Option<String> {
    let iana = std::fs::read_link("/etc/localtime")
        .ok()
        .and_then(|target| {
            let target = target.to_string_lossy().into_owned();
            target
                .split_once("zoneinfo/")
                .map(|(_, zone)| zone.to_string())
        })
        .or_else(|| {
            std::fs::read_to_string("/etc/timezone")
                .ok()
                .map(|zone| zone.trim().to_string())
        })?;
    windows_zone_for(&iana).map(str::to_string)
}

/// The Windows name of an IANA zone, for the zones people are most likely
/// to be in (CLDR's `windowsZones`, abridged). An unknown zone is `None`,
/// and the guest gets UTC.
#[cfg_attr(windows, allow(dead_code))]
fn windows_zone_for(iana: &str) -> Option<&'static str> {
    IANA_TO_WINDOWS
        .iter()
        .find(|(zone, _)| *zone == iana)
        .map(|(_, windows)| *windows)
}

#[cfg_attr(windows, allow(dead_code))]
const IANA_TO_WINDOWS: &[(&str, &str)] = &[
    ("Etc/UTC", "UTC"),
    ("UTC", "UTC"),
    ("Europe/London", "GMT Standard Time"),
    ("Europe/Dublin", "GMT Standard Time"),
    ("Europe/Lisbon", "GMT Standard Time"),
    ("Europe/Warsaw", "Central European Standard Time"),
    ("Europe/Prague", "Central Europe Standard Time"),
    ("Europe/Budapest", "Central Europe Standard Time"),
    ("Europe/Bratislava", "Central Europe Standard Time"),
    ("Europe/Berlin", "W. Europe Standard Time"),
    ("Europe/Amsterdam", "W. Europe Standard Time"),
    ("Europe/Rome", "W. Europe Standard Time"),
    ("Europe/Vienna", "W. Europe Standard Time"),
    ("Europe/Zurich", "W. Europe Standard Time"),
    ("Europe/Stockholm", "W. Europe Standard Time"),
    ("Europe/Oslo", "W. Europe Standard Time"),
    ("Europe/Paris", "Romance Standard Time"),
    ("Europe/Madrid", "Romance Standard Time"),
    ("Europe/Brussels", "Romance Standard Time"),
    ("Europe/Copenhagen", "Romance Standard Time"),
    ("Europe/Helsinki", "FLE Standard Time"),
    ("Europe/Kiev", "FLE Standard Time"),
    ("Europe/Kyiv", "FLE Standard Time"),
    ("Europe/Vilnius", "FLE Standard Time"),
    ("Europe/Riga", "FLE Standard Time"),
    ("Europe/Tallinn", "FLE Standard Time"),
    ("Europe/Athens", "GTB Standard Time"),
    ("Europe/Bucharest", "GTB Standard Time"),
    ("Europe/Istanbul", "Turkey Standard Time"),
    ("Europe/Moscow", "Russian Standard Time"),
    ("America/New_York", "Eastern Standard Time"),
    ("America/Toronto", "Eastern Standard Time"),
    ("America/Chicago", "Central Standard Time"),
    ("America/Denver", "Mountain Standard Time"),
    ("America/Phoenix", "US Mountain Standard Time"),
    ("America/Los_Angeles", "Pacific Standard Time"),
    ("America/Vancouver", "Pacific Standard Time"),
    ("America/Anchorage", "Alaskan Standard Time"),
    ("Pacific/Honolulu", "Hawaiian Standard Time"),
    ("America/Sao_Paulo", "E. South America Standard Time"),
    ("America/Mexico_City", "Central Standard Time (Mexico)"),
    ("Asia/Tokyo", "Tokyo Standard Time"),
    ("Asia/Seoul", "Korea Standard Time"),
    ("Asia/Shanghai", "China Standard Time"),
    ("Asia/Hong_Kong", "China Standard Time"),
    ("Asia/Singapore", "Singapore Standard Time"),
    ("Asia/Kolkata", "India Standard Time"),
    ("Asia/Dubai", "Arabian Standard Time"),
    ("Asia/Jerusalem", "Israel Standard Time"),
    ("Australia/Sydney", "AUS Eastern Standard Time"),
    ("Australia/Melbourne", "AUS Eastern Standard Time"),
    ("Australia/Perth", "W. Australia Standard Time"),
    ("Pacific/Auckland", "New Zealand Standard Time"),
];

/// Presses a key at the disc's "boot from CD" prompt — once. After Setup's
/// first restart the firmware boots Windows Boot Manager from the disk, and
/// should it reach the disc again, the prompt times out on its own; pressing
/// there would start Setup over.
#[derive(Default)]
struct BootKey {
    pressed: bool,
}

impl BootKey {
    fn step(&mut self, log: &str) -> Option<Vec<u8>> {
        if self.pressed || !log.contains(BOOT_PROMPT) {
            return None;
        }
        self.pressed = true;
        tracing::info!("pressed a key at the disc's boot prompt");
        Some(b"\r".to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// XML forbids `--` inside a comment, and Setup refuses a malformed
    /// answer file with a dialog nobody is there to read. It has happened
    /// twice (`--headless`, `--product-key`).
    #[test]
    fn the_template_comments_have_no_double_hyphen() {
        for comment in TEMPLATE.split("<!--").skip(1) {
            let body = comment.split("-->").next().unwrap_or_default();
            assert!(!body.contains("--"), "a comment holds `--`: {body}");
        }
    }

    #[test]
    fn iana_zones_map_to_windows_names() {
        assert_eq!(
            windows_zone_for("Europe/Warsaw"),
            Some("Central European Standard Time")
        );
        assert_eq!(windows_zone_for("Etc/UTC"), Some("UTC"));
        assert_eq!(windows_zone_for("Mars/Olympus_Mons"), None);
        for (iana, windows) in IANA_TO_WINDOWS {
            assert!(iana.contains('/') || *iana == "UTC", "{iana}");
            assert!(!windows.is_empty() && windows.len() <= 64, "{windows}");
        }
    }

    const XML: &str = "<WIM><TOTALBYTES>1</TOTALBYTES>\
        <IMAGE INDEX=\"1\"><NAME>Windows 10 Home</NAME><WINDOWS><ARCH>9</ARCH>\
        <EDITIONID>Core</EDITIONID><VERSION><MAJOR>10</MAJOR><BUILD>19041</BUILD></VERSION>\
        </WINDOWS></IMAGE>\
        <IMAGE INDEX=\"6\"><NAME>Windows 10 Pro</NAME><WINDOWS><ARCH>9</ARCH>\
        <EDITIONID>Professional</EDITIONID><VERSION><MAJOR>10</MAJOR><BUILD>19041</BUILD></VERSION>\
        </WINDOWS></IMAGE></WIM>";

    #[test]
    fn the_wim_xml_lists_editions_and_pro_is_the_default() {
        let images = parse_wim_xml(XML);
        assert_eq!(images.len(), 2);
        assert_eq!(
            images[1],
            WimImage {
                index: 6,
                name: "Windows 10 Pro".into(),
                edition: "Professional".into(),
                build: 19041,
                arch: 9,
            }
        );
        assert_eq!(choose_image(&images, None).unwrap().index, 6);
        assert_eq!(choose_image(&images, Some("core")).unwrap().index, 1);
        assert_eq!(choose_image(&images, Some("1")).unwrap().index, 1);
        assert_eq!(
            choose_image(&images, Some("windows 10 pro")).unwrap().index,
            6
        );
        let error = choose_image(&images, Some("Enterprise")).unwrap_err();
        assert!(
            error.contains("6: Windows 10 Pro (Professional)"),
            "{error}"
        );
        assert!(choose_image(&images[..1], None).is_ok());
        assert!(choose_image(&images[..1], Some("Professional")).is_err());
    }

    #[test]
    fn the_answer_file_is_filled_in_and_windows_11_skips_its_hardware_checks() {
        let mut image = parse_wim_xml(XML).remove(1);
        let ten = answer_file(&image, "WIN10", "Central European Standard Time", None);
        for placeholder in [
            "@IMAGE_INDEX@",
            "@COMPUTER_NAME@",
            "@TIMEZONE@",
            "@PE_COMMANDS@",
        ] {
            assert!(!ten.contains(placeholder), "{placeholder} left unfilled");
        }
        assert!(ten.contains("<Value>6</Value>"));
        assert!(ten.contains("<ComputerName>WIN10</ComputerName>"));
        assert!(ten.contains("<TimeZone>Central European Standard Time</TimeZone>"));
        assert!(!ten.contains("LabConfig"));
        image.build = 22631;
        let eleven = answer_file(
            &image,
            "WIN11",
            "UTC",
            Some("ABCDE-12345-FGHIJ-67890-KLMNO"),
        );
        assert!(eleven.contains("<Key>ABCDE-12345-FGHIJ-67890-KLMNO</Key>"));
        assert!(ten.contains("<Key></Key>"));
        assert_eq!(
            product_key(" abcde-12345-fghij-67890-klmno ").unwrap(),
            "ABCDE-12345-FGHIJ-67890-KLMNO"
        );
        assert!(product_key("ABCDE-12345").is_err());
        assert!(product_key("ABCDE-12345-FGHIJ-67890-KLM<>").is_err());
        assert_eq!(eleven.matches("LabConfig").count(), 3);
        assert!(eleven.contains("<RunSynchronous>"));
    }

    #[test]
    fn computer_names_fit_netbios() {
        assert_eq!(computer_name("win10"), "WIN10");
        assert_eq!(computer_name("my_windows_machine_one"), "MY-WINDOWS-MACH");
        assert_eq!(computer_name("1234"), "ENTANGLED");
        assert_eq!(computer_name("_"), "ENTANGLED");
    }

    #[test]
    fn the_boot_prompt_gets_one_key() {
        let mut key = BootKey::default();
        assert_eq!(key.step("BdsDxe: starting Boot0002"), None);
        let prompt = format!("\x1b[2J{BOOT_PROMPT}.");
        assert_eq!(key.step(&prompt), Some(b"\r".to_vec()));
        assert_eq!(key.step(&format!("{prompt}\n{prompt}")), None);
    }

    /// The 3D package goes on the disc as first-logon.cmd expects it: the
    /// certificate, the runtime under one name, the driver tree without
    /// symbols; a directory that is not the package is refused by name.
    #[test]
    fn the_gpu3d_package_is_carried_without_symbols_and_checked() {
        let dir = std::env::temp_dir().join(format!("entangled-gpu3d-{}", std::process::id()));
        let amd64 = dir.join("Win10").join("amd64");
        std::fs::create_dir_all(amd64.join("x64")).unwrap();
        for (path, bytes) in [
            (amd64.join("viogpu3d.inf"), &b"inf"[..]),
            (amd64.join("viogpu3d.pdb"), b"pdb"),
            (amd64.join("x64").join("opengl32_x64.dll"), b"dll"),
        ] {
            std::fs::write(path, bytes).unwrap();
        }
        let refused = gpu3d_files(&dir).unwrap_err();
        assert!(refused.contains("VirtIOTestCert.cer"), "{refused}");
        std::fs::write(dir.join("VirtIOTestCert.cer"), b"cer").unwrap();
        let refused = gpu3d_files(&dir).unwrap_err();
        assert!(refused.contains("VulkanRT"), "{refused}");
        std::fs::write(dir.join("VulkanRT-1.4.363.0-Installer.exe"), b"exe").unwrap();
        let names: Vec<String> = gpu3d_files(&dir)
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(
            names,
            [
                "VirtIOTestCert.cer",
                "VulkanRT-Installer.exe",
                "Win10/amd64/viogpu3d.inf",
                "Win10/amd64/x64/opengl32_x64.dll",
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_flat_package_carries_32_bit_twins_into_their_own_folders() {
        let dir = std::env::temp_dir().join(format!("entangled-x86gpu-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "VulkanRT-1.4.363.0-Installer.exe",
            "entangled-gpu.inf",
            "entangled-gpu.sys",
            "entangled-gpu.cat",
            "gpu-test.exe",
            "vulkan_virtio.dll",
            "vulkan_virtio_x86.dll",
            "d3d9_x86.dll",
            "d3d10core_x86.dll",
            "d3d11_x86.dll",
        ] {
            std::fs::write(dir.join(name), name).unwrap();
        }
        // Three of DXVK's four: not a set, so not carried (and not an error:
        // a flat package's stray DLL is not a folder someone half filled).
        let files = own_gpu_files(&dir).unwrap();
        let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
        assert!(names.contains(&"x86/vulkan_virtio.dll"), "{names:?}");
        assert!(!names.iter().any(|n| n.starts_with("dxvk/")), "{names:?}");
        // The 32-bit driver is the 32-bit file, not its 64-bit twin.
        let x86 = files.iter().find(|(n, _)| n == "x86/vulkan_virtio.dll");
        assert_eq!(x86.unwrap().1, b"vulkan_virtio_x86.dll");
        std::fs::write(dir.join("dxgi_x86.dll"), b"dll").unwrap();
        let names: Vec<String> = own_gpu_files(&dir)
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(names.iter().any(|n| n == "dxvk/x86/d3d9.dll"), "{names:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn our_own_gpu_package_is_recognised_and_checked() {
        let dir = std::env::temp_dir().join(format!("entangled-owngpu-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!is_own_gpu_package(&dir));
        std::fs::write(dir.join("entangled-gpu.inf"), b"inf").unwrap();
        assert!(is_own_gpu_package(&dir));
        let refused = own_gpu_files(&dir).unwrap_err();
        assert!(refused.contains("VulkanRT"), "{refused}");
        std::fs::write(dir.join("VulkanRT-1.4.363.0-Installer.exe"), b"exe").unwrap();
        let refused = own_gpu_files(&dir).unwrap_err();
        assert!(refused.contains("entangled-gpu.sys"), "{refused}");
        for name in [
            "entangled-gpu.sys",
            "entangled-gpu.cat",
            "gpu-test.exe",
            "vulkan_virtio.dll",
            "vk-smoke.exe",
        ] {
            std::fs::write(dir.join(name), name).unwrap();
        }
        std::fs::create_dir_all(dir.join("dxvk")).unwrap();
        std::fs::write(dir.join("dxvk").join("d3d11.dll"), b"dll").unwrap();
        let refused = own_gpu_files(&dir).unwrap_err();
        assert!(refused.contains("d3d9.dll"), "{refused}");
        for name in ["d3d9.dll", "d3d10core.dll", "dxgi.dll"] {
            std::fs::write(dir.join("dxvk").join(name), b"dll").unwrap();
        }
        // A Microsoft-signed package: no certificate, nothing test-signed.
        let signed: Vec<String> = own_gpu_files(&dir)
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(!signed.iter().any(|n| n.ends_with(".cer")), "{signed:?}");
        std::fs::write(dir.join("entangled-gpu-test.cer"), b"cer").unwrap();
        let names: Vec<String> = own_gpu_files(&dir)
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(
            names,
            [
                "VulkanRT-Installer.exe",
                "dxvk/d3d10core.dll",
                "dxvk/d3d11.dll",
                "dxvk/d3d9.dll",
                "dxvk/dxgi.dll",
                "entangled-gpu.cat",
                "entangled-gpu.cer",
                "entangled-gpu.inf",
                "entangled-gpu.sys",
                "gpu-test.exe",
                "vulkan_virtio.dll",
            ]
        );
        // The published package is flat: OpenGL's DLLs beside the driver
        // still land in their folder on the disc.
        for name in ["opengl32.dll", "libgallium_wgl.dll"] {
            std::fs::write(dir.join(name), b"dll").unwrap();
        }
        let names: Vec<String> = own_gpu_files(&dir)
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(
            names.contains(&"opengl/opengl32.dll".to_owned()),
            "{names:?}"
        );
        assert!(
            names.contains(&"opengl/libgallium_wgl.dll".to_owned()),
            "{names:?}"
        );
        // The display driver is all three files or none: a package from
        // before it (above) is fine, half of one is refused.
        assert!(!names.iter().any(|n| n.starts_with("entangled-display")));
        std::fs::write(dir.join("entangled-display.sys"), b"sys").unwrap();
        let refused = own_gpu_files(&dir).unwrap_err();
        assert!(refused.contains("entangled-display.inf"), "{refused}");
        for name in ["entangled-display.inf", "entangled-display.cat"] {
            std::fs::write(dir.join(name), name).unwrap();
        }
        let names: Vec<String> = own_gpu_files(&dir)
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        for name in DISPLAY_DRIVER_FILES {
            assert!(names.contains(&(*name).to_owned()), "{names:?}");
        }
        // The WDDM driver likewise: none so far, half refused, all carried.
        assert!(!names.iter().any(|n| n.starts_with("entangled-wddm")));
        std::fs::write(dir.join("entangled_d3d10.dll"), b"dll").unwrap();
        let refused = own_gpu_files(&dir).unwrap_err();
        assert!(refused.contains("entangled-wddm.inf"), "{refused}");
        for name in [
            "entangled-wddm.inf",
            "entangled-wddm.sys",
            "entangled-wddm.cat",
        ] {
            std::fs::write(dir.join(name), name).unwrap();
        }
        let names: Vec<String> = own_gpu_files(&dir)
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        for name in WDDM_DRIVER_FILES {
            assert!(names.contains(&(*name).to_owned()), "{names:?}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An answer disc built from two synthetic ISOs: the edition is read out
    /// of the WIM header, the drivers of the right version are copied.
    #[test]
    fn an_answer_disc_carries_the_answer_file_and_the_drivers() {
        let dir = std::env::temp_dir().join(format!("entangled-win-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A WIM whose XML follows its 208-byte header.
        let xml: Vec<u8> = std::iter::once(0xfeff)
            .chain(XML.encode_utf16())
            .flat_map(u16::to_le_bytes)
            .collect();
        let mut wim = vec![0u8; 208];
        wim[..8].copy_from_slice(WIM_MAGIC);
        let reshdr = (xml.len() as u64) | (2u64 << 56);
        wim[0x48..0x50].copy_from_slice(&reshdr.to_le_bytes());
        wim[0x50..0x58].copy_from_slice(&208u64.to_le_bytes());
        wim.extend_from_slice(&xml);
        let mut windows = IsoBuilder::new("WIN");
        windows.add_file("sources/install.esd", wim).unwrap();
        let windows_iso = dir.join("windows.iso");
        std::fs::write(&windows_iso, windows.build()).unwrap();
        let mut virtio = IsoBuilder::new("VIRTIO");
        for driver in ["viostor", "NetKVM", "vioinput"] {
            for version in ["w10", "w11"] {
                virtio
                    .add_file(
                        &format!("{driver}/{version}/amd64/{driver}.inf"),
                        version.as_bytes().to_vec(),
                    )
                    .unwrap();
            }
        }
        let virtio_iso = dir.join("virtio.iso");
        std::fs::write(&virtio_iso, virtio.build()).unwrap();

        let (disc, image) = answer_disc(
            &windows_iso,
            &virtio_iso,
            None,
            "WIN10",
            "UTC",
            None,
            Extras::default(),
        )
        .unwrap();
        assert_eq!(image.index, 6);
        let mut answers = optical::iso9660::IsoImage::new(std::io::Cursor::new(disc)).unwrap();
        let root = answers.root().clone();
        let files = optical::walk(&mut answers, &root).unwrap();
        let names: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            names,
            [
                "$WinPEDriver$/NetKVM/NetKVM.inf",
                "$WinPEDriver$/vioinput/vioinput.inf",
                "$WinPEDriver$/viostor/viostor.inf",
                "Autounattend.xml",
                "entangled/first-logon.cmd",
            ]
        );
        let inf = &files[2].1;
        assert_eq!(answers.read_file(&inf.file, 16).unwrap(), b"w10");
        let script = answers.read_file(&files[4].1.file, 1 << 20).unwrap();
        let script = String::from_utf8(script).unwrap();
        assert!(script.contains("viogpuap.exe"));
        assert_eq!(
            script.matches('\n').count(),
            script.matches("\r\n").count(),
            "CRLF throughout"
        );
        let xml = answers.read_file(&files[3].1.file, 1 << 20).unwrap();
        assert!(String::from_utf8(xml).unwrap().contains("<Value>6</Value>"));

        // A virtio-win ISO without the disk driver is refused.
        let mut empty = IsoBuilder::new("EMPTY");
        empty.add_file("README", b"x".to_vec()).unwrap();
        std::fs::write(&virtio_iso, empty.build()).unwrap();
        let error = answer_disc(
            &windows_iso,
            &virtio_iso,
            None,
            "W",
            "UTC",
            None,
            Extras::default(),
        )
        .unwrap_err();
        assert!(error.contains("viostor/w10/amd64"), "{error}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
