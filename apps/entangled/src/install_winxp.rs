//! `entangled install windows` for a Windows XP CD (ADR-0012, phase X5).
//!
//! Windows XP installs through nothing ADR-0008's path has: no EFI loader,
//! no virtio drivers, no answer file on a disc. It boots through the BIOS
//! (`boot.mode = "bios"`), finds its disk and its CD on the IDE controller,
//! and reads its answer file, `WINNT.SIF`, from a floppy — which here is an
//! image the BIOS serves and no controller backs. Setup's loader reads it
//! through `int 13h` before the kernel starts and carries the answers
//! along, so nothing after the loader needs the drive.
//!
//! The CD is the user's, untouched, and so is the product key: XP's Setup
//! refuses to go on without one, and there is none to default to.

use std::path::{Path, PathBuf};

use control_api::{
    BootMode, BootSection, DiskSection, DisplayAdapter, DisplaySection, OpticalSection,
    VirtioTransport, VmConfig,
};
use disk_image::optical;

use crate::install;
use crate::install_windows::{CLAP_DEFAULT_MEMORY_MIB, CLAP_DEFAULT_SIZE};
use crate::run_vm;
use crate::InstallArgs;

/// XP is comfortable in 512 MiB and installs in 128; a gigabyte is room.
const DEFAULT_MEMORY_MIB: u64 = 1024;
const MIN_MEMORY_MIB: u64 = 128;
const DEFAULT_DISK_BYTES: u64 = 16 << 30;
const MIN_DISK_BYTES: u64 = 3 << 30;
/// The desktop the answer file asks for, through the BIOS's VBE.
const DESKTOP: (u32, u32) = (1024, 768);

/// Whether `iso` is a Windows XP (or 2000/2003) installation CD: the ones
/// that carry Setup's loader under `I386`.
pub(crate) fn is_xp_disc(iso: &Path) -> bool {
    optical::open(iso)
        .ok()
        .and_then(|mut disc| disc.find("I386/SETUPLDR.BIN").ok().flatten())
        .is_some()
}

/// The answer file. Everything Setup would ask is here, so it asks nothing:
/// the whole disk as one NTFS partition, the licence, the names, the first
/// logon — which runs one command, `shutdown`, because the guest powering
/// off is how the installer knows it finished.
const WINNT_SIF: &str = r#"[Data]
AutoPartition=1
MsDosInitiated="0"
UnattendedInstall="Yes"

[Unattended]
UnattendMode=FullUnattended
OemSkipEula=Yes
OemPreinstall=No
TargetPath=\WINDOWS
Repartition=Yes
FileSystem=ConvertNTFS
UnattendSwitch="Yes"
WaitForReboot="No"
NoWaitAfterTextMode=1
NoWaitAfterGUIMode=1
DriverSigningPolicy=Ignore

[GuiUnattended]
AdminPassword=*
EncryptedAdminPassword=NO
AutoLogon=Yes
AutoLogonCount=1
OEMSkipRegional=1
TimeZone=85
OemSkipWelcome=1

[UserData]
ProductKey=@PRODUCT_KEY@
FullName="Entangled"
OrgName="Entangled"
ComputerName=@COMPUTER_NAME@

[Display]
BitsPerPel=32
Xresolution=@WIDTH@
YResolution=@HEIGHT@

[Identification]
JoinWorkgroup=WORKGROUP

[Networking]
InstallDefaultComponents=Yes

[GuiRunOnce]
Command0="@FIRST_LOGON@"
"#;

