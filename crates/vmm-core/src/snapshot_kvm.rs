//! Reading and writing a KVM vCPU's *whole* architectural state
//! ([ADR-0006](../../../../docs/adr/0006-suspend-restore.md)).
//!
//! # Getting the list right matters more than getting it fast
//!
//! A snapshot that forgets one MSR is not a snapshot that is slightly wrong; it
//! is a guest that resumes and then dies somewhere unrelated. Lose
//! `KERNEL_GS_BASE` and the next `swapgs` in a syscall entry lands the kernel on
//! a null per-CPU base. Lose `LSTAR` and the first `syscall` after resume jumps
//! to zero. Lose `PAT` and the framebuffer becomes uncacheable. None of those
//! failures point back here.
//!
//! So the list is not written down. `KVM_GET_MSR_INDEX_LIST` is the kernel
//! saying which MSRs *it* is prepared to save and restore on this host, and
//! that is what gets read — every one of them, minus a documented handful, with
//! the ones the vCPU refuses dropped one at a time rather than losing the whole
//! batch. On a 6.x kernel that is around 100 registers, which is a few hundred
//! microseconds and no maintenance.
//!
//! # What is deliberately not here
//!
//! * **The x2APIC MSR window (`0x800..=0x8ff`)**, when the kernel offers it: it
//!   is the same local APIC the `KVM_GET_LAPIC` page already carries, and
//!   writing both back would have the two fight over which one wins.
//! * **`KVM_GET_XSAVE2`'s dynamic components** (AMX tile data). `KVM_GET_XSAVE`
//!   carries the fixed 4 KiB area, which is everything up to and including
//!   AVX-512; a guest that had negotiated AMX would lose its tile registers.
//!   This machine's CPUID policy does not offer AMX, so there is nothing to
//!   lose today — but it is the first thing to add if it ever does.
//! * **Nested virtualization state** (`KVM_GET_NESTED_STATE`). The CPUID policy
//!   does not expose VMX or SVM to the guest, so there is no L2 to save.

use kvm_bindings::{
    kvm_debugregs, kvm_lapic_state, kvm_mp_state, kvm_msr_entry, kvm_vcpu_events, kvm_xcrs,
    kvm_xsave, Msrs,
};

use crate::hv::{
    BlobFormat, HvError, MpState, VcpuRegisters, X86CpuState, X86DebugRegisters, X86Msr,
    X86OpaqueState, X86PendingEvents,
};
use crate::vcpu::Vcpu;

/// `IA32_XSS`-adjacent register indices we never save: the x2APIC window is the
/// local APIC page in disguise.
const X2APIC_MSR_FIRST: u32 = 0x800;
const X2APIC_MSR_LAST: u32 = 0x8ff;

/// How many MSRs go into one `KVM_GET_MSRS`/`KVM_SET_MSRS` call.
///
/// The ioctl takes a FAM struct, so a batch is one allocation; the only reason
/// to bound it is that a partial failure costs a re-split of whatever is in the
/// batch, and a hundred-entry retry is cheaper to reason about than a thousand.
const MSR_BATCH: usize = 256;

/// `kvm_xsave` is a fixed 4 KiB region of `u32`s.
const XSAVE_BYTES: usize = std::mem::size_of::<kvm_xsave>();

/// `kvm_lapic_state` is the architectural 1 KiB APIC page.
#[cfg(test)]
const LAPIC_BYTES: usize = std::mem::size_of::<kvm_lapic_state>();

fn err(index: u32, what: &str, e: kvm_ioctls::Error) -> HvError {
    HvError::Registers(format!("vCPU {index}: {what} failed: {e}"))
}

impl Vcpu {
    /// Which MSRs to attempt, in ascending order.
    fn snapshot_msr_indices(&self) -> Vec<u32> {
        let mut indices: Vec<u32> = self
            .msr_index_list()
            .iter()
            .copied()
            .filter(|index| !(X2APIC_MSR_FIRST..=X2APIC_MSR_LAST).contains(index))
            .collect();
        indices.sort_unstable();
        indices.dedup();
        indices
    }

