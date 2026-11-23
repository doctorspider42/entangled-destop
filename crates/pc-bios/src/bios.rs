//! The services behind the ROM's stubs.
//!
//! [`Bios::call`] is one BIOS interrupt: it reads the registers the stub
//! saved at `SS:SP`, does what the interrupt asks against guest memory and
//! the drive images, and writes the registers back. Everything the guest
//! supplies — the stack address, buffer pointers, counts, drive numbers — is
//! untrusted: guest memory goes through `vm-memory`'s checked calls, lengths
//! are bounded, and a request that does not make sense fails that request.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use virtio_core::GuestMem;
use vm_memory::{Bytes, GuestAddress};

use crate::font::{COLUMNS, PAGE_BYTES, ROWS};
use crate::media::{Drive, DriveKind, FIRST_DISK, FIRST_OPTICAL};
use crate::rom::{self, frame, Status};

/// One range of the map `int 15h, AX=E820h` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryRange {
    pub base: u64,
    pub len: u64,
    /// 1 usable, 2 reserved, 3 ACPI reclaimable, 4 ACPI NVS.
    pub kind: u32,
}

/// How the caller of a BIOS service translates addresses, when it runs
/// with paging on: an operating system calling the BIOS from virtual 8086
/// mode. `segment:offset` is then a linear address under these page tables,
/// not a physical one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Paging {
    /// The page directory (or, with `pae`, the directory pointer table).
    pub cr3: u64,
    /// 64-bit entries in three levels, rather than 32-bit ones in two.
    pub pae: bool,
}

/// The VGA behind `int 10h, AH=00h`: the BIOS asks it for a standard mode,
/// and it programs its own registers. Without one the BIOS has text mode 3
/// only, which is memory and needs no registers.
pub trait VideoModes: Send {
    /// Sets VGA mode `mode` (03h, 12h, 13h…), clearing display memory
    /// unless told not to. `false` for a mode the VGA has no table for.
    fn set_mode(&mut self, mode: u8, clear: bool) -> bool;
}

/// The display behind the VESA BIOS Extensions (`int 10h, AH=4Fh`): a
/// linear framebuffer whose mode the BIOS sets on a guest's behalf. An
/// operating system with no driver for the machine's display — Windows XP's
/// `vga.sys` — gets its true-colour modes this way.
pub trait Vbe: Send {
    /// Bytes of video memory.
    fn memory_bytes(&self) -> u64;
    /// Guest physical address of the framebuffer, below 4 GiB; `None` when
    /// the display cannot be reached by a 32-bit guest.
    fn framebuffer(&mut self) -> Option<u32>;
    /// Shows `width` x `height` at 32 bits a pixel, clearing the screen
    /// unless told not to. `false` when the display refuses the mode.
    fn set_mode(&mut self, width: u16, height: u16, clear: bool) -> bool;
    /// Back to the VGA.
    fn disable(&mut self);
}

/// The resolutions the VBE mode list offers, all at 32 bits a pixel; mode
/// number [`VBE_FIRST_MODE`] and up, in this order.
const VBE_MODES: [(u16, u16); 16] = [
    (640, 480),
    (800, 600),
    (1024, 768),
    (1152, 864),
    (1280, 720),
    (1280, 768),
    (1280, 800),
    (1280, 960),
    (1280, 1024),
    (1366, 768),
    (1440, 900),
    (1600, 900),
    (1600, 1200),
    (1680, 1050),
    (1920, 1080),
    (1920, 1200),
];
const VBE_FIRST_MODE: u16 = 0x140;
/// VBE return values in AX: done, failed, not supported.
const VBE_OK: u16 = 0x004f;
const VBE_FAILED: u16 = 0x014f;
const VBE_UNSUPPORTED: u16 = 0x0100;

/// Where text mode's memory is.
pub const TEXT_BASE: u64 = 0x000b_8000;
/// The BIOS data area.
const BDA: u64 = 0x400;
/// The extended BIOS data area: the last kilobyte below 640 KiB.
const EBDA: u64 = 0x0009_fc00;
const CONVENTIONAL_KB: u16 = 639;

mod bda {
    pub const COM1: u64 = 0x00;
    pub const EBDA_SEGMENT: u64 = 0x0e;
    pub const EQUIPMENT: u64 = 0x10;
    pub const MEMORY_KB: u64 = 0x13;
    pub const SHIFT_FLAGS: u64 = 0x17;
    pub const KBD_HEAD: u64 = 0x1a;
    pub const TICKS: u64 = 0x6c;
    pub const KBD_TAIL: u64 = 0x1c;
    pub const VIDEO_MODE: u64 = 0x49;
    pub const COLUMNS: u64 = 0x4a;
    pub const PAGE_SIZE: u64 = 0x4c;
    pub const CURSOR_POS: u64 = 0x50;
    pub const CURSOR_SHAPE: u64 = 0x60;
    pub const ACTIVE_PAGE: u64 = 0x62;
    pub const CRTC_PORT: u64 = 0x63;
    pub const DISK_STATUS: u64 = 0x74;
    pub const DISK_COUNT: u64 = 0x75;
    pub const KBD_START: u64 = 0x80;
    pub const KBD_END: u64 = 0x82;
    pub const ROWS_MINUS_1: u64 = 0x84;
    pub const CHAR_HEIGHT: u64 = 0x85;
    pub const VIDEO_CTL: u64 = 0x87;
    pub const KBD_FLAGS_3: u64 = 0x96;
}

const FLAG_CARRY: u16 = 1 << 0;
const FLAG_ZERO: u16 = 1 << 6;
const FLAG_INTERRUPT: u16 = 1 << 9;

/// The most `int 13h` moves through host memory at once.
const TRANSFER_CHUNK: usize = 64 * 1024;
/// Keys waiting for `int 16h`.
const KEY_QUEUE: usize = 32;
/// How long a call that has nothing yet sleeps before the stub asks again.
const RETRY_SLEEP: Duration = Duration::from_millis(2);

/// The registers of one interrupt, as the stub saved them.
#[derive(Debug, Clone, Copy, Default)]
struct Regs {
    es: u16,
    ds: u16,
    edi: u32,
    esi: u32,
    ebp: u32,
    ebx: u32,
    edx: u32,
    ecx: u32,
    eax: u32,
    vector: u8,
    ip: u16,
    cs: u16,
    flags: u16,
}

impl Regs {
    fn parse(raw: &[u8; frame::LEN]) -> Self {
        let word = |at: u64| u16::from_le_bytes([raw[at as usize], raw[at as usize + 1]]);
        let dword = |at: u64| {
            let at = at as usize;
            u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]])
        };
        Self {
            es: word(frame::ES),
            ds: word(frame::DS),
            edi: dword(frame::EDI),
            esi: dword(frame::ESI),
            ebp: dword(frame::EBP),
            ebx: dword(frame::EBX),
            edx: dword(frame::EDX),
            ecx: dword(frame::ECX),
            eax: dword(frame::EAX),
            vector: raw[frame::VECTOR as usize],
            ip: word(frame::IP),
            cs: word(frame::CS),
            flags: word(frame::FLAGS),
        }
    }

    fn store(&self, raw: &mut [u8; frame::LEN]) {
        let mut word = |at: u64, v: u16| {
            raw[at as usize..at as usize + 2].copy_from_slice(&v.to_le_bytes());
        };
        word(frame::ES, self.es);
        word(frame::DS, self.ds);
        word(frame::IP, self.ip);
        word(frame::CS, self.cs);
        word(frame::FLAGS, self.flags);
        let mut dword = |at: u64, v: u32| {
            raw[at as usize..at as usize + 4].copy_from_slice(&v.to_le_bytes());
        };
        dword(frame::EDI, self.edi);
        dword(frame::ESI, self.esi);
        dword(frame::EBP, self.ebp);
        dword(frame::EBX, self.ebx);
        dword(frame::EDX, self.edx);
        dword(frame::ECX, self.ecx);
        dword(frame::EAX, self.eax);
    }

    fn ax(&self) -> u16 {
        self.eax as u16
    }
    fn ah(&self) -> u8 {
        (self.eax >> 8) as u8
    }
    fn al(&self) -> u8 {
        self.eax as u8
    }
    fn bx(&self) -> u16 {
        self.ebx as u16
    }
    fn bh(&self) -> u8 {
        (self.ebx >> 8) as u8
    }
    fn bl(&self) -> u8 {
        self.ebx as u8
    }
    fn cx(&self) -> u16 {
        self.ecx as u16
    }
    fn ch(&self) -> u8 {
        (self.ecx >> 8) as u8
    }
    fn cl(&self) -> u8 {
        self.ecx as u8
    }
    fn dx(&self) -> u16 {
        self.edx as u16
    }
    fn dh(&self) -> u8 {
        (self.edx >> 8) as u8
    }
    fn dl(&self) -> u8 {
        self.edx as u8
    }
    fn si(&self) -> u16 {
        self.esi as u16
    }
    fn di(&self) -> u16 {
        self.edi as u16
    }
    fn bp(&self) -> u16 {
        self.ebp as u16
    }
    fn set_ax(&mut self, v: u16) {
        self.eax = (self.eax & 0xffff_0000) | u32::from(v);
    }
    fn set_ah(&mut self, v: u8) {
        self.eax = (self.eax & 0xffff_00ff) | (u32::from(v) << 8);
    }
    fn set_al(&mut self, v: u8) {
        self.eax = (self.eax & 0xffff_ff00) | u32::from(v);
    }
    fn set_bx(&mut self, v: u16) {
        self.ebx = (self.ebx & 0xffff_0000) | u32::from(v);
    }
    fn set_cx(&mut self, v: u16) {
        self.ecx = (self.ecx & 0xffff_0000) | u32::from(v);
    }
    fn set_dx(&mut self, v: u16) {
        self.edx = (self.edx & 0xffff_0000) | u32::from(v);
    }
    fn set_carry(&mut self, on: bool) {
        self.set_flag(FLAG_CARRY, on);
    }
    fn set_flag(&mut self, flag: u16, on: bool) {
        if on {
            self.flags |= flag;
        } else {
            self.flags &= !flag;
        }
    }
}

/// A real-mode `segment:offset` as a guest physical address.
fn linear(segment: u16, offset: u16) -> u64 {
    (u64::from(segment) << 4) + u64::from(offset)
}

