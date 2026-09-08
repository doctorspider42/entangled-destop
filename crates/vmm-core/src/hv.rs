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

// ---- full CPU state, for suspend/restore (ADR-0006) ----------------------

/// One model-specific register, as an index/value pair.
///
/// The *list* is not hardcoded: each backend enumerates what its hypervisor
/// says it can save and restore (`KVM_GET_MSR_INDEX_LIST`, WHP's MSR register
/// names) and reads that. A missing MSR is a guest that resumes subtly wrong —
/// a `KERNEL_GS_BASE` that never came back is a kernel that faults on its next
/// `swapgs` — so the failure mode of guessing is much worse than the cost of
/// asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct X86Msr {
    pub index: u32,
    pub data: u64,
}

/// Which hypervisor produced an opaque state blob, and what shape it is.
///
/// A snapshot is bound to the host that took it (the file header says so and a
/// restore refuses otherwise), so an opaque blob is honest rather than lazy:
/// KVM's local-APIC page and WHP's interrupt-controller state describe the same
/// hardware in formats neither one accepts from the other, and inventing a
/// third would mean re-deriving one from the other on every save.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobFormat {
    /// KVM `kvm_lapic_state`: the architectural 1 KiB local-APIC register page.
    KvmLapicPage,
    /// WHP `WHvVirtualProcessorStateTypeInterruptControllerState`.
    WhpInterruptController,
    /// The architectural XSAVE area (`KVM_GET_XSAVE`,
    /// `WHvGetVirtualProcessorXsaveState`).
    XsaveArea,
    /// Nothing was captured; the field is absent rather than empty.
    Absent,
}

impl BlobFormat {
    pub const fn as_str(self) -> &'static str {
        match self {
            BlobFormat::KvmLapicPage => "kvm-lapic-page",
            BlobFormat::WhpInterruptController => "whp-interrupt-controller",
            BlobFormat::XsaveArea => "xsave-area",
            BlobFormat::Absent => "absent",
        }
    }
}

/// A blob of hypervisor state that has an architectural meaning but no neutral
/// Rust shape worth inventing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct X86OpaqueState {
    pub format: BlobFormat,
    pub bytes: Vec<u8>,
}

impl Default for X86OpaqueState {
    fn default() -> Self {
        Self {
            format: BlobFormat::Absent,
            bytes: Vec::new(),
        }
    }
}

impl X86OpaqueState {
    pub fn new(format: BlobFormat, bytes: Vec<u8>) -> Self {
        Self { format, bytes }
    }

    pub fn is_absent(&self) -> bool {
        self.format == BlobFormat::Absent
    }

    /// Refuses a blob a backend must not feed to its hypervisor.
    pub fn expect(&self, format: BlobFormat) -> Result<&[u8], HvError> {
        if self.format != format {
            return Err(HvError::Registers(format!(
                "snapshot carries a {} blob where a {} one is needed",
                self.format.as_str(),
                format.as_str()
            )));
        }
        Ok(&self.bytes)
    }
}

/// Where a vCPU is in the multiprocessor startup dance.
///
/// The states both hypervisors can express. An application processor that was
/// still waiting for its INIT/SIPI when the snapshot was taken has to come back
/// waiting, or the restored guest brings up a CPU that is already running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MpState {
    /// Executing, or ready to.
    #[default]
    Runnable,
    /// An application processor that has never been started.
    Uninitialized,
    /// INIT received, waiting for the startup IPI.
    InitReceived,
    /// Halted (`hlt`), waiting for an interrupt.
    Halted,
    /// Startup IPI received, not yet running.
    SipiReceived,
    /// Stopped by the host.
    Stopped,
}

impl MpState {
    pub const fn as_str(self) -> &'static str {
        match self {
            MpState::Runnable => "runnable",
            MpState::Uninitialized => "uninitialized",
            MpState::InitReceived => "init-received",
            MpState::Halted => "halted",
            MpState::SipiReceived => "sipi-received",
            MpState::Stopped => "stopped",
        }
    }
}

/// Interrupts and exceptions the hypervisor was holding for the guest at the
/// moment of the snapshot.
///
/// The one piece of vCPU state that is invisible from the registers and fatal
/// to lose: an interrupt the hypervisor had accepted but not yet delivered is
/// gone for ever if it is not carried across, and the device that raised it is
/// waiting for an acknowledgement that will never come.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct X86PendingEvents {
    pub exception_injected: bool,
    pub exception_pending: bool,
    pub exception_vector: u8,
    pub exception_has_error_code: bool,
    pub exception_error_code: u32,
    pub interrupt_injected: bool,
    pub interrupt_vector: u8,
    pub interrupt_soft: bool,
    /// Interrupt shadow: the guest is between a `sti`/`mov ss` and the next
    /// instruction, where an interrupt may not be delivered.
    pub interrupt_shadow: u8,
    pub nmi_injected: bool,
    pub nmi_pending: bool,
    pub nmi_masked: bool,
    pub sipi_vector: u32,
    pub smi_smm: bool,
    pub smi_pending: bool,
    pub smi_inside_nmi: bool,
    pub smi_latched_init: u8,
    /// The host's own validity word for the fields above (KVM's
    /// `kvm_vcpu_events.flags`), carried verbatim.
    ///
    /// Not neutral, and deliberately so: it says which of the sub-structures
    /// the *kernel* considers meaningful, and inventing our own answer would be
    /// guessing on the kernel's behalf. A snapshot is bound to the host that
    /// took it, so this never reaches a hypervisor that did not write it.
    pub host_event_flags: u32,
}

