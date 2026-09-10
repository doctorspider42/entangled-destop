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

#[cfg(target_os = "linux")]
pub fn run() -> Result<(), String> {
    use vmm_core::Hypervisor;

    println!("entangled doctor");
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
                engines();
                install_readiness();
                println!("host looks ready to run VMs");
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
pub fn run() -> Result<(), String> {
    use vmm_core::whp::{WhpHypervisor, WHP_ENABLE_HINT};

    println!("entangled doctor");
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
    engines();
    install_readiness();
    println!("host looks ready to run VMs");
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
