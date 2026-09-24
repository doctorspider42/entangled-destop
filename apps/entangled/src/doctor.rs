//! `entangled doctor` — host prerequisite checks (backlog MVP-005, WHP-1703).
//!
//! One arm per hypervisor backend, each reporting what *that* host can and cannot
//! do rather than a common denominator: the two machines genuinely differ (no
//! in-kernel interrupt controllers on WHP, no TAP on Windows, one VM per process
//! on WHP), and a check that hid the differences would be worse than none.
//!
//! After the hypervisor, one portable section: **can this host install an OS?**
//! It exists because every one of its lines has cost somebody a long wait — a
//! 2.9 GiB ISO the CLI was looking for in the wrong directory, a firmware
//! artifact a fresh worktree never had, a 20 GiB disk on a volume with 8 GiB
//! left. `doctor` is where you find that out in a second instead of forty
//! minutes.

#[cfg(any(target_os = "linux", windows))]
use std::path::Path;

#[cfg(any(target_os = "linux", windows))]
use disk_image::ops::disk_space;

/// The hypervisor verdict and the portable inventory, in that order, with the
/// verdict deciding only the *exit code*.
///
/// The order is load-bearing and it was learned the expensive way. A host whose
/// hypervisor is unavailable is exactly the host whose owner most needs the
/// second half — "is the firmware there, did the installer ship it, is there an
/// engine in WSL" — and until this function existed `doctor` answered that host
/// with one line and an error. The person who has just turned a Windows feature
/// on and not yet rebooted, or who is not in the `kvm` group, learned nothing
/// about their installation; neither did a CI runner, which is why the
/// fresh-install acceptance could not assert the shipped-firmware regression on
/// one (`scripts/fresh-install-acceptance.ps1`).
///
/// So: print what this host *has* whatever the hypervisor said, and then fail
/// if it said no. Nothing about the exit code changes — a host that cannot run
/// VMs still exits non-zero, and the manager's Diagnostics panel still says
/// "This host cannot run machines yet" over a panel that now has content in it.
#[cfg(any(target_os = "linux", windows))]
fn report(hypervisor: Result<(), String>) -> Result<(), String> {
    engines();
    three_d();
    install_readiness();
    hypervisor?;
    println!("host looks ready to run VMs");
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn run() -> Result<(), String> {
    println!("entangled doctor");
    report(kvm())
}

/// The KVM arm's verdict: prints what it found, returns whether it is usable.
#[cfg(target_os = "linux")]
fn kvm() -> Result<(), String> {
    use vmm_core::Hypervisor;

    if !std::path::Path::new("/dev/kvm").exists() {
        return Err(
            "/dev/kvm not found — KVM is unavailable (kernel module missing or no \
             virtualization support)"
                .into(),
        );
    }
    match Hypervisor::probe() {
        Ok(caps) => {
            println!("  KVM API version : {}", caps.api_version);
            println!("  max vCPUs       : {}", caps.max_vcpus);
            println!("  memory slots    : {}", caps.nr_memslots);
            if caps.is_runnable() {
                println!("  required caps   : all present");
                Ok(())
            } else {
                Err(format!(
                    "missing KVM capabilities: {}",
                    caps.missing.join(", ")
                ))
            }
        }
        Err(e) => Err(format!(
            "/dev/kvm exists but cannot be used: {e} (check permissions — user must be \
             in the 'kvm' group)"
        )),
    }
}

/// The WHP arm (EPIC 17 phase 3).
#[cfg(windows)]
pub fn run() -> Result<(), String> {
    println!("entangled doctor");
    report(whp())
}

/// The WHP arm's verdict: prints what it found, returns whether it is usable.
///
/// WHP has no device node to check, so "is it there" is a capability query, and a
/// disabled optional feature is reported rather than thrown: that is the whole
/// reason `WhpHypervisor::probe` exists next to `open`.
///
/// The lines after it are the ones a user actually needs, because they are where
/// the Windows machine differs from the Linux one and every one of them has bitten
/// somebody: interrupt controllers in this process rather than the kernel, no
/// ioeventfd so queue kicks are synchronous, no TAP so no networking yet, and one
/// VM per process because WHP will only map guest memory for a single partition
/// per host process.
#[cfg(windows)]
fn whp() -> Result<(), String> {
    use vmm_core::whp::{WhpHypervisor, WHP_ENABLE_HINT};

    let caps = WhpHypervisor::probe()
        .map_err(|e| format!("cannot query the Windows Hypervisor Platform: {e}"))?;
    println!(
        "  hypervisor      : Windows Hypervisor Platform {}",
        if caps.hypervisor_present {
            "present"
        } else {
            "NOT present"
        }
    );
    println!(
        "  processor vendor: {}",
        caps.processor_vendor.unwrap_or("unknown")
    );
    if !caps.is_runnable() {
        return Err(format!("WHP is not usable — {WHP_ENABLE_HINT}"));
    }
    println!("  interrupt chips : 8259/8254/IOAPIC emulated in this process");
    println!("                    (WHP provides each vCPU's local APIC and nothing above it)");
    println!("  smp             : yes — WHP's own APIC brings application processors up");
    println!("  virtio          : both transports — virtio-mmio and virtio-pci (MSI-X included,");
    println!("                    decoded in this process and injected per message); queue kicks");
    println!("                    handled inline on the vCPU thread (no ioeventfd on WHP;");
    println!("                    measured at ~285 MiB/s on virtio-blk)");
    println!("  uefi            : yes — CloudHv firmware via the PVH entry, pflash-backed");
    println!("                    NVRAM (boot.nvram) persists UEFI variables across runs");
    println!("  networking      : backend = \"usernet\" — user-mode NAT in this process (DHCP,");
    println!("                    DNS relay, outbound TCP), no TAP and no administrator");
    println!("                    (there is no TAP on Windows, and the drivers that provide");
    println!("                    one are GPL). backend = \"tap\" is Linux-only.");
    println!("  VMs per process : 1 — WHP maps guest memory for one partition per process,");
    println!("                    so a second VM needs a second `entangled` process");
    Ok(())
}

/// Which engine each backend would use, and whether it runs.
///
/// A Windows host has two: `entangled.exe` on WHP (this program), and the Linux
/// build inside WSL. The second one is the one that goes missing — the Windows
/// installer ships no Linux binary — and until this section existed the only
/// way to find that out was to start a machine and read
/// `execvpe entangled failed 2`. So `doctor` asks the same three questions the
/// manager's pre-flight asks, through the same code
/// (`control_api::wsl::probe`), and prints the answer either way.
///
/// Which distribution: `ENTANGLED_WSL_DISTRO`, else the manager's default. The
/// CLI does not read the manager's settings file — the two are independent
/// programs — so the environment variable is the override.
#[cfg(any(target_os = "linux", windows))]
fn engines() {
    let this = std::env::current_exe()
        .map(|exe| exe.display().to_string())
        .unwrap_or_else(|_| "entangled".to_string());
    println!(
        "  engines         : {:<8} {this} ({}) — this program",
        if cfg!(windows) { "windows" } else { "linux" },
        crate::VERSION
    );
    wsl_engine();
}

/// The WSL half of [`engines`]. Windows only: inside WSL this *is* the Linux
/// engine, and probing `wsl.exe` back out through interop would report on the
/// host that is already running the command.
#[cfg(windows)]
fn wsl_engine() {
    let distro = std::env::var("ENTANGLED_WSL_DISTRO")
        .ok()
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| control_api::wsl::DEFAULT_DISTRO.to_string());
    let engine = std::env::var("ENTANGLED_WSL_ENGINE")
        .ok()
        .filter(|e| !e.trim().is_empty());
    match control_api::wsl::probe(&distro, engine.as_deref()) {
        Ok(found) => println!("                    wsl      {}", found.summary()),
        Err(fault) => {
            println!("                    wsl      MISSING — {}", fault.what);
            println!("                             {}", fault.fix);
            // The shared sentence is venue-neutral (the manager prints it too),
            // so the one fix that only exists *here* is named here: in a
            // terminal there is a command for this, and it is the same code the
            // manager's button and the installer's optional task run.
            if fault.fault.installable() {
                println!(
                    "                             or, from this terminal: `entangled wsl \
                     install-engine{}`",
                    if distro == control_api::wsl::DEFAULT_DISTRO {
                        String::new()
                    } else {
                        format!(" --distro {distro}")
                    }
                );
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn wsl_engine() {
    println!(
        "                    wsl      n/a — the WSL backend runs this same Linux engine, \
         from a Windows host"
    );
}

/// What `entangled install` needs on this host, and whether it is present.
///
/// Portable on purpose: the same questions have the same answers on both hosts,
/// only the paths differ. Nothing here fails the command — a host that cannot
/// install Ubuntu today can still run VMs, and the point is to say which
/// artifact is missing *before* a download or a forty-minute boot.
#[cfg(any(target_os = "linux", windows))]
fn install_readiness() {
    println!("  install         : ubuntu — UEFI + verified ISO, offline (no mirror needed)");
    println!("                    debian — d-i on the bootstrap kernel, needs the network");
    println!("                    fedora — netinst through UEFI, kickstart, needs the network");
    uefi_firmware();
    bootstrap_artifacts();

    match crate::paths::ubuntu_cache_dir() {
        Ok(dir) => match crate::paths::newest_iso(&dir) {
            Some(iso) => println!("    ubuntu ISO      {} ({})", iso.display(), size_of(&iso)),
            None => {
                println!("    ubuntu ISO      MISSING in {}", dir.display());
                println!(
                    "                    `bash scripts/fetch-ubuntu-iso.sh` (2.9 GiB, \
                     GPG + SHA-256 verified), or pass --iso"
                );
            }
        },
        Err(e) => println!("    ubuntu ISO      cache directory unknown: {e}"),
    }

    // Where a machine goes when --disk was not given, and whether it fits. An
    // Ubuntu Server install writes a few GiB into a sparse 20 GiB image, so the
    // headline is free space rather than the image's nominal size.
    if let Some(dir) = disk_image::refs::manager_vm_dir() {
        let space = existing_ancestor(&dir)
            .and_then(|probe| disk_space(&probe))
            .map(|(free, total)| {
                format!(
                    "{} free of {}",
                    disk_image::ops::format_bytes(free),
                    disk_image::ops::format_bytes(total)
                )
            })
            .unwrap_or_else(|| "free space unknown".to_string());
        println!("    VM directory    {} ({space})", dir.display());
    }
    println!(
        "    network         --network {} by default on this host",
        crate::DEFAULT_NETWORK
    );
}

/// The UEFI firmware every UEFI guest boots: present, or fetchable, and from
/// where.
///
/// Through the same resolver `install` and `run` use, so this line answers the
/// question they would fail on rather than a similar-looking one. "MISSING"
/// here used to be a dead end on Windows — "copy it in from a Linux checkout" —
/// and it is now a command.
#[cfg(any(target_os = "linux", windows))]
fn uefi_firmware() {
    match crate::firmware::locate() {
        Some(found) => {
            println!(
                "    firmware         {} ({}, from {})",
                found.path.display(),
                size_of(&found.path),
                found.origin.as_str()
            );
        }
        None => {
            let pinned = crate::firmware::pinned()
                .map(|pin| format!("{} ({})", pin.tag, pin.edk2_tag))
                .unwrap_or_else(|e| format!("pin unreadable: {e}"));
            println!("    firmware         MISSING — needed by every UEFI machine");
            // Indented under the label, one line per sentence, so the manager's
            // diagnostics panel marks the fault once and the fixes as info.
            for line in crate::firmware::missing_message().lines() {
                println!("                    {}", line.trim());
            }
            println!("                    pinned release: {pinned}");
        }
    }
}

/// The Debian path's kernel + initramfs: present, or fetchable, and from where.
///
/// It answers the question `install debian` would fail on, through the same
/// resolver `install debian` uses — so a checkout that built its own, a host
/// that downloaded the published pair and a host that has neither each report
/// what they actually are. "MISSING" here used to be a dead end on Windows; it
/// is now a command.
#[cfg(any(target_os = "linux", windows))]
fn bootstrap_artifacts() {
    match crate::bootstrap::locate() {
        Some(found) => {
            println!(
                "    bootstrap kernel {} ({}, from {})",
                found.kernel.display(),
                size_of(&found.kernel),
                found.origin.as_str()
            );
            println!(
                "    bootstrap initrd {} ({})",
                found.initrd.display(),
                size_of(&found.initrd)
            );
        }
        None => {
            let pinned = crate::bootstrap::pinned()
                .map(|pin| format!("{} (Linux {})", pin.tag, pin.kernel_version))
                .unwrap_or_else(|e| format!("pin unreadable: {e}"));
            println!("    bootstrap kernel MISSING — needed by `install debian` only");
            println!("                    {}", crate::bootstrap::missing_hint());
            println!("                    pinned release: {pinned}");
        }
    }
}

#[cfg(any(target_os = "linux", windows))]
fn size_of(path: &Path) -> String {
    std::fs::metadata(path)
        .map(|m| disk_image::ops::format_bytes(m.len()))
        .unwrap_or_else(|_| "size unknown".to_string())
}

/// The nearest existing directory at or above `dir`: the VM directory itself may
/// not exist yet, and a free-space query on a missing path answers nothing.
#[cfg(any(target_os = "linux", windows))]
fn existing_ancestor(dir: &Path) -> Option<std::path::PathBuf> {
    dir.ancestors().find(|p| p.is_dir()).map(Path::to_path_buf)
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn run() -> Result<(), String> {
    Err(
        "entangled needs a Linux host with KVM or a Windows host with the Windows \
         Hypervisor Platform"
            .into(),
    )
}

/// Whether this host can serve the guest's 3D, and with what
/// ([ADR-0004](../../../docs/adr/0004-virtio-gpu-3d.md)).
///
/// `doctor` was silent about 3D until 2026-09-16, which meant the one tool
/// whose job is "will this host run VMs" could not answer "will this host do
/// 3D" — and the answer is not obvious: it depends on an artifact that is
/// neither in the installer nor in any distribution, and `[display] virgl =
/// true` fails at VM start rather than at configuration time. So this reports
/// it before anybody spends a boot finding out.
///
/// What it deliberately does *not* claim: whether a distribution's own
/// `libvirglrenderer` is present. Finding that out means `dlopen`ing it, and
/// `doctor` starts no renderers — so a host with no pinned pair is reported as
/// having no *Venus*, with the system library named as the thing that may still
/// serve classic VirGL.
#[cfg(target_os = "linux")]
fn three_d() {
    match crate::virgl_lib::locate() {
        Some(found) => {
            println!(
                "  3D              : Venus — virglrenderer at {} (from {})",
                found.lib.display(),
                found.origin.as_str()
            );
            println!(
                "                    render server {} (needs libvulkan.so.1 at run time)",
                found.server.display()
            );
        }
        None => {
            println!(
                "  3D              : no Venus renderer — `[display] virgl = true` will use this"
            );
            println!("                    host's own libvirglrenderer if it has one, which serves");
            println!("                    classic VirGL and no Venus (jammy packages 0.9.1).");
            for line in wrap_hint(&crate::virgl_lib::missing_hint()) {
                println!("                    {line}");
            }
        }
    }
    for line in venus_lines(&venus_host(), false) {
        println!("                    {line}");
    }
}

/// The Windows arm, where the answer is short and fixed: the host renderer
/// speaks EGL, and this host has no equivalent yet (ADR-0004's Windows plan).
///
/// Since the Venus renderer (ADR-0004, "how a user turns it on") that is only
/// half of it: `[display] venus = true` is 3D on this host, on its own Vulkan
/// device, and the lines before virgl's say whether it has one.
#[cfg(windows)]
fn three_d() {
    let mut lines = venus_lines(&venus_host(), true).into_iter();
    if let Some(first) = lines.next() {
        println!("  3D              : {first}");
    }
    for line in lines {
        println!("                    {line}");
    }
    println!("                    virgl    Linux-only — `[display] virgl = true` needs");
    println!("                             virglrenderer, which speaks EGL; on this host use");
    println!("                             venus, or the WSL (KVM) backend for virgl");
}

/// A host Vulkan device as the Venus renderer would show it to a guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VenusDevice {
    pub name: String,
    /// The version the guest is shown (the host's, capped at 1.3).
    pub api_version: u32,
    /// `minImportedHostPointerAlignment` of `VK_EXT_external_memory_host`,
    /// which every shown device has: it is how a guest's mapping is our pages.
    pub import_alignment: u64,
    /// `VK_KHR_external_memory_win32` with the device/driver UUIDs an import
    /// is checked against: device-local memory shared between guest
    /// contexts, which is what a GPU-composited desktop's scanout is (ADR-0004
    /// stage S1). Never on a Linux host yet.
    pub memory_export: bool,
}

/// What `[display] venus = true` would find on this host: the loader, and the
/// devices the renderer would show — through the same probe `entangled run`
/// refuses a start with, so the answer here is the answer there.
///
/// # Errors
/// The sentence `run` would fail with: no loader, no instance, or every
/// device hidden with its reason (a missing `VK_EXT_external_memory_host`
/// among them).
pub fn venus_host() -> Result<Vec<VenusDevice>, String> {
    let host = virtio_gpu::host_vulkan::AshVulkan::load()?;
    Ok(host
        .usable_devices()?
        .iter()
        .map(|device| VenusDevice {
            name: device.name(),
            api_version: device.properties.properties.api_version,
            import_alignment: device.import_alignment,
            memory_export: device.memory_export,
        })
        .collect())
}

/// The indentation of a Venus line under its `venus` sub-label.
const VENUS_SUB: &str = "         ";

/// `doctor`'s Venus lines: the first unindented (it follows a label, or sits
/// in the 3D column), the rest in the sub-column under it. `windows` picks
/// the host's story for shared scanout memory; it is a parameter so both
/// stories are tested on either host.
fn venus_lines(found: &Result<Vec<VenusDevice>, String>, windows: bool) -> Vec<String> {
    let devices = match found {
        Ok(devices) if !devices.is_empty() => devices,
        Ok(_) => return vec!["venus    unavailable — the host has no Vulkan device".into()],
        Err(why) => {
            let mut lines = vec![
                "venus    unavailable — `[display] venus = true` would refuse to start:".into(),
            ];
            lines.extend(
                wrap_hint(why)
                    .into_iter()
                    .map(|l| format!("{VENUS_SUB}{l}")),
            );
            return lines;
        }
    };
    let mut lines = vec![
        "venus    ready — `[display] venus = true`: the guest's Vulkan, and its OpenGL and"
            .to_string(),
        format!("{VENUS_SUB}desktop through Zink, on this host's GPU"),
    ];
    for device in devices {
        let v = device.api_version;
        lines.push(format!(
            "{VENUS_SUB}device {} (Vulkan {}.{}.{} to the guest)",
            device.name,
            v >> 22,
            (v >> 12) & 0x3ff,
            v & 0xfff
        ));
        lines.push(format!(
            "{VENUS_SUB}  VK_EXT_external_memory_host   yes (pages imported at {}-byte alignment)",
            device.import_alignment
        ));
        let export = match (windows, device.memory_export) {
            (true, true) => [
                "VK_KHR_external_memory_win32  yes (shared scanout: GNOME composites on",
                "                              the GPU)",
            ],
            (true, false) => [
                "VK_KHR_external_memory_win32  no — clients run on the GPU, GNOME's scanout",
                "                              buffers stay in software",
            ],
            (false, _) => [
                "shared scanout memory         none on Linux hosts yet (ADR-0004 S1):",
                "                              clients run on the GPU, GNOME's scanout stays dumb",
            ],
        };
        lines.extend(export.iter().map(|l| format!("{VENUS_SUB}  {l}")));
    }
    lines
}

/// Breaks a one-line hint at word boundaries so it sits inside `doctor`'s
/// indented column rather than wrapping raggedly in a narrow terminal.
fn wrap_hint(hint: &str) -> Vec<String> {
    const WIDTH: usize = 74;
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in hint.split_whitespace() {
        if !current.is_empty() && current.len() + 1 + word.len() > WIDTH {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::{venus_lines, VenusDevice, VENUS_SUB};

    fn rtx(memory_export: bool) -> VenusDevice {
        VenusDevice {
            name: "NVIDIA GeForce RTX 2070".into(),
            api_version: (1 << 22) | (3 << 12) | 289,
            import_alignment: 4096,
            memory_export,
        }
    }

    /// A host that can serve Venus says so, names each device and the two
    /// memory capabilities the renderer and the GPU desktop rest on.
    #[test]
    fn a_ready_host_names_its_device_and_the_extensions() {
        let lines = venus_lines(&Ok(vec![rtx(true)]), true);
        assert!(
            lines[0].starts_with("venus    ready — `[display] venus = true`"),
            "{lines:?}"
        );
        let text = lines.join("\n");
        for needle in [
            "device NVIDIA GeForce RTX 2070 (Vulkan 1.3.289 to the guest)",
            "VK_EXT_external_memory_host   yes (pages imported at 4096-byte alignment)",
            "VK_KHR_external_memory_win32  yes",
        ] {
            assert!(text.contains(needle), "missing {needle:?}:\n{text}");
        }
        // Every line after the first sits in the sub-column, never flush left,
        // and fits doctor's column.
        assert!(
            lines[1..].iter().all(|l| l.starts_with(VENUS_SUB)),
            "{lines:?}"
        );
        assert!(lines.iter().all(|l| l.chars().count() <= 90), "{lines:?}");

        let no_export = venus_lines(&Ok(vec![rtx(false)]), true).join("\n");
        assert!(
            no_export.contains("VK_KHR_external_memory_win32  no — "),
            "{no_export}"
        );
        let linux = venus_lines(&Ok(vec![rtx(false)]), false).join("\n");
        assert!(!linux.contains("win32"), "{linux}");
        assert!(linux.contains("none on Linux hosts yet"), "{linux}");
    }

    /// A host that cannot says what `run` would refuse with, wrapped into the
    /// column, and never with doctor's MISSING marker: 3D is optional, and the
    /// manager paints MISSING as a fault.
    #[test]
    fn an_unready_host_says_why_without_calling_it_a_fault() {
        let why = "every host Vulkan device is hidden (llvmpipe: it lacks \
                   VK_EXT_external_memory_host, so no guest mapping could be our own pages)";
        for lines in [
            venus_lines(&Err(why.into()), true),
            venus_lines(&Err("the host has no usable Vulkan loader".into()), false),
            venus_lines(&Ok(Vec::new()), true),
        ] {
            assert!(lines[0].starts_with("venus    unavailable — "), "{lines:?}");
            assert!(lines.iter().all(|l| !l.contains("MISSING")), "{lines:?}");
            assert!(lines.iter().all(|l| l.chars().count() <= 90), "{lines:?}");
        }
        let text = venus_lines(&Err(why.into()), true).join(" ");
        assert!(text.contains("VK_EXT_external_memory_host"), "{text}");
    }
}