/// The x86 debug registers, as a set.
///
/// Cheap to carry and expensive to lose: a guest with a hardware watchpoint set
/// (a kernel debugger, a `gdb` inside the guest) that resumed without them
/// would simply stop stopping.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct X86DebugRegisters {
    pub db: [u64; 4],
    pub dr6: u64,
    pub dr7: u64,
}

/// Everything one virtual CPU is, at a stop point.
///
/// Read on the vCPU's own thread while it is parked at a lifecycle checkpoint,
/// which is the only moment at which "everything" is a well-defined set: the
/// previous exit has been dispatched and the next entry has not begun.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct X86CpuState {
    pub index: u32,
    pub registers: X86Registers,
    pub special_registers: X86SpecialRegisters,
    /// Every MSR the host reports as saveable, in index order.
    pub msrs: Vec<X86Msr>,
    /// `XCR0`, the extended-state enable mask. Restored *before* the XSAVE
    /// area, because it decides which of its components exist.
    pub xcr0: u64,
    pub xsave: X86OpaqueState,
    pub lapic: X86OpaqueState,
    pub mp_state: MpState,
    pub events: X86PendingEvents,
    pub debug_registers: X86DebugRegisters,
}

impl X86CpuState {
    /// The value of one MSR in this snapshot, if it was captured.
    pub fn msr(&self, index: u32) -> Option<u64> {
        self.msrs.iter().find(|m| m.index == index).map(|m| m.data)
    }
}

/// VM-wide time, as opposed to per-vCPU time.
///
/// A restored guest whose paravirtual clock still points at the instant the
/// snapshot was taken sees time jump by however long the snapshot sat on disk:
/// timers fire in a storm, `ntpd` panics, and a watchdog reboots the machine.
/// KVM's `KVM_GET_CLOCK`/`KVM_SET_CLOCK` is how that is put back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VmClockState {
    /// The guest's paravirtual clock, in nanoseconds.
    pub clock_ns: u64,
    /// The host's own validity/behaviour flags for the value above.
    pub flags: u32,
    /// Host realtime and TSC at the moment of the read, where the host reports
    /// them (KVM's `KVM_CLOCK_REALTIME`/`KVM_CLOCK_HOST_TSC`). Zero otherwise.
    pub realtime_ns: u64,
    pub host_tsc: u64,
}

/// The interrupt controllers the **hypervisor** owns, where it owns any.
///
/// The half of the machine that `machine_x86::state` cannot see. On WHP the
/// 8259 pair, the 8254 and the IOAPIC live in this process and are saved with
/// every other device; on KVM they live in the kernel, behind
/// `KVM_GET_IRQCHIP`/`KVM_GET_PIT2`, and nothing in userspace has a copy.
///
/// Losing them is not subtle and it is not survivable. The IOAPIC's redirection
/// table is where the guest recorded which vector each device's line delivers;
/// a restored VM whose IOAPIC came back at power-on has **every pin masked**,
/// so the 16550 can never interrupt again and a guest waiting for its transmit
/// interrupt simply stops writing. (Measured exactly that way: the first
/// restored guest ran — its GPU kept drawing, because MSI-X bypasses the
/// IOAPIC — and never printed another line.)
///
/// The contents are opaque per-chip blobs, because they are the *kernel's*
/// structures and a snapshot is bound to the host that wrote it anyway.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostIrqChipState {
    pub pic_master: Vec<u8>,
    pub pic_slave: Vec<u8>,
    pub ioapic: Vec<u8>,
    /// The in-kernel 8254, where there is one.
    pub pit: Vec<u8>,
}

impl HostIrqChipState {
    /// True when the hypervisor had nothing to report.
    pub fn is_empty(&self) -> bool {
        self.pic_master.is_empty()
            && self.pic_slave.is_empty()
            && self.ioapic.is_empty()
            && self.pit.is_empty()
    }
}

/// The hypervisor's own interrupt controllers, where it has them.
///
/// A trait for the same reason [`GuestClock`] is one: the machine layer holds
/// it without holding a backend type, and a host whose chips are in userspace
/// (WHP) simply does not implement it — its chips are saved with the rest of
/// the machine instead.
pub trait HostIrqChip: Send + Sync {
    fn save_irqchip(&self) -> Result<HostIrqChipState, HvError>;
    fn load_irqchip(&self, state: &HostIrqChipState) -> Result<(), HvError>;
}

/// The VM-wide clock, where the hypervisor has one.
///
/// A trait rather than a method on the VM object so the machine layer can hold
/// it without holding a `kvm_ioctls::VmFd` (ADR-0002: a backend type outside
/// `vmm-core` is a bug), and so a host without a paravirtual clock simply does
/// not implement it.
pub trait GuestClock: Send + Sync {
    fn save_clock(&self) -> Result<VmClockState, HvError>;
    fn load_clock(&self, state: &VmClockState) -> Result<(), HvError>;
}
