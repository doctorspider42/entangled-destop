//! Hypervisor abstraction seam (backlog WHP-1701, ADR-0002).
//!
//! Everything above the hypervisor speaks these types; KVM implements them
//! today and Windows Hypervisor Platform implements them next. The x86
//! register structs are OURS — machine code (GDT/long-mode setup, CPUID
//! policy) must never touch `kvm_bindings` types directly, or the WHP port
//! turns back into a rewrite.
//!
//! Deliberately small: this seam covers what the machine model actually uses.
//! Device-side interrupt *signalling* goes through `virtio_core::Interrupt`/
//! `IrqLine`, which needs no abstraction here; what does need one is the last
//! hop — asking the CPU's local APIC to deliver an interrupt message. KVM has an
//! in-kernel IOAPIC that does it for us, WHP has only the local APIC, so
//! [`InterruptDelivery`] is the seam a userspace IOAPIC
//! (`machine_x86::irqchip`) delivers through.

use thiserror::Error;

/// Hardware shape of a VM, independent of what it boots and of which
/// hypervisor runs it.
#[derive(Debug, Clone, Copy)]
pub struct MachineConfig {
    pub memory_mib: u64,
    pub vcpu_count: u32,
}

/// General-purpose register state for an x86-64 vCPU, hypervisor-neutral.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct X86Registers {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rsp: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
}

/// One segment register, hypervisor-neutral (mirrors the architectural
/// descriptor cache fields both KVM and WHP expose).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct X86Segment {
    pub base: u64,
    pub limit: u32,
    pub selector: u16,
    pub type_: u8,
    pub present: u8,
    pub dpl: u8,
    pub db: u8,
    pub s: u8,
    pub l: u8,
    pub g: u8,
    pub avl: u8,
    pub unusable: u8,
}

/// Descriptor table register (GDTR/IDTR).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct X86DescriptorTable {
    pub base: u64,
    pub limit: u16,
}

/// Special register state for an x86-64 vCPU, hypervisor-neutral.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct X86SpecialRegisters {
    pub cs: X86Segment,
    pub ds: X86Segment,
    pub es: X86Segment,
    pub fs: X86Segment,
    pub gs: X86Segment,
    pub ss: X86Segment,
    pub tr: X86Segment,
    pub ldt: X86Segment,
    pub gdt: X86DescriptorTable,
    pub idt: X86DescriptorTable,
    pub cr0: u64,
    pub cr2: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub cr8: u64,
    pub efer: u64,
    pub apic_base: u64,
}

#[derive(Debug, Error)]
pub enum HvError {
    #[error("hypervisor register access failed: {0}")]
    Registers(String),

    #[error("hypervisor run failed: {0}")]
    Run(String),

    #[error("interrupt delivery failed: {0}")]
    Interrupt(String),
}

// ---- interrupt delivery (WHP-1703) ---------------------------------------

/// How a local APIC selects the target CPU of an interrupt message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DestinationMode {
    /// `destination` is an APIC id.
    #[default]
    Physical,
    /// `destination` is a logical-destination bitmask.
    Logical,
}

/// Trigger mode of an interrupt message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TriggerMode {
    #[default]
    Edge,
    Level,
}

/// The delivery modes an interrupt controller may ask for. Only the ones an
/// IOAPIC redirection entry can legitimately carry towards a local APIC and that
/// both backends can express; SMI/INIT/ExtINT are deliberately absent — see
/// `machine_x86::irqchip::ioapic` for what it does with those instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InterruptKind {
    /// Deliver `vector` to the destination(s).
    #[default]
    Fixed,
    /// Deliver `vector` to the lowest-priority CPU among the destination(s).
    LowestPriority,
    /// Non-maskable interrupt; `vector` is ignored.
    Nmi,
}

/// One interrupt message, hypervisor-neutral: exactly the fields an IOAPIC
/// redirection-table entry contributes to the APIC bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InterruptRequest {
    pub vector: u8,
    pub destination: u32,
    pub kind: InterruptKind,
    pub destination_mode: DestinationMode,
    pub trigger: TriggerMode,
}

/// The hypervisor's ability to inject an interrupt into a guest local APIC.
///
/// Implemented by the WHP backend (`crate::whp`) over `WHvRequestInterrupt`, and
/// consumed by `machine_x86::irqchip::ioapic::IoApic`. **KVM does not implement
/// it**: there the IOAPIC lives in the kernel and irqfds reach it without
/// userspace, so an implementation would be dead weight. Keeping the trait here
/// rather than in the WHP module is what lets the IOAPIC model — a pure
/// redirection-table decoder — be portable and unit-tested on both hosts.
///
/// Implementations must be cheap, non-blocking and callable from any thread: the
/// callers are vCPU threads inside an exit and the PIT's timer thread.
pub trait InterruptDelivery: Send + Sync {
    fn request(&self, interrupt: &InterruptRequest) -> Result<(), HvError>;
}

