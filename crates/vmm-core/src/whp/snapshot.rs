//! Reading and writing a WHP virtual processor's *whole* architectural state
//! ([ADR-0006](../../../../docs/adr/0006-suspend-restore.md)).
//!
//! The peer of `crate::snapshot_kvm`, filling the same neutral
//! [`X86CpuState`]. Everything the two hosts disagree about is confined here.
//!
//! # Where WHP's "MSR list" comes from
//!
//! KVM answers `KVM_GET_MSR_INDEX_LIST` at runtime; WHP has no such call. Its
//! enumeration is its **register-name space**: every MSR WHP is prepared to
//! save and restore has a `WHvX64Register*` name, and the ones it does not have
//! a name for it will not hand over at all. So [`MSRS`] is that list, mapped to
//! architectural indices so the file says `0xc0000102` rather than a WHP enum
//! value — and every entry is *probed* rather than assumed: the batch is read
//! in one call, and on failure re-read one register at a time so that a
//! partition which refuses one loses only that one.
//!
//! # What is deliberately not here
//!
//! * **`IA32_MISC_ENABLE`.** WHP has no register name for it, so there is
//!   nothing to ask for. (KVM does, and carries it.)
//! * **The paravirtual clock.** A WHP partition with local APIC emulation has
//!   no kvmclock for the guest to have been using, and WHP exposes no
//!   equivalent of `KVM_GET_CLOCK`. `TSC` and `TSC_AUX` are carried as MSRs, so
//!   the guest's own timekeeping comes back.
//! * **A pending exception *event*** (`WHvRegisterPendingEvent`). It is a
//!   128-bit register with a fault parameter the neutral struct has no room
//!   for, and a vCPU parked between two exits should never have one. If one
//!   turns up, the snapshot is **refused** rather than written without it —
//!   losing an exception the guest is owed is exactly the kind of "resumed
//!   subtly wrong" this whole file exists to avoid.
//! * **Nested state.** The CPUID policy exposes no VMX or SVM, so there is no
//!   L2 to save.

use windows::Win32::System::Hypervisor::{
    WHvGetVirtualProcessorInterruptControllerState, WHvGetVirtualProcessorXsaveState,
    WHvRegisterInternalActivityState, WHvRegisterInterruptState, WHvRegisterPendingEvent,
    WHvRegisterPendingInterruption, WHvSetVirtualProcessorInterruptControllerState,
    WHvSetVirtualProcessorXsaveState, WHvX64RegisterBndcfgs, WHvX64RegisterCstar,
    WHvX64RegisterDr0, WHvX64RegisterDr1, WHvX64RegisterDr2, WHvX64RegisterDr3, WHvX64RegisterDr6,
    WHvX64RegisterDr7, WHvX64RegisterEfer, WHvX64RegisterKernelGsBase, WHvX64RegisterLstar,
    WHvX64RegisterMsrMtrrCap, WHvX64RegisterMsrMtrrDefType, WHvX64RegisterMsrMtrrFix16k80000,
    WHvX64RegisterMsrMtrrFix16kA0000, WHvX64RegisterMsrMtrrFix4kC0000,
    WHvX64RegisterMsrMtrrFix4kC8000, WHvX64RegisterMsrMtrrFix4kD0000,
    WHvX64RegisterMsrMtrrFix4kD8000, WHvX64RegisterMsrMtrrFix4kE0000,
    WHvX64RegisterMsrMtrrFix4kE8000, WHvX64RegisterMsrMtrrFix4kF0000,
    WHvX64RegisterMsrMtrrFix4kF8000, WHvX64RegisterMsrMtrrFix64k00000, WHvX64RegisterPat,
    WHvX64RegisterSfmask, WHvX64RegisterSpecCtrl, WHvX64RegisterStar, WHvX64RegisterSysenterCs,
    WHvX64RegisterSysenterEip, WHvX64RegisterSysenterEsp, WHvX64RegisterTsc,
    WHvX64RegisterTscAdjust, WHvX64RegisterTscAux, WHvX64RegisterTscDeadline, WHvX64RegisterXCr0,
    WHvX64RegisterXss, WHV_REGISTER_NAME,
};

