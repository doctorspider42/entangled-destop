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
    install_readiness();
    println!("host looks ready to run VMs");
    Ok(())
}

/// What `entangled install` needs on this host, and whether it is present.
///
/// Portable on purpose: the same questions have the same answers on both hosts,
/// only the paths differ. Nothing here fails the command — a host that cannot
/// install Ubuntu today can still run VMs, and the point is to say which
/// artifact is missing *before* a download or a forty-minute boot.
#[cfg(any(target_os = "linux", windows))]
fn install_readiness() {
    /// The Ubuntu path's firmware and the Debian path's kernel, by the same
    /// relative paths `install` resolves them with — so `doctor` run from the
    /// wrong working directory reports exactly what `install` would fail on.
    const FIRMWARE: &str = "artifacts/firmware/CLOUDHV.fd";
    const BOOTSTRAP_KERNEL: &str = "artifacts/bootstrap/vmlinuz";
    const FIRMWARE_HINT: &str =
        "`bash guest/firmware/build-cloudhv.sh` (~2.5 min), or pass --firmware";
    const KERNEL_HINT: &str = concat!(
        "Debian only: `bash guest/bootstrap-kernel/build.sh` on Linux, ",
        "or copy artifacts/bootstrap/ in from a Linux checkout"
    );

    println!("  install         : ubuntu — UEFI + verified ISO, offline (no mirror needed)");
    println!("                    debian — d-i on the bootstrap kernel, needs the network");
    artifact("firmware        ", Path::new(FIRMWARE), FIRMWARE_HINT);
    artifact("bootstrap kernel", Path::new(BOOTSTRAP_KERNEL), KERNEL_HINT);

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

#[cfg(any(target_os = "linux", windows))]
fn artifact(label: &str, path: &Path, hint: &str) {
    if path.is_file() {
        println!("    {label} {} ({})", path.display(), size_of(path));
    } else {
        println!("    {label} MISSING at {}", path.display());
        println!("                    {hint}");
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
