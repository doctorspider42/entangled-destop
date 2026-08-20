//! Windows Hypervisor Platform backend (backlog EPIC 17 / WHP-1702,
//! [ADR-0002](../../../../docs/adr/0002-linux-first-whp-ready.md)).
//!
//! The second hypervisor backend behind the [`crate::hv`] seam. It is a peer of
//! the KVM backend, not a layer on top of it: [`WhpHypervisor`] mirrors
//! [`crate::Hypervisor`], [`WhpPartition`] mirrors [`crate::Vm`], [`WhpVcpu`]
//! mirrors [`crate::Vcpu`], and all four speak the same neutral register
//! structs, [`crate::ExitHandler`] and [`crate::RunOutcome`].
//!
//! ```text
//! WhpHypervisor::open()          ~ Hypervisor::open()      (capability probe)
//! WhpPartition::new(&hv, &cfg)   ~ Vm::new(&hv, &cfg)
//!   WHvCreatePartition
//!   WHvSetPartitionProperty(ProcessorCount)
//!   WHvSetupPartition
//!   WHvMapGpaRange(guest RAM, RWX)                          ~ memslots
//!   WHvCreateVirtualProcessor × n
//! whp::spawn_vcpus(...).stop()   ~ spawn_vcpus(...).stop()  (cancel vs signal)
//! ```
//!
//! # What is WHP-specific and what is not
//!
//! WHP exposes only each vCPU's **local** APIC, so the PIC, IOAPIC and PIT are
//! emulated in userspace — but they live in `machine_x86::irqchip`, portable and
//! tested on both hosts, because a redirection table and a counter are machine
//! devices. What lives here is the two things that genuinely are WHP:
//!
//! * [`WhpInterruptDelivery`] — one `WHvRequestInterrupt` per decoded interrupt
//!   message, implementing [`crate::hv::InterruptDelivery`], plus the
//!   [`HaltGate`] a halted vCPU waits on.
//! * [`emulator`] — `WHvEmulatorCreateEmulator` and its callback table, because
//!   WHP's MMIO exit carries raw instruction bytes where KVM's carries a decoded
//!   access.
//!
//! Plus [`cpuid`], which is policy rather than plumbing: WHP intercepts the
//! leaves in [`CPUID_EXIT_LEAVES`] and the backend edits WHP's own default
//! result, so a guest cannot tell the two backends apart from CPUID.
//!
//! A real guest needs all of it switched on, which is what
//! [`WhpOptions::for_guest`] asks for; [`WhpPartition::new`] keeps the phase-1
//! shape so the real-mode smoke guests still behave as they did.

pub(crate) mod cpuid;
mod emulator;
mod interrupt;
mod partition;
mod regs;
mod vcpu;

pub use cpuid::{CpuidPolicy, CpuidResult, CPUID_EXIT_LEAVES};
pub use interrupt::{HaltGate, WhpInterruptDelivery, HALT_POLL};
pub use partition::{WhpCapabilities, WhpHypervisor, WhpOptions, WhpPartition, WHP_ENABLE_HINT};
pub use vcpu::{
    spawn_vcpus, spawn_vcpus_with, VcpuCanceller, WhpVcpu, WhpVcpuThreads, TRACE_EXITS_ENV,
};