    /// Reads `indices` in batches, dropping the ones this vCPU refuses.
    ///
    /// `KVM_GET_MSRS` returns *how many* entries it filled and stops at the
    /// first one it cannot answer, so a single unsupported register would
    /// otherwise cost every register behind it. Dropping the offender and
    /// carrying on is what turns the kernel's generous index list into the set
    /// this vCPU actually has.
    fn read_msrs(&self, indices: &[u32]) -> Result<(Vec<X86Msr>, Vec<u32>), HvError> {
        let mut out = Vec::with_capacity(indices.len());
        let mut refused = Vec::new();
        let mut rest = indices;
        while !rest.is_empty() {
            let take = rest.len().min(MSR_BATCH);
            let entries: Vec<kvm_msr_entry> = rest[..take]
                .iter()
                .map(|&index| kvm_msr_entry {
                    index,
                    ..Default::default()
                })
                .collect();
            let mut msrs = Msrs::from_entries(&entries).map_err(|e| {
                HvError::Registers(format!("vCPU {}: MSR batch: {e:?}", self.index))
            })?;
            let read = self
                .fd()
                .get_msrs(&mut msrs)
                .map_err(|e| err(self.index, "KVM_GET_MSRS", e))?;
            for entry in msrs.as_slice().iter().take(read) {
                out.push(X86Msr {
                    index: entry.index,
                    data: entry.data,
                });
            }
            if read < take {
                // `rest[read]` is the one that stopped the batch.
                refused.push(rest[read]);
                rest = &rest[read + 1..];
            } else {
                rest = &rest[take..];
            }
        }
        Ok((out, refused))
    }

    /// Writes `msrs` back, dropping the ones this vCPU refuses the same way.
    fn write_msrs(&self, msrs: &[X86Msr]) -> Result<Vec<u32>, HvError> {
        let mut refused = Vec::new();
        let mut rest = msrs;
        while !rest.is_empty() {
            let take = rest.len().min(MSR_BATCH);
            let entries: Vec<kvm_msr_entry> = rest[..take]
                .iter()
                .map(|msr| kvm_msr_entry {
                    index: msr.index,
                    data: msr.data,
                    ..Default::default()
                })
                .collect();
            let batch = Msrs::from_entries(&entries).map_err(|e| {
                HvError::Registers(format!("vCPU {}: MSR batch: {e:?}", self.index))
            })?;
            let written = self
                .fd()
                .set_msrs(&batch)
                .map_err(|e| err(self.index, "KVM_SET_MSRS", e))?;
            if written < take {
                refused.push(rest[written].index);
                rest = &rest[written + 1..];
            } else {
                rest = &rest[take..];
            }
        }
        Ok(refused)
    }
}

/// KVM's `mp_state` word as a neutral value.
pub(crate) fn mp_state_from_kvm(value: u32) -> MpState {
    match value {
        kvm_bindings::KVM_MP_STATE_RUNNABLE => MpState::Runnable,
        kvm_bindings::KVM_MP_STATE_UNINITIALIZED => MpState::Uninitialized,
        kvm_bindings::KVM_MP_STATE_INIT_RECEIVED => MpState::InitReceived,
        kvm_bindings::KVM_MP_STATE_HALTED => MpState::Halted,
        kvm_bindings::KVM_MP_STATE_SIPI_RECEIVED => MpState::SipiReceived,
        // Everything else KVM can report (`STOPPED`, and the s390/arm values
        // that cannot occur here) means "not executing".
        _ => MpState::Stopped,
    }
}

fn mp_state_to_kvm(state: MpState) -> u32 {
    match state {
        MpState::Runnable => kvm_bindings::KVM_MP_STATE_RUNNABLE,
        MpState::Uninitialized => kvm_bindings::KVM_MP_STATE_UNINITIALIZED,
        MpState::InitReceived => kvm_bindings::KVM_MP_STATE_INIT_RECEIVED,
        MpState::Halted => kvm_bindings::KVM_MP_STATE_HALTED,
        MpState::SipiReceived => kvm_bindings::KVM_MP_STATE_SIPI_RECEIVED,
        MpState::Stopped => kvm_bindings::KVM_MP_STATE_STOPPED,
    }
}

