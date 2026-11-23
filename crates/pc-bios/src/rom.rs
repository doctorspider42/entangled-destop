//! The BIOS ROM: 64 KiB at `F000:0000`, almost all of it empty.
//!
//! There is no BIOS code in it in the usual sense. Every interrupt vector
//! points at a five-byte stub that pushes its own number and jumps to one
//! common routine, and that routine saves the registers on the guest's stack
//! and tells the host where the stack is (two port writes). The host does the
//! service by editing the saved registers in guest memory
//! ([`crate::Bios::call`]); the routine pops them and returns.
//!
//! ```text
//! SS:SP ->  ES DS EDI ESI EBP ESP EBX EDX ECX EAX  vector  IP CS FLAGS
//!           0  2  4   8   12  16  20  24  28  32   36      38 40 42
//! ```
//!
//! The machine code is assembled here, by hand, so the build needs no 16-bit
//! toolchain and the bytes are reviewable next to the comment that names them.

use crate::font;

/// The real-mode segment the ROM is addressed through.
pub const SEGMENT: u16 = 0xf000;
/// Where the ROM sits in guest physical memory.
pub const BASE: u64 = 0x000f_0000;
/// Its size: the whole F segment.
pub const SIZE: usize = 0x1_0000;
/// The same image is mapped here for the first instruction after reset, which
/// is fetched through a `CS` base of `0xffff_0000`.
pub const RESET_ALIAS: u64 = 0xffff_0000;

/// Nothing of the ROM below this offset: the machine's MP table is written at
/// `F000:0000` (`machine_x86::layout::MPTABLE_START`).
pub const FIRST_USED: usize = 0x1000;
/// 256 stubs of [`STUB_LEN`] bytes.
pub const STUBS: usize = 0x1000;
pub const STUB_LEN: usize = 5;
const COMMON: usize = 0x1500;
/// The timer interrupt's handler (IRQ 0, `int 08h`).
const TIMER: usize = 0x1540;
/// A lone `iret`, for vectors whose default is to do nothing.
const IRET: usize = 0x1560;
const INIT: usize = 0x1580;
/// The table `int 15h, AH=C0h` points at.
pub const CONFIG_TABLE: usize = 0x1600;
/// The 8x16 font, 256 glyphs of 16 rows, most significant bit leftmost.
pub const FONT_8X16: usize = 0x2000;
/// The 8x8 font, 256 glyphs of 8 rows.
pub const FONT_8X8: usize = 0x3000;
const RESET_VECTOR: usize = 0xfff0;
const DATE: usize = 0xfff5;
const MODEL: usize = 0xfffe;

/// The guest writes `SS` here (a word)…
pub const PORT_STACK_SEGMENT: u16 = 0x04e0;
/// …and `SP` here, which runs the service. A word read returns its status.
pub const PORT_CALL: u16 = 0x04e2;

/// What the common routine reads back after a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Status {
    /// The saved registers hold the answer: pop them and return.
    Done = 0,
    /// Nothing yet (a key that has not been pressed): call again.
    Retry = 1,
    /// The machine cannot go on (no boot device, a CPU exception): halt.
    Stop = 2,
}

/// Offsets into the saved-register frame at `SS:SP`.
pub mod frame {
    pub const ES: u64 = 0;
    pub const DS: u64 = 2;
    pub const EDI: u64 = 4;
    pub const ESI: u64 = 8;
    pub const EBP: u64 = 12;
    pub const EBX: u64 = 20;
    pub const EDX: u64 = 24;
    pub const ECX: u64 = 28;
    pub const EAX: u64 = 32;
    pub const VECTOR: u64 = 36;
    pub const IP: u64 = 38;
    pub const CS: u64 = 40;
    pub const FLAGS: u64 = 42;
    pub const LEN: usize = 44;
}

/// The real-mode stack the ROM starts the boot on.
pub const INITIAL_STACK: u16 = 0x7c00;