/// The modifier keys `int 16h` reports and translates with.
#[derive(Debug, Default, Clone, Copy)]
struct Modifiers {
    left_shift: bool,
    right_shift: bool,
    ctrl: bool,
    alt: bool,
    caps: bool,
    /// The last byte was the `E0` prefix.
    extended: bool,
}

/// A BIOS for one machine.
pub struct Bios {
    mem: Arc<GuestMem>,
    drives: Vec<(u8, Drive)>,
    memory_map: Vec<MemoryRange>,
    /// `SS`, latched by the first of the stub's two port writes.
    stack_segment: u16,
    /// The page tables the call in progress was made under, if any.
    paging: Option<Paging>,
    /// The linear-framebuffer display, when the machine has one.
    vbe: Option<Box<dyn Vbe>>,
    /// The VGA, when the machine has one whose modes the BIOS can set.
    modes: Option<Box<dyn VideoModes>>,
    /// The VBE mode in effect, as `4F03h` reports it; a VGA mode number
    /// while the VGA has the screen.
    vbe_mode: u16,
    status: Status,
    keys: VecDeque<u16>,
    modifiers: Modifiers,
    /// The line `int 10h` teletype output is building, for the log.
    console_line: String,
    /// One bit per (vector, AH) already reported as unhandled.
    reported: Box<[u64; 1024]>,
}

/// What [`Bios::new`] and [`Bios::install`] can fail with.
#[derive(Debug, thiserror::Error)]
pub enum BiosError {
    #[error("guest memory does not cover the BIOS area at {0:#x}")]
    Memory(u64),
}

impl Bios {
    /// A BIOS over `mem`. `disks` and `opticals` are numbered in order from
    /// `0x80` and `0xE0`.
    pub fn new(
        mem: Arc<GuestMem>,
        memory_map: Vec<MemoryRange>,
        disks: Vec<Drive>,
        opticals: Vec<Drive>,
    ) -> Self {
        let numbered = |first: u8, drives: Vec<Drive>| {
            drives
                .into_iter()
                .take(16)
                .enumerate()
                .map(move |(n, drive)| (first + n as u8, drive))
        };
        let drives = numbered(FIRST_DISK, disks)
            .chain(numbered(FIRST_OPTICAL, opticals))
            .collect();
        Self {
            mem,
            drives,
            memory_map,
            stack_segment: 0,
            paging: None,
            vbe: None,
            modes: None,
            vbe_mode: 3,
            status: Status::Done,
            keys: VecDeque::new(),
            modifiers: Modifiers::default(),
            console_line: String::new(),
            reported: Box::new([0; 1024]),
        }
    }

    /// Puts a floppy in the next drive, numbered from `0x00`. Call before
    /// the machine boots.
    pub fn add_floppy(&mut self, drive: Drive) {
        let number = self
            .drives
            .iter()
            .filter(|(_, d)| d.kind == DriveKind::Floppy)
            .count() as u8;
        if number < 2 {
            self.drives.push((number, drive));
        }
    }

    /// Gives the BIOS a VGA whose standard modes it can set.
    pub fn set_video_modes(&mut self, modes: Box<dyn VideoModes>) {
        self.modes = Some(modes);
    }

    /// Gives the BIOS a linear-framebuffer display to offer through VBE.
    pub fn set_vbe(&mut self, vbe: Box<dyn Vbe>) {
        self.vbe = Some(vbe);
    }

    /// Whether `port` is one of the two the stub writes.
    pub fn contains(port: u16) -> bool {
        (rom::PORT_STACK_SEGMENT..rom::PORT_CALL + 2).contains(&port)
    }

    /// Puts the machine's memory in its power-on state: the ROM at
    /// `F000:1000` and up, the interrupt vectors, the BIOS data area and an
    /// empty text screen. Called at every boot and reset, before the vCPU
    /// runs; the host maps the same ROM image at [`rom::RESET_ALIAS`].
    pub fn install(&mut self) -> Result<(), BiosError> {
        self.reset();
        let image = rom::image();
        self.put(
            rom::BASE + rom::FIRST_USED as u64,
            &image[rom::FIRST_USED..],
        )?;

        let mut vectors = [0u8; 1024];
        for (vector, slot) in vectors.chunks_exact_mut(4).enumerate() {
            slot[..2].copy_from_slice(&rom::vector_offset(vector as u8).to_le_bytes());
            slot[2..].copy_from_slice(&rom::SEGMENT.to_le_bytes());
        }
        self.put(0, &vectors)?;

        self.put(BDA, &[0u8; 0x100])?;
        let disks = self
            .drives
            .iter()
            .filter(|(_, d)| d.kind == DriveKind::Disk)
            .count() as u8;
        self.put_word(BDA + bda::COM1, 0x3f8)?;
        self.put_word(BDA + bda::EBDA_SEGMENT, (EBDA >> 4) as u16)?;
        // An x87, an 80x25 colour display, one serial port, and the floppy
        // drives: bit 0 says there are any, bits 6-7 how many more than one.
        let floppies = self
            .drives
            .iter()
            .filter(|(_, d)| d.kind == DriveKind::Floppy)
            .count() as u16;
        let floppy_bits = match floppies {
            0 => 0,
            n => 1 | (n - 1) << 6,
        };
        self.put_word(BDA + bda::EQUIPMENT, 0x0222 | floppy_bits)?;
        self.put_word(BDA + bda::MEMORY_KB, CONVENTIONAL_KB)?;
        self.put_word(BDA + bda::KBD_HEAD, 0x1e)?;
        self.put_word(BDA + bda::KBD_TAIL, 0x1e)?;
        self.put_word(BDA + bda::KBD_START, 0x1e)?;
        self.put_word(BDA + bda::KBD_END, 0x3e)?;
        self.put(BDA + bda::DISK_COUNT, &[disks])?;
        // An enhanced (101-key) keyboard.
        self.put(BDA + bda::KBD_FLAGS_3, &[0x10])?;
        self.put(BDA + bda::VIDEO_CTL, &[0x60, 0xf9, 0x51, 0x08])?;
        self.put_word(EBDA, 1)?;
        // The tick count starts at the host's time of day; the timer
        // interrupt counts on from there.
        self.put(BDA + bda::TICKS, &ticks_since_midnight().to_le_bytes())?;
        self.set_text_mode(true)?;
        Ok(())
    }

    /// Forgets everything a boot accumulated. Guest memory is
    /// [`Self::install`]'s to restore.
    pub fn reset(&mut self) {
        self.stack_segment = 0;
        self.paging = None;
        self.vbe_mode = 3;
        if let Some(vbe) = self.vbe.as_mut() {
            vbe.disable();
        }
        self.status = Status::Done;
        self.keys.clear();
        self.modifiers = Modifiers::default();
        self.console_line.clear();
        self.reported.fill(0);
    }

    /// A port write from the stub. `paging` is how the caller translates
    /// addresses, when it runs with paging on.
    pub fn io_out(&mut self, port: u16, data: &[u8], paging: Option<Paging>) {
        let value = match data {
            [lo, hi, ..] => u16::from_le_bytes([*lo, *hi]),
            _ => return,
        };
        match port {
            rom::PORT_STACK_SEGMENT => self.stack_segment = value,
            rom::PORT_CALL => {
                self.paging = paging;
                self.status = self.call(self.stack_segment, value);
                self.paging = None;
                if self.status == Status::Retry {
                    std::thread::sleep(RETRY_SLEEP);
                }
            }
            _ => {}
        }
    }

    /// A port read from the stub: the last call's status.
    pub fn io_in(&mut self, port: u16, data: &mut [u8]) {
        data.fill(0);
        if port == rom::PORT_CALL {
            let status = (self.status as u16).to_le_bytes();
            for (out, byte) in data.iter_mut().zip(status) {
                *out = byte;
            }
        }
    }

    /// One byte from the keyboard controller, scancode set 1.
    pub fn scancode(&mut self, code: u8) {
        if let Some(key) = self.translate(code) {
            if self.keys.len() < KEY_QUEUE {
                self.keys.push_back(key);
            }
        }
        let m = self.modifiers;
        let flags = u8::from(m.right_shift)
            | u8::from(m.left_shift) << 1
            | u8::from(m.ctrl) << 2
            | u8::from(m.alt) << 3
            | u8::from(m.caps) << 6;
        let _ = self.put(BDA + bda::SHIFT_FLAGS, &[flags]);
    }

    /// The guest memory this BIOS serves, for whoever draws its text screen.
    pub fn memory(&self) -> &Arc<GuestMem> {
        &self.mem
    }

    /// Runs the interrupt whose saved registers are at `ss:sp`.
    pub fn call(&mut self, ss: u16, sp: u16) -> Status {
        let at = linear(ss, sp);
        let mut raw = [0u8; frame::LEN];
        if !self.read(at, &mut raw) {
            tracing::error!(
                stack = format_args!("{ss:04x}:{sp:04x}"),
                "BIOS call with a stack outside guest memory"
            );
            return Status::Stop;
        }
        let mut regs = Regs::parse(&raw);
        tracing::trace!(
            int = format_args!("{:02x}", regs.vector),
            ax = format_args!("{:04x}", regs.ax()),
            bx = format_args!("{:04x}", regs.bx()),
            cx = format_args!("{:04x}", regs.cx()),
            dx = format_args!("{:04x}", regs.dx()),
            si = format_args!("{:04x}", regs.si()),
            di = format_args!("{:04x}", regs.di()),
            ds = format_args!("{:04x}", regs.ds),
            es = format_args!("{:04x}", regs.es),
            from = format_args!("{:04x}:{:04x}", regs.cs, regs.ip),
            "bios call"
        );
        let status = self.dispatch(&mut regs);
        if status == Status::Done {
            regs.store(&mut raw);
            if !self.write(at, &raw) {
                return Status::Stop;
            }
        }
        status
    }

    fn dispatch(&mut self, regs: &mut Regs) -> Status {
        match regs.vector {
            0x00 | 0x06 | 0x0d => return self.exception(regs),
            0x10 => self.video(regs),
            0x11 => regs.set_ax(self.word(BDA + bda::EQUIPMENT)),
            0x12 => regs.set_ax(CONVENTIONAL_KB),
            0x13 => self.disk(regs),
            // Serial port services: timed out. Printer: no printer.
            0x14 => regs.set_ah(0x80),
            0x17 => regs.set_ah(0x01),
            0x15 => self.system(regs),
            0x16 => return self.keyboard(regs),
            0x18 => return self.no_boot_device(),
            0x19 => return self.boot(regs),
            0x1a => self.time(regs),
            // The timer tick's user hook, Ctrl-Break, and the hardware
            // interrupts nothing here raises: nothing to do.
            0x08..=0x0f | 0x1b | 0x1c | 0x70..=0x77 => {}
            _ => self.unhandled(regs),
        }
        Status::Done
    }

