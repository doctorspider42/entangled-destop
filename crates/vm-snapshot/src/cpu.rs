//! One vCPU's architectural state, on disk.
//!
//! Field order here **is** the format. Every value is written explicitly, in a
//! fixed width, so that adding a register to `X86CpuState` is a visible change
//! to this file and a bump of [`CPU_VERSION`] rather than a silent change of
//! layout.
//!
//! The two opaque blobs — the local-APIC page and the XSAVE area — carry the
//! tag of the hypervisor that produced them, and the loading backend refuses a
//! tag it did not write. That is a second lock on the same door as the file
//! header's host field: even a snapshot whose header was tampered with cannot
//! feed WHP's interrupt-controller state to `KVM_SET_LAPIC`.

use vmm_core::hv::{
    BlobFormat, MpState, VmClockState, X86CpuState, X86DebugRegisters, X86DescriptorTable, X86Msr,
    X86OpaqueState, X86PendingEvents, X86Registers, X86Segment, X86SpecialRegisters,
};

use crate::codec::{Reader, Writer};
use crate::error::{Result, SnapshotError};

/// Version of the CPU section's encoding.
pub const CPU_VERSION: u32 = 1;

/// Version of the clock section's encoding.
pub const CLOCK_VERSION: u32 = 1;

/// More MSRs than any x86 host reports (a 6.x kernel lists about a hundred),
/// and small enough that the count can never drive a large allocation.
const MAX_MSRS: usize = 4096;

/// Largest opaque blob: the XSAVE area is 4 KiB and the APIC page is 1 KiB, so
/// this is four orders of magnitude of headroom and still bounded.
const MAX_BLOB: usize = 1 << 20;

fn blob_code(format: BlobFormat) -> u32 {
    match format {
        BlobFormat::Absent => 0,
        BlobFormat::KvmLapicPage => 1,
        BlobFormat::WhpInterruptController => 2,
        BlobFormat::XsaveArea => 3,
    }
}

fn blob_from_code(code: u32) -> Result<BlobFormat> {
    Ok(match code {
        0 => BlobFormat::Absent,
        1 => BlobFormat::KvmLapicPage,
        2 => BlobFormat::WhpInterruptController,
        3 => BlobFormat::XsaveArea,
        other => {
            return Err(SnapshotError::BadValue {
                what: "opaque blob format",
                value: u64::from(other),
            })
        }
    })
}

fn mp_code(state: MpState) -> u32 {
    match state {
        MpState::Runnable => 0,
        MpState::Uninitialized => 1,
        MpState::InitReceived => 2,
        MpState::Halted => 3,
        MpState::SipiReceived => 4,
        MpState::Stopped => 5,
    }
}

fn mp_from_code(code: u32) -> Result<MpState> {
    Ok(match code {
        0 => MpState::Runnable,
        1 => MpState::Uninitialized,
        2 => MpState::InitReceived,
        3 => MpState::Halted,
        4 => MpState::SipiReceived,
        5 => MpState::Stopped,
        other => {
            return Err(SnapshotError::BadValue {
                what: "mp state",
                value: u64::from(other),
            })
        }
    })
}

fn put_registers(w: &mut Writer, r: &X86Registers) {
    for value in [
        r.rax, r.rbx, r.rcx, r.rdx, r.rsi, r.rdi, r.rsp, r.rbp, r.r8, r.r9, r.r10, r.r11, r.r12,
        r.r13, r.r14, r.r15, r.rip, r.rflags,
    ] {
        w.u64(value);
    }
}

fn get_registers(r: &mut Reader<'_>) -> Result<X86Registers> {
    let mut values = [0u64; 18];
    for slot in &mut values {
        *slot = r.u64("general register")?;
    }
    Ok(X86Registers {
        rax: values[0],
        rbx: values[1],
        rcx: values[2],
        rdx: values[3],
        rsi: values[4],
        rdi: values[5],
        rsp: values[6],
        rbp: values[7],
        r8: values[8],
        r9: values[9],
        r10: values[10],
        r11: values[11],
        r12: values[12],
        r13: values[13],
        r14: values[14],
        r15: values[15],
        rip: values[16],
        rflags: values[17],
    })
}

