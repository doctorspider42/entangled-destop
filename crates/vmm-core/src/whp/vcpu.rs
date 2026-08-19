//! WHP virtual processors: register access through [`VcpuRegisters`], the
//! `WHvRunVirtualProcessor` loop with exit translation, and controlled stop
//! via `WHvCancelRunVirtualProcessor` (backlog WHP-1702).
//!
//! # Exit translation
//!
//! | `WHV_RUN_VP_EXIT_REASON` | This backend |
//! |---|---|
//! | `X64Halt` | wait on the partition's [`HaltGate`] and re-enter, or [`RunOutcome::Halted`] when there is no local APIC to wake the CPU |
//! | `X64IoPortAccess` | [`ExitHandler::io_out`] / [`ExitHandler::io_in`], then RIP advanced by the exit context's instruction length; string/`REP` forms go through the instruction emulator |
//! | `MemoryAccess` | `WHvEmulatorTryMmioEmulation`, whose callbacks reach [`ExitHandler::mmio_read`]/[`ExitHandler::mmio_write`] |
//! | `X64Cpuid` | this machine's CPUID policy applied on top of WHP's default result, then RIP advanced |
//! | `UnrecoverableException`, `InvalidVpRegisterValue` | [`RunOutcome::Shutdown`] (the triple-fault equivalent) |
//! | `Canceled` | re-check the stop flag, then re-enter or return [`RunOutcome::Stopped`] |
//! | `None` | re-enter (WHP reports it for internal reschedules) |
//! | everything else | [`VmmError::WhpUnsupportedExit`] naming the reason |
//!
//! Unlike KVM, WHP never advances RIP for us and never decodes the faulting
//! instruction; the backend is responsible for both. (The one exception is the
//! instruction emulator, which advances RIP itself through the register-write
//! callback — see [`crate::whp::emulator`].)
//!
//! # `hlt` is an idle loop, not an ending
//!
//! KVM with an in-kernel irqchip absorbs `hlt` in the kernel; WHP always reports
//! it. A Linux guest executes `hlt` on every trip through `default_idle()`, so a
//! run loop that returned on the first one would stop the guest a few
//! milliseconds into boot, and one that re-entered immediately would spin a host
//! core. With local APIC emulation on, the loop therefore waits on the
//! partition's [`HaltGate`] — bumped by every `WHvRequestInterrupt` — and then
//! re-enters. With it off (the phase-1 smoke guests) nothing can ever wake the
//! CPU, so `hlt` really is the end and [`RunOutcome::Halted`] is right.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use windows::Win32::System::Hypervisor::{
    WHvCancelRunVirtualProcessor, WHvDeleteVirtualProcessor, WHvGetVirtualProcessorRegisters,
    WHvRunVirtualProcessor, WHvRunVpExitReasonCanceled, WHvRunVpExitReasonInvalidVpRegisterValue,
    WHvRunVpExitReasonMemoryAccess, WHvRunVpExitReasonNone,
    WHvRunVpExitReasonUnrecoverableException, WHvRunVpExitReasonX64Cpuid,
    WHvRunVpExitReasonX64Halt, WHvRunVpExitReasonX64IoPortAccess, WHvSetVirtualProcessorRegisters,
    WHvX64RegisterApicBase, WHvX64RegisterRax, WHvX64RegisterRbx, WHvX64RegisterRcx,
    WHvX64RegisterRdx, WHvX64RegisterRip, WHV_PARTITION_HANDLE, WHV_REGISTER_NAME,
    WHV_REGISTER_VALUE, WHV_RUN_VP_EXIT_CONTEXT,
};

use crate::hv::{
    ExitHandler, HvError, RunOutcome, VcpuRegisters, X86Registers, X86SpecialRegisters,
};
use crate::whp::cpuid::{CpuidPolicy, CpuidResult};
use crate::whp::emulator::Emulator;
use crate::whp::interrupt::{HaltGate, HALT_POLL};
use crate::whp::partition::{create_virtual_processor, whp_err, Partition};
use crate::whp::regs::{
    gp_from_values, gp_values, sreg_from_values, sreg_values, zeroed_value, Aligned16, GP_NAMES,
    SREG_NAMES,
};
use crate::VmmError;

