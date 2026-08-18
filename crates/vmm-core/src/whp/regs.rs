//! Mapping between the hypervisor-neutral register structs in
//! [`crate::hv`] and WHP's `WHV_REGISTER_NAME`/`WHV_REGISTER_VALUE` pairs.
//!
//! # Why arrays and not a match
//!
//! `WHvGetVirtualProcessorRegisters`/`WHvSetVirtualProcessorRegisters` take
//! parallel arrays of names and values, so one call moves the whole register
//! set. The `*_NAMES` constants below and the `*_values`/`*_from_values`
//! functions must stay index-for-index in sync; each pair lives next to its
//! sibling and the unit tests at the bottom pin the order.
//!
//! # Segment attribute packing
//!
//! `WHV_X64_SEGMENT_REGISTER` (WinHvPlatformDefs.h) carries the descriptor
//! cache attributes as a 16-bit bitfield unioned with `Attributes: UINT16`.
//! The `windows` crate exposes only the opaque `_bitfield`, so we pack and
//! unpack `Attributes` by hand. The layout, LSB first, is the same encoding
//! VMX uses for segment access rights:
//!
//! | Bits | Header field | [`X86Segment`] field |
//! |---|---|---|
//! | 0–3 | `SegmentType` | `type_` |
//! | 4 | `NonSystemSegment` | `s` |
//! | 5–6 | `DescriptorPrivilegeLevel` | `dpl` |
//! | 7 | `Present` | `present` |
//! | 8–11 | `Reserved` | — (written as 0) |
//! | 12 | `Available` | `avl` |
//! | 13 | `Long` | `l` |
//! | 14 | `Default` | `db` |
//! | 15 | `Granularity` | `g` |
//!
//! WHP has no equivalent of KVM's `unusable` flag: an unusable segment is
//! expressed as `Present = 0`. So [`seg_to_whp`] clears `Present` when
//! `unusable` is set, and [`seg_from_whp`] always reports `unusable: 0` — WHP
//! does not distinguish "not present" from "unusable", and inventing the
//! difference on read would make get/set round-trips lossy in the other
//! direction. `machine-x86` only ever writes `unusable: 0`.

use windows::Win32::System::Hypervisor::{
    WHvX64RegisterCr0, WHvX64RegisterCr2, WHvX64RegisterCr3, WHvX64RegisterCr4, WHvX64RegisterCr8,
    WHvX64RegisterCs, WHvX64RegisterDs, WHvX64RegisterEfer, WHvX64RegisterEs, WHvX64RegisterFs,
    WHvX64RegisterGdtr, WHvX64RegisterGs, WHvX64RegisterIdtr, WHvX64RegisterLdtr,
    WHvX64RegisterR10, WHvX64RegisterR11, WHvX64RegisterR12, WHvX64RegisterR13, WHvX64RegisterR14,
    WHvX64RegisterR15, WHvX64RegisterR8, WHvX64RegisterR9, WHvX64RegisterRax, WHvX64RegisterRbp,
    WHvX64RegisterRbx, WHvX64RegisterRcx, WHvX64RegisterRdi, WHvX64RegisterRdx,
    WHvX64RegisterRflags, WHvX64RegisterRip, WHvX64RegisterRsi, WHvX64RegisterRsp,
    WHvX64RegisterSs, WHvX64RegisterTr, WHV_REGISTER_NAME, WHV_REGISTER_VALUE,
    WHV_X64_SEGMENT_REGISTER, WHV_X64_SEGMENT_REGISTER_0, WHV_X64_TABLE_REGISTER,
};

use crate::hv::{X86DescriptorTable, X86Registers, X86Segment, X86SpecialRegisters};

/// 16-byte-aligned storage for values handed to WHP.
///
/// **This wrapper is load-bearing, not cosmetic.** WinHvPlatformDefs.h declares
/// `WHV_UINT128` — the widest arm of `WHV_REGISTER_VALUE` — with
/// `DECLSPEC_ALIGN(16)`, and `WHvGetVirtualProcessorRegisters` /
/// `WHvSetVirtualProcessorRegisters` require the register-value buffer to be
/// 16-byte aligned, because WHP moves 128-bit register values with aligned SSE
/// instructions. The `windows` crate's generated binding drops that
/// `align(16)`, leaving `WHV_REGISTER_VALUE` at alignment 8: a bare
/// `[WHV_REGISTER_VALUE; N]` local therefore lands on an 8-mod-16 address about
/// half the time and WHP faults with `STATUS_ACCESS_VIOLATION` *inside* the
/// call, which reads as a random crash rather than an error. Every buffer
/// passed to those two functions must go through here.
#[repr(C, align(16))]
pub(super) struct Aligned16<T>(pub(super) T);