fn put_segment(w: &mut Writer, s: &X86Segment) {
    w.u64(s.base)
        .u32(s.limit)
        .u16(s.selector)
        .u8(s.type_)
        .u8(s.present)
        .u8(s.dpl)
        .u8(s.db)
        .u8(s.s)
        .u8(s.l)
        .u8(s.g)
        .u8(s.avl)
        .u8(s.unusable);
}

fn get_segment(r: &mut Reader<'_>) -> Result<X86Segment> {
    Ok(X86Segment {
        base: r.u64("segment base")?,
        limit: r.u32("segment limit")?,
        selector: r.u16("segment selector")?,
        type_: r.u8("segment type")?,
        present: r.u8("segment present")?,
        dpl: r.u8("segment dpl")?,
        db: r.u8("segment db")?,
        s: r.u8("segment s")?,
        l: r.u8("segment l")?,
        g: r.u8("segment g")?,
        avl: r.u8("segment avl")?,
        unusable: r.u8("segment unusable")?,
    })
}

fn put_special(w: &mut Writer, s: &X86SpecialRegisters) {
    for seg in [&s.cs, &s.ds, &s.es, &s.fs, &s.gs, &s.ss, &s.tr, &s.ldt] {
        put_segment(w, seg);
    }
    for table in [&s.gdt, &s.idt] {
        w.u64(table.base).u16(table.limit);
    }
    for value in [s.cr0, s.cr2, s.cr3, s.cr4, s.cr8, s.efer, s.apic_base] {
        w.u64(value);
    }
}

fn get_special(r: &mut Reader<'_>) -> Result<X86SpecialRegisters> {
    let cs = get_segment(r)?;
    let ds = get_segment(r)?;
    let es = get_segment(r)?;
    let fs = get_segment(r)?;
    let gs = get_segment(r)?;
    let ss = get_segment(r)?;
    let tr = get_segment(r)?;
    let ldt = get_segment(r)?;
    let gdt = X86DescriptorTable {
        base: r.u64("gdt base")?,
        limit: r.u16("gdt limit")?,
    };
    let idt = X86DescriptorTable {
        base: r.u64("idt base")?,
        limit: r.u16("idt limit")?,
    };
    Ok(X86SpecialRegisters {
        cs,
        ds,
        es,
        fs,
        gs,
        ss,
        tr,
        ldt,
        gdt,
        idt,
        cr0: r.u64("cr0")?,
        cr2: r.u64("cr2")?,
        cr3: r.u64("cr3")?,
        cr4: r.u64("cr4")?,
        cr8: r.u64("cr8")?,
        efer: r.u64("efer")?,
        apic_base: r.u64("apic base")?,
    })
}

fn put_events(w: &mut Writer, e: &X86PendingEvents) {
    w.bool(e.exception_injected)
        .bool(e.exception_pending)
        .u8(e.exception_vector)
        .bool(e.exception_has_error_code)
        .u32(e.exception_error_code)
        .bool(e.interrupt_injected)
        .u8(e.interrupt_vector)
        .bool(e.interrupt_soft)
        .u8(e.interrupt_shadow)
        .bool(e.nmi_injected)
        .bool(e.nmi_pending)
        .bool(e.nmi_masked)
        .u32(e.sipi_vector)
        .bool(e.smi_smm)
        .bool(e.smi_pending)
        .bool(e.smi_inside_nmi)
        .u8(e.smi_latched_init)
        .u32(e.host_event_flags);
}

fn get_events(r: &mut Reader<'_>) -> Result<X86PendingEvents> {
    Ok(X86PendingEvents {
        exception_injected: r.bool("exception injected")?,
        exception_pending: r.bool("exception pending")?,
        exception_vector: r.u8("exception vector")?,
        exception_has_error_code: r.bool("exception has error code")?,
        exception_error_code: r.u32("exception error code")?,
        interrupt_injected: r.bool("interrupt injected")?,
        interrupt_vector: r.u8("interrupt vector")?,
        interrupt_soft: r.bool("interrupt soft")?,
        interrupt_shadow: r.u8("interrupt shadow")?,
        nmi_injected: r.bool("nmi injected")?,
        nmi_pending: r.bool("nmi pending")?,
        nmi_masked: r.bool("nmi masked")?,
        sipi_vector: r.u32("sipi vector")?,
        smi_smm: r.bool("smi smm")?,
        smi_pending: r.bool("smi pending")?,
        smi_inside_nmi: r.bool("smi inside nmi")?,
        smi_latched_init: r.u8("smi latched init")?,
        host_event_flags: r.u32("host event flags")?,
    })
}

