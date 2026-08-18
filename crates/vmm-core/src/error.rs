use thiserror::Error;

/// Top-level errors produced by the VMM core.
#[derive(Debug, Error)]
pub enum VmmError {
    #[error("failed to open /dev/kvm: {0}")]
    KvmOpen(#[source] std::io::Error),

    #[error("KVM API version {found} is not supported (need {required})")]
    KvmApiVersion { found: i32, required: i32 },

    #[error("required KVM capability missing: {0}")]
    MissingCapability(&'static str),

    #[cfg(target_os = "linux")]
    #[error("KVM operation failed: {0}")]
    Kvm(#[from] kvm_ioctls::Error),

    /// The Windows Hypervisor Platform optional feature is off, or Hyper-V is
    /// not running. Flipping it needs admin rights and a reboot, so this is a
    /// user-actionable diagnosis, not a bug.
    #[cfg(windows)]
    #[error("Windows Hypervisor Platform unavailable: {0}")]
    WhpUnavailable(String),

    /// A WHP API call failed; `call` names the function so a failure is never
    /// an anonymous HRESULT.
    #[cfg(windows)]
    #[error("{call} failed: {message}")]
    Whp { call: &'static str, message: String },

    /// A guest exit the WHP backend does not emulate yet (EPIC 17 phase 2).
    #[cfg(windows)]
    #[error("unsupported WHP exit: {0}")]
    WhpUnsupportedExit(String),

    #[error("guest memory setup failed: {0}")]
    GuestMemory(String),

    #[error("vCPU {index} error: {message}")]
    Vcpu { index: usize, message: String },

    #[error("invalid VM state transition: {0}")]
    State(#[from] crate::state::VmStateError),
}