/// An all-zero `WHV_REGISTER_VALUE`, used both as the destination buffer for
/// reads and as the base for writes so no padding byte is ever left
/// uninitialised.
pub(super) fn zeroed_value() -> WHV_REGISTER_VALUE {
    // SAFETY: `WHV_REGISTER_VALUE` is a `repr(C)` union whose arms are all
    // plain integers or `repr(C)` structs of plain integers — there is no
    // niche, pointer or reference among them, so the all-zero bit pattern is
    // valid for every arm.
    unsafe { core::mem::zeroed() }
}

// ---- general-purpose registers --------------------------------------------

/// Every name here selects a 64-bit register, i.e. WHP uses the `Reg64` arm of
/// the value union. Must stay in sync with [`gp_values`]/[`gp_from_values`].
pub(super) const GP_NAMES: [WHV_REGISTER_NAME; 18] = [
    WHvX64RegisterRax,
    WHvX64RegisterRbx,
    WHvX64RegisterRcx,
    WHvX64RegisterRdx,
    WHvX64RegisterRsi,
    WHvX64RegisterRdi,
    WHvX64RegisterRsp,
    WHvX64RegisterRbp,
    WHvX64RegisterR8,
    WHvX64RegisterR9,
    WHvX64RegisterR10,
    WHvX64RegisterR11,
    WHvX64RegisterR12,
    WHvX64RegisterR13,
    WHvX64RegisterR14,
    WHvX64RegisterR15,
    WHvX64RegisterRip,
    WHvX64RegisterRflags,
];

pub(super) fn gp_values(regs: &X86Registers) -> [WHV_REGISTER_VALUE; GP_NAMES.len()] {
    let mut values = [zeroed_value(); GP_NAMES.len()];
    let raw = [
        regs.rax,
        regs.rbx,
        regs.rcx,
        regs.rdx,
        regs.rsi,
        regs.rdi,
        regs.rsp,
        regs.rbp,
        regs.r8,
        regs.r9,
        regs.r10,
        regs.r11,
        regs.r12,
        regs.r13,
        regs.r14,
        regs.r15,
        regs.rip,
        regs.rflags,
    ];
    for (value, raw) in values.iter_mut().zip(raw) {
        value.Reg64 = raw;
    }
    values
}

pub(super) fn gp_from_values(values: &[WHV_REGISTER_VALUE; GP_NAMES.len()]) -> X86Registers {
    // SAFETY: every name in `GP_NAMES` is a 64-bit general-purpose register,
    // so `WHvGetVirtualProcessorRegisters` filled the `Reg64` arm of each
    // union. Reading a different arm of a `repr(C)` union of integers would
    // still be defined, but this is the arm WHP wrote.
    let r = |i: usize| unsafe { values[i].Reg64 };
    X86Registers {
        rax: r(0),
        rbx: r(1),
        rcx: r(2),
        rdx: r(3),
        rsi: r(4),
        rdi: r(5),
        rsp: r(6),
        rbp: r(7),
        r8: r(8),
        r9: r(9),
        r10: r(10),
        r11: r(11),
        r12: r(12),
        r13: r(13),
        r14: r(14),
        r15: r(15),
        rip: r(16),
        rflags: r(17),
    }
}

// ---- segment attribute packing --------------------------------------------

/// Packs [`X86Segment`]'s descriptor-cache flags into
/// `WHV_X64_SEGMENT_REGISTER::Attributes` (layout documented at the top of
/// this module).
pub(super) fn seg_attributes(seg: &X86Segment) -> u16 {
    // WHP expresses "unusable" as "not present".
    let present = if seg.unusable != 0 {
        0
    } else {
        u16::from(seg.present) & 1
    };
    (u16::from(seg.type_) & 0xf)
        | ((u16::from(seg.s) & 1) << 4)
        | ((u16::from(seg.dpl) & 3) << 5)
        | (present << 7)
        | ((u16::from(seg.avl) & 1) << 12)
        | ((u16::from(seg.l) & 1) << 13)
        | ((u16::from(seg.db) & 1) << 14)
        | ((u16::from(seg.g) & 1) << 15)
}

pub(super) fn seg_to_whp(seg: &X86Segment) -> WHV_X64_SEGMENT_REGISTER {
    WHV_X64_SEGMENT_REGISTER {
        Base: seg.base,
        Limit: seg.limit,
        Selector: seg.selector,
        Anonymous: WHV_X64_SEGMENT_REGISTER_0 {
            Attributes: seg_attributes(seg),
        },
    }
}