/// One WHP virtual processor.
///
/// Owns the VP: `Drop` deletes it, and the `Arc<Partition>` guarantees the
/// partition outlives every VP inside it. WHP permits only one concurrent
/// `WHvRunVirtualProcessor` per VP index, which single ownership enforces.
pub struct WhpVcpu {
    pub index: u32,
    partition: Arc<Partition>,
    /// This machine's CPUID policy, applied to the `X64Cpuid` exits the
    /// partition's exit list produces.
    cpuid: CpuidPolicy,
}

impl WhpVcpu {
    pub(super) fn new(partition: Arc<Partition>, index: u32) -> Result<Self, VmmError> {
        create_virtual_processor(partition.handle(), index)?;
        Ok(Self {
            index,
            partition,
            cpuid: CpuidPolicy::new(index),
        })
    }

    /// A handle another thread can use to kick this vCPU out of
    /// `WHvRunVirtualProcessor`. The WHP equivalent of the KVM backend's kick
    /// signal.
    pub fn canceller(&self) -> VcpuCanceller {
        VcpuCanceller {
            partition: Arc::clone(&self.partition),
            index: self.index,
        }
    }

    fn fail(&self, message: String) -> VmmError {
        VmmError::Vcpu {
            index: self.index as usize,
            message,
        }
    }

    /// Validates a name/value batch: equal lengths, a count WHP can express,
    /// and the 16-byte value-buffer alignment WHP requires (see
    /// [`Aligned16`]). Returning an error rather than trusting the caller is
    /// what keeps a misaligned buffer from becoming an access violation inside
    /// WHP.
    fn batch_count(
        &self,
        names: &[WHV_REGISTER_NAME],
        values: *const WHV_REGISTER_VALUE,
        values_len: usize,
    ) -> Result<u32, VmmError> {
        if names.len() != values_len {
            return Err(self.fail(format!(
                "register batch mismatch: {} names, {values_len} values",
                names.len()
            )));
        }
        if values as usize % 16 != 0 {
            return Err(self.fail(format!(
                "register value buffer at {values:p} is not 16-byte aligned (wrap it in Aligned16)"
            )));
        }
        u32::try_from(names.len())
            .map_err(|_| self.fail(format!("register batch of {} is too large", names.len())))
    }

    /// Reads an arbitrary register batch. `names` and `values` must be the same
    /// length; the caller decides which union arm each value carries.
    fn get_raw(
        &self,
        names: &[WHV_REGISTER_NAME],
        values: &mut [WHV_REGISTER_VALUE],
    ) -> Result<(), VmmError> {
        let count = self.batch_count(names, values.as_ptr(), values.len())?;
        get_regs_raw(self.partition.handle(), self.index, names, values, count)
    }

    /// Writes an arbitrary register batch.
    fn set_raw(
        &self,
        names: &[WHV_REGISTER_NAME],
        values: &[WHV_REGISTER_VALUE],
    ) -> Result<(), VmmError> {
        let count = self.batch_count(names, values.as_ptr(), values.len())?;
        set_regs_raw(self.partition.handle(), self.index, names, values, count)
    }
}

/// Reads `count` registers of VP `index`.
///
/// Free rather than a method because one caller is not the VP itself:
/// [`crate::whp::WhpPartition::processor_summary`] reports any VP's state for
/// diagnostics, from whichever thread is asking. The alignment and length checks
/// stay with the callers, which know their own buffers —
/// [`WhpVcpu::batch_count`] for a vCPU, an `Aligned16` fixed array there.
pub(super) fn get_regs_raw(
    handle: WHV_PARTITION_HANDLE,
    index: u32,
    names: &[WHV_REGISTER_NAME],
    values: &mut [WHV_REGISTER_VALUE],
    count: u32,
) -> Result<(), VmmError> {
    // SAFETY: both pointers are to live slices of at least `count` elements, the
    // value buffer is 16-byte aligned as WHP requires (`Aligned16` at every call
    // site), and WHP writes exactly `count` values into it.
    unsafe {
        WHvGetVirtualProcessorRegisters(handle, index, names.as_ptr(), count, values.as_mut_ptr())
    }
    .map_err(|e| whp_err("WHvGetVirtualProcessorRegisters", e))
}