    fn exception(&mut self, regs: &Regs) -> Status {
        let name = match regs.vector {
            0x00 => "divide error",
            0x06 => "invalid opcode",
            _ => "general protection fault",
        };
        let mut code = [0u8; 8];
        let _ = self.read(linear(regs.cs, regs.ip), &mut code);
        tracing::error!(
            vector = regs.vector,
            at = format_args!("{:04x}:{:04x}", regs.cs, regs.ip),
            code = format_args!("{code:02x?}"),
            "{name} in real mode with no handler but the BIOS; the machine is halted"
        );
        Status::Stop
    }

    fn unhandled(&mut self, regs: &mut Regs) {
        let bit = usize::from(regs.vector) << 8 | usize::from(regs.ah());
        if let Some(word) = self.reported.get_mut(bit / 64) {
            if *word & (1 << (bit % 64)) == 0 {
                *word |= 1 << (bit % 64);
                tracing::debug!(
                    int = format_args!("{:02x}", regs.vector),
                    ax = format_args!("{:04x}", regs.ax()),
                    bx = format_args!("{:04x}", regs.bx()),
                    cx = format_args!("{:04x}", regs.cx()),
                    dx = format_args!("{:04x}", regs.dx()),
                    from = format_args!("{:04x}:{:04x}", regs.cs, regs.ip),
                    "BIOS service not implemented"
                );
            }
        }
    }

    // ---- guest memory -----------------------------------------------------

    /// The guest physical address behind linear address `linear`: itself
    /// in real mode, a walk of the caller's page tables otherwise. `None`
    /// for an address that is not mapped.
    fn physical(&self, linear: u64) -> Option<u64> {
        let Some(paging) = self.paging else {
            return Some(linear);
        };
        const PRESENT: u64 = 1;
        const LARGE: u64 = 1 << 7;
        const FRAME: u64 = 0x000f_ffff_ffff_f000;
        let linear = linear & 0xffff_ffff;
        let entry = |at: u64, wide: bool| -> Option<u64> {
            let mut raw = [0u8; 8];
            let len = if wide { 8 } else { 4 };
            self.mem
                .read_slice(&mut raw[..len], GuestAddress(at))
                .ok()?;
            Some(u64::from_le_bytes(raw)).filter(|entry| entry & PRESENT != 0)
        };
        if paging.pae {
            let pointer = entry((paging.cr3 & 0xffff_ffe0) + (linear >> 30 & 3) * 8, true)?;
            let directory = entry((pointer & FRAME) + (linear >> 21 & 0x1ff) * 8, true)?;
            if directory & LARGE != 0 {
                return Some((directory & FRAME & !0x1f_ffff) | (linear & 0x1f_ffff));
            }
            let table = entry((directory & FRAME) + (linear >> 12 & 0x1ff) * 8, true)?;
            Some((table & FRAME) | (linear & 0xfff))
        } else {
            let directory = entry((paging.cr3 & 0xffff_f000) + (linear >> 22) * 4, false)?;
            if directory & LARGE != 0 {
                return Some((directory & 0xffc0_0000) | (linear & 0x3f_ffff));
            }
            let table = entry(
                (directory & 0xffff_f000) + (linear >> 12 & 0x3ff) * 4,
                false,
            )?;
            Some((table & 0xffff_f000) | (linear & 0xfff))
        }
    }

    /// Reads guest memory at a linear address, a page at a time. `false`
    /// when any of it is unmapped or not RAM.
    fn read(&self, linear: u64, buf: &mut [u8]) -> bool {
        let mut done = 0usize;
        while done < buf.len() {
            let at = linear.wrapping_add(done as u64);
            let len = (0x1000 - (at & 0xfff) as usize).min(buf.len() - done);
            let Some(physical) = self.physical(at) else {
                return false;
            };
            if self
                .mem
                .read_slice(&mut buf[done..done + len], GuestAddress(physical))
                .is_err()
            {
                return false;
            }
            done += len;
        }
        true
    }

    /// Writes guest memory at a linear address, a page at a time.
    fn write(&self, linear: u64, bytes: &[u8]) -> bool {
        let mut done = 0usize;
        while done < bytes.len() {
            let at = linear.wrapping_add(done as u64);
            let len = (0x1000 - (at & 0xfff) as usize).min(bytes.len() - done);
            let Some(physical) = self.physical(at) else {
                return false;
            };
            if self
                .mem
                .write_slice(&bytes[done..done + len], GuestAddress(physical))
                .is_err()
            {
                return false;
            }
            done += len;
        }
        true
    }

    fn put(&self, at: u64, bytes: &[u8]) -> Result<(), BiosError> {
        if self.write(at, bytes) {
            Ok(())
        } else {
            Err(BiosError::Memory(at))
        }
    }

    fn put_word(&self, at: u64, value: u16) -> Result<(), BiosError> {
        self.put(at, &value.to_le_bytes())
    }

    fn byte(&self, at: u64) -> u8 {
        let mut b = [0u8; 1];
        let _ = self.read(at, &mut b);
        b[0]
    }

    fn word(&self, at: u64) -> u16 {
        let mut b = [0u8; 2];
        let _ = self.read(at, &mut b);
        u16::from_le_bytes(b)
    }

    // ---- int 19h / 18h ----------------------------------------------------

    fn boot(&mut self, regs: &mut Regs) -> Status {
        let order: Vec<usize> = (0..self.drives.len())
            .filter(|&i| self.drives[i].1.kind == DriveKind::Optical)
            .chain((0..self.drives.len()).filter(|&i| self.drives[i].1.kind == DriveKind::Disk))
            .collect();
        for index in order {
            let Some((number, drive)) = self.drives.get_mut(index) else {
                continue;
            };
            let number = *number;
            let loaded = match drive.kind {
                DriveKind::Optical => drive.el_torito().and_then(|entry| {
                    let bytes = usize::from(entry.sector_count) * 512;
                    let mut image = vec![0u8; bytes.div_ceil(2048) * 2048];
                    drive.read(u64::from(entry.lba), &mut image).ok()?;
                    image.truncate(bytes);
                    Some((entry.load_segment, 0u16, image))
                }),
                // Reached only through the order above, which has no
                // floppies: this BIOS does not boot from one.
                DriveKind::Disk | DriveKind::Floppy => {
                    let mut sector = vec![0u8; 512];
                    drive
                        .read(0, &mut sector)
                        .ok()
                        .filter(|()| sector[510] == 0x55 && sector[511] == 0xaa)
                        .map(|()| (0u16, 0x7c00u16, sector))
                }
            };
            let Some((segment, offset, image)) = loaded else {
                continue;
            };
            if self.put(linear(segment, offset), &image).is_err() {
                continue;
            }
            tracing::info!(
                drive = format_args!("{number:#04x}"),
                entry = format_args!("{segment:04x}:{offset:04x}"),
                bytes = image.len(),
                "BIOS boots"
            );
            *regs = Regs {
                edx: u32::from(number),
                vector: regs.vector,
                ip: offset,
                cs: segment,
                flags: 0x0002 | FLAG_INTERRUPT,
                ..Regs::default()
            };
            return Status::Done;
        }
        self.no_boot_device()
    }

    fn no_boot_device(&mut self) -> Status {
        for &c in b"\r\nNo bootable device.\r\n" {
            self.teletype(c, 0, Some(0x07));
        }
        tracing::warn!("the BIOS found nothing to boot");
        Status::Stop
    }

    // ---- int 10h ----------------------------------------------------------

    fn set_text_mode(&mut self, clear: bool) -> Result<(), BiosError> {
        self.put(BDA + bda::VIDEO_MODE, &[3])?;
        self.put_word(BDA + bda::COLUMNS, COLUMNS as u16)?;
        self.put_word(BDA + bda::PAGE_SIZE, 0x1000)?;
        self.put(BDA + bda::CURSOR_POS - 2, &[0u8; 2 + 16])?;
        // Scan lines 13..14 of a 16-line cell.
        self.put_word(BDA + bda::CURSOR_SHAPE, 0x0d0e)?;
        self.put(BDA + bda::ACTIVE_PAGE, &[0])?;
        self.put_word(BDA + bda::CRTC_PORT, 0x3d4)?;
        self.put(BDA + bda::ROWS_MINUS_1, &[(ROWS - 1) as u8])?;
        self.put_word(BDA + bda::CHAR_HEIGHT, 16)?;
        if clear {
            let blank: Vec<u8> = [b' ', 0x07].repeat(COLUMNS * ROWS);
            self.put(TEXT_BASE, &blank)?;
        }
        Ok(())
    }

    fn page_base(page: u8) -> u64 {
        TEXT_BASE + u64::from(page & 7) * 0x1000
    }

    fn cursor(&self, page: u8) -> (usize, usize) {
        let at = BDA + bda::CURSOR_POS + u64::from(page & 7) * 2;
        (usize::from(self.byte(at)), usize::from(self.byte(at + 1)))
    }

    fn set_cursor(&self, page: u8, col: usize, row: usize) {
        let at = BDA + bda::CURSOR_POS + u64::from(page & 7) * 2;
        let _ = self.put(at, &[col.min(255) as u8, row.min(255) as u8]);
    }

    fn cell_addr(page: u8, col: usize, row: usize) -> Option<u64> {
        (col < COLUMNS && row < ROWS)
            .then(|| Self::page_base(page) + ((row * COLUMNS + col) * 2) as u64)
    }