pub(super) fn seg_from_whp(seg: &WHV_X64_SEGMENT_REGISTER) -> X86Segment {
    // SAFETY: `WHV_X64_SEGMENT_REGISTER_0` is a `repr(C)` union of a `u16`
    // bitfield struct and `Attributes: u16`; both arms are 2 plain bytes, so
    // reading `Attributes` is exactly reading the bitfield's storage.
    let attrs = unsafe { seg.Anonymous.Attributes };
    X86Segment {
        base: seg.Base,
        limit: seg.Limit,
        selector: seg.Selector,
        type_: (attrs & 0xf) as u8,
        s: ((attrs >> 4) & 1) as u8,
        dpl: ((attrs >> 5) & 3) as u8,
        present: ((attrs >> 7) & 1) as u8,
        avl: ((attrs >> 12) & 1) as u8,
        l: ((attrs >> 13) & 1) as u8,
        db: ((attrs >> 14) & 1) as u8,
        g: ((attrs >> 15) & 1) as u8,
        // WHP does not model KVM's "unusable" bit; see the module docs.
        unusable: 0,
    }
}

// ---- special registers ----------------------------------------------------

/// Indices 0–7 are segment registers (`Segment` arm), 8–9 descriptor table
/// registers (`Table` arm) and 10–15 64-bit control registers (`Reg64` arm).
///
/// `WHvX64RegisterApicBase` is deliberately absent: it is only meaningful once
/// local APIC emulation is enabled (EPIC 17 phase 2 / WHP-1703), and asking
/// for it while it is off makes the whole batched call fail. It is fetched
/// separately, best-effort, by the caller.
pub(super) const SREG_NAMES: [WHV_REGISTER_NAME; 16] = [
    WHvX64RegisterCs,
    WHvX64RegisterDs,
    WHvX64RegisterEs,
    WHvX64RegisterFs,
    WHvX64RegisterGs,
    WHvX64RegisterSs,
    WHvX64RegisterTr,
    WHvX64RegisterLdtr,
    WHvX64RegisterGdtr,
    WHvX64RegisterIdtr,
    WHvX64RegisterCr0,
    WHvX64RegisterCr2,
    WHvX64RegisterCr3,
    WHvX64RegisterCr4,
    WHvX64RegisterCr8,
    WHvX64RegisterEfer,
];

/// Number of leading [`SREG_NAMES`] entries that are segment registers.
const SREG_SEGMENT_COUNT: usize = 8;

pub(super) fn sreg_values(sregs: &X86SpecialRegisters) -> [WHV_REGISTER_VALUE; SREG_NAMES.len()] {
    let mut values = [zeroed_value(); SREG_NAMES.len()];
    let segments = [
        &sregs.cs, &sregs.ds, &sregs.es, &sregs.fs, &sregs.gs, &sregs.ss, &sregs.tr, &sregs.ldt,
    ];
    for (value, seg) in values.iter_mut().zip(segments) {
        value.Segment = seg_to_whp(seg);
    }
    for (value, table) in values[SREG_SEGMENT_COUNT..]
        .iter_mut()
        .zip([&sregs.gdt, &sregs.idt])
    {
        value.Table = WHV_X64_TABLE_REGISTER {
            Pad: [0; 3],
            Limit: table.limit,
            Base: table.base,
        };
    }
    for (value, raw) in values[SREG_SEGMENT_COUNT + 2..].iter_mut().zip([
        sregs.cr0, sregs.cr2, sregs.cr3, sregs.cr4, sregs.cr8, sregs.efer,
    ]) {
        value.Reg64 = raw;
    }
    values
}