use crate::hv::{
    BlobFormat, HvError, MpState, VcpuRegisters, X86CpuState, X86DebugRegisters, X86Msr,
    X86OpaqueState, X86PendingEvents,
};
use crate::whp::regs::{zeroed_value, Aligned16};
use crate::whp::vcpu::WhpVcpu;

/// Every MSR WHP names, with the architectural index the snapshot records it
/// under.
///
/// `IA32_EFER` and `IA32_APIC_BASE` also arrive with the special registers;
/// EFER is here as well because writing the same value twice is free and the
/// list is meant to be the whole MSR story on its own.
const MSRS: &[(u32, WHV_REGISTER_NAME)] = &[
    (0x0000_0010, WHvX64RegisterTsc),
    (0x0000_003b, WHvX64RegisterTscAdjust),
    (0x0000_0048, WHvX64RegisterSpecCtrl),
    (0x0000_00fe, WHvX64RegisterMsrMtrrCap),
    (0x0000_0174, WHvX64RegisterSysenterCs),
    (0x0000_0175, WHvX64RegisterSysenterEsp),
    (0x0000_0176, WHvX64RegisterSysenterEip),
    (0x0000_0250, WHvX64RegisterMsrMtrrFix64k00000),
    (0x0000_0258, WHvX64RegisterMsrMtrrFix16k80000),
    (0x0000_0259, WHvX64RegisterMsrMtrrFix16kA0000),
    (0x0000_0268, WHvX64RegisterMsrMtrrFix4kC0000),
    (0x0000_0269, WHvX64RegisterMsrMtrrFix4kC8000),
    (0x0000_026a, WHvX64RegisterMsrMtrrFix4kD0000),
    (0x0000_026b, WHvX64RegisterMsrMtrrFix4kD8000),
    (0x0000_026c, WHvX64RegisterMsrMtrrFix4kE0000),
    (0x0000_026d, WHvX64RegisterMsrMtrrFix4kE8000),
    (0x0000_026e, WHvX64RegisterMsrMtrrFix4kF0000),
    (0x0000_026f, WHvX64RegisterMsrMtrrFix4kF8000),
    (0x0000_0277, WHvX64RegisterPat),
    (0x0000_02ff, WHvX64RegisterMsrMtrrDefType),
    (0x0000_06e0, WHvX64RegisterTscDeadline),
    (0x0000_0d90, WHvX64RegisterBndcfgs),
    (0x0000_0da0, WHvX64RegisterXss),
    (0xc000_0080, WHvX64RegisterEfer),
    (0xc000_0081, WHvX64RegisterStar),
    (0xc000_0082, WHvX64RegisterLstar),
    (0xc000_0083, WHvX64RegisterCstar),
    (0xc000_0084, WHvX64RegisterSfmask),
    (0xc000_0102, WHvX64RegisterKernelGsBase),
    (0xc000_0103, WHvX64RegisterTscAux),
];

/// `IA32_MTRRCAP` is read-only architecture; WHP reports it and refuses to
/// take it back. Saved for the record, skipped on the way in.
const READ_ONLY_MSRS: &[u32] = &[0x0000_00fe];

/// The six debug registers, in the order [`X86DebugRegisters`] lists them.
const DEBUG_NAMES: [WHV_REGISTER_NAME; 6] = [
    WHvX64RegisterDr0,
    WHvX64RegisterDr1,
    WHvX64RegisterDr2,
    WHvX64RegisterDr3,
    WHvX64RegisterDr6,
    WHvX64RegisterDr7,
];

/// Buffer for the local-APIC blob. WHP's `WHV_LOCAL_INTERRUPT_CONTROLLER_STATE`
/// is about 1 KiB; this is headroom and a bound.
const LAPIC_BUFFER: usize = 8192;

/// Buffer for the XSAVE blob. The architectural area is 4 KiB before AMX; WHP
/// prepends a small header of its own.
const XSAVE_BUFFER: usize = 16384;

