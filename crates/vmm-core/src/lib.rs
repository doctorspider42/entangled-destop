//! Core VMM building blocks: the hypervisor handle, guest memory and the VM
//! lifecycle state machine.
//!
//! Two hypervisor backends live here, each behind a target gate, both speaking
//! the neutral types in [`hv`] (ADR-0002):
//!
//! * `cfg(target_os = "linux")` — KVM ([`Hypervisor`], [`Vm`], [`Vcpu`]).
//! * `cfg(windows)` — Windows Hypervisor Platform ([`whp`]).
//!
//! Guest memory ([`GuestMem`]), the exit-handler seam ([`ExitHandler`]), the
//! run outcome ([`RunOutcome`]), the state machine and the errors are portable
//! and build on any development OS.

mod error;
pub mod hv;
pub mod lifecycle;
mod memory;
pub mod shm;
mod state;

#[cfg(target_os = "linux")]
mod hypervisor;
#[cfg(target_os = "linux")]
mod snapshot_kvm;
#[cfg(target_os = "linux")]
mod vcpu;
#[cfg(target_os = "linux")]
mod vm;

#[cfg(windows)]
pub mod whp;

pub use error::VmmError;
pub use hv::{
    BlobFormat, DestinationMode, ExitHandler, GuestClock, HostIrqChip, HostIrqChipState,
    InterruptDelivery, InterruptKind, InterruptRequest, MachineConfig, MpState, RunOutcome,
    TriggerMode, VmClockState, X86CpuState, X86DebugRegisters, X86Msr, X86OpaqueState,
    X86PendingEvents,
};
pub use lifecycle::{
    Checkpoint, Lifecycle, LifecycleError, MachineLifecycle, ResettableVcpu, RunState, VcpuKick,
};
pub use memory::{create_guest_memory, GuestMem, HIGH_RAM_START, LOW_RAM_END};
pub use shm::{
    GpaMapper, HostShmRegion, SharedWindow, ShmAccessError, UnmappedGpaMapper,
    MAX_SHM_WINDOW_BYTES, SHM_PAGE_SIZE,
};
pub use state::{VmState, VmStateError};

#[cfg(target_os = "linux")]
pub use hypervisor::{HostCapabilities, Hypervisor, MIN_KVM_API_VERSION};
#[cfg(target_os = "linux")]
pub use vcpu::{spawn_vcpus, spawn_vcpus_with, Vcpu, VcpuThreads};
#[cfg(target_os = "linux")]
pub use vm::{RomRegion, Vm};