    /// Moves the rows of a window by `lines` (all of them when 0 or more
    /// than it has), filling what is uncovered with `attr`.
    #[allow(clippy::too_many_arguments)]
    fn scroll(
        &self,
        page: u8,
        up: bool,
        lines: usize,
        attr: u8,
        top: usize,
        left: usize,
        bottom: usize,
        right: usize,
    ) {
        let bottom = bottom.min(ROWS - 1);
        let right = right.min(COLUMNS - 1);
        if top > bottom || left > right {
            return;
        }
        let mut text = vec![0u8; PAGE_BYTES];
        if self
            .mem
            .read_slice(&mut text, GuestAddress(Self::page_base(page)))
            .is_err()
        {
            return;
        }
        let height = bottom - top + 1;
        let lines = if lines == 0 || lines > height {
            height
        } else {
            lines
        };
        let width = (right - left + 1) * 2;
        let at = |row: usize| (row * COLUMNS + left) * 2;
        let blank: Vec<u8> = [b' ', attr].repeat(right - left + 1);
        if up {
            for row in top..=bottom {
                if row + lines <= bottom {
                    text.copy_within(at(row + lines)..at(row + lines) + width, at(row));
                } else {
                    text[at(row)..at(row) + width].copy_from_slice(&blank);
                }
            }
        } else {
            for row in (top..=bottom).rev() {
                if row >= top + lines {
                    text.copy_within(at(row - lines)..at(row - lines) + width, at(row));
                } else {
                    text[at(row)..at(row) + width].copy_from_slice(&blank);
                }
            }
        }
        let _ = self.put(Self::page_base(page), &text);
    }

    /// Writes `c` at the cursor as a terminal would. `attr` of `None` keeps
    /// the cell's attribute.
    fn teletype(&mut self, c: u8, page: u8, attr: Option<u8>) {
        let (mut col, mut row) = self.cursor(page);
        match c {
            0x07 => {}
            0x08 => col = col.saturating_sub(1),
            b'\n' => {
                row += 1;
                let line = std::mem::take(&mut self.console_line);
                if !line.trim().is_empty() {
                    tracing::info!(target: "pc_bios::console", "{line}");
                }
            }
            b'\r' => col = 0,
            _ => {
                if let Some(at) = Self::cell_addr(page, col, row) {
                    let _ = self.put(at, &[c]);
                    if let Some(attr) = attr {
                        let _ = self.put(at + 1, &[attr]);
                    }
                }
                if self.console_line.len() < 200 {
                    self.console_line.push(crate::font::cp437(c));
                }
                col += 1;
                if col >= COLUMNS {
                    col = 0;
                    row += 1;
                }
            }
        }
        if row >= ROWS {
            let fill = Self::cell_addr(page, 0, ROWS - 1).map_or(0x07, |at| self.byte(at + 1));
            self.scroll(page, true, 1, fill, 0, 0, ROWS - 1, COLUMNS - 1);
            row = ROWS - 1;
        }
        self.set_cursor(page, col, row);
    }

    fn video(&mut self, regs: &mut Regs) {
        let page = regs.bh();
        match regs.ah() {
            0x00 => {
                let mode = regs.al() & 0x7f;
                // A VGA mode takes the screen back from the framebuffer.
                if let Some(vbe) = self.vbe.as_mut() {
                    vbe.disable();
                }
                self.vbe_mode = u16::from(mode);
                let clear = regs.al() & 0x80 == 0;
                let text = matches!(mode, 0..=3 | 7);
                // The VGA programs itself for the mode; text mode 3 needs
                // nothing from a machine that has no VGA to program.
                let set = match self.modes.as_mut() {
                    Some(vga) => vga.set_mode(if text { 3 } else { mode }, clear),
                    None => text,
                };
                if !set {
                    tracing::warn!(mode, "int 10h: a video mode this BIOS has no display for");
                }
                let _ = self.set_text_mode(clear && text);
                let _ = self.put(BDA + bda::VIDEO_MODE, &[mode]);
            }
            0x01 => {
                let _ = self.put_word(BDA + bda::CURSOR_SHAPE, regs.cx());
            }
            0x02 => self.set_cursor(page, usize::from(regs.dl()), usize::from(regs.dh())),
            0x03 => {
                let (col, row) = self.cursor(page);
                regs.set_dx((row as u16) << 8 | col as u16);
                regs.set_cx(self.word(BDA + bda::CURSOR_SHAPE));
            }
            0x05 => {
                let _ = self.put(BDA + bda::ACTIVE_PAGE, &[regs.al() & 7]);
            }
            0x06 | 0x07 => {
                let active = self.byte(BDA + bda::ACTIVE_PAGE);
                self.scroll(
                    active,
                    regs.ah() == 0x06,
                    usize::from(regs.al()),
                    regs.bh(),
                    usize::from(regs.ch()),
                    usize::from(regs.cl()),
                    usize::from(regs.dh()),
                    usize::from(regs.dl()),
                );
            }
            0x08 => {
                let (col, row) = self.cursor(page);
                let cell = Self::cell_addr(page, col, row).map_or(0x0720, |at| self.word(at));
                regs.set_ax(cell);
            }
            0x09 | 0x0a => {
                let (col, row) = self.cursor(page);
                let first = row * COLUMNS + col;
                let count = usize::from(regs.cx()).min((COLUMNS * ROWS).saturating_sub(first));
                for n in 0..count {
                    let at = Self::page_base(page) + ((first + n) * 2) as u64;
                    let _ = self.put(at, &[regs.al()]);
                    if regs.ah() == 0x09 {
                        let _ = self.put(at + 1, &[regs.bl()]);
                    }
                }
            }
            0x0e => self.teletype(regs.al(), self.byte(BDA + bda::ACTIVE_PAGE), None),
            0x0f => {
                regs.set_ax((COLUMNS as u16) << 8 | u16::from(self.byte(BDA + bda::VIDEO_MODE)));
                regs.ebx =
                    (regs.ebx & 0xffff_00ff) | u32::from(self.byte(BDA + bda::ACTIVE_PAGE)) << 8;
            }
            // Palette and blink control: one fixed palette.
            0x10 => {}
            0x11 => {
                if regs.al() == 0x30 {
                    // The font a caller asks for by BH: 3 is the 8x8 one,
                    // everything else gets the 8x16.
                    let offset = match regs.bh() {
                        3 | 4 => rom::FONT_8X8,
                        _ => rom::FONT_8X16,
                    };
                    regs.es = rom::SEGMENT;
                    regs.ebp = (regs.ebp & 0xffff_0000) | offset as u32;
                    regs.set_cx(16);
                    regs.edx = (regs.edx & 0xffff_ff00) | (ROWS as u32 - 1);
                }
            }
            0x12 => match regs.bl() {
                // EGA information: colour, 256 KiB.
                0x10 => {
                    regs.set_bx(0x0003);
                    regs.set_cx(0x0009);
                }
                _ => regs.set_al(0x12),
            },
            0x13 => self.write_string(regs),
            0x1a => {
                if regs.al() == 0 {
                    // A VGA with a colour monitor, and no second display.
                    regs.set_al(0x1a);
                    regs.set_bx(0x0008);
                }
            }
            0x4f => self.vbe_call(regs),
            _ => self.unhandled(regs),
        }
    }

    /// The modes on offer: those of [`VBE_MODES`] that fit in video memory,
    /// with their numbers.
    fn vbe_modes(&self) -> Vec<(u16, u16, u16)> {
        let memory = self.vbe.as_ref().map_or(0, |vbe| vbe.memory_bytes());
        VBE_MODES
            .iter()
            .enumerate()
            .filter(|(_, (w, h))| u64::from(*w) * u64::from(*h) * 4 <= memory)
            .map(|(n, (w, h))| (VBE_FIRST_MODE + n as u16, *w, *h))
            .collect()
    }

    /// `int 10h, AH=4Fh`: the VESA BIOS Extensions, version 2.0, linear
    /// framebuffer modes only (no bank switching, no palette calls).
    fn vbe_call(&mut self, regs: &mut Regs) {
        if self.vbe.is_none() {
            return regs.set_ax(VBE_UNSUPPORTED);
        }
        let block = linear(regs.es, regs.di());
        let result = match regs.al() {
            0x00 => {
                // The controller information. The mode list and the name
                // live in the block's own reserved area, so the far
                // pointers to them are ES:DI plus an offset.
                let modes = self.vbe_modes();
                let memory = self.vbe.as_ref().map_or(0, |vbe| vbe.memory_bytes());
                let mut info = [0u8; 256];
                info[0..4].copy_from_slice(b"VESA");
                info[4..6].copy_from_slice(&0x0200u16.to_le_bytes());
                let list_at = 0x22u16;
                let name_at = list_at + (modes.len() as u16 + 1) * 2;
                let far = |offset: u16| {
                    let mut pointer = [0u8; 4];
                    pointer[..2].copy_from_slice(&regs.di().wrapping_add(offset).to_le_bytes());
                    pointer[2..].copy_from_slice(&regs.es.to_le_bytes());
                    pointer
                };
                info[0x06..0x0a].copy_from_slice(&far(name_at));
                info[0x0e..0x12].copy_from_slice(&far(list_at));
                info[0x12..0x14]
                    .copy_from_slice(&((memory >> 16).min(0xffff) as u16).to_le_bytes());
                info[0x14..0x16].copy_from_slice(&0x0100u16.to_le_bytes());
                for at in [0x16usize, 0x1a, 0x1e] {
                    info[at..at + 4].copy_from_slice(&far(name_at));
                }
                let mut at = usize::from(list_at);
                for (number, _, _) in &modes {
                    info[at..at + 2].copy_from_slice(&number.to_le_bytes());
                    at += 2;
                }
                info[at..at + 2].copy_from_slice(&0xffffu16.to_le_bytes());
                let name = b"Entangled\0";
                let at = usize::from(name_at);
                info[at..at + name.len()].copy_from_slice(name);
                self.write(block, &info)
            }
            0x01 => {
                let wanted = regs.cx() & 0x1ff;
                let mode = self.vbe_modes().into_iter().find(|m| m.0 == wanted);
                let base = self.vbe.as_mut().and_then(|vbe| vbe.framebuffer());
                match (mode, base) {
                    (Some((_, width, height)), Some(base)) => {
                        let mut info = [0u8; 256];
                        let word = |info: &mut [u8; 256], at: usize, v: u16| {
                            info[at..at + 2].copy_from_slice(&v.to_le_bytes());
                        };
                        // Supported, colour, graphics, not VGA compatible,
                        // no bank switching, linear framebuffer.
                        word(&mut info, 0x00, 0x00fb);
                        word(&mut info, 0x04, 64);
                        word(&mut info, 0x06, 64);
                        word(&mut info, 0x10, width * 4);
                        word(&mut info, 0x12, width);
                        word(&mut info, 0x14, height);
                        info[0x16] = 8;
                        info[0x17] = 16;
                        info[0x18] = 1; // planes
                        info[0x19] = 32; // bits a pixel
                        info[0x1a] = 1; // banks
                        info[0x1b] = 6; // direct colour
                        info[0x1e] = 1;
                        // Red, green, blue, reserved: size and position.
                        info[0x1f..0x27].copy_from_slice(&[8, 16, 8, 8, 8, 0, 8, 24]);
                        info[0x27] = 0x02;
                        info[0x28..0x2c].copy_from_slice(&base.to_le_bytes());
                        // The VBE 3.0 linear fields say the same again.
                        word(&mut info, 0x32, width * 4);
                        info[0x36..0x3e].copy_from_slice(&[8, 16, 8, 8, 8, 0, 8, 24]);
                        self.write(block, &info)
                    }
                    _ => false,
                }
            }
            0x02 => {
                let wanted = regs.bx() & 0x1ff;
                if wanted < 0x100 {
                    // A VGA mode through the VBE call: the same as AH=00h.
                    let mut vga = *regs;
                    vga.set_ax(wanted & 0x7f);
                    self.video(&mut vga);
                    true
                } else {
                    let mode = self.vbe_modes().into_iter().find(|m| m.0 == wanted);
                    let clear = regs.bx() & 0x8000 == 0;
                    let set = match (mode, self.vbe.as_mut()) {
                        (Some((_, width, height)), Some(vbe)) => vbe.set_mode(width, height, clear),
                        _ => false,
                    };
                    if set {
                        self.vbe_mode = regs.bx() & 0x7fff;
                    }
                    set
                }
            }
            0x03 => {
                regs.set_bx(self.vbe_mode);
                true
            }
            _ => return regs.set_ax(VBE_UNSUPPORTED),
        };
        regs.set_ax(if result { VBE_OK } else { VBE_FAILED });
    }

