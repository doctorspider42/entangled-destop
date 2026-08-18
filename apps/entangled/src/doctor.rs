//! `entangled doctor` — host prerequisite checks (backlog MVP-005).

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

#[cfg(not(target_os = "linux"))]
pub fn run() -> Result<(), String> {
    Err("entangled requires a Linux host with KVM; on Windows use WSL2".into())
}
