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
#[cfg(target_os = "linux")]
mod memory;
#[cfg(target_os = "linux")]
mod vcpu;
#[cfg(target_os = "linux")]
mod vm;

pub use error::VmmError;
pub use state::{VmState, VmStateError};

#[cfg(target_os = "linux")]
pub use hypervisor::{HostCapabilities, Hypervisor, MIN_KVM_API_VERSION};
#[cfg(target_os = "linux")]
pub use memory::{create_guest_memory, GuestMem};
#[cfg(target_os = "linux")]
pub use vcpu::{spawn_vcpus, ExitHandler, RunOutcome, Vcpu, VcpuThreads};
#[cfg(target_os = "linux")]
pub use vm::{MachineConfig, Vm};
