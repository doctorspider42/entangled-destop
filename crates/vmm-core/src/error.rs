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

    #[error("guest memory setup failed: {0}")]
    GuestMemory(String),

    #[error("vCPU {index} error: {message}")]
    Vcpu { index: usize, message: String },

    #[error("invalid VM state transition: {0}")]
    State(#[from] crate::state::VmStateError),
}