// ---- the three "event" registers, as raw bit patterns ---------------------
//
// Each is a union whose only non-bitfield arm is `AsUINT64`, so the bits are
// read and written directly rather than through a generated bitfield accessor
// this crate would then have to trust. The layouts are WHP's, and stable:
//
//   PendingInterruption: pending:1 type:3 deliver_error:1 insn_len:4 nested:1
//                        reserved:6 vector:16 | error_code:32
//   InterruptState:      shadow:1 nmi_masked:1
//   InternalActivity:    startup_suspend:1 halt_suspend:1 idle_suspend:1

const INT_PENDING: u64 = 1 << 0;
const INT_TYPE_SHIFT: u32 = 1;
const INT_TYPE_MASK: u64 = 0b111;
const INT_DELIVER_ERROR: u64 = 1 << 4;
const INT_LEN_SHIFT: u32 = 5;
const INT_LEN_MASK: u64 = 0b1111;
const INT_NESTED: u64 = 1 << 9;
const INT_VECTOR_SHIFT: u32 = 16;
const INT_VECTOR_MASK: u64 = 0xffff;
const INT_ERROR_SHIFT: u32 = 32;

/// `WHV_X64_PENDING_INTERRUPTION_TYPE`. `External` is 0 and is the `_ =>` arm
/// of the match below, so it is named only where the tests build a register.
#[cfg(test)]
const TYPE_EXTERNAL: u64 = 0;
const TYPE_NMI: u64 = 2;
const TYPE_HARDWARE_EXCEPTION: u64 = 3;
const TYPE_SOFTWARE_INTERRUPT: u64 = 4;

const STATE_SHADOW: u64 = 1 << 0;
const STATE_NMI_MASKED: u64 = 1 << 1;

const ACTIVITY_STARTUP_SUSPEND: u64 = 1 << 0;
const ACTIVITY_HALT_SUSPEND: u64 = 1 << 1;

/// How [`X86PendingEvents::host_event_flags`] is packed on this host: the parts
/// of WHP's pending-interruption register the neutral fields have nowhere to
/// put, so the register can be rebuilt exactly.
const FLAGS_TYPE_SHIFT: u32 = 0;
const FLAGS_LEN_SHIFT: u32 = 4;
const FLAGS_NESTED: u32 = 1 << 8;
/// Set when there was an interruption at all, so an all-zero flags word is
/// unambiguously "nothing pending".
const FLAGS_PENDING: u32 = 1 << 9;

/// Largest register batch this module asks for in one call.
///
/// The value buffer must be a **16-byte-aligned local**, not a `Vec`: WHP moves
/// 128-bit register values with aligned SSE instructions and the `windows`
/// crate's binding drops the `DECLSPEC_ALIGN(16)` (see `regs::Aligned16`). A
/// `Vec<WHV_REGISTER_VALUE>`'s heap buffer is only 8-aligned, so a fixed array
/// inside `Aligned16` is what makes these calls safe rather than
/// half-the-time-safe.
const MAX_BATCH: usize = 64;

impl WhpVcpu {
    /// This processor's multiprocessing state, as the neutral [`MpState`].
    ///
    /// The peer of the KVM backend's `Vcpu::mp_state`, and the same rule
    /// applies: only the thread that owns the VP may ask, so the census is
    /// filed by the run loop's own thread.
    pub(crate) fn mp_state(&self) -> Result<MpState, HvError> {
        let activity = self.read_u64s(&[WHvRegisterInternalActivityState])?;
        Ok(mp_state_from_activity(activity[0]))
    }