fn events_from_kvm(events: &kvm_vcpu_events) -> X86PendingEvents {
    X86PendingEvents {
        exception_injected: events.exception.injected != 0,
        exception_pending: events.exception.pending != 0,
        exception_vector: events.exception.nr,
        exception_has_error_code: events.exception.has_error_code != 0,
        exception_error_code: events.exception.error_code,
        interrupt_injected: events.interrupt.injected != 0,
        interrupt_vector: events.interrupt.nr,
        interrupt_soft: events.interrupt.soft != 0,
        interrupt_shadow: events.interrupt.shadow,
        nmi_injected: events.nmi.injected != 0,
        nmi_pending: events.nmi.pending != 0,
        nmi_masked: events.nmi.masked != 0,
        sipi_vector: events.sipi_vector,
        smi_smm: events.smi.smm != 0,
        smi_pending: events.smi.pending != 0,
        smi_inside_nmi: events.smi.smm_inside_nmi != 0,
        smi_latched_init: events.smi.latched_init,
        host_event_flags: events.flags,
    }
}

fn events_to_kvm(events: &X86PendingEvents) -> kvm_vcpu_events {
    let mut out = kvm_vcpu_events {
        flags: events.host_event_flags,
        sipi_vector: events.sipi_vector,
        ..Default::default()
    };
    out.exception.injected = u8::from(events.exception_injected);
    out.exception.pending = u8::from(events.exception_pending);
    out.exception.nr = events.exception_vector;
    out.exception.has_error_code = u8::from(events.exception_has_error_code);
    out.exception.error_code = events.exception_error_code;
    out.interrupt.injected = u8::from(events.interrupt_injected);
    out.interrupt.nr = events.interrupt_vector;
    out.interrupt.soft = u8::from(events.interrupt_soft);
    out.interrupt.shadow = events.interrupt_shadow;
    out.nmi.injected = u8::from(events.nmi_injected);
    out.nmi.pending = u8::from(events.nmi_pending);
    out.nmi.masked = u8::from(events.nmi_masked);
    out.smi.smm = u8::from(events.smi_smm);
    out.smi.pending = u8::from(events.smi_pending);
    out.smi.smm_inside_nmi = u8::from(events.smi_inside_nmi);
    out.smi.latched_init = events.smi_latched_init;
    out
}

/// The XSAVE area as bytes. `kvm_xsave` is `#[repr(C)]` over a `[u32; 1024]`
/// with no padding and no pointers, so this is a plain re-view of the same
/// 4 KiB the architecture defines.
fn xsave_bytes(xsave: &kvm_xsave) -> Vec<u8> {
    let mut out = Vec::with_capacity(XSAVE_BYTES);
    for word in xsave.region.iter() {
        out.extend_from_slice(&word.to_le_bytes());
    }
    // Any trailing fields (`extra` on kernels with XSAVE2) are not part of what
    // `KVM_GET_XSAVE` filled, so the region is the whole story.
    out.truncate(xsave.region.len() * 4);
    out
}