    fn write_string(&mut self, regs: &mut Regs) {
        let page = regs.bh();
        let mode = regs.al();
        let saved = self.cursor(page);
        self.set_cursor(page, usize::from(regs.dl()), usize::from(regs.dh()));
        let with_attrs = mode & 2 != 0;
        let mut at = linear(regs.es, regs.bp());
        // CX is at most 65535 characters; a screen holds 2000.
        for _ in 0..regs.cx().min(4096) {
            let c = self.byte(at);
            at += 1;
            let attr = if with_attrs {
                let a = self.byte(at);
                at += 1;
                a
            } else {
                regs.bl()
            };
            self.teletype(c, page, Some(attr));
        }
        if mode & 1 == 0 {
            self.set_cursor(page, saved.0, saved.1);
        }
    }

    // ---- int 13h ----------------------------------------------------------

    fn drive(&mut self, number: u8) -> Option<&mut Drive> {
        self.drives
            .iter_mut()
            .find(|(n, _)| *n == number)
            .map(|(_, drive)| drive)
    }

    /// Moves `count` sectors between the drive and guest memory at `buffer`.
    /// Returns how many moved before the first failure.
    fn transfer(
        &mut self,
        number: u8,
        write: bool,
        lba: u64,
        count: u32,
        buffer: u64,
    ) -> (u32, u8) {
        let Some((size, writable)) = self
            .drive(number)
            .map(|drive| (drive.sector_size() as usize, drive.writable))
        else {
            return (0, 0x01);
        };
        let per_chunk = (TRANSFER_CHUNK / size).max(1) as u32;
        let mut chunk = vec![0u8; per_chunk as usize * size];
        let mut done = 0u32;
        while done < count {
            let n = (count - done).min(per_chunk);
            let bytes = &mut chunk[..n as usize * size];
            let at = buffer + u64::from(done) * size as u64;
            let sector = lba.saturating_add(u64::from(done));
            let ok = if write {
                self.read(at, bytes)
                    && self
                        .drive(number)
                        .is_some_and(|drive| drive.write(sector, bytes).is_ok())
            } else {
                self.drive(number)
                    .is_some_and(|drive| drive.read(sector, bytes).is_ok())
                    && self.write(at, bytes)
            };
            if !ok {
                // 04h: sector not found. 03h: write protected.
                let status = if write && !writable { 0x03 } else { 0x04 };
                return (done, status);
            }
            done += n;
        }
        (done, 0)
    }

    fn disk(&mut self, regs: &mut Regs) {
        let number = regs.dl();
        let status = self.disk_service(regs, number);
        if let Some(status) = status {
            regs.set_ah(status);
            regs.set_carry(status != 0);
            let _ = self.put(BDA + bda::DISK_STATUS, &[status]);
        }
    }

    /// One `int 13h` function. `Some(status)` goes in AH with the carry
    /// flag; `None` means the function set its own registers.
    fn disk_service(&mut self, regs: &mut Regs, number: u8) -> Option<u8> {
        let function = regs.ah();
        // Reset works on any drive number: a loader resets drive 0 first.
        if function == 0x00 {
            return Some(0);
        }
        if function == 0x01 {
            return Some(self.byte(BDA + bda::DISK_STATUS));
        }
        let (kind, sectors, geometry, sector_size) = {
            let Some(drive) = self.drive(number) else {
                // 15h on a drive that is not there answers "no drive"
                // without an error; everything else fails.
                if function == 0x15 {
                    regs.set_ah(0);
                    regs.set_carry(false);
                    return None;
                }
                return Some(0x01);
            };
            (
                drive.kind,
                drive.sectors,
                drive.geometry,
                drive.sector_size(),
            )
        };
        match function {
            0x02 | 0x03 => {
                if kind == DriveKind::Optical {
                    return Some(0x01);
                }
                let cylinder = u32::from(regs.ch()) | u32::from(regs.cl() & 0xc0) << 2;
                let Some(lba) =
                    geometry.lba(cylinder, u32::from(regs.dh()), u32::from(regs.cl() & 0x3f))
                else {
                    return Some(0x04);
                };
                let count = u32::from(regs.al());
                let (done, status) = self.transfer(
                    number,
                    function == 0x03,
                    lba,
                    count,
                    linear(regs.es, regs.bx()),
                );
                regs.set_al(done as u8);
                Some(status)
            }
            // Verify, seek, recalibrate, initialise, the media-lock calls.
            0x04 | 0x09 | 0x0c | 0x0d | 0x10 | 0x11 | 0x14 | 0x44 | 0x45 | 0x46 | 0x47 => Some(0),
            0x08 if kind == DriveKind::Floppy => {
                // Drive type 4 (1.44 MB), the geometry, the number of
                // floppy drives, and a parameter table nobody steps by.
                let floppies = self
                    .drives
                    .iter()
                    .filter(|(_, d)| d.kind == DriveKind::Floppy)
                    .count() as u16;
                regs.set_ax(0);
                regs.set_bx(0x0004);
                regs.set_cx(((geometry.cylinders - 1) << 8 | geometry.sectors) as u16);
                regs.set_dx(((geometry.heads - 1) << 8) as u16 | floppies);
                regs.es = rom::SEGMENT;
                regs.edi = (regs.edi & 0xffff_0000) | rom::CONFIG_TABLE as u32;
                regs.set_carry(false);
                None
            }
            0x08 => {
                if kind != DriveKind::Disk {
                    return Some(0x01);
                }
                let last_cylinder = geometry.cylinders - 1;
                regs.set_cx(
                    ((last_cylinder & 0xff) << 8 | (last_cylinder >> 2 & 0xc0) | geometry.sectors)
                        as u16,
                );
                let disks = self.byte(BDA + bda::DISK_COUNT);
                regs.set_dx(((geometry.heads - 1) << 8) as u16 | u16::from(disks));
                regs.set_bx(0);
                regs.set_ax(0);
                regs.set_carry(false);
                None
            }
            0x15 => {
                // 3: a hard disk, with its size in CX:DX. 2: removable.
                match kind {
                    DriveKind::Disk => {
                        let total = sectors.min(u64::from(u32::MAX)) as u32;
                        regs.set_cx((total >> 16) as u16);
                        regs.set_dx(total as u16);
                        regs.set_ah(3);
                    }
                    // A floppy without change-line support.
                    DriveKind::Floppy => regs.set_ah(1),
                    DriveKind::Optical => regs.set_ah(2),
                }
                regs.set_carry(false);
                None
            }
            0x41 => {
                // A floppy has no extensions: CHS only.
                if regs.bx() != 0x55aa || kind == DriveKind::Floppy {
                    return Some(0x01);
                }
                regs.set_bx(0xaa55);
                // EDD 3.0; extended access and the parameter call, plus
                // locking and ejecting on a disc.
                regs.set_ah(0x30);
                regs.set_cx(if kind == DriveKind::Optical {
                    0x0007
                } else {
                    0x0005
                });
                regs.set_carry(false);
                None
            }
            0x42 | 0x43 => {
                let packet = linear(regs.ds, regs.si());
                let mut dap = [0u8; 16];
                if !self.read(packet, &mut dap) || dap[0] < 16 {
                    return Some(0x01);
                }
                let count = u32::from(u16::from_le_bytes([dap[2], dap[3]]));
                let buffer = linear(
                    u16::from_le_bytes([dap[6], dap[7]]),
                    u16::from_le_bytes([dap[4], dap[5]]),
                );
                let lba = u64::from_le_bytes([
                    dap[8], dap[9], dap[10], dap[11], dap[12], dap[13], dap[14], dap[15],
                ]);
                let (done, status) = self.transfer(number, function == 0x43, lba, count, buffer);
                if status != 0 {
                    let _ = self.put_word(packet + 2, done as u16);
                }
                Some(status)
            }
            0x48 => {
                let at = linear(regs.ds, regs.si());
                if self.word(at) < 0x1a {
                    return Some(0x01);
                }
                let mut out = [0u8; 0x1a];
                out[0..2].copy_from_slice(&0x1au16.to_le_bytes());
                let (flags, c, h, s) = match kind {
                    // The geometry is valid.
                    DriveKind::Disk | DriveKind::Floppy => (
                        0x0002u16,
                        geometry.cylinders,
                        geometry.heads,
                        geometry.sectors,
                    ),
                    // Removable, lockable, no geometry.
                    DriveKind::Optical => (0x0074, u32::MAX, u32::MAX, u32::MAX),
                };
                out[2..4].copy_from_slice(&flags.to_le_bytes());
                out[4..8].copy_from_slice(&c.to_le_bytes());
                out[8..12].copy_from_slice(&h.to_le_bytes());
                out[12..16].copy_from_slice(&s.to_le_bytes());
                out[16..24].copy_from_slice(&sectors.to_le_bytes());
                out[24..26].copy_from_slice(&(sector_size as u16).to_le_bytes());
                if self.put(at, &out).is_err() {
                    return Some(0x01);
                }
                Some(0)
            }
            0x4b => {
                // El Torito: the status of the disc emulation. Only a disc
                // booted without emulation has one.
                if kind != DriveKind::Optical {
                    return Some(0x01);
                }
                let entry = self.drive(number).and_then(|d| d.el_torito());
                let Some(entry) = entry else {
                    return Some(0x01);
                };
                let mut out = [0u8; 0x13];
                out[0] = 0x13;
                out[2] = number;
                out[4..8].copy_from_slice(&entry.lba.to_le_bytes());
                out[0x0c..0x0e].copy_from_slice(&entry.load_segment.to_le_bytes());
                out[0x0e..0x10].copy_from_slice(&entry.sector_count.to_le_bytes());
                if self.put(linear(regs.ds, regs.si()), &out).is_err() {
                    return Some(0x01);
                }
                Some(0)
            }
            _ => {
                self.unhandled(regs);
                Some(0x01)
            }
        }
    }

