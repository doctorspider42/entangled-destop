//! Core VMM building blocks: the hypervisor handle, guest memory and the VM
//! lifecycle state machine.
//!
//! Everything KVM-specific lives behind `cfg(target_os = "linux")` so the
//! platform-independent parts (state machine, errors) build and test on any
//! development OS.

mod error;
mod state;

#[cfg(target_os = "linux")]
mod hypervisor;

pub use error::VmmError;
pub use state::{VmState, VmStateError};

#[cfg(target_os = "linux")]
pub use hypervisor::{HostCapabilities, Hypervisor, MIN_KVM_API_VERSION};