/// Builds the ROM image.
pub fn image() -> Vec<u8> {
    let mut rom = vec![0u8; SIZE];

    for vector in 0..256usize {
        let at = STUBS + vector * STUB_LEN;
        let rel = (COMMON as isize - (at + STUB_LEN) as isize) as i16;
        // push imm8 (sign-extended; the host reads the low byte); jmp near.
        rom[at] = 0x6a;
        rom[at + 1] = vector as u8;
        rom[at + 2] = 0xe9;
        rom[at + 3..at + 5].copy_from_slice(&rel.to_le_bytes());
    }

    let [port_lo, port_hi] = PORT_STACK_SEGMENT.to_le_bytes();
    #[rustfmt::skip]
    let common: [u8; 35] = [
        0x66, 0x60,             //  0 pushad
        0x1e,                   //  2 push ds
        0x06,                   //  3 push es
        // retry:
        0xba, port_lo, port_hi, //  4 mov dx, PORT_STACK_SEGMENT
        0x8c, 0xd0,             //  7 mov ax, ss
        0xef,                   //  9 out dx, ax
        0x42, 0x42,             // 10 inc dx; inc dx        (PORT_CALL)
        0x89, 0xe0,             // 12 mov ax, sp
        0xef,                   // 14 out dx, ax            the service runs
        0xed,                   // 15 in ax, dx             its status
        0x48,                   // 16 dec ax
        0x74, 0xf1,             // 17 jz retry              Status::Retry
        0x79, 0x08,             // 19 jns stop              Status::Stop
        0x07,                   // 21 pop es
        0x1f,                   // 22 pop ds
        0x66, 0x61,             // 23 popad
        0x83, 0xc4, 0x02,       // 25 add sp, 2             the vector
        0xcf,                   // 28 iret
        // stop:
        0xfa,                   // 29 cli
        0xf4,                   // 30 hlt
        0xeb, 0xfd,             // 31 jmp stop + 1
        0x90, 0x90,             // 33
    ];
    rom[COMMON..COMMON + common.len()].copy_from_slice(&common);

    // The one interrupt this BIOS takes: the 8254's tick on IRQ 0. It
    // counts the tick in the BIOS data area, calls the user hook (int 1Ch,
    // which a loader hooks to time itself) and ends the interrupt. It runs
    // on the interrupted code's stack and never calls the host.
    #[rustfmt::skip]
    let timer: [u8; 19] = [
        0x1e,                         // push ds
        0x50,                         // push ax
        0x31, 0xc0,                   // xor ax, ax
        0x8e, 0xd8,                   // mov ds, ax
        0x66, 0xff, 0x06, 0x6c, 0x04, // inc dword [0x046c]
        0xcd, 0x1c,                   // int 0x1c
        0xb0, 0x20,                   // mov al, 0x20
        0xe6, 0x20,                   // out 0x20, al        end of interrupt
        0x58,                         // pop ax
        0x1f,                         // pop ds
    ];
    rom[TIMER..TIMER + timer.len()].copy_from_slice(&timer);
    rom[TIMER + timer.len()] = 0xcf; // iret
    rom[IRET] = 0xcf;

    let [sp_lo, sp_hi] = INITIAL_STACK.to_le_bytes();
    #[rustfmt::skip]
    let init: [u8; 64] = [
        0xfa,                   // cli
        0x31, 0xc0,             // xor ax, ax
        0x8e, 0xd0,             // mov ss, ax
        0xbc, sp_lo, sp_hi,     // mov sp, INITIAL_STACK
        0x8e, 0xd8,             // mov ds, ax
        0x8e, 0xc0,             // mov es, ax
        // The 8259 pair as a PC BIOS leaves it: vectors 08h and 70h, the
        // slave on IR 2, 8086 mode.
        0xb0, 0x11,             // mov al, 0x11         ICW1
        0xe6, 0x20,             // out 0x20, al
        0xe6, 0xa0,             // out 0xa0, al
        0xb0, 0x08,             // mov al, 0x08         ICW2, master
        0xe6, 0x21,             // out 0x21, al
        0xb0, 0x70,             // mov al, 0x70         ICW2, slave
        0xe6, 0xa1,             // out 0xa1, al
        0xb0, 0x04,             // mov al, 0x04         ICW3, master
        0xe6, 0x21,             // out 0x21, al
        0xb0, 0x02,             // mov al, 0x02         ICW3, slave
        0xe6, 0xa1,             // out 0xa1, al
        0xb0, 0x01,             // mov al, 0x01         ICW4
        0xe6, 0x21,             // out 0x21, al
        0xe6, 0xa1,             // out 0xa1, al
        // Only the timer unmasked: the keys come from the host.
        0xb0, 0xfe,             // mov al, 0xfe
        0xe6, 0x21,             // out 0x21, al
        0xb0, 0xff,             // mov al, 0xff
        0xe6, 0xa1,             // out 0xa1, al
        // The 8254's channel 0: a square wave, 65536 counts, 18.2 Hz.
        0xb0, 0x36,             // mov al, 0x36
        0xe6, 0x43,             // out 0x43, al
        0xb0, 0x00,             // mov al, 0x00
        0xe6, 0x40,             // out 0x40, al
        0xe6, 0x40,             // out 0x40, al
        0xcd, 0x19,             // int 0x19
        0xfa,                   // cli
        0xf4,                   // hlt
        0xeb, 0xfd,             // jmp back to the hlt
    ];
    rom[INIT..INIT + init.len()].copy_from_slice(&init);

    // int 15h, AH=C0h: length, model, submodel, revision, five feature bytes.
    // Feature byte 1: a second 8259, a real-time clock, an extended BIOS data
    // area, the keyboard intercept (int 15h/4Fh).
    rom[CONFIG_TABLE..CONFIG_TABLE + 10]
        .copy_from_slice(&[0x08, 0x00, 0xfc, 0x00, 0x01, 0x74, 0x00, 0x00, 0x00, 0x00]);

    for code in 0..256usize {
        let glyph = font::glyph(code as u8);
        rom[FONT_8X16 + code * 16..FONT_8X16 + code * 16 + 16].copy_from_slice(&glyph);
        for row in 0..8 {
            rom[FONT_8X8 + code * 8 + row] = glyph[row * 2];
        }
    }

    // jmp far F000:INIT
    let [init_lo, init_hi] = (INIT as u16).to_le_bytes();
    rom[RESET_VECTOR..RESET_VECTOR + 5].copy_from_slice(&[0xea, init_lo, init_hi, 0x00, 0xf0]);
    rom[DATE..DATE + 8].copy_from_slice(b"11/11/26");
    rom[MODEL] = 0xfc;
    rom
}