    // ---- int 15h ----------------------------------------------------------

    /// Bytes of RAM from 1 MiB up to the first gap.
    fn extended_memory(&self) -> u64 {
        self.memory_map
            .iter()
            .find(|r| r.base == 0x10_0000 && r.kind == 1)
            .map_or(0, |r| r.len)
    }

    fn system(&mut self, regs: &mut Regs) {
        let ok = |regs: &mut Regs| {
            regs.set_ah(0);
            regs.set_carry(false);
        };
        let unsupported = |regs: &mut Regs| {
            regs.set_ah(0x86);
            regs.set_carry(true);
        };
        match regs.ah() {
            // The A20 gate: this machine has none, the line is always on.
            0x24 => match regs.al() {
                0x00 | 0x01 => ok(regs),
                0x02 => {
                    ok(regs);
                    regs.set_al(1);
                }
                0x03 => {
                    ok(regs);
                    regs.set_bx(3);
                }
                _ => unsupported(regs),
            },
            // The keyboard intercept: carry set means "process the key".
            0x4f => regs.set_carry(true),
            0x86 => {
                let micros = u64::from(regs.cx()) << 16 | u64::from(regs.dx());
                std::thread::sleep(Duration::from_micros(micros.min(50_000)));
                ok(regs);
            }
            0x87 => {
                // Copy CX words between two addresses named by the
                // descriptors at ES:SI + 10h (source) and + 18h (target).
                let table = linear(regs.es, regs.si());
                let base = |bios: &Self, at: u64| {
                    u64::from(bios.word(at + 2))
                        | u64::from(bios.byte(at + 4)) << 16
                        | u64::from(bios.byte(at + 7)) << 24
                };
                let (from, to) = (base(self, table + 0x10), base(self, table + 0x18));
                let mut bytes = vec![0u8; usize::from(regs.cx()) * 2];
                let moved = self.read(from, &mut bytes) && self.write(to, &bytes);
                if moved {
                    ok(regs);
                } else {
                    regs.set_ah(0x02);
                    regs.set_carry(true);
                }
            }
            0x88 => {
                let kb = (self.extended_memory() >> 10).min(0xfc00);
                regs.set_ax(kb as u16);
                regs.set_carry(false);
            }
            0xc0 => {
                regs.es = rom::SEGMENT;
                regs.set_bx(rom::CONFIG_TABLE as u16);
                ok(regs);
            }
            0xc1 => {
                regs.es = (EBDA >> 4) as u16;
                regs.set_carry(false);
            }
            0xe8 => match regs.al() {
                0x01 => {
                    let ext = self.extended_memory();
                    let below_16m = (ext.min(15 << 20) >> 10) as u16;
                    let above_16m = (ext.saturating_sub(15 << 20) >> 16).min(0xffff) as u16;
                    regs.set_ax(below_16m);
                    regs.set_cx(below_16m);
                    regs.set_bx(above_16m);
                    regs.set_dx(above_16m);
                    regs.set_carry(false);
                }
                0x20 => self.memory_map_entry(regs),
                _ => unsupported(regs),
            },
            _ => {
                self.unhandled(regs);
                unsupported(regs);
            }
        }
    }

    fn memory_map_entry(&mut self, regs: &mut Regs) {
        const SMAP: u32 = 0x534d_4150;
        let index = regs.ebx as usize;
        let Some(range) = self.memory_map.get(index).copied() else {
            regs.set_ah(0x86);
            regs.set_carry(true);
            return;
        };
        if regs.edx != SMAP || regs.ecx < 20 {
            regs.set_ah(0x86);
            regs.set_carry(true);
            return;
        }
        let mut out = [0u8; 20];
        out[..8].copy_from_slice(&range.base.to_le_bytes());
        out[8..16].copy_from_slice(&range.len.to_le_bytes());
        out[16..].copy_from_slice(&range.kind.to_le_bytes());
        if self.put(linear(regs.es, regs.di()), &out).is_err() {
            regs.set_ah(0x86);
            regs.set_carry(true);
            return;
        }
        regs.eax = SMAP;
        regs.ecx = 20;
        regs.ebx = if index + 1 < self.memory_map.len() {
            index as u32 + 1
        } else {
            0
        };
        regs.set_carry(false);
    }

    // ---- int 16h ----------------------------------------------------------

    fn translate(&mut self, code: u8) -> Option<u16> {
        const PLAIN: &[u8; 58] =
            b"\0\x1b1234567890-=\x08\tqwertyuiop[]\r\0asdfghjkl;'`\0\\zxcvbnm,./\0*\0 ";
        const SHIFTED: &[u8; 58] =
            b"\0\x1b!@#$%^&*()_+\x08\tQWERTYUIOP{}\r\0ASDFGHJKL:\"~\0|ZXCVBNM<>?\0*\0 ";
        if code == 0xe0 {
            self.modifiers.extended = true;
            return None;
        }
        let extended = std::mem::take(&mut self.modifiers.extended);
        let pressed = code & 0x80 == 0;
        let scan = code & 0x7f;
        match scan {
            0x2a if !extended => self.modifiers.left_shift = pressed,
            0x36 if !extended => self.modifiers.right_shift = pressed,
            0x2a | 0x36 => {}
            0x1d => self.modifiers.ctrl = pressed,
            0x38 => self.modifiers.alt = pressed,
            0x3a => {
                if pressed {
                    self.modifiers.caps = !self.modifiers.caps;
                }
            }
            _ if !pressed => {}
            _ => {
                let shift = self.modifiers.left_shift || self.modifiers.right_shift;
                let mut ascii = if extended {
                    // The grey Enter and `/` are characters; the rest of the
                    // extended keys (arrows, Home, Delete…) have none.
                    match scan {
                        0x1c => b'\r',
                        0x35 => b'/',
                        _ => 0,
                    }
                } else {
                    let table = if shift { SHIFTED } else { PLAIN };
                    table.get(usize::from(scan)).copied().unwrap_or(0)
                };
                if ascii.is_ascii_alphabetic() {
                    if self.modifiers.caps {
                        ascii ^= 0x20;
                    }
                    if self.modifiers.ctrl {
                        ascii &= 0x1f;
                    }
                }
                if self.modifiers.alt {
                    ascii = 0;
                }
                return Some(u16::from(scan) << 8 | u16::from(ascii));
            }
        }
        None
    }

    fn keyboard(&mut self, regs: &mut Regs) -> Status {
        match regs.ah() {
            0x00 | 0x10 => match self.keys.pop_front() {
                Some(key) => regs.set_ax(key),
                None => return Status::Retry,
            },
            0x01 | 0x11 => match self.keys.front() {
                Some(&key) => {
                    regs.set_ax(key);
                    regs.set_flag(FLAG_ZERO, false);
                }
                None => regs.set_flag(FLAG_ZERO, true),
            },
            0x02 | 0x12 => regs.set_al(self.byte(BDA + bda::SHIFT_FLAGS)),
            // Typematic rate.
            0x03 => {}
            0x05 => {
                let stored = self.keys.len() < KEY_QUEUE;
                if stored {
                    self.keys.push_back(regs.cx());
                }
                regs.set_al(u8::from(!stored));
            }
            _ => self.unhandled(regs),
        }
        Status::Done
    }

    // ---- int 1Ah ----------------------------------------------------------

    fn time(&mut self, regs: &mut Regs) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let secs = now.as_secs();
        let of_day = secs % 86_400;
        let bcd = |v: u64| (((v / 10 % 10) << 4) | (v % 10)) as u8;
        match regs.ah() {
            0x00 => {
                // What the timer interrupt has counted.
                let ticks = u32::from(self.word(BDA + bda::TICKS))
                    | u32::from(self.word(BDA + bda::TICKS + 2)) << 16;
                regs.set_cx((ticks >> 16) as u16);
                regs.set_dx(ticks as u16);
                regs.set_al(0);
            }
            0x02 => {
                regs.set_cx(u16::from(bcd(of_day / 3600)) << 8 | u16::from(bcd(of_day / 60 % 60)));
                regs.set_dx(u16::from(bcd(of_day % 60)) << 8);
                regs.set_carry(false);
            }
            0x04 => {
                let (year, month, day) = civil_from_days(secs / 86_400);
                regs.set_cx(u16::from(bcd(year / 100)) << 8 | u16::from(bcd(year % 100)));
                regs.set_dx(u16::from(bcd(month)) << 8 | u16::from(bcd(day)));
                regs.set_carry(false);
            }
            // Setting the clock: the host's is not the guest's to set.
            0x01 | 0x03 | 0x05 => regs.set_carry(false),
            0xb1 => match regs.al() {
                // PCI BIOS present: configuration mechanism 1, version
                // 2.10, one bus. The guest reads the configuration ports
                // itself; none of the other calls is here.
                0x01 => {
                    regs.edx = 0x2049_4350;
                    regs.set_ax(0x0001);
                    regs.set_bx(0x0210);
                    regs.set_cx(regs.cx() & 0xff00);
                    regs.set_carry(false);
                }
                _ => {
                    self.unhandled(regs);
                    regs.set_ah(0x81);
                    regs.set_carry(true);
                }
            },
            _ => {
                self.unhandled(regs);
                regs.set_carry(true);
            }
        }
    }
}

