//! Hypervisor abstraction seam (backlog WHP-1701, ADR-0002).
//!
//! Everything above the hypervisor speaks these types; KVM implements them
//! today and Windows Hypervisor Platform implements them next. The x86
//! register structs are OURS — machine code (GDT/long-mode setup, CPUID
//! policy) must never touch `kvm_bindings` types directly, or the WHP port
//! turns back into a rewrite.
//!
//! Deliberately small: this seam covers what the machine model actually
//! uses. Interrupt delivery already goes through `virtio_core::Interrupt`/
//! `IrqLine` and the serial trigger, so it needs no new abstraction here.

use thiserror::Error;

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

/// Register-level access to one virtual CPU, implemented per hypervisor.
///
/// The KVM implementation lives on [`crate::Vcpu`]; the WHP one arrives with
/// EPIC 17. Machine setup code (machine-x86) must go through this trait.
pub trait VcpuRegisters {
    fn get_registers(&self) -> Result<X86Registers, HvError>;
    fn set_registers(&self, regs: &X86Registers) -> Result<(), HvError>;
    fn get_special_registers(&self) -> Result<X86SpecialRegisters, HvError>;
    fn set_special_registers(&self, sregs: &X86SpecialRegisters) -> Result<(), HvError>;
}
