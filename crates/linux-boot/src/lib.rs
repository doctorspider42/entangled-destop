//! Direct Linux boot (backlog EPIC 2): loading a `bzImage` and initramfs into
//! guest memory, building `boot_params` + E820 and pointing vCPU0 at the
//! kernel's 64-bit entry point. The heavy lifting will use the `linux-loader`
//! crate; this crate owns the VMHost-specific policy around it.

use std::path::PathBuf;

use thiserror::Error;

/// What to boot and how (resolved from the VM config by `control-api`).
#[derive(Debug, Clone)]
pub struct BootConfig {
    /// Path to the kernel `bzImage`.
    pub kernel: PathBuf,
    /// Optional initramfs image loaded after the kernel.
    pub initramfs: Option<PathBuf>,
    /// Kernel command line, without trailing NUL.
    pub cmdline: String,
}

#[derive(Debug, Error)]
pub enum BootError {
    #[error("kernel command line exceeds {max} bytes ({len})")]
    CmdlineTooLong { len: usize, max: usize },

    #[error("kernel command line contains a NUL byte")]
    CmdlineNul,

    #[error("failed to load {what}: {message}")]
    Load { what: &'static str, message: String },
}

impl BootConfig {
    /// Validates host-independent invariants (length and content of the
    /// command line). File existence is checked at load time.
    pub fn validate(&self) -> Result<(), BootError> {
        let max = machine_x86::layout::CMDLINE_MAX_LEN - 1; // room for NUL
        if self.cmdline.len() > max {
            return Err(BootError::CmdlineTooLong {
                len: self.cmdline.len(),
                max,
            });
        }
        if self.cmdline.contains('\0') {
            return Err(BootError::CmdlineNul);
        }
        Ok(())
    }
}

/// Marker printed by guest test images once userspace is up; boot tests wait
/// for this on the serial console (backlog MVP-208).
pub const GUEST_READY_MARKER: &str = "VMHOST_GUEST_READY";

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(cmdline: &str) -> BootConfig {
        BootConfig {
            kernel: "vmlinuz".into(),
            initramfs: None,
            cmdline: cmdline.into(),
        }
    }

    #[test]
    fn accepts_typical_cmdline() {
        cfg("console=ttyS0 earlyprintk=serial panic=1 reboot=k")
            .validate()
            .unwrap();
    }

    #[test]
    fn rejects_nul_and_overlong() {
        assert!(matches!(cfg("a\0b").validate(), Err(BootError::CmdlineNul)));
        let long = "x".repeat(machine_x86::layout::CMDLINE_MAX_LEN);
        assert!(matches!(
            cfg(&long).validate(),
            Err(BootError::CmdlineTooLong { .. })
        ));
    }
}