fn xsave_from_bytes(bytes: &[u8], index: u32) -> Result<kvm_xsave, HvError> {
    let mut xsave = kvm_xsave::default();
    let words = xsave.region.len();
    if bytes.len() != words * 4 {
        return Err(HvError::Registers(format!(
            "vCPU {index}: snapshot XSAVE area is {} bytes, this host wants {}",
            bytes.len(),
            words * 4
        )));
    }
    for (slot, chunk) in xsave.region.iter_mut().zip(bytes.chunks_exact(4)) {
        *slot = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    Ok(xsave)
}

/// Bits of the XSTATE bit vector that belong to *dynamically enabled* features
/// (AMX and anything the architecture adds above it). Bit 63 of `XCOMP_BV` is
/// the compaction flag, not a component, so it is not in the mask.
const DYNAMIC_XSTATE_MASK: u64 = 0x7fff_ffff_fffe_0000;

/// Offsets of the two XSTATE header words inside the XSAVE area.
const XSTATE_BV_AT: usize = 512;
const XCOMP_BV_AT: usize = 520;

/// Refuses an XSAVE area that claims a dynamically enabled component.
///
/// Such an area is larger than the 4 KiB `kvm_xsave` this build moves, so it
/// could only have come from a host that had AMX (or its successors) enabled —
/// and feeding it to `KVM_SET_XSAVE` would have the kernel read past the
/// struct. A refusal names the problem; a silent truncation would be a guest
/// with corrupt floating-point state.
fn refuse_dynamic_xstate(xsave: &kvm_xsave, index: u32) -> Result<(), HvError> {
    let word = |at: usize| -> u64 {
        let lo = xsave.region.get(at / 4).copied().unwrap_or(0);
        let hi = xsave.region.get(at / 4 + 1).copied().unwrap_or(0);
        u64::from(lo) | (u64::from(hi) << 32)
    };
    let claimed = (word(XSTATE_BV_AT) | word(XCOMP_BV_AT)) & DYNAMIC_XSTATE_MASK;
    if claimed != 0 {
        return Err(HvError::Registers(format!(
            "vCPU {index}: snapshot XSAVE area claims dynamically enabled XSTATE components              ({claimed:#x}); this build carries only the fixed 4 KiB area"
        )));
    }
    Ok(())
}

fn lapic_bytes(lapic: &kvm_lapic_state) -> Vec<u8> {
    lapic.regs.iter().map(|&c| c as u8).collect()
}

fn lapic_from_bytes(bytes: &[u8], index: u32) -> Result<kvm_lapic_state, HvError> {
    let mut lapic = kvm_lapic_state::default();
    if bytes.len() != lapic.regs.len() {
        return Err(HvError::Registers(format!(
            "vCPU {index}: snapshot local-APIC page is {} bytes, this host wants {}",
            bytes.len(),
            lapic.regs.len()
        )));
    }
    for (slot, &byte) in lapic.regs.iter_mut().zip(bytes) {
        *slot = byte as std::os::raw::c_char;
    }
    Ok(lapic)
}

impl Vcpu {
    /// Everything this vCPU is. See the module docs for what is left out.
    pub(crate) fn snapshot(&self) -> Result<X86CpuState, HvError> {
        let index = self.index;
        let fd = self.fd();

        let registers = self.get_registers()?;
        let special_registers = self.get_special_registers()?;

        let indices = self.snapshot_msr_indices();
        let (msrs, refused) = self.read_msrs(&indices)?;
        if !refused.is_empty() {
            tracing::debug!(
                vcpu = index,
                count = refused.len(),
                first = format_args!("{:#x}", refused[0]),
                "some MSRs on this host's save list are not readable on this vCPU; skipped"
            );
        }

        let xcrs = fd.get_xcrs().map_err(|e| err(index, "KVM_GET_XCRS", e))?;
        // XCR0 is register 0; a host that reports none leaves the guest with
        // legacy x87/SSE state only, which is the architectural default.
        let xcr0 = xcrs
            .xcrs
            .iter()
            .take(xcrs.nr_xcrs as usize)
            .find(|x| x.xcr == 0)
            .map(|x| x.value)
            .unwrap_or(1);

        let xsave = fd.get_xsave().map_err(|e| err(index, "KVM_GET_XSAVE", e))?;
        let lapic = fd.get_lapic().map_err(|e| err(index, "KVM_GET_LAPIC", e))?;
        let mp_state = fd
            .get_mp_state()
            .map_err(|e| err(index, "KVM_GET_MP_STATE", e))?;
        let events = fd
            .get_vcpu_events()
            .map_err(|e| err(index, "KVM_GET_VCPU_EVENTS", e))?;
        let debug = fd
            .get_debug_regs()
            .map_err(|e| err(index, "KVM_GET_DEBUGREGS", e))?;

        Ok(X86CpuState {
            index,
            registers,
            special_registers,
            msrs,
            xcr0,
            xsave: X86OpaqueState::new(BlobFormat::XsaveArea, xsave_bytes(&xsave)),
            lapic: X86OpaqueState::new(BlobFormat::KvmLapicPage, lapic_bytes(&lapic)),
            mp_state: mp_state_from_kvm(mp_state.mp_state),
            events: events_from_kvm(&events),
            debug_registers: X86DebugRegisters {
                db: debug.db,
                dr6: debug.dr6,
                dr7: debug.dr7,
            },
        })
    }

    /// Puts a snapshotted state back, in the order KVM needs it.
    ///
    /// 1. **`mp_state` first.** An application processor that was waiting for
    ///    its INIT/SIPI must be put back into that wait before anything else
    ///    touches it, or KVM treats the writes as belonging to a running CPU.
    /// 2. **The local APIC before the MSRs.** `IA32_TSC_DEADLINE` is meaningless
    ///    until the APIC timer it arms exists, and `APIC_BASE` arrives with the
    ///    special registers below.
    /// 3. **`XCR0` before the XSAVE area.** The enable mask decides which
    ///    components the area is allowed to carry; writing the area first has
    ///    KVM reject the components the guest had enabled.
    /// 4. **Pending events last.** They are the one piece of state a later
    ///    write could clear, and the restored guest must find its interrupt
    ///    still waiting.
    pub(crate) fn restore(&mut self, state: &X86CpuState) -> Result<(), HvError> {
        let index = self.index;
        let fd = self.fd();

        fd.set_mp_state(kvm_mp_state {
            mp_state: mp_state_to_kvm(state.mp_state),
        })
        .map_err(|e| err(index, "KVM_SET_MP_STATE", e))?;

        let lapic = lapic_from_bytes(state.lapic.expect(BlobFormat::KvmLapicPage)?, index)?;
        fd.set_lapic(&lapic)
            .map_err(|e| err(index, "KVM_SET_LAPIC", e))?;

        self.set_special_registers(&state.special_registers)?;
        self.set_registers(&state.registers)?;

        let mut xcrs = kvm_xcrs {
            nr_xcrs: 1,
            ..Default::default()
        };
        if let Some(slot) = xcrs.xcrs.first_mut() {
            slot.xcr = 0;
            slot.value = state.xcr0;
        }
        fd.set_xcrs(&xcrs)
            .map_err(|e| err(index, "KVM_SET_XCRS", e))?;

        let xsave = xsave_from_bytes(state.xsave.expect(BlobFormat::XsaveArea)?, index)?;
        refuse_dynamic_xstate(&xsave, index)?;
        // SAFETY: `KVM_SET_XSAVE` reads past the traditional 4096-byte
        // `kvm_xsave` only when the *host task* has dynamically enabled an
        // XSTATE feature through `arch_prctl(ARCH_REQ_XCOMP_GUEST_PERM)`, which
        // grows the kernel's guest xstate size beyond that struct. This process
        // never calls `arch_prctl`, and this machine's CPUID policy offers the
        // guest no dynamic component to ask for; `refuse_dynamic_xstate` above
        // additionally refuses a snapshot whose own XSTATE header claims one,
        // so the kernel cannot be handed an area it would read past the end of.
        unsafe { fd.set_xsave(&xsave) }.map_err(|e| err(index, "KVM_SET_XSAVE", e))?;

        let refused = self.write_msrs(&state.msrs)?;
        if !refused.is_empty() {
            // Worth a warning rather than a debug line: on the way *in* a
            // refused MSR is a register the guest had and no longer has.
            tracing::warn!(
                vcpu = index,
                count = refused.len(),
                first = format_args!("{:#x}", refused[0]),
                "this host refused to restore some MSRs the snapshot carries"
            );
        }

        fd.set_debug_regs(&kvm_debugregs {
            db: state.debug_registers.db,
            dr6: state.debug_registers.dr6,
            dr7: state.debug_registers.dr7,
            ..Default::default()
        })
        .map_err(|e| err(index, "KVM_SET_DEBUGREGS", e))?;

        fd.set_vcpu_events(&events_to_kvm(&state.events))
            .map_err(|e| err(index, "KVM_SET_VCPU_EVENTS", e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The neutral `mp_state` maps onto KVM's and back for every value both
    /// sides can express — the one that matters is `Uninitialized`, which is
    /// where an application processor waits for its INIT/SIPI.
    #[test]
    fn mp_state_round_trips() {
        for state in [
            MpState::Runnable,
            MpState::Uninitialized,
            MpState::InitReceived,
            MpState::Halted,
            MpState::SipiReceived,
            MpState::Stopped,
        ] {
            assert_eq!(
                mp_state_from_kvm(mp_state_to_kvm(state)),
                state,
                "{state:?}"
            );
        }
    }

    /// Every field of the pending-event set survives the trip in both
    /// directions — this is the state nothing else can reconstruct.
    #[test]
    fn pending_events_round_trip() {
        let events = X86PendingEvents {
            exception_injected: true,
            exception_pending: false,
            exception_vector: 14,
            exception_has_error_code: true,
            exception_error_code: 0xdead,
            interrupt_injected: true,
            interrupt_vector: 0x21,
            interrupt_soft: true,
            interrupt_shadow: 2,
            nmi_injected: false,
            nmi_pending: true,
            nmi_masked: true,
            sipi_vector: 0x9a,
            smi_smm: true,
            smi_pending: true,
            smi_inside_nmi: false,
            smi_latched_init: 3,
            host_event_flags: 0x1f,
        };
        assert_eq!(events_from_kvm(&events_to_kvm(&events)), events);
    }

    #[test]
    fn the_xsave_area_round_trips_as_bytes() {
        let mut xsave = kvm_xsave::default();
        for (i, word) in xsave.region.iter_mut().enumerate() {
            *word = (i as u32).wrapping_mul(0x0101_0101);
        }
        let bytes = xsave_bytes(&xsave);
        assert_eq!(bytes.len(), XSAVE_BYTES.min(xsave.region.len() * 4));
        let back = xsave_from_bytes(&bytes, 0).unwrap();
        assert_eq!(back.region, xsave.region);
    }

    #[test]
    fn a_wrong_sized_xsave_area_is_refused_rather_than_padded() {
        let err = xsave_from_bytes(&[0u8; 16], 0).unwrap_err();
        assert!(err.to_string().contains("XSAVE"), "{err}");
    }

    #[test]
    fn the_lapic_page_round_trips_as_bytes() {
        let mut lapic = kvm_lapic_state::default();
        for (i, byte) in lapic.regs.iter_mut().enumerate() {
            *byte = (i % 256) as std::os::raw::c_char;
        }
        let bytes = lapic_bytes(&lapic);
        assert_eq!(bytes.len(), LAPIC_BYTES.min(lapic.regs.len()));
        let back = lapic_from_bytes(&bytes, 0).unwrap();
        assert_eq!(back.regs, lapic.regs);
    }

    #[test]
    fn a_wrong_sized_lapic_page_is_refused() {
        let err = lapic_from_bytes(&[0u8; 8], 0).unwrap_err();
        assert!(err.to_string().contains("local-APIC"), "{err}");
    }

    /// A blob from the other hypervisor is refused by tag before it can reach
    /// an ioctl — the same refusal the snapshot header makes, one layer down.
    #[test]
    fn a_foreign_blob_is_refused_by_its_tag() {
        let blob = X86OpaqueState::new(BlobFormat::WhpInterruptController, vec![0; 1024]);
        let err = blob.expect(BlobFormat::KvmLapicPage).unwrap_err();
        assert!(
            err.to_string().contains("whp-interrupt-controller"),
            "{err}"
        );
    }
}