fn put_blob(w: &mut Writer, blob: &X86OpaqueState) {
    w.u32(blob_code(blob.format)).blob(&blob.bytes);
}

fn get_blob(r: &mut Reader<'_>, what: &'static str) -> Result<X86OpaqueState> {
    let format = blob_from_code(r.u32(what)?)?;
    let bytes = r.blob(what, MAX_BLOB)?.to_vec();
    if format == BlobFormat::Absent && !bytes.is_empty() {
        return Err(SnapshotError::BadValue {
            what: "absent blob with contents",
            value: bytes.len() as u64,
        });
    }
    Ok(X86OpaqueState { format, bytes })
}

/// Encodes one vCPU's state.
pub fn encode(state: &X86CpuState) -> Vec<u8> {
    let mut w = Writer::with_capacity(8 * 1024);
    w.u32(state.index);
    put_registers(&mut w, &state.registers);
    put_special(&mut w, &state.special_registers);
    w.count(state.msrs.len());
    for msr in &state.msrs {
        w.u32(msr.index).u64(msr.data);
    }
    w.u64(state.xcr0);
    put_blob(&mut w, &state.xsave);
    put_blob(&mut w, &state.lapic);
    w.u32(mp_code(state.mp_state));
    put_events(&mut w, &state.events);
    for value in state.debug_registers.db {
        w.u64(value);
    }
    w.u64(state.debug_registers.dr6)
        .u64(state.debug_registers.dr7);
    w.into_bytes()
}

/// Decodes one vCPU's state.
pub fn decode(bytes: &[u8]) -> Result<X86CpuState> {
    let mut r = Reader::new(bytes);
    let index = r.u32("vcpu index")?;
    let registers = get_registers(&mut r)?;
    let special_registers = get_special(&mut r)?;
    let count = r.count("msr count", MAX_MSRS, 12)?;
    let mut msrs = Vec::with_capacity(count);
    for _ in 0..count {
        msrs.push(X86Msr {
            index: r.u32("msr index")?,
            data: r.u64("msr value")?,
        });
    }
    let xcr0 = r.u64("xcr0")?;
    let xsave = get_blob(&mut r, "xsave area")?;
    let lapic = get_blob(&mut r, "local apic state")?;
    let mp_state = mp_from_code(r.u32("mp state")?)?;
    let events = get_events(&mut r)?;
    let mut db = [0u64; 4];
    for slot in &mut db {
        *slot = r.u64("debug register")?;
    }
    let debug_registers = X86DebugRegisters {
        db,
        dr6: r.u64("dr6")?,
        dr7: r.u64("dr7")?,
    };
    r.finish("cpu section")?;
    Ok(X86CpuState {
        index,
        registers,
        special_registers,
        msrs,
        xcr0,
        xsave,
        lapic,
        mp_state,
        events,
        debug_registers,
    })
}

/// Encodes the VM-wide clock.
pub fn encode_clock(clock: &VmClockState) -> Vec<u8> {
    let mut w = Writer::with_capacity(32);
    w.u64(clock.clock_ns)
        .u32(clock.flags)
        .u64(clock.realtime_ns)
        .u64(clock.host_tsc);
    w.into_bytes()
}