/// Writes `count` registers of VP `index`. See [`get_regs_raw`].
pub(super) fn set_regs_raw(
    handle: WHV_PARTITION_HANDLE,
    index: u32,
    names: &[WHV_REGISTER_NAME],
    values: &[WHV_REGISTER_VALUE],
    count: u32,
) -> Result<(), VmmError> {
    // SAFETY: as `get_regs_raw`, except that WHP only reads the value buffer.
    unsafe {
        WHvSetVirtualProcessorRegisters(handle, index, names.as_ptr(), count, values.as_ptr())
    }
    .map_err(|e| whp_err("WHvSetVirtualProcessorRegisters", e))
}

impl Drop for WhpVcpu {
    fn drop(&mut self) {
        // SAFETY: this VP was created by `WHvCreateVirtualProcessor` on the
        // partition we hold an `Arc` on (so it is still alive) and is deleted
        // exactly once, since `Drop::drop` runs once and `WhpVcpu` is not
        // `Clone`.
        if let Err(e) = unsafe { WHvDeleteVirtualProcessor(self.partition.handle(), self.index) } {
            tracing::warn!(vcpu = self.index, error = %e, "WHvDeleteVirtualProcessor failed");
        }
    }
}

// ---- hypervisor-neutral register access (WHP-1701, ADR-0002) --------------

impl VcpuRegisters for WhpVcpu {
    fn get_registers(&self) -> Result<X86Registers, HvError> {
        let mut values = Aligned16([zeroed_value(); GP_NAMES.len()]);
        self.get_raw(&GP_NAMES, &mut values.0)
            .map_err(|e| HvError::Registers(e.to_string()))?;
        Ok(gp_from_values(&values.0))
    }

    fn set_registers(&self, regs: &X86Registers) -> Result<(), HvError> {
        let values = Aligned16(gp_values(regs));
        self.set_raw(&GP_NAMES, &values.0)
            .map_err(|e| HvError::Registers(e.to_string()))
    }

    fn get_special_registers(&self) -> Result<X86SpecialRegisters, HvError> {
        let mut values = Aligned16([zeroed_value(); SREG_NAMES.len()]);
        self.get_raw(&SREG_NAMES, &mut values.0)
            .map_err(|e| HvError::Registers(e.to_string()))?;
        let mut sregs = sreg_from_values(&values.0);
        // APIC base is only readable once local APIC emulation is on
        // (WHP-1703); until then leave it at 0 rather than failing the whole
        // read.
        let mut apic = Aligned16([zeroed_value(); 1]);
        if self.get_raw(&[WHvX64RegisterApicBase], &mut apic.0).is_ok() {
            // SAFETY: `WHvX64RegisterApicBase` is a 64-bit MSR-like register,
            // so WHP filled the `Reg64` arm.
            sregs.apic_base = unsafe { apic.0[0].Reg64 };
        }
        Ok(sregs)
    }

    fn set_special_registers(&self, sregs: &X86SpecialRegisters) -> Result<(), HvError> {
        // No read-modify-write needed (unlike KVM's `kvm_sregs`, which carries
        // the pending-interrupt bitmap): WHP addresses registers individually.
        let values = Aligned16(sreg_values(sregs));
        self.set_raw(&SREG_NAMES, &values.0)
            .map_err(|e| HvError::Registers(e.to_string()))?;
        if sregs.apic_base != 0 {
            let mut apic = Aligned16([zeroed_value(); 1]);
            apic.0[0].Reg64 = sregs.apic_base;
            // Best-effort for the same reason as in `get_special_registers`.
            let _ = self.set_raw(&[WHvX64RegisterApicBase], &apic.0);
        }
        Ok(())
    }
}