    /// Reads a batch of named registers as plain 64-bit values.
    fn read_u64s(&self, names: &[WHV_REGISTER_NAME]) -> Result<Vec<u64>, HvError> {
        if names.len() > MAX_BATCH {
            return Err(HvError::Registers(format!(
                "register batch of {} exceeds {MAX_BATCH}",
                names.len()
            )));
        }
        let mut buffer = Aligned16([zeroed_value(); MAX_BATCH]);
        let values = &mut buffer.0[..names.len()];
        self.get_raw(names, values)
            .map_err(|e| HvError::Registers(e.to_string()))?;
        // SAFETY: every name in `names` selects a 64-bit register, so WHP filled
        // the `Reg64` arm of each value. `WHV_REGISTER_VALUE` is a `repr(C)`
        // union of plain integers with no niche, so reading that arm is defined
        // for any bit pattern WHP could have written.
        Ok(values.iter().map(|v| unsafe { v.Reg64 }).collect())
    }

    fn write_u64s(&self, names: &[WHV_REGISTER_NAME], data: &[u64]) -> Result<(), HvError> {
        if names.len() > MAX_BATCH {
            return Err(HvError::Registers(format!(
                "register batch of {} exceeds {MAX_BATCH}",
                names.len()
            )));
        }
        let mut buffer = Aligned16([zeroed_value(); MAX_BATCH]);
        for (slot, &value) in buffer.0.iter_mut().zip(data) {
            slot.Reg64 = value;
        }
        self.set_raw(names, &buffer.0[..names.len()])
            .map_err(|e| HvError::Registers(e.to_string()))
    }

    /// Every MSR WHP names, with the ones this partition refuses dropped.
    fn read_msrs(&self) -> (Vec<X86Msr>, Vec<u32>) {
        let names: Vec<WHV_REGISTER_NAME> = MSRS.iter().map(|(_, name)| *name).collect();
        if let Ok(values) = self.read_u64s(&names) {
            return (
                MSRS.iter()
                    .zip(values)
                    .map(|((index, _), data)| X86Msr {
                        index: *index,
                        data,
                    })
                    .collect(),
                Vec::new(),
            );
        }
        // One refused register fails the whole batch, so fall back to asking
        // for them one at a time — the same "drop the offender, keep the rest"
        // rule the KVM side gets from `KVM_GET_MSRS`'s partial-read count.
        let mut out = Vec::with_capacity(MSRS.len());
        let mut refused = Vec::new();
        for (index, name) in MSRS {
            match self.read_u64s(&[*name]) {
                Ok(values) if !values.is_empty() => out.push(X86Msr {
                    index: *index,
                    data: values[0],
                }),
                _ => refused.push(*index),
            }
        }
        (out, refused)
    }

    fn write_msrs(&self, msrs: &[X86Msr]) -> Vec<u32> {
        let mut refused = Vec::new();
        for msr in msrs {
            if READ_ONLY_MSRS.contains(&msr.index) {
                continue;
            }
            let Some((_, name)) = MSRS.iter().find(|(index, _)| *index == msr.index) else {
                // A snapshot from a build with a longer list. Not fatal on its
                // own; the caller sees the count.
                refused.push(msr.index);
                continue;
            };
            if self.write_u64s(&[*name], &[msr.data]).is_err() {
                refused.push(msr.index);
            }
        }
        refused
    }

    fn lapic_blob(&self) -> Result<Vec<u8>, HvError> {
        let mut buffer = vec![0u8; LAPIC_BUFFER];
        let mut written = 0u32;
        // SAFETY: the partition handle is live for the life of this vCPU, the
        // buffer is a live allocation of `LAPIC_BUFFER` bytes described by the
        // size argument, and `written` is a valid out-pointer. WHP writes at
        // most `LAPIC_BUFFER` bytes and reports how many.
        unsafe {
            WHvGetVirtualProcessorInterruptControllerState(
                self.partition_handle(),
                self.index,
                buffer.as_mut_ptr().cast(),
                LAPIC_BUFFER as u32,
                Some(&mut written),
            )
        }
        .map_err(|e| {
            HvError::Registers(format!(
                "vCPU {}: WHvGetVirtualProcessorInterruptControllerState: {e}",
                self.index
            ))
        })?;
        buffer.truncate(written as usize);
        Ok(buffer)
    }