/// What the first logon runs when there is nothing to install.
const SHUTDOWN: &str = "shutdown -s -t 5";
/// What it runs when the additions disc is in a drive: the disc's own
/// script, wherever the disc got its letter.
const RUN_ADDITIONS: &str = r"cmd /c for %i in (D E F G H) do if exist %i:\entangled\setup.cmd call %i:\entangled\setup.cmd";
/// The additions disc's script: the drivers, then the power-off the
/// installer waits for.
const SETUP_CMD: &str = "@echo off\r\ncopy /y \"%~dp0gltest.exe\" \"%SystemRoot%\\gltest.exe\"\r\n\"%~dp0entxp-setup.exe\"\r\nshutdown -s -t 5\r\n";
/// The files of `guest/winxp/build.sh`'s output the additions disc carries.
const ADDITIONS: [&str; 6] = [
    "entxp.sys",
    "entxp.dll",
    "entxp.inf",
    "entxp-setup.exe",
    "entgl.dll",
    "gltest.exe",
];
/// The directory with those files, when the user has them: the display
/// driver of ADR-0013. Without it Windows keeps its own VGA driver.
const ADDITIONS_ENV: &str = "ENTANGLED_WINXP_ADDITIONS";

/// The additions disc: the driver files and the script that installs them,
/// under `entangled/`.
fn additions_disc(dir: &Path) -> Result<Vec<u8>, String> {
    let mut disc = optical::iso9660::IsoBuilder::new("ENTANGLED");
    for name in ADDITIONS {
        let path = dir.join(name);
        let bytes = std::fs::read(&path)
            .map_err(|e| format!("{ADDITIONS_ENV}: cannot read {}: {e}", path.display()))?;
        disc.add_file(&format!("entangled/{name}"), bytes)
            .map_err(|e| e.to_string())?;
    }
    disc.add_file("entangled/setup.cmd", SETUP_CMD.as_bytes().to_vec())
        .map_err(|e| e.to_string())?;
    Ok(disc.build())
}

fn answer_file(product_key: &str, computer_name: &str, additions: bool) -> String {
    WINNT_SIF
        .replace(
            "@FIRST_LOGON@",
            if additions { RUN_ADDITIONS } else { SHUTDOWN },
        )
        .replace("@PRODUCT_KEY@", product_key)
        .replace("@COMPUTER_NAME@", computer_name)
        .replace("@WIDTH@", &DESKTOP.0.to_string())
        .replace("@HEIGHT@", &DESKTOP.1.to_string())
        .replace('\n', "\r\n")
}