// ---- run loop -------------------------------------------------------------

/// Fields decoded out of `WHV_X64_IO_PORT_ACCESS_INFO` (WinHvPlatformDefs.h):
/// `IsWrite:1, AccessSize:3, StringOp:1, RepPrefix:1`.
struct IoAccessInfo {
    is_write: bool,
    access_size: usize,
    string_op: bool,
    rep_prefix: bool,
}

impl IoAccessInfo {
    fn decode(raw: u32) -> Self {
        Self {
            is_write: raw & 1 != 0,
            access_size: ((raw >> 1) & 0x7) as usize,
            string_op: raw & (1 << 4) != 0,
            rep_prefix: raw & (1 << 5) != 0,
        }
    }
}

impl WhpVcpu {
    /// Runs the vCPU until the guest shuts down, `running` turns false, or an
    /// unrecoverable error occurs. Every error carries the vCPU index and a
    /// readable message, matching the KVM backend's contract.
    ///
    /// See the module docs for the full exit table, and for why `hlt` is an idle
    /// wait rather than an ending once local APIC emulation is on.
    // `WHvRunVpExitReason*` are `WHV_RUN_VP_EXIT_REASON(i32)` newtype constants
    // from the `windows` crate, so matching on them trips
    // `non_upper_case_globals`; matching on `.0` integers instead would throw
    // away the only readable names we have.
    #[allow(non_upper_case_globals)]
    pub fn run_loop(
        &mut self,
        handler: &mut dyn ExitHandler,
        running: &AtomicBool,
    ) -> Result<RunOutcome, VmmError> {
        // Created on the first exit that needs decoding, so a guest that only
        // does simple port I/O never loads winhvemulation.dll.
        let mut emulator: Option<Emulator> = None;
        // One env read per run loop, not per exit: `$ENTANGLED_WHP_TRACE_EXITS`
        // prints every exit reason and RIP, which is the only way to tell "the
        // guest is spinning on an exit we mishandle" from "WHP is blocking inside
        // the run call" — the two look identical from a serial log, and telling
        // them apart is what identified WHP's own INIT/SIPI handling (see
        // `WhpPartition`'s SMP notes).
        let trace_exits = std::env::var_os(TRACE_EXITS_ENV).is_some();
        let gate: Option<&Arc<HaltGate>> = self
            .partition
            .options()
            .local_apic
            .then(|| self.partition.halt_gate());

        while running.load(Ordering::Acquire) {
            // Snapshot the interrupt epoch *before* entering the guest: an
            // injection that lands between `hlt` executing and the exit being
            // observed must not be slept through.
            let epoch = gate.map_or(0, |gate| gate.epoch());
            let exit = self.run_once()?;
            if trace_exits {
                eprintln!(
                    "vp{} exit={} rip={:#x}",
                    self.index, exit.ExitReason.0, exit.VpContext.Rip
                );
            }
            match exit.ExitReason {
                WHvRunVpExitReasonX64Halt => match gate {
                    // Nothing can wake a CPU without a local APIC, so `hlt`
                    // really is the end (the phase-1 smoke guests rely on this).
                    None => return Ok(RunOutcome::Halted),
                    Some(gate) => {
                        gate.wait_since(epoch, HALT_POLL);
                        continue;
                    }
                },
                // WHP's triple-fault equivalents.
                WHvRunVpExitReasonUnrecoverableException
                | WHvRunVpExitReasonInvalidVpRegisterValue => return Ok(RunOutcome::Shutdown),
                WHvRunVpExitReasonX64IoPortAccess => {
                    self.handle_io(&exit, handler, &mut emulator)?
                }
                WHvRunVpExitReasonMemoryAccess => {
                    self.handle_mmio(&exit, handler, &mut emulator)?
                }
                WHvRunVpExitReasonX64Cpuid => self.handle_cpuid(&exit)?,
                // Kicked by `WHvCancelRunVirtualProcessor`, or an internal
                // reschedule: re-check the stop flag and continue.
                WHvRunVpExitReasonCanceled | WHvRunVpExitReasonNone => continue,
                reason => {
                    return Err(self.fail(format!(
                        "unhandled WHP exit reason {} at rip {:#x}",
                        reason.0, exit.VpContext.Rip
                    )))
                }
            }
            // Same check as the KVM loop: a device (the ACPI PM block) may have
            // latched a power-off request while handling that exit, and the
            // guest will not exit again on its own afterwards.
            if handler.shutdown_requested() {
                return Ok(RunOutcome::Shutdown);
            }
        }
        Ok(RunOutcome::Stopped)
    }