/// Decodes the VM-wide clock.
pub fn decode_clock(bytes: &[u8]) -> Result<VmClockState> {
    let mut r = Reader::new(bytes);
    let clock = VmClockState {
        clock_ns: r.u64("clock")?,
        flags: r.u32("clock flags")?,
        realtime_ns: r.u64("clock realtime")?,
        host_tsc: r.u64("clock host tsc")?,
    };
    r.finish("clock section")?;
    Ok(clock)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> X86CpuState {
        X86CpuState {
            index: 3,
            registers: X86Registers {
                rax: 1,
                rbx: 2,
                rcx: 3,
                rdx: 4,
                rsi: 5,
                rdi: 6,
                rsp: 7,
                rbp: 8,
                r8: 9,
                r9: 10,
                r10: 11,
                r11: 12,
                r12: 13,
                r13: 14,
                r14: 15,
                r15: 16,
                rip: 0xffff_ffff_8100_0000,
                rflags: 0x246,
            },
            special_registers: X86SpecialRegisters {
                cs: X86Segment {
                    base: 0,
                    limit: 0xfffff,
                    selector: 0x10,
                    type_: 11,
                    present: 1,
                    dpl: 0,
                    db: 0,
                    s: 1,
                    l: 1,
                    g: 1,
                    avl: 0,
                    unusable: 0,
                },
                gdt: X86DescriptorTable {
                    base: 0x1000,
                    limit: 0x7f,
                },
                cr0: 0x8005_0033,
                cr3: 0x1_0000,
                cr4: 0x3406e0,
                efer: 0xd01,
                apic_base: 0xfee0_0900,
                ..X86SpecialRegisters::default()
            },
            msrs: vec![
                X86Msr {
                    index: 0xc000_0080,
                    data: 0xd01,
                },
                X86Msr {
                    index: 0xc000_0102,
                    data: 0xffff_8880_0000_0000,
                },
            ],
            xcr0: 0x207,
            xsave: X86OpaqueState::new(BlobFormat::XsaveArea, vec![0x5a; 4096]),
            lapic: X86OpaqueState::new(BlobFormat::KvmLapicPage, vec![0xa5; 1024]),
            mp_state: MpState::Halted,
            events: X86PendingEvents {
                interrupt_injected: true,
                interrupt_vector: 0x30,
                nmi_masked: true,
                host_event_flags: 0x0f,
                ..X86PendingEvents::default()
            },
            debug_registers: X86DebugRegisters {
                db: [1, 2, 3, 4],
                dr6: 0xffff_0ff0,
                dr7: 0x400,
            },
        }
    }

    #[test]
    fn a_cpu_state_round_trips_field_for_field() {
        let state = sample();
        assert_eq!(decode(&encode(&state)).unwrap(), state);
    }

    /// The one property the whole section exists for: nothing is dropped. A
    /// re-encode of the decoded bytes must be identical, so a field that
    /// silently failed to survive would change the length.
    #[test]
    fn re_encoding_is_byte_identical() {
        let bytes = encode(&sample());
        assert_eq!(encode(&decode(&bytes).unwrap()), bytes);
    }

    #[test]
    fn every_truncation_is_an_error_and_never_a_panic() {
        let bytes = encode(&sample());
        for cut in 0..bytes.len() {
            let _ = decode(&bytes[..cut]).unwrap_err().to_string();
        }
    }

    #[test]
    fn an_unknown_blob_tag_is_refused() {
        let mut w = Writer::new();
        w.u32(99).blob(&[]);
        let bytes = w.into_bytes();
        let err = get_blob(&mut Reader::new(&bytes), "xsave").unwrap_err();
        assert!(matches!(err, SnapshotError::BadValue { .. }), "{err}");
    }

    #[test]
    fn an_absent_blob_may_not_carry_bytes() {
        let mut w = Writer::new();
        w.u32(0).blob(&[1, 2, 3]);
        let bytes = w.into_bytes();
        let err = get_blob(&mut Reader::new(&bytes), "xsave").unwrap_err();
        assert!(matches!(err, SnapshotError::BadValue { .. }), "{err}");
    }

    #[test]
    fn an_absurd_msr_count_is_refused_without_allocating() {
        let mut w = Writer::new();
        w.u32(0);
        put_registers(&mut w, &X86Registers::default());
        put_special(&mut w, &X86SpecialRegisters::default());
        w.u64(u64::MAX / 2);
        let bytes = w.into_bytes();
        let err = decode(&bytes).unwrap_err();
        assert!(matches!(err, SnapshotError::TooLarge { .. }), "{err}");
    }

    #[test]
    fn an_unknown_mp_state_is_refused() {
        let err = mp_from_code(42).unwrap_err();
        assert!(matches!(err, SnapshotError::BadValue { .. }), "{err}");
    }

    #[test]
    fn the_clock_round_trips() {
        let clock = VmClockState {
            clock_ns: 123_456_789,
            flags: 2,
            realtime_ns: 1_700_000_000_000_000_000,
            host_tsc: 0xdead_beef,
        };
        assert_eq!(decode_clock(&encode_clock(&clock)).unwrap(), clock);
        assert!(decode_clock(&[]).is_err());
        let mut too_long = encode_clock(&clock);
        too_long.push(0);
        assert!(matches!(
            decode_clock(&too_long).unwrap_err(),
            SnapshotError::TrailingBytes { .. }
        ));
    }
}