    fn set_lapic_blob(&self, bytes: &[u8]) -> Result<(), HvError> {
        if bytes.is_empty() || bytes.len() > LAPIC_BUFFER {
            return Err(HvError::Registers(format!(
                "vCPU {}: snapshot local-APIC blob is {} bytes",
                self.index,
                bytes.len()
            )));
        }
        // SAFETY: as above, except that WHP only reads the buffer, and its
        // length is exactly the size argument.
        unsafe {
            WHvSetVirtualProcessorInterruptControllerState(
                self.partition_handle(),
                self.index,
                bytes.as_ptr().cast(),
                bytes.len() as u32,
            )
        }
        .map_err(|e| {
            HvError::Registers(format!(
                "vCPU {}: WHvSetVirtualProcessorInterruptControllerState: {e}",
                self.index
            ))
        })
    }

    fn xsave_blob(&self) -> Result<Vec<u8>, HvError> {
        let mut buffer = vec![0u8; XSAVE_BUFFER];
        let mut written = 0u32;
        // SAFETY: the same contract as the interrupt-controller call above —
        // live handle, live buffer of the stated size, valid out-pointer.
        unsafe {
            WHvGetVirtualProcessorXsaveState(
                self.partition_handle(),
                self.index,
                buffer.as_mut_ptr().cast(),
                XSAVE_BUFFER as u32,
                &mut written,
            )
        }
        .map_err(|e| {
            HvError::Registers(format!(
                "vCPU {}: WHvGetVirtualProcessorXsaveState: {e}",
                self.index
            ))
        })?;
        buffer.truncate(written as usize);
        Ok(buffer)
    }

    fn set_xsave_blob(&self, bytes: &[u8]) -> Result<(), HvError> {
        if bytes.is_empty() || bytes.len() > XSAVE_BUFFER {
            return Err(HvError::Registers(format!(
                "vCPU {}: snapshot XSAVE blob is {} bytes",
                self.index,
                bytes.len()
            )));
        }
        // SAFETY: live handle, live buffer whose length is the size argument,
        // read-only from WHP's side.
        unsafe {
            WHvSetVirtualProcessorXsaveState(
                self.partition_handle(),
                self.index,
                bytes.as_ptr().cast(),
                bytes.len() as u32,
            )
        }
        .map_err(|e| {
            HvError::Registers(format!(
                "vCPU {}: WHvSetVirtualProcessorXsaveState: {e}",
                self.index
            ))
        })
    }

    /// Everything this vCPU is. See the module docs for what is left out.
    pub(crate) fn snapshot(&self) -> Result<X86CpuState, HvError> {
        let registers = self.get_registers()?;
        let special_registers = self.get_special_registers()?;

        let (msrs, refused) = self.read_msrs();
        if !refused.is_empty() {
            tracing::debug!(
                vcpu = self.index,
                count = refused.len(),
                first = format_args!("{:#x}", refused[0]),
                "this partition would not report some of the MSRs WHP names; skipped"
            );
        }

        let events_raw = self.read_u64s(&[
            WHvRegisterPendingInterruption,
            WHvRegisterInterruptState,
            WHvRegisterInternalActivityState,
        ])?;
        let (pending, interrupt_state, activity) = (events_raw[0], events_raw[1], events_raw[2]);

        // A pending *exception event* is 128 bits with a fault parameter this
        // build has nowhere to put. Refuse rather than drop it.
        let mut event = Aligned16([zeroed_value(); MAX_BATCH]);
        self.get_raw(&[WHvRegisterPendingEvent], &mut event.0[..1])
            .map_err(|e| HvError::Registers(e.to_string()))?;
        // SAFETY: `Reg128` is one arm of the same all-integer `repr(C)` union;
        // reading it is defined for any bit pattern, and its low word carries
        // the `EventPending` bit whichever event kind WHP reported.
        let event_low = unsafe { event.0[0].Reg128.Anonymous.Low64 };
        if event_low & 1 != 0 {
            return Err(HvError::Registers(format!(
                "vCPU {}: a pending exception event ({event_low:#x}) this build cannot carry \
                 across a snapshot",
                self.index
            )));
        }

        let xcr0 = self.read_u64s(&[WHvX64RegisterXCr0])?[0];
        let debug = self.read_u64s(&DEBUG_NAMES)?;

        Ok(X86CpuState {
            index: self.index,
            registers,
            special_registers,
            msrs,
            xcr0,
            xsave: X86OpaqueState::new(BlobFormat::XsaveArea, self.xsave_blob()?),
            lapic: X86OpaqueState::new(BlobFormat::WhpInterruptController, self.lapic_blob()?),
            mp_state: mp_state_from_activity(activity),
            events: events_from_whp(pending, interrupt_state),
            debug_registers: X86DebugRegisters {
                db: [debug[0], debug[1], debug[2], debug[3]],
                dr6: debug[4],
                dr7: debug[5],
            },
        })
    }