    /// Lazily creates the instruction emulator. One per run loop, kept for its
    /// lifetime: `WHvEmulatorCreateEmulator` is not free and MMIO exits come in
    /// bursts.
    fn emulator<'a>(&self, slot: &'a mut Option<Emulator>) -> Result<&'a Emulator, VmmError> {
        if slot.is_none() {
            *slot = Some(Emulator::new()?);
        }
        slot.as_ref()
            .ok_or_else(|| self.fail("the WHP instruction emulator vanished after creation".into()))
    }

    fn run_once(&self) -> Result<WHV_RUN_VP_EXIT_CONTEXT, VmmError> {
        // SAFETY: `WHV_RUN_VP_EXIT_CONTEXT` is a `repr(C)` aggregate of
        // integers, bitfields and unions of the same; all-zero is a valid bit
        // pattern (`ExitReason` 0 is `WHvRunVpExitReasonNone`).
        //
        // Wrapped in `Aligned16` for the same reason as register values: the
        // union arms include contexts the `windows` crate generates without the
        // header's `DECLSPEC_ALIGN(16)`, and over-aligning costs nothing.
        let mut exit = Aligned16::<WHV_RUN_VP_EXIT_CONTEXT>(unsafe { core::mem::zeroed() });
        let size = u32::try_from(size_of::<WHV_RUN_VP_EXIT_CONTEXT>()).unwrap_or(u32::MAX);
        // SAFETY: `exit` is a live, writable buffer of exactly `size` bytes,
        // and this is the only thread running this VP index (single ownership
        // of `WhpVcpu`).
        unsafe {
            WHvRunVirtualProcessor(
                self.partition.handle(),
                self.index,
                (&raw mut exit.0).cast(),
                size,
            )
        }
        .map_err(|e| {
            self.fail(format!(
                "WHvRunVirtualProcessor failed: {e} ({:#010x})",
                e.code().0 as u32
            ))
        })?;
        Ok(exit.0)
    }

    /// `WHV_VP_EXIT_CONTEXT` packs `InstructionLength:4` and `Cr8:4` into one
    /// byte, which the `windows` crate exposes only as an opaque `_bitfield`.
    fn instruction_length(exit: &WHV_RUN_VP_EXIT_CONTEXT) -> u64 {
        u64::from(exit.VpContext._bitfield & 0x0f)
    }

    /// Emulates a simple `IN`/`OUT`: dispatch to the handler, then advance RIP
    /// past the instruction (WHP does not do it for us) and, for `IN`, write
    /// the result back into RAX.
    ///
    /// String and repeated port I/O (`INS`/`OUTS`, `REP` prefix) needs the same
    /// decode MMIO does, so it goes through `WHvEmulatorTryIoEmulation`. Nothing
    /// in the MVP device set issues them — the serial port, the debug port and
    /// virtio notifications are all single-width `IN`/`OUT` — but a guest is
    /// untrusted and may.
    fn handle_io(
        &self,
        exit: &WHV_RUN_VP_EXIT_CONTEXT,
        handler: &mut dyn ExitHandler,
        emulator: &mut Option<Emulator>,
    ) -> Result<(), VmmError> {
        // SAFETY: `ExitReason == WHvRunVpExitReasonX64IoPortAccess` selects the
        // `IoPortAccess` arm of the exit context union, per WinHvPlatform docs.
        let io = unsafe { exit.Anonymous.IoPortAccess };
        // SAFETY: `WHV_X64_IO_PORT_ACCESS_INFO` is a `repr(C)` union of a
        // 32-bit bitfield struct and `AsUINT32: u32`; reading `AsUINT32` reads
        // the bitfield's storage.
        let info = IoAccessInfo::decode(unsafe { io.AccessInfo.AsUINT32 });

        if info.string_op || info.rep_prefix {
            return self.emulator(emulator)?.emulate_io(
                self.partition.handle(),
                self.index,
                handler,
                &exit.VpContext,
                &io,
            );
        }
        if !matches!(info.access_size, 1 | 2 | 4) {
            return Err(VmmError::WhpUnsupportedExit(format!(
                "port I/O access size {} on port {:#06x}",
                info.access_size, io.PortNumber
            )));
        }

        let next_rip = exit
            .VpContext
            .Rip
            .wrapping_add(Self::instruction_length(exit));
        let mut names = [WHvX64RegisterRip; 2];
        let mut values = Aligned16([zeroed_value(); 2]);
        values.0[0].Reg64 = next_rip;
        let mut count = 1;

        if info.is_write {
            let bytes = io.Rax.to_le_bytes();
            handler.io_out(io.PortNumber, &bytes[..info.access_size]);
        } else {
            let mut data = [0u8; 4];
            handler.io_in(io.PortNumber, &mut data[..info.access_size]);
            // Per the SDM, a 32-bit `IN` zero-extends into RAX while 8- and
            // 16-bit forms leave the upper bits of RAX untouched.
            let rax = match info.access_size {
                1 => (io.Rax & !0xff) | u64::from(data[0]),
                2 => (io.Rax & !0xffff) | u64::from(u16::from_le_bytes([data[0], data[1]])),
                _ => u64::from(u32::from_le_bytes(data)),
            };
            names[1] = WHvX64RegisterRax;
            values.0[1].Reg64 = rax;
            count = 2;
        }
        self.set_raw(&names[..count], &values.0[..count])
    }

    /// Completes an MMIO access through WHP's instruction emulator.
    ///
    /// WHP's exit record carries the guest physical address, the access type and
    /// the raw instruction bytes — no width, no data, and RIP not advanced — so
    /// the only way to serve it is to decode the instruction. The emulator's
    /// memory callback lands on [`ExitHandler::mmio_read`]/
    /// [`ExitHandler::mmio_write`] and its register-write callback advances RIP.
    ///
    /// An **execute** fault is not MMIO: the guest jumped into an unmapped page,
    /// which the emulator cannot help with and which means the machine (or the
    /// guest) is broken. It is reported rather than emulated, with the GPA, so a
    /// bad jump target is diagnosable instead of looping.
    fn handle_mmio(
        &self,
        exit: &WHV_RUN_VP_EXIT_CONTEXT,
        handler: &mut dyn ExitHandler,
        emulator: &mut Option<Emulator>,
    ) -> Result<(), VmmError> {
        // SAFETY: `ExitReason == WHvRunVpExitReasonMemoryAccess` selects the
        // `MemoryAccess` arm of the exit context union.
        let access = unsafe { exit.Anonymous.MemoryAccess };
        // SAFETY: `WHV_MEMORY_ACCESS_INFO` is a `repr(C)` union of a 32-bit
        // bitfield struct (`AccessType:2, GpaUnmapped:1, GvaValid:1`) and
        // `AsUINT32: u32`.
        let info = unsafe { access.AccessInfo.AsUINT32 };
        if info & 0x3 == MEMORY_ACCESS_EXECUTE {
            return Err(VmmError::WhpUnsupportedExit(format!(
                "the guest tried to execute from gpa {:#x} (rip {:#x}), which is not mapped as \
                 RAM and is not a device window",
                access.Gpa, exit.VpContext.Rip
            )));
        }
        self.emulator(emulator)?.emulate_mmio(
            self.partition.handle(),
            self.index,
            handler,
            &exit.VpContext,
            &access,
        )
    }

    /// Answers a `cpuid` the partition's exit list trapped, by applying this
    /// machine's policy to the result WHP would have returned.
    ///
    /// WHP does not write the output registers for an intercepted `cpuid`, and it
    /// does not advance RIP; both are done here.
    fn handle_cpuid(&self, exit: &WHV_RUN_VP_EXIT_CONTEXT) -> Result<(), VmmError> {
        // SAFETY: `ExitReason == WHvRunVpExitReasonX64Cpuid` selects the
        // `CpuidAccess` arm of the exit context union.
        let access = unsafe { exit.Anonymous.CpuidAccess };
        let result = self.cpuid.apply(
            access.Rax as u32,
            access.Rcx as u32,
            CpuidResult {
                eax: access.DefaultResultRax as u32,
                ebx: access.DefaultResultRbx as u32,
                ecx: access.DefaultResultRcx as u32,
                edx: access.DefaultResultRdx as u32,
            },
        );
        // `cpuid` zero-extends into the 64-bit registers, so writing the 32-bit
        // values as `u64` is exactly the architectural result.
        let names = [
            WHvX64RegisterRip,
            WHvX64RegisterRax,
            WHvX64RegisterRbx,
            WHvX64RegisterRcx,
            WHvX64RegisterRdx,
        ];
        let mut values = Aligned16([zeroed_value(); 5]);
        values.0[0].Reg64 = exit
            .VpContext
            .Rip
            .wrapping_add(Self::instruction_length(exit));
        values.0[1].Reg64 = u64::from(result.eax);
        values.0[2].Reg64 = u64::from(result.ebx);
        values.0[3].Reg64 = u64::from(result.ecx);
        values.0[4].Reg64 = u64::from(result.edx);
        self.set_raw(&names, &values.0)
    }
}