/// A 1.44 MB FAT12 floppy image with one file, `name` (8.3, upper case), in
/// its root directory.
fn floppy_image(name: &str, data: &[u8]) -> Result<Vec<u8>, String> {
    const SECTOR: usize = 512;
    const SECTORS: usize = 2880;
    const FATS: usize = 2;
    const SECTORS_PER_FAT: usize = 9;
    const ROOT_ENTRIES: usize = 224;
    let (base, extension) = name.split_once('.').unwrap_or((name, ""));
    if base.is_empty() || base.len() > 8 || extension.len() > 3 || !name.is_ascii() {
        return Err(format!("{name:?} is not an 8.3 file name"));
    }
    let root_at = (1 + FATS * SECTORS_PER_FAT) * SECTOR;
    let data_at = root_at + ROOT_ENTRIES * 32;
    let clusters = data.len().div_ceil(SECTOR).max(1);
    if data_at + clusters * SECTOR > SECTORS * SECTOR {
        return Err(format!("{name} does not fit on a floppy"));
    }
    let mut image = vec![0u8; SECTORS * SECTOR];

    // The boot sector: a jump nobody takes, and the BIOS parameter block.
    image[0..3].copy_from_slice(&[0xeb, 0x3c, 0x90]);
    image[3..11].copy_from_slice(b"MSDOS5.0");
    image[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
    image[13] = 1; // sectors a cluster
    image[14..16].copy_from_slice(&1u16.to_le_bytes()); // reserved sectors
    image[16] = FATS as u8;
    image[17..19].copy_from_slice(&(ROOT_ENTRIES as u16).to_le_bytes());
    image[19..21].copy_from_slice(&(SECTORS as u16).to_le_bytes());
    image[21] = 0xf0; // media: 1.44 MB
    image[22..24].copy_from_slice(&(SECTORS_PER_FAT as u16).to_le_bytes());
    image[24..26].copy_from_slice(&18u16.to_le_bytes()); // sectors a track
    image[26..28].copy_from_slice(&2u16.to_le_bytes()); // heads
    image[38] = 0x29;
    image[39..43].copy_from_slice(&0x0e17_a961u32.to_le_bytes());
    image[43..54].copy_from_slice(b"ENTANGLED  ");
    image[54..62].copy_from_slice(b"FAT12   ");
    image[510] = 0x55;
    image[511] = 0xaa;

    // The allocation table: twelve bits an entry, the file as one chain
    // from cluster 2.
    let mut fat = vec![0u8; SECTORS_PER_FAT * SECTOR];
    let mut set = |cluster: usize, value: u16| {
        let at = cluster * 3 / 2;
        if cluster & 1 == 1 {
            fat[at] = (fat[at] & 0x0f) | ((value << 4) as u8 & 0xf0);
            fat[at + 1] = (value >> 4) as u8;
        } else {
            fat[at] = value as u8;
            fat[at + 1] = (fat[at + 1] & 0xf0) | (value >> 8) as u8;
        }
    };
    set(0, 0xff0);
    set(1, 0xfff);
    for n in 0..clusters {
        let next = if n + 1 == clusters {
            0xfff
        } else {
            (n + 3) as u16
        };
        set(n + 2, next);
    }
    for copy in 0..FATS {
        let at = (1 + copy * SECTORS_PER_FAT) * SECTOR;
        image[at..at + fat.len()].copy_from_slice(&fat);
    }

    let entry = &mut image[root_at..root_at + 32];
    entry[..11].fill(b' ');
    entry[..base.len()].copy_from_slice(base.to_ascii_uppercase().as_bytes());
    entry[8..8 + extension.len()].copy_from_slice(extension.to_ascii_uppercase().as_bytes());
    entry[11] = 0x20; // archive
    entry[26..28].copy_from_slice(&2u16.to_le_bytes()); // first cluster
    entry[28..32].copy_from_slice(&(data.len() as u32).to_le_bytes());
    image[data_at..data_at + data.len()].copy_from_slice(data);
    Ok(image)
}

fn absolute(path: &Path) -> Result<PathBuf, String> {
    std::path::absolute(path).map_err(|e| e.to_string())
}

/// Everything decided before the VM starts.
struct Plan {
    /// The machine Setup runs on: the installed one plus the answer floppy.
    install: VmConfig,
    installed: VmConfig,
    profile: PathBuf,
    floppy: PathBuf,
    /// The additions disc, when one was written.
    additions: Option<PathBuf>,
}

fn prepare(args: &InstallArgs) -> Result<Plan, String> {
    let iso = absolute(
        args.iso
            .as_deref()
            .ok_or("install windows requires --iso")?,
    )?;
    let key = args.product_key.as_deref().ok_or(
        "Windows XP's Setup does not go on without a product key: pass yours with \
         --product-key XXXXX-XXXXX-XXXXX-XXXXX-XXXXX",
    )?;
    let key = crate::install_windows::product_key(key)?;
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
    let floppy = target.with_file_name(format!("{name}-answers.img"));
    if profile.exists() {
        return Err(format!(
            "{} already exists: this machine has been installed, or an install of it \
             was interrupted. Remove it (and the disk) to start over",
            profile.display()
        ));
    }
    let size = if args.size == CLAP_DEFAULT_SIZE {
        DEFAULT_DISK_BYTES
    } else {
        crate::disk::parse_size(&args.size).map_err(|e| e.to_string())?
    };
    if size < MIN_DISK_BYTES {
        return Err(format!(
            "Windows XP needs a disk of at least {} GiB; --size {} is smaller",
            MIN_DISK_BYTES >> 30,
            args.size
        ));
    }
    let memory_mib = if args.memory_mib == CLAP_DEFAULT_MEMORY_MIB {
        DEFAULT_MEMORY_MIB
    } else if args.memory_mib < MIN_MEMORY_MIB {
        return Err(format!(
            "Windows XP's Setup needs at least {MIN_MEMORY_MIB} MiB; --memory-mib {} is less",
            args.memory_mib
        ));
    } else {
        // A 32-bit guest without PAE addresses under 4 GiB, and this
        // machine's devices sit in the top gigabyte of that.
        args.memory_mib.min(3072)
    };

    let net = install::net_plan(&args.network, &args.interface)?;
    let installed = VmConfig {
        architecture: Default::default(),
        name: name.clone(),
        memory_mib,
        // One processor: the HAL Setup picks is the uniprocessor one.
        vcpus: 1,
        transport: VirtioTransport::Pci,
        boot: BootSection {
            mode: BootMode::Bios,
            ..Default::default()
        },
        disks: vec![DiskSection {
            path: target.clone(),
            writable: true,
        }],
        cdrom: None,
        // The CD stays in the drive: XP asks for it when a component is added.
        optical: vec![OpticalSection { path: iso }],
        // An RTL8139, which XP has a driver for in the box.
        network: net.machine_section(&name, &target),
        display: DisplaySection {
            width: DESKTOP.0,
            height: DESKTOP.1,
            // The linear framebuffer XP's own `vga.sys` reaches through the
            // BIOS's VBE, beside the VGA Setup draws on.
            adapter: DisplayAdapter::Framebuffer,
            ..Default::default()
        },
        sound: Default::default(),
        gamepad: Default::default(),
        shares: Vec::new(),
        reclaim_memory: false,
        // XP probes model-specific registers the hypervisor does not have.
        ignore_msrs: true,
        hyperv_enlightenments: false,
        guest_agent: false,
    };
    let mut install = installed.clone();
    install.boot.floppy = Some(floppy.clone());
    // The additions disc rides in the second drive for the install only.
    let additions = match std::env::var_os(ADDITIONS_ENV) {
        Some(dir) => {
            let path = target.with_file_name(format!("{name}-additions.iso"));
            std::fs::write(&path, additions_disc(Path::new(&dir))?)
                .map_err(|e| format!("cannot write the additions disc {}: {e}", path.display()))?;
            install.optical.push(OpticalSection { path: path.clone() });
            Some(path)
        }
        None => None,
    };
    // Both through the validator a profile on disk would meet.
    let reparse = |cfg: &VmConfig| {
        VmConfig::from_toml(&toml::to_string_pretty(cfg).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())
    };
    let (installed, install) = (reparse(&installed)?, reparse(&install)?);

    let answers = answer_file(
        &key,
        &crate::install_windows::computer_name(&name),
        additions.is_some(),
    );
    std::fs::write(&floppy, floppy_image("WINNT.SIF", answers.as_bytes())?)
        .map_err(|e| format!("cannot write the answer floppy {}: {e}", floppy.display()))?;
    if !target.exists() {
        crate::disk::create_raw(&target, size).map_err(|e| e.to_string())?;
    }
    Ok(Plan {
        install,
        installed,
        profile,
        floppy,
        additions,
    })
}

/// `ENTANGLED_INSTALL_SCREENSHOT=<file.png>`: a numbered screenshot of the
/// installer every minute, beside that file. An hour of Setup with no
/// window is otherwise an hour of not knowing which screen it is on.
fn progress_screenshots() -> Option<crate::run_options::ScreenshotRequest> {
    let path = std::env::var_os("ENTANGLED_INSTALL_SCREENSHOT")?;
    Some(crate::run_options::ScreenshotRequest {
        after: std::time::Duration::from_secs(60),
        path: PathBuf::from(path),
        burst: Some(std::time::Duration::from_secs(60)),
    })
}

pub(crate) fn run(args: &InstallArgs) -> Result<(), String> {
    let plan = prepare(args)?;
    tracing::info!(
        vm = %plan.install.name,
        answers = %plan.floppy.display(),
        "starting Windows XP Setup; it powers off when done (about an hour)"
    );
    let report = run_vm::run_with(
        plan.install.clone(),
        None,
        run_vm::RunOptions {
            headless: args.headless,
            screenshot: progress_screenshots(),
            ..Default::default()
        },
    )
    .map_err(|e| format!("installer VM failed: {e}"))?;
    if !report.guest_shutdown {
        return Err(format!(
            "Windows XP did not power off, so the installation did not finish. The disk \
             {} and the answer floppy {} are left as they are",
            plan.install.disks[0].path.display(),
            plan.floppy.display(),
        ));
    }
    crate::install_windows::write_profile(&plan.profile, &plan.installed)?;
    // The floppy holds the product key: it does not outlive the install.
    if let Err(error) = std::fs::remove_file(&plan.floppy) {
        tracing::warn!(%error, path = %plan.floppy.display(), "could not remove the answer floppy");
    }
    if let Some(disc) = &plan.additions {
        let _ = std::fs::remove_file(disc);
    }
    tracing::info!(
        profile = %plan.profile.display(),
        "Windows XP is installed; start it with `entangled run {}`",
        plan.profile.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answer_file_carries_the_key_and_the_name_with_dos_line_ends() {
        let text = answer_file("ABCDE-12345-FGHIJ-67890-KLMNO", "XPBOX", false);
        assert!(text.contains("Command0=\"shutdown -s -t 5\"\r\n"));
        let with_disc = answer_file("ABCDE-12345-FGHIJ-67890-KLMNO", "XPBOX", true);
        assert!(with_disc.contains(r"if exist %i:\entangled\setup.cmd call"));
        assert!(text.contains("ProductKey=ABCDE-12345-FGHIJ-67890-KLMNO\r\n"));
        assert!(text.contains("ComputerName=XPBOX\r\n"));
        assert!(text.contains("Xresolution=1024\r\n"));
        assert!(!text.contains('@'));
        assert!(!text.replace("\r\n", "").contains('\n'));
    }

    #[test]
    fn the_floppy_is_a_fat12_volume_with_the_file_in_its_root() {
        let data: Vec<u8> = (0..1500u32).map(|n| n as u8).collect();
        let image = floppy_image("WINNT.SIF", &data).unwrap();
        assert_eq!(image.len(), 1_474_560);
        assert_eq!(&image[510..512], &[0x55, 0xaa]);
        assert_eq!(&image[54..59], b"FAT12");
        // The root directory follows the two tables.
        let root = (1 + 2 * 9) * 512;
        assert_eq!(&image[root..root + 11], b"WINNT   SIF");
        assert_eq!(
            u32::from_le_bytes(image[root + 28..root + 32].try_into().unwrap()),
            1500
        );
        // Three clusters: 2 -> 3 -> 4 -> end, twelve bits each.
        let fat = &image[512..];
        let entry = |n: usize| {
            let pair = u16::from_le_bytes([fat[n * 3 / 2], fat[n * 3 / 2 + 1]]);
            if n & 1 == 1 {
                pair >> 4
            } else {
                pair & 0x0fff
            }
        };
        assert_eq!([entry(2), entry(3), entry(4)], [3, 4, 0xfff]);
        // Both copies of the table agree, and the data is where cluster 2 is.
        assert_eq!(&image[512..512 + 9 * 512], &image[10 * 512..19 * 512]);
        let data_at = root + 224 * 32;
        assert_eq!(&image[data_at..data_at + 1500], &data[..]);
        assert!(floppy_image("TOOLONGNAME.TXT", b"x").is_err());
        assert!(floppy_image("BIG.BIN", &vec![0u8; 2 << 20]).is_err());
    }
}