    /// Puts a snapshotted state back, in the order WHP needs it.
    ///
    /// 1. **The local APIC first**, because `IA32_TSC_DEADLINE` below is
    ///    meaningless until the timer it arms is back.
    /// 2. **`XCR0` before the XSAVE blob**, which is what decides which
    ///    components the blob may carry.
    /// 3. **The activity state last of the "where is this CPU" group**: putting
    ///    an application processor back into startup-suspend has to happen after
    ///    its registers, or the writes land on a CPU WHP thinks is running.
    /// 4. **Pending events last of all**, so nothing above can clear an
    ///    interrupt the restored guest is owed.
    pub(crate) fn restore(&mut self, state: &X86CpuState) -> Result<(), HvError> {
        self.set_lapic_blob(state.lapic.expect(BlobFormat::WhpInterruptController)?)?;
        self.set_special_registers(&state.special_registers)?;
        self.set_registers(&state.registers)?;

        self.write_u64s(&[WHvX64RegisterXCr0], &[state.xcr0])?;
        self.set_xsave_blob(state.xsave.expect(BlobFormat::XsaveArea)?)?;

        let refused = self.write_msrs(&state.msrs);
        if !refused.is_empty() {
            tracing::warn!(
                vcpu = self.index,
                count = refused.len(),
                first = format_args!("{:#x}", refused[0]),
                "this partition refused to restore some MSRs the snapshot carries"
            );
        }

        self.write_u64s(
            &DEBUG_NAMES,
            &[
                state.debug_registers.db[0],
                state.debug_registers.db[1],
                state.debug_registers.db[2],
                state.debug_registers.db[3],
                state.debug_registers.dr6,
                state.debug_registers.dr7,
            ],
        )?;

        self.write_u64s(
            &[WHvRegisterInternalActivityState],
            &[activity_from_mp_state(state.mp_state)],
        )?;
        let (pending, interrupt_state) = events_to_whp(&state.events);
        self.write_u64s(
            &[WHvRegisterPendingInterruption, WHvRegisterInterruptState],
            &[pending, interrupt_state],
        )
    }
}

/// WHP's internal activity word as the neutral [`MpState`].
///
/// `StartupSuspend` is the one that matters: it is an application processor
/// that has not had its INIT/SIPI yet, and a restored VM that brought it back
/// runnable would have a CPU executing whatever was in its registers.
fn mp_state_from_activity(activity: u64) -> MpState {
    if activity & ACTIVITY_STARTUP_SUSPEND != 0 {
        MpState::Uninitialized
    } else if activity & ACTIVITY_HALT_SUSPEND != 0 {
        MpState::Halted
    } else {
        MpState::Runnable
    }
}

fn activity_from_mp_state(state: MpState) -> u64 {
    match state {
        // `InitReceived` and `SipiReceived` are KVM's finer grain; WHP models
        // the same waiting CPU with one bit, and a restored one that is not yet
        // running belongs in startup-suspend either way.
        MpState::Uninitialized | MpState::InitReceived | MpState::SipiReceived => {
            ACTIVITY_STARTUP_SUSPEND
        }
        MpState::Halted => ACTIVITY_HALT_SUSPEND,
        MpState::Runnable | MpState::Stopped => 0,
    }
}