/// Environment variable that turns on per-exit tracing in the run loop.
pub const TRACE_EXITS_ENV: &str = "ENTANGLED_WHP_TRACE_EXITS";

/// `WHV_MEMORY_ACCESS_INFO::AccessType`: 0 read, 1 write, 2 execute.
const MEMORY_ACCESS_EXECUTE: u32 = 2;

/// Lets another thread kick a running vCPU out of `WHvRunVirtualProcessor`.
///
/// The KVM backend uses an RT signal for this; WHP has a first-class API, so
/// there is no signal handler to install. Holding an `Arc<Partition>` keeps the
/// partition (and therefore the VP) alive for as long as a canceller exists.
#[derive(Clone)]
pub struct VcpuCanceller {
    partition: Arc<Partition>,
    index: u32,
}

impl VcpuCanceller {
    /// Requests that the vCPU's current (or next) run be interrupted; the run
    /// then returns `WHvRunVpExitReasonCanceled`.
    ///
    /// Also wakes the vCPU if it is *halted*, which is not the same state: a
    /// halted vCPU is not inside `WHvRunVirtualProcessor` at all, so cancelling
    /// only arms the next entry. Without the wake it would sit out the rest of its
    /// [`HALT_POLL`] window before noticing, and `stop()` would spin cancelling at
    /// it the whole time.
    pub fn cancel(&self) -> Result<(), VmmError> {
        self.partition.halt_gate().notify();
        // SAFETY: the partition is kept alive by our `Arc` and `index` names a
        // VP created on it. `WHvCancelRunVirtualProcessor` is documented to be
        // callable from any thread; `flags` must be 0.
        unsafe { WHvCancelRunVirtualProcessor(self.partition.handle(), self.index, 0) }
            .map_err(|e| whp_err("WHvCancelRunVirtualProcessor", e))
    }
}