/// The `offset` of vector `n`'s stub inside [`SEGMENT`].
pub fn stub_offset(vector: u8) -> u16 {
    (STUBS + usize::from(vector) * STUB_LEN) as u16
}

/// Where the interrupt vector table points `vector` at power-on: the timer
/// interrupt and its user hook stay in the ROM, everything else goes to the
/// host through its stub.
pub fn vector_offset(vector: u8) -> u16 {
    match vector {
        0x08 => TIMER as u16,
        0x1c => IRET as u16,
        _ => stub_offset(vector),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_stub_jumps_to_the_common_routine() {
        let rom = image();
        for vector in 0..256usize {
            let at = STUBS + vector * STUB_LEN;
            assert_eq!(rom[at], 0x6a);
            assert_eq!(usize::from(rom[at + 1]), vector);
            let rel = i16::from_le_bytes([rom[at + 3], rom[at + 4]]);
            assert_eq!((at + STUB_LEN) as isize + rel as isize, COMMON as isize);
        }
    }

    #[test]
    fn the_branches_in_the_common_routine_land_where_the_comments_say() {
        let rom = image();
        let c = &rom[COMMON..COMMON + 35];
        // jz retry: from the byte after it (19) back to 4.
        assert_eq!(c[17], 0x74);
        assert_eq!(19 + (c[18] as i8) as isize, 4);
        // jns stop: from 21 to 29.
        assert_eq!(c[19], 0x79);
        assert_eq!(21 + (c[20] as i8) as isize, 29);
        // jmp stop + 1: from 33 to the hlt at 30.
        assert_eq!(c[31], 0xeb);
        assert_eq!(33 + (c[32] as i8) as isize, 30);
        assert_eq!(c[30], 0xf4);
    }

    #[test]
    fn the_timer_interrupt_stays_in_the_rom() {
        let rom = image();
        // push ds … iret, with the tick count, the user hook and the EOI.
        let handler = &rom[TIMER..TIMER + 20];
        assert_eq!(handler[0], 0x1e);
        assert_eq!(&handler[6..11], &[0x66, 0xff, 0x06, 0x6c, 0x04]);
        assert_eq!(&handler[11..13], &[0xcd, 0x1c]);
        assert_eq!(&handler[13..17], &[0xb0, 0x20, 0xe6, 0x20]);
        assert_eq!(handler[19], 0xcf);
        assert_eq!(vector_offset(0x08), TIMER as u16);
        // The hook's default is a bare iret, so a tick costs no host call.
        assert_eq!(rom[usize::from(vector_offset(0x1c))], 0xcf);
        assert_eq!(vector_offset(0x13), stub_offset(0x13));
        // The init code ends by booting, then halting for good.
        let init = &rom[INIT..INIT + 64];
        assert_eq!(&init[58..], &[0xcd, 0x19, 0xfa, 0xf4, 0xeb, 0xfd]);
    }

    #[test]
    fn the_reset_vector_enters_the_init_code_and_the_mp_table_area_is_empty() {
        let rom = image();
        assert_eq!(rom[RESET_VECTOR], 0xea);
        assert_eq!(
            usize::from(u16::from_le_bytes([
                rom[RESET_VECTOR + 1],
                rom[RESET_VECTOR + 2]
            ])),
            INIT
        );
        assert!(rom[..FIRST_USED].iter().all(|&b| b == 0));
        assert_eq!(rom.len(), SIZE);
    }
}
