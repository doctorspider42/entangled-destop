//! `entangled doctor` — host prerequisite checks (backlog MVP-005, WHP-1703).
//!
//! One arm per hypervisor backend, each reporting what *that* host can and cannot
//! do rather than a common denominator: the two machines genuinely differ (no
//! in-kernel interrupt controllers on WHP, no TAP on Windows, one VM per process
//! on WHP), and a check that hid the differences would be worse than none.

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
    println!("  virtio          : virtio-mmio, queue kicks handled inline on the vCPU thread");
    println!("                    (no ioeventfd on WHP; measured at ~285 MiB/s on virtio-blk)");
    println!("  virtio-pci      : not yet — its notification area follows a guest-programmable");
    println!("                    BAR, which needs the ioeventfd rebasing KVM has");
    println!("  networking      : user-mode NAT in this process — DHCP, DNS and outbound TCP,");
    println!("                    no TAP and no administrator (there is no TAP on Windows, and");
    println!("                    the drivers that provide one are GPL). Not yet wired into");
    println!("                    the Windows run path.");
    println!("  VMs per process : 1 — WHP maps guest memory for one partition per process,");
    println!("                    so a second VM needs a second `entangled` process");
    println!("host looks ready to run VMs");
    Ok(())
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn run() -> Result<(), String> {
    Err(
        "entangled needs a Linux host with KVM or a Windows host with the Windows \
         Hypervisor Platform"
            .into(),
    )
}