/// What a single step of guest execution produced, hypervisor-neutral.
/// Buffers borrow from the backend's run context (KVM's kvm_run mmap; WHP's
/// exit context), so they are consumed before the next `run`.
#[derive(Debug)]
pub enum VcpuEvent<'a> {
    /// Guest halted with interrupts enabled handling left to the backend;
    /// reaching userspace means "treat as done" on machines without an
    /// in-kernel APIC path for it.
    Halted,
    /// Guest requested shutdown (triple fault etc.).
    Shutdown,
    IoOut {
        port: u16,
        data: &'a [u8],
    },
    IoIn {
        port: u16,
        data: &'a mut [u8],
    },
    MmioWrite {
        addr: u64,
        data: &'a [u8],
    },
    MmioRead {
        addr: u64,
        data: &'a mut [u8],
    },
    /// The run was interrupted by the host (signal/cancel); check the stop
    /// flag and re-enter.
    Interrupted,
}

/// How a vCPU's run loop ended, hypervisor-neutral.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// The guest executed `hlt` and the exit reached userspace.
    ///
    /// KVM with an in-kernel irqchip emulates `hlt` in the kernel (the vCPU
    /// blocks waiting for an interrupt), so this outcome only surfaces there
    /// on machines without it; WHP always reports
    /// `WHvRunVpExitReasonX64Halt` while local APIC emulation is off. Test
    /// guests therefore accept either this or [`RunOutcome::Shutdown`].
    Halted,
    /// The guest asked to shut down: a triple fault (`KVM_EXIT_SHUTDOWN`), or
    /// WHP's `UnrecoverableException`/`InvalidVpRegisterValue`.
    Shutdown,
    /// The host asked the loop to stop.
    Stopped,
}

/// Where VM exits are dispatched. The device bus implements this; tests use
/// small recording handlers. Shared by both backends.
pub trait ExitHandler: Send {
    fn io_out(&mut self, port: u16, data: &[u8]);
    fn io_in(&mut self, port: u16, data: &mut [u8]);
    fn mmio_write(&mut self, addr: u64, data: &[u8]);
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]);

    /// True once a device has asked the machine to power off — today only the
    /// ACPI PM block (`machine_x86::acpi::pm`), when the guest writes
    /// `SLP_TYP = S5` with `SLP_EN`.
    ///
    /// Both run loops check this after every dispatched exit and return
    /// [`RunOutcome::Shutdown`], which is how an ACPI `poweroff` ends a VM
    /// without the run loop knowing what ACPI is. The default is `false`, so a
    /// handler that has no such device (the test recorders) needs no code.
    ///
    /// Must not block: it is called on the vCPU thread between guest exits, and
    /// the flag it reads is shared by every vCPU's handler clone.
    fn shutdown_requested(&self) -> bool {
        false
    }

    /// True once a device has latched a guest **reset** request — the 0xCF9
    /// reset control register, the keyboard controller's `0xFE` pulse, or the
    /// ACPI reset register the FADT points at
    /// (`machine_x86::reset::ResetControl`).
    ///
    /// The sibling of [`Self::shutdown_requested`] and read in the same place,
    /// but with a different ending: with a
    /// [`Lifecycle`](crate::lifecycle::Lifecycle) that can restart the machine,
    /// the run loop turns this into an in-place reboot; without one it is an
    /// ending, because the guest has already jumped into its own dead loop and
    /// will never produce another exit.
    ///
    /// Same contract as the shutdown latch: non-blocking, latching, shared by
    /// every vCPU's clone of the handler.
    fn reset_requested(&self) -> bool {
        false
    }
}

/// Register-level access to one virtual CPU, implemented per hypervisor.
///
/// The KVM implementation lives on `crate::Vcpu`, the WHP one on
/// `crate::whp::WhpVcpu`. Machine setup code (machine-x86) must go through
/// this trait.
pub trait VcpuRegisters {
    fn get_registers(&self) -> Result<X86Registers, HvError>;
    fn set_registers(&self, regs: &X86Registers) -> Result<(), HvError>;
    fn get_special_registers(&self) -> Result<X86SpecialRegisters, HvError>;
    fn set_special_registers(&self, sregs: &X86SpecialRegisters) -> Result<(), HvError>;
}