pub(super) fn sreg_from_values(
    values: &[WHV_REGISTER_VALUE; SREG_NAMES.len()],
) -> X86SpecialRegisters {
    // SAFETY: each closure reads the union arm that matches the register class
    // of the corresponding `SREG_NAMES` entry (documented on that constant),
    // which is the arm `WHvGetVirtualProcessorRegisters` filled in.
    let seg = |i: usize| seg_from_whp(unsafe { &values[i].Segment });
    let table = |i: usize| {
        // SAFETY: as above — indices 8 and 9 are descriptor table registers.
        let t = unsafe { values[i].Table };
        X86DescriptorTable {
            base: t.Base,
            limit: t.Limit,
        }
    };
    // SAFETY: as above — indices 10..16 are 64-bit control registers.
    let raw = |i: usize| unsafe { values[i].Reg64 };
    X86SpecialRegisters {
        cs: seg(0),
        ds: seg(1),
        es: seg(2),
        fs: seg(3),
        gs: seg(4),
        ss: seg(5),
        tr: seg(6),
        ldt: seg(7),
        gdt: table(8),
        idt: table(9),
        cr0: raw(10),
        cr2: raw(11),
        cr3: raw(12),
        cr4: raw(13),
        cr8: raw(14),
        efer: raw(15),
        // Filled in separately, best-effort; see `SREG_NAMES`.
        apic_base: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the alignment fix documented on [`Aligned16`]: if the `windows`
    /// crate ever restores `align(16)` on `WHV_UINT128` this test still passes,
    /// but if the wrapper is removed while the binding is still 8-aligned it
    /// fails instead of turning into an intermittent access violation.
    #[test]
    fn register_value_buffers_are_16_byte_aligned() {
        assert_eq!(size_of::<WHV_REGISTER_VALUE>(), 16);
        assert_eq!(align_of::<Aligned16<[WHV_REGISTER_VALUE; 18]>>(), 16);
        let buf = Aligned16([zeroed_value(); 18]);
        assert_eq!(buf.0.as_ptr() as usize % 16, 0);
    }

    /// The boot GDT's 64-bit code descriptor as `machine-x86` builds it:
    /// present, exec/read, L=1, G=1, DPL=0.
    fn long_mode_code_segment() -> X86Segment {
        X86Segment {
            base: 0,
            limit: 0xffff_ffff,
            selector: 8,
            type_: 0b1011,
            present: 1,
            dpl: 0,
            db: 0,
            s: 1,
            l: 1,
            g: 1,
            avl: 0,
            unusable: 0,
        }
    }

    /// Pins the documented bit positions against a hand-computed value, so a
    /// silent reshuffle cannot pass.
    #[test]
    fn segment_attribute_bit_positions() {
        let attrs = seg_attributes(&long_mode_code_segment());
        // type=0xb | s<<4 | dpl<<5 | present<<7 | l<<13 | g<<15
        assert_eq!(attrs, 0b1010_0000_1001_1011);
        assert_eq!(attrs & 0xf, 0xb, "SegmentType");
        assert_eq!((attrs >> 4) & 1, 1, "NonSystemSegment");
        assert_eq!((attrs >> 7) & 1, 1, "Present");
        assert_eq!((attrs >> 8) & 0xf, 0, "Reserved bits must be zero");
        assert_eq!((attrs >> 13) & 1, 1, "Long");
        assert_eq!((attrs >> 15) & 1, 1, "Granularity");
    }

    #[test]
    fn segment_round_trips_through_whp() {
        let seg = long_mode_code_segment();
        assert_eq!(seg_from_whp(&seg_to_whp(&seg)), seg);

        // A 16-bit real-mode data segment (the smoke test's shape).
        let real_mode = X86Segment {
            base: 0,
            limit: 0xffff,
            selector: 0,
            type_: 0b0011,
            present: 1,
            s: 1,
            ..Default::default()
        };
        assert_eq!(seg_from_whp(&seg_to_whp(&real_mode)), real_mode);
    }

    /// An unusable segment becomes "not present"; the reverse mapping reports
    /// `present: 0` rather than re-inventing `unusable`.
    #[test]
    fn unusable_segment_becomes_not_present() {
        let seg = X86Segment {
            present: 1,
            unusable: 1,
            ..Default::default()
        };
        let back = seg_from_whp(&seg_to_whp(&seg));
        assert_eq!(back.present, 0);
        assert_eq!(back.unusable, 0);
    }

    #[test]
    fn gp_registers_round_trip_in_name_order() {
        let regs = X86Registers {
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
            rip: 0x1000,
            rflags: 2,
        };
        let values = gp_values(&regs);
        assert_eq!(gp_from_values(&values), regs);
        // Order check: index 0 is RAX and index 16 is RIP.
        assert_eq!(GP_NAMES[0], WHvX64RegisterRax);
        assert_eq!(GP_NAMES[16], WHvX64RegisterRip);
        // SAFETY: `gp_values` wrote the `Reg64` arm of every element.
        assert_eq!(unsafe { values[16].Reg64 }, 0x1000);
    }

    #[test]
    fn special_registers_round_trip_in_name_order() {
        let sregs = X86SpecialRegisters {
            cs: long_mode_code_segment(),
            gdt: X86DescriptorTable {
                base: 0x500,
                limit: 31,
            },
            idt: X86DescriptorTable {
                base: 0x520,
                limit: 7,
            },
            cr0: 0x8005_0033,
            cr3: 0x9000,
            cr4: 0x20,
            efer: 0x500,
            // Not part of the batched call; `sreg_from_values` reports 0.
            apic_base: 0xfee0_0900,
            ..Default::default()
        };
        let back = sreg_from_values(&sreg_values(&sregs));
        assert_eq!(back.apic_base, 0);
        assert_eq!(
            back,
            X86SpecialRegisters {
                apic_base: 0,
                ..sregs
            }
        );
        assert_eq!(SREG_NAMES[SREG_SEGMENT_COUNT], WHvX64RegisterGdtr);
        assert_eq!(SREG_NAMES[SREG_NAMES.len() - 1], WHvX64RegisterEfer);
    }
}
