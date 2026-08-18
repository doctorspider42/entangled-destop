//! Opening and validating the KVM hypervisor handle (backlog MVP-101).

use kvm_ioctls::{Cap, Kvm};

use crate::VmmError;

/// Minimum stable KVM API version; constant since Linux 2.6.
pub const MIN_KVM_API_VERSION: i32 = 12;

/// Capabilities Entangled Desktop cannot run without. Checked once at startup so device
/// code can rely on them unconditionally.
const REQUIRED_CAPS: &[(Cap, &str)] = &[
    (Cap::UserMemory, "KVM_CAP_USER_MEMORY"),
    (Cap::Irqchip, "KVM_CAP_IRQCHIP"),
    (Cap::Ioeventfd, "KVM_CAP_IOEVENTFD"),
    (Cap::Irqfd, "KVM_CAP_IRQFD"),
    (Cap::PitState2, "KVM_CAP_PIT_STATE2"),
    (Cap::ImmediateExit, "KVM_CAP_IMMEDIATE_EXIT"),
];

/// Snapshot of what the host's KVM offers, for `entangled doctor` (MVP-005).
#[derive(Debug, Clone)]
pub struct HostCapabilities {
    pub api_version: i32,
    pub max_vcpus: usize,
    pub nr_memslots: usize,
    /// Names of required capabilities that are missing (empty when runnable).
    pub missing: Vec<&'static str>,
}

impl HostCapabilities {
    pub fn is_runnable(&self) -> bool {
        self.missing.is_empty()
    }
}

/// Validated handle to /dev/kvm.
pub struct Hypervisor {
    kvm: Kvm,
}

impl Hypervisor {
    /// Opens /dev/kvm and validates the API version and required capabilities.
    pub fn open() -> Result<Self, VmmError> {
        let kvm = Kvm::new().map_err(|e| VmmError::KvmOpen(e.into()))?;
        let version = kvm.get_api_version();
        if version != MIN_KVM_API_VERSION {
            return Err(VmmError::KvmApiVersion {
                found: version,
                required: MIN_KVM_API_VERSION,
            });
        }
        let hv = Self { kvm };
        if let Some(&(_, name)) = REQUIRED_CAPS
            .iter()
            .find(|(cap, _)| !hv.kvm.check_extension(*cap))
        {
            return Err(VmmError::MissingCapability(name));
        }
        Ok(hv)
    }

    /// Probes host capabilities without failing on missing ones; used by
    /// `entangled doctor` to print a diagnosis instead of an error.
    pub fn probe() -> Result<HostCapabilities, VmmError> {
        let kvm = Kvm::new().map_err(|e| VmmError::KvmOpen(e.into()))?;
        let missing = REQUIRED_CAPS
            .iter()
            .filter(|(cap, _)| !kvm.check_extension(*cap))
            .map(|&(_, name)| name)
            .collect();
        Ok(HostCapabilities {
            api_version: kvm.get_api_version(),
            max_vcpus: kvm.get_max_vcpus(),
            nr_memslots: kvm.get_nr_memslots(),
            missing,
        })
    }

    pub fn kvm(&self) -> &Kvm {
        &self.kvm
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Requires a usable /dev/kvm; skips (with a note) where it is absent or
    /// not accessible (CI without nested virt, user not in the `kvm` group —
    /// `entangled doctor` is the tool that *diagnoses* those states).
    #[test]
    fn open_validates_api_and_caps() {
        if !std::path::Path::new("/dev/kvm").exists() {
            eprintln!("skipping: /dev/kvm not present");
            return;
        }
        let hv = match Hypervisor::open() {
            Ok(hv) => hv,
            Err(VmmError::KvmOpen(e)) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("skipping: /dev/kvm not accessible (add user to the 'kvm' group)");
                return;
            }
            Err(e) => panic!("unexpected: {e}"),
        };
        assert_eq!(hv.kvm().get_api_version(), MIN_KVM_API_VERSION);
        let caps = Hypervisor::probe().unwrap();
        assert!(caps.is_runnable(), "missing: {:?}", caps.missing);
        assert!(caps.max_vcpus >= 1);
    }
}