/// WHP's pending-interruption and interrupt-state words as neutral events.
fn events_from_whp(pending: u64, interrupt_state: u64) -> X86PendingEvents {
    let mut events = X86PendingEvents {
        interrupt_shadow: u8::from(interrupt_state & STATE_SHADOW != 0),
        nmi_masked: interrupt_state & STATE_NMI_MASKED != 0,
        ..X86PendingEvents::default()
    };
    if pending & INT_PENDING == 0 {
        return events;
    }
    let kind = (pending >> INT_TYPE_SHIFT) & INT_TYPE_MASK;
    let vector = ((pending >> INT_VECTOR_SHIFT) & INT_VECTOR_MASK) as u8;
    let error_code = (pending >> INT_ERROR_SHIFT) as u32;
    let has_error = pending & INT_DELIVER_ERROR != 0;
    match kind {
        TYPE_NMI => events.nmi_injected = true,
        TYPE_HARDWARE_EXCEPTION => {
            events.exception_injected = true;
            events.exception_vector = vector;
            events.exception_has_error_code = has_error;
            events.exception_error_code = error_code;
        }
        other => {
            events.interrupt_injected = true;
            events.interrupt_vector = vector;
            events.interrupt_soft = other >= TYPE_SOFTWARE_INTERRUPT;
        }
    }
    // The bits the neutral fields have nowhere for, so the register can be
    // rebuilt exactly rather than approximately.
    events.host_event_flags = FLAGS_PENDING
        | ((kind as u32) << FLAGS_TYPE_SHIFT)
        | ((((pending >> INT_LEN_SHIFT) & INT_LEN_MASK) as u32) << FLAGS_LEN_SHIFT)
        | if pending & INT_NESTED != 0 {
            FLAGS_NESTED
        } else {
            0
        };
    events
}

