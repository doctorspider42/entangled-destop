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
/// the first boot need; the rest make the installed system whole.
const DRIVERS: &[(&str, bool)] = &[
    ("viostor", true),
    ("NetKVM", true),
    ("vioinput", false),
    ("Balloon", false),
    ("vioserial", false),
    ("viorng", false),
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

fn prepare(args: &InstallArgs) -> Result<Plan, String> {
    let iso = absolute(args.iso.as_deref().ok_or(
        "install windows requires --iso with a Windows 10 or 11 x64 installation ISO \
         (Microsoft's own, untouched)",
    )?)?;
    let drivers = absolute(args.drivers.as_deref().ok_or(
        "install windows requires --drivers with the virtio-win ISO \
         (https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/)",
    )?)?;
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
            adapter: DisplayAdapter::Framebuffer,
            refresh_hz: install::installed_refresh_hz_here(args),
            ..Default::default()
        },
        sound: Default::default(),
        gamepad: Default::default(),
        shares: Vec::new(),
        reclaim_memory: false,
        // Windows reads model-specific MSRs this machine does not have and
        // bugchecks on the #GP (ADR-0008 §3).
        ignore_msrs: true,
        // The Hyper-V reference clock: without it every
        // QueryPerformanceCounter is a PM timer port exit (ADR-0008 §2).
        hyperv_enlightenments: true,
    };
    let mut install = installed.clone();
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

/// The answer disc's bytes and the image it installs.
fn answer_disc(
    windows_iso: &Path,
    drivers_iso: &Path,
    edition: Option<&str>,
    computer_name: &str,
    timezone: &str,
) -> Result<(Vec<u8>, WimImage), String> {
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
        answer_file(&image, computer_name, timezone).into_bytes(),
    )
    .map_err(|e| e.to_string())?;
    let mut virtio = optical::open(drivers_iso).map_err(|e| e.to_string())?;
    for (path, data) in driver_files(virtio.as_mut(), image.is_windows_11())
        .map_err(|e| format!("{}: {e}", drivers_iso.display()))?
    {
        disc.add_file(&format!("$WinPEDriver$/{path}"), data)
            .map_err(|e| e.to_string())?;
    }
    Ok((disc.build(), image))
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
fn answer_file(image: &WimImage, computer_name: &str, timezone: &str) -> String {
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
/// (a Windows name, such as `Central European Standard Time`), else UTC.
fn timezone() -> String {
    std::env::var("ENTANGLED_WIN_TIMEZONE")
        .ok()
        .filter(|tz| !tz.trim().is_empty())
        .unwrap_or_else(|| "UTC".into())
}

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
        let ten = answer_file(&image, "WIN10", "Central European Standard Time");
        assert!(!ten.contains('@'), "an unfilled placeholder");
        assert!(ten.contains("<Value>6</Value>"));
        assert!(ten.contains("<ComputerName>WIN10</ComputerName>"));
        assert!(ten.contains("<TimeZone>Central European Standard Time</TimeZone>"));
        assert!(!ten.contains("LabConfig"));
        image.build = 22631;
        let eleven = answer_file(&image, "WIN11", "UTC");
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

        let (disc, image) = answer_disc(&windows_iso, &virtio_iso, None, "WIN10", "UTC").unwrap();
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
            ]
        );
        let inf = &files[2].1;
        assert_eq!(answers.read_file(&inf.file, 16).unwrap(), b"w10");
        let xml = answers.read_file(&files[3].1.file, 1 << 20).unwrap();
        assert!(String::from_utf8(xml).unwrap().contains("<Value>6</Value>"));

        // A virtio-win ISO without the disk driver is refused.
        let mut empty = IsoBuilder::new("EMPTY");
        empty.add_file("README", b"x".to_vec()).unwrap();
        std::fs::write(&virtio_iso, empty.build()).unwrap();
        let error = answer_disc(&windows_iso, &virtio_iso, None, "W", "UTC").unwrap_err();
        assert!(error.contains("viostor/w10/amd64"), "{error}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