/// The 8254's ticks since midnight by the host's clock: 1 193 182 / 65 536
/// (18.2) a second.
fn ticks_since_midnight() -> u32 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let micros = now.as_secs() % 86_400 * 1_000_000 + u64::from(now.subsec_micros());
    (u128::from(micros) * 1_193_182 / 65_536 / 1_000_000) as u32
}

/// The year, month and day of a count of days since 1970-01-01.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

/// One page of text memory and where its cursor is, for whoever draws the
/// screen. `cells` is filled with [`PAGE_BYTES`] bytes; the cursor is `None`
/// while the guest has it hidden or off the screen.
pub fn read_text_screen(mem: &GuestMem, cells: &mut Vec<u8>) -> Option<(usize, usize)> {
    cells.clear();
    cells.resize(PAGE_BYTES, 0);
    let mut small = [0u8; 2];
    let mut byte = |at: u64| {
        let ok = mem.read_slice(&mut small[..1], GuestAddress(at)).is_ok();
        if ok {
            small[0]
        } else {
            0
        }
    };
    let page = byte(BDA + bda::ACTIVE_PAGE) & 7;
    let col = usize::from(byte(BDA + bda::CURSOR_POS + u64::from(page) * 2));
    let row = usize::from(byte(BDA + bda::CURSOR_POS + u64::from(page) * 2 + 1));
    // Bit 5 of the start scan line turns the cursor off.
    let hidden = byte(BDA + bda::CURSOR_SHAPE + 1) & 0x20 != 0;
    let _ = mem.read_slice(cells, GuestAddress(Bios::page_base(page)));
    (!hidden && col < COLUMNS && row < ROWS).then_some((col, row))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::tests::{bootable_disc, drive};
    use vm_memory::GuestMemoryMmap;

    const STACK: u16 = 0x7000;

    fn bios(disks: Vec<Drive>, opticals: Vec<Drive>) -> Bios {
        let mem = Arc::new(
            GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 2 << 20)]).expect("memory"),
        );
        let map = vec![
            MemoryRange {
                base: 0,
                len: 0x9fc00,
                kind: 1,
            },
            MemoryRange {
                base: 0x9fc00,
                len: 0x60400,
                kind: 2,
            },
            MemoryRange {
                base: 0x10_0000,
                len: 1 << 20,
                kind: 1,
            },
        ];
        let mut bios = Bios::new(mem, map, disks, opticals);
        bios.install().expect("install");
        bios
    }

    /// Runs `int vector` with `regs`, as the stub would, and returns the
    /// registers it leaves.
    fn int(bios: &mut Bios, vector: u8, mut regs: Regs) -> (Status, Regs) {
        regs.vector = vector;
        let mut raw = [0u8; frame::LEN];
        regs.store(&mut raw);
        raw[frame::VECTOR as usize] = vector;
        bios.mem
            .write_slice(&raw, GuestAddress(u64::from(STACK)))
            .expect("frame");
        bios.io_out(rom::PORT_STACK_SEGMENT, &0u16.to_le_bytes(), None);
        let status = bios.call(0, STACK);
        bios.mem
            .read_slice(&mut raw, GuestAddress(u64::from(STACK)))
            .expect("frame");
        (status, Regs::parse(&raw))
    }

    fn text(bios: &Bios, row: usize) -> String {
        (0..COLUMNS)
            .map(|col| bios.byte(TEXT_BASE + ((row * COLUMNS + col) * 2) as u64) as char)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn install_points_every_vector_at_its_stub() {
        let bios = bios(vec![], vec![]);
        assert_eq!(bios.word(0x13 * 4), rom::stub_offset(0x13));
        assert_eq!(bios.word(0x13 * 4 + 2), rom::SEGMENT);
        assert_eq!(bios.word(BDA + bda::MEMORY_KB), 639);
        // The stub is in memory where the vector says.
        assert_eq!(
            bios.byte(rom::BASE + u64::from(rom::stub_offset(0x13))),
            0x6a
        );
    }

    #[test]
    fn teletype_writes_wraps_and_scrolls() {
        let mut bios = bios(vec![], vec![]);
        for &c in b"Hello\r\nWorld" {
            int(
                &mut bios,
                0x10,
                Regs {
                    eax: 0x0e00 | u32::from(c),
                    ..Regs::default()
                },
            );
        }
        assert_eq!(text(&bios, 0), "Hello");
        assert_eq!(text(&bios, 1), "World");
        for _ in 0..30 {
            int(
                &mut bios,
                0x10,
                Regs {
                    eax: 0x0e0a,
                    ..Regs::default()
                },
            );
        }
        // Everything scrolled away; the cursor stays on the last row.
        assert_eq!(text(&bios, 0), "");
        assert_eq!(bios.cursor(0).1, ROWS - 1);
    }

    #[test]
    fn scroll_with_zero_lines_clears_the_window_in_its_attribute() {
        let mut bios = bios(vec![], vec![]);
        int(
            &mut bios,
            0x10,
            Regs {
                eax: 0x0e41,
                ..Regs::default()
            },
        );
        int(
            &mut bios,
            0x10,
            Regs {
                eax: 0x0600,
                ebx: 0x1f00,
                ecx: 0,
                edx: 0x184f,
                ..Regs::default()
            },
        );
        assert_eq!(bios.word(TEXT_BASE), 0x1f20);
        assert_eq!(bios.word(TEXT_BASE + (PAGE_BYTES as u64 - 2)), 0x1f20);
    }

    #[test]
    fn the_memory_map_is_walked_to_its_end() {
        let mut bios = bios(vec![], vec![]);
        let ask = |ebx| Regs {
            eax: 0xe820,
            ebx,
            ecx: 20,
            edx: 0x534d_4150,
            es: 0,
            edi: 0x6000,
            ..Regs::default()
        };
        let (_, first) = int(&mut bios, 0x15, ask(0));
        assert_eq!(first.eax, 0x534d_4150);
        assert_eq!(first.ebx, 1);
        assert_eq!(first.flags & FLAG_CARRY, 0);
        let (_, last) = int(&mut bios, 0x15, ask(2));
        assert_eq!(last.ebx, 0);
        assert_eq!(bios.word(0x6000 + 16), 1);
        let (_, past) = int(&mut bios, 0x15, ask(3));
        assert_ne!(past.flags & FLAG_CARRY, 0);
    }

    #[test]
    fn a_bootable_disc_is_loaded_and_entered() {
        let mut bios = bios(vec![], vec![drive(&bootable_disc(), DriveKind::Optical)]);
        let (status, regs) = int(&mut bios, 0x19, Regs::default());
        assert_eq!(status, Status::Done);
        assert_eq!((regs.cs, regs.ip), (0x7c0, 0));
        assert_eq!(regs.dl(), FIRST_OPTICAL);
        assert_eq!(bios.byte(0x7c00), 0xfa);
    }

    #[test]
    fn with_nothing_to_boot_the_machine_stops_and_says_so() {
        let mut bios = bios(vec![drive(&[0u8; 1024], DriveKind::Disk)], vec![]);
        let (status, _) = int(&mut bios, 0x19, Regs::default());
        assert_eq!(status, Status::Stop);
        assert_eq!(text(&bios, 1), "No bootable device.");
    }

    #[test]
    fn an_extended_read_copies_disc_sectors_into_guest_memory() {
        let mut disc = bootable_disc();
        disc[33 * 2048] = 0x5a;
        let mut bios = bios(vec![], vec![drive(&disc, DriveKind::Optical)]);
        // The packet at 0000:6000: two sectors from LBA 32 to 0000:8000.
        let mut dap = [0u8; 16];
        dap[0] = 16;
        dap[2] = 2;
        dap[4..6].copy_from_slice(&0x8000u16.to_le_bytes());
        dap[8] = 32;
        bios.put(0x6000, &dap).expect("packet");
        let (_, regs) = int(
            &mut bios,
            0x13,
            Regs {
                eax: 0x4200,
                edx: 0xe0,
                esi: 0x6000,
                ..Regs::default()
            },
        );
        assert_eq!(regs.flags & FLAG_CARRY, 0);
        assert_eq!(regs.ah(), 0);
        assert_eq!(bios.byte(0x8000 + 2048), 0x5a);
    }

    #[test]
    fn a_read_past_the_disc_or_into_nowhere_fails_the_call() {
        let mut bios = bios(vec![], vec![drive(&bootable_disc(), DriveKind::Optical)]);
        let mut dap = [0u8; 16];
        dap[0] = 16;
        dap[2] = 1;
        dap[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        bios.put(0x6000, &dap).expect("packet");
        let ask = Regs {
            eax: 0x4200,
            edx: 0xe0,
            esi: 0x6000,
            ..Regs::default()
        };
        let (_, regs) = int(&mut bios, 0x13, ask);
        assert_ne!(regs.flags & FLAG_CARRY, 0);
        // A drive that is not there.
        let (_, regs) = int(
            &mut bios,
            0x13,
            Regs {
                eax: 0x4200,
                edx: 0x81,
                ..Regs::default()
            },
        );
        assert_ne!(regs.flags & FLAG_CARRY, 0);
    }

    #[test]
    fn chs_reads_and_writes_reach_the_disk() {
        let mut image = vec![0u8; 2048 * 512];
        image[63 * 512] = 0x77;
        let mut bios = bios(vec![drive(&image, DriveKind::Disk)], vec![]);
        // Head 1, sector 1 is LBA 63.
        let read = Regs {
            eax: 0x0201,
            ebx: 0x8000,
            ecx: 0x0001,
            edx: 0x0180,
            ..Regs::default()
        };
        let (_, regs) = int(&mut bios, 0x13, read);
        assert_eq!(regs.flags & FLAG_CARRY, 0);
        assert_eq!(regs.al(), 1);
        assert_eq!(bios.byte(0x8000), 0x77);
        bios.put(0x8000, &[0x99]).expect("buffer");
        let (_, regs) = int(
            &mut bios,
            0x13,
            Regs {
                eax: 0x0301,
                ..read
            },
        );
        assert_eq!(regs.flags & FLAG_CARRY, 0);
        bios.put(0x8000, &[0]).expect("buffer");
        int(&mut bios, 0x13, read);
        assert_eq!(bios.byte(0x8000), 0x99);
        let (_, params) = int(
            &mut bios,
            0x13,
            Regs {
                eax: 0x0800,
                edx: 0x80,
                ..Regs::default()
            },
        );
        assert_eq!(params.dl(), 1);
        assert_eq!(params.cl() & 0x3f, 63);
    }

    #[test]
    fn a_floppy_is_read_by_chs_reported_and_never_booted() {
        // A 1.44 MB image with a boot signature and a marked second track.
        let mut image = vec![0u8; 2880 * 512];
        image[510] = 0x55;
        image[511] = 0xaa;
        image[36 * 512] = 0x77;
        let mut bios = bios(vec![], vec![]);
        bios.add_floppy(drive(&image, DriveKind::Floppy));
        bios.install().expect("install");
        // One floppy drive in the equipment word.
        assert_eq!(bios.word(BDA + bda::EQUIPMENT) & 0x00c1, 0x0001);
        // Cylinder 1, head 0, sector 1 is LBA 36.
        let read = Regs {
            eax: 0x0201,
            ebx: 0x8000,
            ecx: 0x0101,
            edx: 0x0000,
            ..Regs::default()
        };
        let (_, regs) = int(&mut bios, 0x13, read);
        assert_eq!(regs.flags & FLAG_CARRY, 0);
        assert_eq!(bios.byte(0x8000), 0x77);
        let (_, params) = int(
            &mut bios,
            0x13,
            Regs {
                eax: 0x0800,
                ..Regs::default()
            },
        );
        assert_eq!(params.bl(), 4);
        assert_eq!(params.cl() & 0x3f, 18);
        assert_eq!(params.ch(), 79);
        assert_eq!(params.dh(), 1);
        assert_eq!(params.dl(), 1);
        // No extensions, and it is not a boot device.
        let (_, ext) = int(
            &mut bios,
            0x13,
            Regs {
                eax: 0x4100,
                ebx: 0x55aa,
                ..Regs::default()
            },
        );
        assert_ne!(ext.flags & FLAG_CARRY, 0);
        let (status, _) = int(&mut bios, 0x19, Regs::default());
        assert_eq!(status, Status::Stop);
    }

    #[test]
    fn keys_are_translated_and_a_wait_with_no_key_retries() {
        let mut bios = bios(vec![], vec![]);
        let (status, _) = int(&mut bios, 0x16, Regs::default());
        assert_eq!(status, Status::Retry);
        let (_, peek) = int(
            &mut bios,
            0x16,
            Regs {
                eax: 0x0100,
                ..Regs::default()
            },
        );
        assert_ne!(peek.flags & FLAG_ZERO, 0);
        // Shift down, 'a' down and up, shift up, then Enter.
        for code in [0x2a, 0x1e, 0x9e, 0xaa, 0x1c, 0x9c] {
            bios.scancode(code);
        }
        let (status, key) = int(&mut bios, 0x16, Regs::default());
        assert_eq!(status, Status::Done);
        assert_eq!(key.ax(), 0x1e41);
        let (_, key) = int(&mut bios, 0x16, Regs::default());
        assert_eq!(key.ax(), 0x1c0d);
    }

    #[test]
    fn an_exception_with_no_handler_stops_the_machine() {
        let mut bios = bios(vec![], vec![]);
        let (status, _) = int(&mut bios, 0x06, Regs::default());
        assert_eq!(status, Status::Stop);
    }

    #[test]
    fn a_stack_outside_memory_stops_instead_of_panicking() {
        let mut bios = bios(vec![], vec![]);
        assert_eq!(bios.call(0xffff, 0xffff), Status::Stop);
        bios.io_out(rom::PORT_STACK_SEGMENT, &[0xff, 0xff], None);
        bios.io_out(rom::PORT_CALL, &[0xff, 0xff], None);
        let mut status = [0u8; 2];
        bios.io_in(rom::PORT_CALL, &mut status);
        assert_eq!(u16::from_le_bytes(status), Status::Stop as u16);
    }

    /// A call made with paging on follows the caller's page tables: the
    /// stack frame and the buffers are linear addresses.
    #[test]
    fn a_call_from_paged_code_reads_its_frame_through_the_page_tables() {
        for pae in [false, true] {
            let mut bios = bios(vec![], vec![]);
            // Linear 0x0040_0000.. maps to physical 0x0010_0000.., one page.
            let (directory, table) = (0x0018_0000u64, 0x0018_1000u64);
            let linear = 0x0040_0000u64;
            if pae {
                let pointers = 0x0018_2000u64;
                bios.put(pointers, &(directory | 1).to_le_bytes()).unwrap();
                // Directory entry 2 (linear >> 21), table entry 0.
                bios.put(directory + 2 * 8, &(table | 1).to_le_bytes())
                    .unwrap();
                bios.put(table, &(0x0010_0000u64 | 1).to_le_bytes())
                    .unwrap();
                bios.paging = Some(Paging { cr3: pointers, pae });
            } else {
                // Directory entry 1 (linear >> 22), table entry 0.
                bios.put(directory + 4, &(table as u32 | 1).to_le_bytes())
                    .unwrap();
                bios.put(table, &(0x0010_0000u32 | 1).to_le_bytes())
                    .unwrap();
                bios.paging = Some(Paging {
                    cr3: directory,
                    pae,
                });
            }
            assert_eq!(bios.physical(linear + 0x123), Some(0x0010_0123));
            assert_eq!(
                bios.physical(linear + 0x1000),
                None,
                "the next page is unmapped"
            );
            assert!(bios.write(linear + 0x10, &[0xa5]));
            bios.paging = None;
            assert_eq!(bios.byte(0x0010_0010), 0xa5);
            // A buffer that runs into the unmapped page fails as a whole.
            bios.paging = Some(Paging {
                cr3: if pae { 0x0018_2000 } else { directory },
                pae,
            });
            assert!(!bios.write(linear + 0xfff, &[1, 2]));
        }
    }

    /// A display that records what the BIOS asked of it.
    struct FakeVbe(Arc<std::sync::Mutex<Vec<String>>>);
    impl Vbe for FakeVbe {
        fn memory_bytes(&self) -> u64 {
            // Room for 1280x1024 and nothing larger.
            1280 * 1024 * 4
        }
        fn framebuffer(&mut self) -> Option<u32> {
            Some(0xc800_0000)
        }
        fn set_mode(&mut self, width: u16, height: u16, clear: bool) -> bool {
            self.0
                .lock()
                .unwrap()
                .push(format!("{width}x{height} clear={clear}"));
            true
        }
        fn disable(&mut self) {
            self.0.lock().unwrap().push("vga".into());
        }
    }

    #[test]
    fn vbe_lists_the_modes_that_fit_and_sets_one() {
        let mut bios = bios(vec![], vec![]);
        // Without a display the extension is not there.
        let (_, none) = int(
            &mut bios,
            0x10,
            Regs {
                eax: 0x4f00,
                edi: 0x6000,
                ..Regs::default()
            },
        );
        assert_eq!(none.ax(), VBE_UNSUPPORTED);

        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        bios.set_vbe(Box::new(FakeVbe(Arc::clone(&log))));
        let (_, info) = int(
            &mut bios,
            0x10,
            Regs {
                eax: 0x4f00,
                edi: 0x6000,
                ..Regs::default()
            },
        );
        assert_eq!(info.ax(), VBE_OK);
        assert_eq!(bios.word(0x6000), u16::from_le_bytes(*b"VE"));
        assert_eq!(bios.word(0x6004), 0x0200);
        // The mode list, through its far pointer: every resolution up to
        // 1280x1024 and none above, then the terminator.
        let list = u64::from(bios.word(0x600e)) + (u64::from(bios.word(0x6010)) << 4);
        let modes: Vec<u16> = (0..32)
            .map(|n| bios.word(list + n * 2))
            .take_while(|m| *m != 0xffff)
            .collect();
        assert_eq!(modes.first(), Some(&VBE_FIRST_MODE));
        assert!(modes.contains(&(VBE_FIRST_MODE + 8)), "1280x1024");
        assert!(
            !modes.contains(&(VBE_FIRST_MODE + 14)),
            "1920x1080 does not fit"
        );

        // Mode information for 1024x768.
        let wanted = u32::from(VBE_FIRST_MODE + 2);
        let (_, mode) = int(
            &mut bios,
            0x10,
            Regs {
                eax: 0x4f01,
                ecx: wanted,
                edi: 0x6200,
                ..Regs::default()
            },
        );
        assert_eq!(mode.ax(), VBE_OK);
        assert_eq!(bios.word(0x6200 + 0x12), 1024);
        assert_eq!(bios.word(0x6200 + 0x14), 768);
        assert_eq!(bios.byte(0x6200 + 0x19), 32);
        assert_eq!(bios.word(0x6200 + 0x10), 4096);
        assert_eq!(
            bios.word(0x6200 + 0x2a),
            0xc800,
            "the framebuffer's address"
        );
        let (_, unknown) = int(
            &mut bios,
            0x10,
            Regs {
                eax: 0x4f01,
                ecx: 0x1ff,
                edi: 0x6200,
                ..Regs::default()
            },
        );
        assert_eq!(unknown.ax(), VBE_FAILED);

        // Set it with the linear framebuffer bit, read it back, then a VGA
        // mode takes the screen back.
        let (_, set) = int(
            &mut bios,
            0x10,
            Regs {
                eax: 0x4f02,
                ebx: 0x4000 | wanted,
                ..Regs::default()
            },
        );
        assert_eq!(set.ax(), VBE_OK);
        let (_, current) = int(
            &mut bios,
            0x10,
            Regs {
                eax: 0x4f03,
                ..Regs::default()
            },
        );
        assert_eq!(current.bx(), 0x4000 | wanted as u16);
        int(
            &mut bios,
            0x10,
            Regs {
                eax: 0x0003,
                ..Regs::default()
            },
        );
        let (_, current) = int(
            &mut bios,
            0x10,
            Regs {
                eax: 0x4f03,
                ..Regs::default()
            },
        );
        assert_eq!(current.bx(), 3);
        assert_eq!(*log.lock().unwrap(), ["1024x768 clear=true", "vga"]);
    }

    #[test]
    fn dates_come_out_of_the_day_count() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_768), (2026, 11, 11));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }
}
