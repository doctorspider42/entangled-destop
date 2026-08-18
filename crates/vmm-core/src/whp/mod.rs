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
//! # What this phase does not do
//!
//! WHP exposes only the local APIC, so there is no in-kernel PIC, IOAPIC or PIT
//! to ask for — those must be emulated in userspace before a real guest can
//! boot (WHP-1703). MMIO exits carry no access width or data, so they need
//! WHP's own instruction emulator (`WHvEmulatorTryMmioEmulation`) to be wired
//! up. See `.claude/skills/whp-backend/SKILL.md` for the full phase-2 list.

mod partition;
mod regs;
mod vcpu;

pub use partition::{WhpCapabilities, WhpHypervisor, WhpPartition, WHP_ENABLE_HINT};
pub use vcpu::{spawn_vcpus, VcpuCanceller, WhpVcpu, WhpVcpuThreads};