/// Running WHP vCPU threads with a shared stop flag; the WHP counterpart of
/// [`crate::VcpuThreads`].
pub struct WhpVcpuThreads {
    running: Arc<AtomicBool>,
    cancellers: Vec<VcpuCanceller>,
    handles: Vec<JoinHandle<Result<RunOutcome, VmmError>>>,
}

/// Spawns one thread per vCPU. `make_handler` builds the exit handler for each
/// vCPU index (usually a clone of the device bus).
pub fn spawn_vcpus(
    vcpus: Vec<WhpVcpu>,
    mut make_handler: impl FnMut(u32) -> Box<dyn ExitHandler>,
) -> Result<WhpVcpuThreads, VmmError> {
    let running = Arc::new(AtomicBool::new(true));
    let mut cancellers = Vec::with_capacity(vcpus.len());
    let mut handles = Vec::with_capacity(vcpus.len());
    for mut vcpu in vcpus {
        let mut handler = make_handler(vcpu.index);
        let flag = Arc::clone(&running);
        cancellers.push(vcpu.canceller());
        let handle = std::thread::Builder::new()
            .name(format!("vcpu{}", vcpu.index))
            .spawn(move || vcpu.run_loop(handler.as_mut(), &flag))
            .map_err(|e| VmmError::Vcpu {
                index: 0,
                message: format!("failed to spawn vCPU thread: {e}"),
            })?;
        handles.push(handle);
    }
    Ok(WhpVcpuThreads {
        running,
        cancellers,
        handles,
    })
}