fn events_to_whp(events: &X86PendingEvents) -> (u64, u64) {
    let interrupt_state = (u64::from(events.interrupt_shadow != 0) * STATE_SHADOW)
        | (u64::from(events.nmi_masked) * STATE_NMI_MASKED);
    if events.host_event_flags & FLAGS_PENDING == 0 {
        return (0, interrupt_state);
    }
    let kind = u64::from((events.host_event_flags >> FLAGS_TYPE_SHIFT) & 0b111);
    let length = u64::from((events.host_event_flags >> FLAGS_LEN_SHIFT) & 0b1111);
    let nested = events.host_event_flags & FLAGS_NESTED != 0;
    let (vector, has_error, error_code) = match kind {
        TYPE_NMI => (0u8, false, 0u32),
        TYPE_HARDWARE_EXCEPTION => (
            events.exception_vector,
            events.exception_has_error_code,
            events.exception_error_code,
        ),
        _ => (events.interrupt_vector, false, 0),
    };
    let pending = INT_PENDING
        | ((kind & INT_TYPE_MASK) << INT_TYPE_SHIFT)
        | if has_error { INT_DELIVER_ERROR } else { 0 }
        | ((length & INT_LEN_MASK) << INT_LEN_SHIFT)
        | if nested { INT_NESTED } else { 0 }
        | (u64::from(vector) << INT_VECTOR_SHIFT)
        | (u64::from(error_code) << INT_ERROR_SHIFT);
    (pending, interrupt_state)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every MSR is listed once, and the architectural indices are the ones
    /// the file will carry.
    #[test]
    fn the_msr_table_has_no_duplicates_and_names_the_ones_that_matter() {
        let mut indices: Vec<u32> = MSRS.iter().map(|(index, _)| *index).collect();
        let count = indices.len();
        indices.sort_unstable();
        indices.dedup();
        assert_eq!(indices.len(), count, "a duplicate MSR index");
        for wanted in [
            0x0000_0010u32, // TSC
            0x0000_0174,    // SYSENTER_CS
            0x0000_0277,    // PAT
            0x0000_06e0,    // TSC_DEADLINE
            0xc000_0080,    // EFER
            0xc000_0081,    // STAR
            0xc000_0082,    // LSTAR
            0xc000_0083,    // CSTAR
            0xc000_0084,    // SFMASK
            0xc000_0102,    // KERNEL_GS_BASE
            0xc000_0103,    // TSC_AUX
        ] {
            assert!(indices.contains(&wanted), "missing MSR {wanted:#x}");
        }
    }

    #[test]
    fn the_activity_word_round_trips_for_every_state_whp_can_express() {
        for state in [MpState::Uninitialized, MpState::Halted, MpState::Runnable] {
            assert_eq!(
                mp_state_from_activity(activity_from_mp_state(state)),
                state,
                "{state:?}"
            );
        }
        // KVM's finer INIT/SIPI grain collapses onto the one bit WHP has, and
        // must land on the safe side: still waiting, not running.
        assert_eq!(
            mp_state_from_activity(activity_from_mp_state(MpState::InitReceived)),
            MpState::Uninitialized
        );
    }

    /// The interruption register must come back **bit for bit**: it is the one
    /// piece of state nothing else can reconstruct, and an approximation of it
    /// is an interrupt delivered with the wrong vector.
    #[test]
    fn the_pending_interruption_register_round_trips_exactly() {
        let cases: [(u64, u64); 6] = [
            (0, 0),
            (0, STATE_SHADOW | STATE_NMI_MASKED),
            // External interrupt, vector 0x30.
            (INT_PENDING | (TYPE_EXTERNAL << 1) | (0x30 << 16), 0),
            // NMI.
            (INT_PENDING | (TYPE_NMI << 1), STATE_NMI_MASKED),
            // #PF with an error code and no instruction length.
            (
                INT_PENDING
                    | (TYPE_HARDWARE_EXCEPTION << 1)
                    | INT_DELIVER_ERROR
                    | (14 << 16)
                    | (0xdead_beefu64 << 32),
                0,
            ),
            // `int 0x80`: a software interrupt with an instruction length.
            (
                INT_PENDING | (TYPE_SOFTWARE_INTERRUPT << 1) | (2 << INT_LEN_SHIFT) | (0x80 << 16),
                STATE_SHADOW,
            ),
        ];
        for (pending, state) in cases {
            let events = events_from_whp(pending, state);
            assert_eq!(
                events_to_whp(&events),
                (pending, state),
                "pending {pending:#x} state {state:#x}"
            );
        }
    }

    /// A software interrupt is not an exception, and an NMI is neither.
    #[test]
    fn the_interruption_type_reaches_the_right_neutral_field() {
        let external = events_from_whp(INT_PENDING | (TYPE_EXTERNAL << 1) | (0x21 << 16), 0);
        assert!(external.interrupt_injected && !external.interrupt_soft);
        assert_eq!(external.interrupt_vector, 0x21);

        let nmi = events_from_whp(INT_PENDING | (TYPE_NMI << 1), 0);
        assert!(nmi.nmi_injected && !nmi.interrupt_injected);

        let fault = events_from_whp(
            INT_PENDING
                | (TYPE_HARDWARE_EXCEPTION << 1)
                | INT_DELIVER_ERROR
                | (13 << 16)
                | (7 << 32),
            0,
        );
        assert!(fault.exception_injected);
        assert_eq!(fault.exception_vector, 13);
        assert!(fault.exception_has_error_code);
        assert_eq!(fault.exception_error_code, 7);

        let soft = events_from_whp(
            INT_PENDING | (TYPE_SOFTWARE_INTERRUPT << 1) | (0x80 << 16),
            0,
        );
        assert!(soft.interrupt_injected && soft.interrupt_soft);
    }

    /// Nothing pending must encode as an all-zero register, so a resumed vCPU
    /// is not handed an interruption the snapshot never recorded.
    #[test]
    fn nothing_pending_encodes_as_zero() {
        let quiet = events_from_whp(0, 0);
        assert_eq!(quiet.host_event_flags, 0);
        assert_eq!(events_to_whp(&quiet), (0, 0));
    }
}