impl WhpVcpuThreads {
    /// Requests all vCPUs to stop, cancels their runs and joins the threads.
    /// Cancels repeatedly to close the race between the flag check and
    /// (re-)entering `WHvRunVirtualProcessor`.
    pub fn stop(self) -> Vec<Result<RunOutcome, VmmError>> {
        self.running.store(false, Ordering::Release);
        let cancellers = self.cancellers;
        self.handles
            .into_iter()
            .zip(cancellers)
            .map(|(handle, canceller)| {
                while !handle.is_finished() {
                    if let Err(e) = canceller.cancel() {
                        tracing::warn!(error = %e, "cancelling vCPU run failed");
                        break;
                    }
                    std::thread::yield_now();
                }
                join_outcome(handle)
            })
            .collect()
    }

    /// Waits for the guests to end on their own (halt/shutdown).
    pub fn join(self) -> Vec<Result<RunOutcome, VmmError>> {
        self.handles.into_iter().map(join_outcome).collect()
    }

    /// Waits until every vCPU ends on its own **or** `should_stop` returns
    /// true, polling at `poll` intervals. Either way all threads are joined
    /// before returning.
    pub fn join_or_stop(
        self,
        should_stop: impl Fn() -> bool,
        poll: std::time::Duration,
    ) -> Vec<Result<RunOutcome, VmmError>> {
        loop {
            if self.handles.iter().all(|h| h.is_finished()) {
                return self.join();
            }
            if should_stop() {
                return self.stop();
            }
            std::thread::sleep(poll);
        }
    }
}

fn join_outcome(handle: JoinHandle<Result<RunOutcome, VmmError>>) -> Result<RunOutcome, VmmError> {
    handle.join().unwrap_or_else(|_| {
        Err(VmmError::Vcpu {
            index: 0,
            message: "vCPU thread panicked".into(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `WHV_X64_IO_PORT_ACCESS_INFO` bit layout, pinned against
    /// hand-computed values.
    #[test]
    fn io_access_info_bit_positions() {
        // OUT with an 8-bit operand: IsWrite=1, AccessSize=1.
        let out8 = IoAccessInfo::decode(0b000011);
        assert!(out8.is_write);
        assert_eq!(out8.access_size, 1);
        assert!(!out8.string_op && !out8.rep_prefix);

        // IN with a 32-bit operand: IsWrite=0, AccessSize=4.
        let in32 = IoAccessInfo::decode(0b001000);
        assert!(!in32.is_write);
        assert_eq!(in32.access_size, 4);

        // REP OUTS: string and rep bits set.
        let outs = IoAccessInfo::decode(0b110011);
        assert!(outs.string_op);
        assert!(outs.rep_prefix);
    }
}
