//! The controller: global registers, the CORB/RIRB command rings, the
//! immediate-command interface, two stream descriptors and the output DMA
//! engine (Intel High Definition Audio Specification 1.0a, §3).
//!
//! Stream 0 is an input descriptor that never runs (Windows' bus driver
//! expects at least one of each), stream 1 the output. The output engine is
//! a host thread ([`Hda::start_pump`]): while the stream runs it walks the
//! buffer descriptor list, reads one period at a time out of guest memory,
//! and plays it into the host sink, which paces it. Every guest memory access
//! is through `vm-memory`'s checked API, and every guest-supplied index and
//! length is bounded before use.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Instant;

use virtio_core::{GuestMem, Quiesce};
use virtio_sound::{AudioSink, SinkFactory, StreamFormat};
use vm_memory::{Bytes, GuestAddress};

use crate::codec::Codec;

/// Size of the register BAR.
pub const BAR_SIZE: u32 = 0x4000;
/// Called when the controller raises its interrupt (an MSI edge).
pub type Raise = Arc<dyn Fn() + Send + Sync>;

/// Input streams, then output streams.
const ISS: usize = 1;
const OSS: usize = 1;
const STREAMS: usize = ISS + OSS;
/// The output stream's descriptor index.
const OUT: usize = ISS;
/// First stream descriptor and the stride between them.
const SD_BASE: u64 = 0x80;
const SD_STRIDE: u64 = 0x20;
/// Where the LPIB aliases start (`SDnLPIB` at `0x2084 + 0x20n`).
const ALIAS_BASE: u64 = 0x2000;

/// Ring entries: 256 commands, 256 responses.
const CORB_ENTRIES: u16 = 256;
const RIRB_ENTRIES: u16 = 256;
/// Most buffer descriptors a list may have (`LVI` is a byte).
const MAX_BDL_ENTRIES: u32 = 256;
/// Largest cyclic buffer accepted: 16 MiB of audio is minutes.
const MAX_CBL: u32 = 16 << 20;
/// What one pump step plays: 10 ms.
const PERIOD_MS: u32 = 10;

mod gctl {
    pub const CRST: u32 = 1;
}
mod intctl {
    pub const GIE: u32 = 1 << 31;
    pub const CIE: u32 = 1 << 30;
}
mod corbctl {
    pub const MEIE: u8 = 1;
    pub const RUN: u8 = 1 << 1;
}
mod rirbctl {
    pub const RINTCTL: u8 = 1;
    pub const DMAEN: u8 = 1 << 1;
}
mod rirbsts {
    pub const RINTFL: u8 = 1;
}
mod sdctl {
    pub const SRST: u32 = 1;
    pub const RUN: u32 = 1 << 1;
    pub const IOCE: u32 = 1 << 2;
}
mod sdsts {
    pub const BCIS: u8 = 1 << 2;
    pub const FIFORDY: u8 = 1 << 5;
}
mod ics {
    pub const ICB: u16 = 1;
    pub const IRV: u16 = 1 << 1;
}

/// One stream descriptor's registers, and the engine's place in its list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Stream {
    ctl: u32,
    sts: u8,
    lpib: u32,
    cbl: u32,
    lvi: u16,
    fmt: u16,
    bdpl: u32,
    bdpu: u32,
    /// The buffer descriptor being played and the bytes of it already done.
    entry: u32,
    entry_offset: u32,
}

/// Every register the guest can see.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Regs {
    gctl: u32,
    wakeen: u16,
    statests: u16,
    intctl: u32,
    ssync: u32,
    corb_lbase: u32,
    corb_ubase: u32,
    corb_wp: u16,
    corb_rp: u16,
    corb_ctl: u8,
    corb_sts: u8,
    rirb_lbase: u32,
    rirb_ubase: u32,
    rirb_wp: u16,
    rintcnt: u16,
    rirb_ctl: u8,
    rirb_sts: u8,
    /// Responses written since the last response interrupt.
    responses: u16,
    ic: u32,
    ir: u32,
    ics: u16,
    dpl: u32,
    dpu: u32,
    streams: [Stream; STREAMS],
}

struct State {
    regs: Regs,
    codec: Codec,
    mem: Option<Arc<GuestMem>>,
    raise: Option<Raise>,
}

struct Shared {
    state: Mutex<State>,
    /// Wakes the pump when the output stream may have started.
    changed: Condvar,
    quiesce: Mutex<Option<Arc<Quiesce>>>,
    stop: AtomicBool,
    started: Instant,
}

/// The controller.
pub struct Hda {
    shared: Arc<Shared>,
    pump: Mutex<Option<JoinHandle<()>>>,
}

impl Default for Hda {
    fn default() -> Self {
        Self::new()
    }
}

impl Hda {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    regs: Regs::default(),
                    codec: Codec::default(),
                    mem: None,
                    raise: None,
                }),
                changed: Condvar::new(),
                quiesce: Mutex::new(None),
                stop: AtomicBool::new(false),
                started: Instant::now(),
            }),
            pump: Mutex::new(None),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.shared.lock()
    }

    /// Wires guest memory and the interrupt. Called once, before the guest
    /// runs.
    pub fn connect(&self, mem: Arc<GuestMem>, raise: Raise) {
        let mut state = self.lock();
        state.mem = Some(mem);
        state.raise = Some(raise);
    }

    /// The VM's pause gate (ADR-0005): the pump takes it before every touch
    /// of guest memory.
    pub fn set_quiesce(&self, quiesce: Arc<Quiesce>) {
        *self
            .shared
            .quiesce
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(quiesce);
        self.shared.changed.notify_all();
    }

    /// Starts the output engine, playing into sinks `factory` makes.
    pub fn start_pump(&self, factory: SinkFactory) -> std::io::Result<()> {
        let shared = Arc::clone(&self.shared);
        let handle = std::thread::Builder::new()
            .name("hda-output".into())
            .spawn(move || pump(&shared, &factory))?;
        *self.pump.lock().unwrap_or_else(PoisonError::into_inner) = Some(handle);
        Ok(())
    }

    /// Power-on (ADR-0005): every register, the codec and the engine's
    /// position, as `GCTL.CRST` would leave them.
    pub fn reset(&self) {
        let mut state = self.lock();
        state.regs = Regs::default();
        state.codec = Codec::default();
        drop(state);
        self.shared.changed.notify_all();
    }

    /// Guest read inside the BAR.
    pub fn mmio_read(&self, offset: u64, data: &mut [u8]) {
        let state = self.lock();
        let now = self.shared.wall_clock();
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = read_byte(&state.regs, offset + i as u64, now);
        }
    }

    /// Guest write inside the BAR.
    pub fn mmio_write(&self, offset: u64, data: &[u8]) {
        let mut state = self.lock();
        let raise = write(&mut state, offset, data);
        let wake = state.regs.streams[OUT].ctl & sdctl::RUN != 0;
        if raise {
            fire(&state);
        }
        drop(state);
        if wake {
            self.shared.changed.notify_all();
        }
    }

    /// Everything a snapshot needs: registers, the engine's place and the
    /// codec (ADR-0006).
    pub fn save(&self) -> Vec<u8> {
        let state = self.lock();
        encode(&state.regs, &state.codec)
    }

    /// Puts [`Self::save`] back; `false` for bytes that are not one.
    pub fn load(&self, bytes: &[u8]) -> bool {
        let Some((regs, codec)) = decode(bytes) else {
            return false;
        };
        let mut state = self.lock();
        state.regs = regs;
        state.codec = codec;
        drop(state);
        self.shared.changed.notify_all();
        true
    }
}

impl Drop for Hda {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.changed.notify_all();
        if let Some(quiesce) = self
            .shared
            .quiesce
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            quiesce.wake();
        }
        let handle = self
            .pump
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            if handle.join().is_err() {
                tracing::error!("the HD Audio output engine panicked");
            }
        }
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `WALCLK`: a 24 MHz counter.
    fn wall_clock(&self) -> u32 {
        (self.started.elapsed().as_nanos() * 24 / 1000) as u32
    }
}

// ------------------------------------------------------------ registers

fn in_reset(regs: &Regs) -> bool {
    regs.gctl & gctl::CRST == 0
}

/// One byte of the register file.
fn read_byte(regs: &Regs, offset: u64, wall_clock: u32) -> u8 {
    // The LPIB aliases read as the stream's LPIB and nothing else.
    if (ALIAS_BASE..ALIAS_BASE + SD_BASE + SD_STRIDE * STREAMS as u64).contains(&offset) {
        let local = offset - ALIAS_BASE;
        if local >= SD_BASE {
            let n = ((local - SD_BASE) / SD_STRIDE) as usize;
            let at = (local - SD_BASE) % SD_STRIDE;
            if (4..8).contains(&at) {
                return regs.streams[n].lpib.to_le_bytes()[(at - 4) as usize];
            }
        }
        return 0;
    }
    let (base, value) = match register_value(regs, offset, wall_clock) {
        Some(found) => found,
        None => return 0,
    };
    let shift = (offset - base) * 8;
    (value >> shift) as u8
}

/// The register containing `offset`: its first byte and its value.
fn register_value(regs: &Regs, offset: u64, wall_clock: u32) -> Option<(u64, u64)> {
    let gcap: u16 = ((OSS as u16) << 12) | ((ISS as u16) << 8) | 1; // 64-bit OK
    let v = |base: u64, value: u64| Some((base, value));
    match offset {
        0x00..=0x01 => v(0x00, u64::from(gcap)),
        0x02 => v(0x02, 0),           // VMIN
        0x03 => v(0x03, 1),           // VMAJ
        0x04..=0x05 => v(0x04, 0x3c), // OUTPAY
        0x06..=0x07 => v(0x06, 0x1d), // INPAY
        0x08..=0x0b => v(0x08, u64::from(regs.gctl)),
        0x0c..=0x0d => v(0x0c, u64::from(regs.wakeen)),
        0x0e..=0x0f => v(0x0e, u64::from(regs.statests)),
        0x18..=0x19 => v(0x18, 0x3c),
        0x1a..=0x1b => v(0x1a, 0x1d),
        0x20..=0x23 => v(0x20, u64::from(regs.intctl)),
        0x24..=0x27 => v(0x24, u64::from(intsts(regs))),
        0x30..=0x33 => v(0x30, u64::from(wall_clock)),
        0x34..=0x3b => v(offset & !3, u64::from(regs.ssync)),
        0x40..=0x43 => v(0x40, u64::from(regs.corb_lbase)),
        0x44..=0x47 => v(0x44, u64::from(regs.corb_ubase)),
        0x48..=0x49 => v(0x48, u64::from(regs.corb_wp)),
        0x4a..=0x4b => v(0x4a, u64::from(regs.corb_rp)),
        0x4c => v(0x4c, u64::from(regs.corb_ctl)),
        0x4d => v(0x4d, u64::from(regs.corb_sts)),
        // CORBSIZE: 256 entries supported and selected.
        0x4e => v(0x4e, 0x42),
        0x50..=0x53 => v(0x50, u64::from(regs.rirb_lbase)),
        0x54..=0x57 => v(0x54, u64::from(regs.rirb_ubase)),
        0x58..=0x59 => v(0x58, u64::from(regs.rirb_wp)),
        0x5a..=0x5b => v(0x5a, u64::from(regs.rintcnt)),
        0x5c => v(0x5c, u64::from(regs.rirb_ctl)),
        0x5d => v(0x5d, u64::from(regs.rirb_sts)),
        0x5e => v(0x5e, 0x42),
        0x60..=0x63 => v(0x60, u64::from(regs.ic)),
        0x64..=0x67 => v(0x64, u64::from(regs.ir)),
        0x68..=0x69 => v(0x68, u64::from(regs.ics)),
        0x70..=0x73 => v(0x70, u64::from(regs.dpl)),
        0x74..=0x77 => v(0x74, u64::from(regs.dpu)),
        _ => {
            let local = offset.checked_sub(SD_BASE)?;
            let n = (local / SD_STRIDE) as usize;
            let s = regs.streams.get(n)?;
            let base = SD_BASE + n as u64 * SD_STRIDE;
            match local % SD_STRIDE {
                // CTL (3 bytes) and STS as one dword.
                0x00..=0x03 => v(
                    base,
                    u64::from(s.ctl & 0x00ff_ffff) | (u64::from(s.sts | sdsts::FIFORDY) << 24),
                ),
                0x04..=0x07 => v(base + 0x04, u64::from(s.lpib)),
                0x08..=0x0b => v(base + 0x08, u64::from(s.cbl)),
                0x0c..=0x0d => v(base + 0x0c, u64::from(s.lvi)),
                // FIFOW, then FIFOS.
                0x0e..=0x0f => v(base + 0x0e, 0x4),
                0x10..=0x11 => v(base + 0x10, 0xff),
                0x12..=0x13 => v(base + 0x12, u64::from(s.fmt)),
                0x18..=0x1b => v(base + 0x18, u64::from(s.bdpl)),
                0x1c..=0x1f => v(base + 0x1c, u64::from(s.bdpu)),
                _ => None,
            }
        }
    }
}

fn intsts(regs: &Regs) -> u32 {
    let mut sts = 0;
    for (n, s) in regs.streams.iter().enumerate() {
        if s.sts & sdsts::BCIS != 0 && s.ctl & sdctl::IOCE != 0 {
            sts |= 1 << n;
        }
    }
    let controller = (regs.rirb_sts & rirbsts::RINTFL != 0
        && regs.rirb_ctl & rirbctl::RINTCTL != 0)
        || (regs.statests & regs.wakeen) != 0
        || (regs.corb_sts & 1 != 0 && regs.corb_ctl & corbctl::MEIE != 0);
    if controller {
        sts |= intctl::CIE;
    }
    if sts != 0 {
        sts |= intctl::GIE;
    }
    sts
}

/// Whether the interrupt is asserted.
fn asserted(regs: &Regs) -> bool {
    let sts = intsts(regs);
    regs.intctl & intctl::GIE != 0 && (sts & regs.intctl & !intctl::GIE) != 0
}

fn fire(state: &State) {
    if asserted(&state.regs) {
        if let Some(raise) = &state.raise {
            raise();
        }
    }
}

/// Applies a write; answers whether something may now have to interrupt.
fn write(state: &mut State, offset: u64, data: &[u8]) -> bool {
    if offset >= ALIAS_BASE {
        return false;
    }
    let mut raise = false;
    // Register by register, with a mask of the bytes this write covers.
    let mut at = 0usize;
    while at < data.len() {
        let off = offset + at as u64;
        let Some((base, old)) = register_value(&state.regs, off, 0) else {
            at += 1;
            continue;
        };
        let width = register_width(base);
        let mut value = old;
        let mut mask = 0u64;
        while at < data.len() && offset + (at as u64) < base + width {
            let shift = (offset + at as u64 - base) * 8;
            value = (value & !(0xff << shift)) | (u64::from(data[at]) << shift);
            mask |= 0xff << shift;
            at += 1;
        }
        raise |= set_register(state, base, value, mask);
    }
    raise
}

fn register_width(base: u64) -> u64 {
    match base {
        0x02 | 0x03 | 0x4c | 0x4d | 0x4e | 0x5c | 0x5d | 0x5e => 1,
        0x00 | 0x04 | 0x06 | 0x0c | 0x0e | 0x18 | 0x1a | 0x48 | 0x4a | 0x58 | 0x5a | 0x68 => 2,
        _ if base >= SD_BASE => match (base - SD_BASE) % SD_STRIDE {
            0x0c | 0x0e | 0x10 | 0x12 => 2,
            _ => 4,
        },
        _ => 4,
    }
}

/// Stores one register. `mask` marks the bytes written.
fn set_register(state: &mut State, base: u64, value: u64, mask: u64) -> bool {
    let regs = &mut state.regs;
    if base == 0x08 {
        let was = regs.gctl & gctl::CRST != 0;
        let now = value as u32 & gctl::CRST != 0;
        if !now {
            // Entering reset: every register but GCTL to its default.
            let wakeen = 0;
            *regs = Regs {
                wakeen,
                ..Regs::default()
            };
            state.codec = Codec::default();
            return false;
        }
        regs.gctl = value as u32 & 0x103;
        if !was {
            // Leaving reset: the codec on link 0 announces itself.
            regs.statests |= 1;
            return true;
        }
        return false;
    }
    if in_reset(regs) {
        return false;
    }
    let v32 = value as u32;
    let v16 = value as u16;
    let v8 = value as u8;
    match base {
        0x0c => regs.wakeen = v16 & 0x7fff,
        0x0e => {
            regs.statests &= !v16;
            return true;
        }
        0x20 => {
            regs.intctl = v32 & (intctl::GIE | intctl::CIE | 0x3fff_ffff);
            return true;
        }
        0x34 | 0x38 => regs.ssync = v32,
        0x40 => regs.corb_lbase = v32 & !0x7f,
        0x44 => regs.corb_ubase = v32,
        0x48 => {
            regs.corb_wp = v16 & 0xff;
            return run_corb(state);
        }
        0x4a => {
            // Bit 15 resets the read pointer; it reads back set until the
            // driver clears it.
            if v16 & 0x8000 != 0 {
                regs.corb_rp = 0x8000;
            } else {
                regs.corb_rp &= 0xff;
            }
        }
        0x4c => {
            regs.corb_ctl = v8 & (corbctl::MEIE | corbctl::RUN);
            return run_corb(state);
        }
        0x4d => regs.corb_sts &= !v8,
        0x50 => regs.rirb_lbase = v32 & !0x7f,
        0x54 => regs.rirb_ubase = v32,
        0x58 => {
            if v16 & 0x8000 != 0 {
                regs.rirb_wp = 0;
            }
        }
        0x5a => regs.rintcnt = v16 & 0xff,
        0x5c => {
            regs.rirb_ctl = v8 & 0x7;
            return true;
        }
        0x5d => {
            regs.rirb_sts &= !v8;
            return true;
        }
        0x60 => regs.ic = v32,
        0x68 => {
            if v16 & ics::IRV != 0 {
                regs.ics &= !ics::IRV;
            }
            if v16 & ics::ICB != 0 && regs.ics & ics::ICB == 0 {
                let response = command(&mut state.codec, regs.ic);
                regs.ir = response;
                regs.ics = (regs.ics & !ics::ICB) | ics::IRV;
            }
        }
        0x70 => regs.dpl = v32 & !0x7e,
        0x74 => regs.dpu = v32,
        _ if base >= SD_BASE => {
            let local = base - SD_BASE;
            let n = (local / SD_STRIDE) as usize;
            let Some(s) = regs.streams.get_mut(n) else {
                return false;
            };
            match local % SD_STRIDE {
                0x00 => {
                    let mut raise = false;
                    if mask & 0x00ff_ffff != 0 {
                        let ctl = v32 & 0x00ff_ffff;
                        if ctl & sdctl::SRST != 0 {
                            // Stream reset: stopped, at the start, no status.
                            *s = Stream {
                                ctl: sdctl::SRST,
                                ..Stream::default()
                            };
                        } else {
                            let starting = ctl & sdctl::RUN != 0 && s.ctl & sdctl::RUN == 0;
                            s.ctl = ctl;
                            if starting {
                                s.entry = 0;
                                s.entry_offset = 0;
                                s.lpib = 0;
                            }
                        }
                        raise = true;
                    }
                    if mask & 0xff00_0000 != 0 {
                        s.sts &= !((v32 >> 24) as u8 & 0x1c);
                        raise = true;
                    }
                    return raise;
                }
                0x08 => s.cbl = v32.min(MAX_CBL),
                0x0c => s.lvi = v16 & 0xff,
                0x12 => s.fmt = v16,
                0x18 => s.bdpl = v32 & !0x7f,
                0x1c => s.bdpu = v32,
                _ => {}
            }
        }
        _ => {}
    }
    false
}

/// A command for the codec on link 0; commands to other addresses are
/// answered by nobody, which reads as 0.
fn command(codec: &mut Codec, verb: u32) -> u32 {
    if verb >> 28 == 0 {
        codec.command(verb)
    } else {
        0
    }
}

/// Serves every command between the read and write pointers, writing the
/// responses to the RIRB. Answers whether a response interrupt may be due.
fn run_corb(state: &mut State) -> bool {
    let regs = &mut state.regs;
    if regs.corb_ctl & corbctl::RUN == 0 {
        return false;
    }
    let Some(mem) = state.mem.clone() else {
        return false;
    };
    let corb = (u64::from(regs.corb_ubase) << 32) | u64::from(regs.corb_lbase);
    let rirb = (u64::from(regs.rirb_ubase) << 32) | u64::from(regs.rirb_lbase);
    let mut wrote = false;
    // At most one lap: the pointers are 8-bit, so this is bounded anyway.
    for _ in 0..CORB_ENTRIES {
        let rp = regs.corb_rp & 0xff;
        if rp == regs.corb_wp {
            break;
        }
        let rp = (rp + 1) % CORB_ENTRIES;
        regs.corb_rp = rp;
        let mut verb = [0u8; 4];
        if mem
            .read_slice(
                &mut verb,
                GuestAddress(corb.wrapping_add(u64::from(rp) * 4)),
            )
            .is_err()
        {
            // A ring outside RAM: the memory error interrupt.
            regs.corb_sts |= 1;
            wrote = true;
            break;
        }
        let response = command(&mut state.codec, u32::from_le_bytes(verb));
        if regs.rirb_ctl & rirbctl::DMAEN == 0 {
            continue;
        }
        let wp = (regs.rirb_wp + 1) % RIRB_ENTRIES;
        regs.rirb_wp = wp;
        let mut entry = [0u8; 8];
        entry[0..4].copy_from_slice(&response.to_le_bytes());
        // Response-extended: the codec address (0), solicited.
        if mem
            .write_slice(&entry, GuestAddress(rirb.wrapping_add(u64::from(wp) * 8)))
            .is_err()
        {
            tracing::warn!("the HD Audio RIRB is outside guest memory; dropping a response");
        }
        regs.responses = regs.responses.saturating_add(1);
        let count = if regs.rintcnt == 0 { 256 } else { regs.rintcnt };
        if regs.responses >= count {
            regs.rirb_sts |= rirbsts::RINTFL;
            regs.responses = 0;
        }
        wrote = true;
    }
    // The CORB has drained: whatever responses are waiting interrupt now
    // rather than waiting for RINTCNT more (what hardware does too, or a
    // driver that sets a large count would wait forever).
    if wrote && regs.responses > 0 {
        regs.rirb_sts |= rirbsts::RINTFL;
        regs.responses = 0;
    }
    wrote
}

// ------------------------------------------------------------ the stream

/// A stream format register decoded: 16-bit samples only reach the sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Format {
    rate_hz: u32,
    channels: u8,
    /// Bytes per sample in guest memory.
    sample_bytes: u8,
}

fn decode_format(fmt: u16) -> Option<Format> {
    let base = if fmt & (1 << 14) != 0 { 44_100 } else { 48_000 };
    let mult = u32::from((fmt >> 11) & 0x7) + 1;
    let div = u32::from((fmt >> 8) & 0x7) + 1;
    let sample_bytes = match (fmt >> 4) & 0x7 {
        0 => 1,
        1 => 2,
        2..=4 => 4,
        _ => return None,
    };
    let channels = (fmt & 0xf) as u8 + 1;
    if mult > 4 {
        return None;
    }
    Some(Format {
        rate_hz: base * mult / div,
        channels,
        sample_bytes,
    })
}

/// Guest samples to interleaved S16LE.
fn to_s16(format: Format, bytes: &[u8], out: &mut Vec<u8>) {
    out.clear();
    match format.sample_bytes {
        2 => out.extend_from_slice(&bytes[..bytes.len() & !1]),
        1 => {
            for &b in bytes {
                let sample = (i16::from(b as i8) << 8).wrapping_add(i16::MIN);
                out.extend_from_slice(&sample.to_le_bytes());
            }
        }
        _ => {
            for chunk in bytes.chunks_exact(4) {
                out.extend_from_slice(&chunk[2..4]);
            }
        }
    }
}

/// One step of the output engine, under the lock: the next piece of the
/// buffer list, already accounted for. `None` when the stream is not
/// playing or its list is unusable.
struct Step {
    data: Vec<u8>,
    format: Format,
    raise: bool,
}

fn next_step(state: &mut State) -> Option<Step> {
    let mem = state.mem.clone()?;
    let dpl = state.regs.dpl;
    let dpu = state.regs.dpu;
    if in_reset(&state.regs) {
        return None;
    }
    let s = &mut state.regs.streams[OUT];
    if s.ctl & sdctl::RUN == 0 {
        return None;
    }
    let format = decode_format(s.fmt)?;
    let entries = u32::from(s.lvi) + 1;
    if entries > MAX_BDL_ENTRIES || s.cbl == 0 {
        return None;
    }
    let bdl = (u64::from(s.bdpu) << 32) | u64::from(s.bdpl);
    let entry_index = s.entry % entries;
    let mut desc = [0u8; 16];
    mem.read_slice(
        &mut desc,
        GuestAddress(bdl.wrapping_add(u64::from(entry_index) * 16)),
    )
    .ok()?;
    let address = u64::from_le_bytes(desc[0..8].try_into().ok()?);
    let length = u32::from_le_bytes(desc[8..12].try_into().ok()?).min(s.cbl);
    let ioc = u32::from_le_bytes(desc[12..16].try_into().ok()?) & 1 != 0;
    if length == 0 {
        // An empty descriptor: the descriptor error, and the stream stops.
        s.sts |= 1 << 4;
        s.ctl &= !sdctl::RUN;
        return None;
    }
    let frame = u32::from(format.channels) * u32::from(format.sample_bytes);
    let period = (format.rate_hz * PERIOD_MS / 1000 * frame).max(frame);
    let offset = s.entry_offset.min(length);
    let take = (length - offset).min(period);
    let mut data = vec![0u8; take as usize];
    mem.read_slice(
        &mut data,
        GuestAddress(address.wrapping_add(u64::from(offset))),
    )
    .ok()?;
    s.entry_offset = offset + take;
    s.lpib = (s.lpib + take) % s.cbl;
    let mut raise = false;
    if s.entry_offset >= length {
        s.entry = (entry_index + 1) % entries;
        s.entry_offset = 0;
        if ioc {
            s.sts |= sdsts::BCIS;
            raise = s.ctl & sdctl::IOCE != 0;
        }
    }
    // The DMA position buffer: every stream's LPIB, 8 bytes apart.
    if dpl & 1 != 0 {
        let base = (u64::from(dpu) << 32) | u64::from(dpl & !0x7f);
        let at = base.wrapping_add(OUT as u64 * 8);
        let _ = mem.write_slice(&s.lpib.to_le_bytes(), GuestAddress(at));
    }
    Some(Step {
        data,
        format,
        raise,
    })
}

fn pump(shared: &Shared, factory: &SinkFactory) {
    let keep_going = || !shared.stop.load(Ordering::Acquire);
    let mut sink: Option<(Box<dyn AudioSink>, Format)> = None;
    let mut pcm = Vec::new();
    while keep_going() {
        // Wait for a running output stream.
        {
            let mut state = shared.lock();
            while keep_going() && state.regs.streams[OUT].ctl & sdctl::RUN == 0 {
                if let Some((mut s, _)) = sink.take() {
                    s.stop();
                }
                state = shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
        let quiesce = shared
            .quiesce
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let pass = match &quiesce {
            Some(q) => match q.wait_while_paused(keep_going) {
                Some(pass) => Some(pass),
                None => break,
            },
            None => None,
        };
        let step = {
            let mut state = shared.lock();
            let step = next_step(&mut state);
            if step.as_ref().is_some_and(|s| s.raise) {
                fire(&state);
            }
            step
        };
        drop(pass);
        let Some(step) = step else {
            // Not playable (a bad format or list): do not spin.
            std::thread::sleep(std::time::Duration::from_millis(u64::from(PERIOD_MS)));
            continue;
        };
        let format = StreamFormat {
            rate_hz: step.format.rate_hz,
            channels: step.format.channels,
        };
        if sink.as_ref().map(|(_, f)| *f) != Some(step.format) {
            if let Some((mut s, _)) = sink.take() {
                s.stop();
            }
            let mut s = factory();
            let period = format.frame_bytes() * (format.rate_hz * PERIOD_MS / 1000) as usize;
            match s.start(format, period) {
                Ok(()) => tracing::info!(%format, sink = s.name(), "HD Audio output playing"),
                Err(error) => {
                    tracing::warn!(%error, %format, "the host sink refused the HD Audio stream")
                }
            }
            sink = Some((s, step.format));
        }
        to_s16(step.format, &step.data, &mut pcm);
        if let Some((s, _)) = sink.as_mut() {
            let mut rest = &pcm[..];
            while !rest.is_empty() && keep_going() {
                match s.write(rest) {
                    Ok(0) => std::thread::sleep(std::time::Duration::from_millis(1)),
                    Ok(n) => rest = &rest[n.min(rest.len())..],
                    Err(error) => {
                        tracing::warn!(%error, "HD Audio output dropped");
                        break;
                    }
                }
            }
        }
    }
    if let Some((mut s, _)) = sink.take() {
        s.stop();
    }
}

// ------------------------------------------------------------ snapshot

const SAVE_VERSION: u8 = 1;

fn encode(regs: &Regs, codec: &Codec) -> Vec<u8> {
    let mut out = vec![SAVE_VERSION];
    let u32s = |out: &mut Vec<u8>, values: &[u32]| {
        for v in values {
            out.extend_from_slice(&v.to_le_bytes());
        }
    };
    u32s(
        &mut out,
        &[
            regs.gctl,
            u32::from(regs.wakeen),
            u32::from(regs.statests),
            regs.intctl,
            regs.ssync,
            regs.corb_lbase,
            regs.corb_ubase,
            u32::from(regs.corb_wp),
            u32::from(regs.corb_rp),
            u32::from(regs.corb_ctl),
            u32::from(regs.corb_sts),
            regs.rirb_lbase,
            regs.rirb_ubase,
            u32::from(regs.rirb_wp),
            u32::from(regs.rintcnt),
            u32::from(regs.rirb_ctl),
            u32::from(regs.rirb_sts),
            u32::from(regs.responses),
            regs.ic,
            regs.ir,
            u32::from(regs.ics),
            regs.dpl,
            regs.dpu,
        ],
    );
    for s in &regs.streams {
        u32s(
            &mut out,
            &[
                s.ctl,
                u32::from(s.sts),
                s.lpib,
                s.cbl,
                u32::from(s.lvi),
                u32::from(s.fmt),
                s.bdpl,
                s.bdpu,
                s.entry,
                s.entry_offset,
            ],
        );
    }
    out.extend_from_slice(&codec.save());
    out
}

fn decode(bytes: &[u8]) -> Option<(Regs, Codec)> {
    let (&version, rest) = bytes.split_first()?;
    if version != SAVE_VERSION {
        return None;
    }
    let words = 23 + 10 * STREAMS;
    if rest.len() != words * 4 + 12 {
        return None;
    }
    let mut it = rest[..words * 4]
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]));
    let mut next = || it.next().unwrap_or(0);
    let mut regs = Regs {
        gctl: next(),
        wakeen: next() as u16,
        statests: next() as u16,
        intctl: next(),
        ssync: next(),
        corb_lbase: next(),
        corb_ubase: next(),
        corb_wp: next() as u16 & 0xff,
        corb_rp: next() as u16,
        corb_ctl: next() as u8,
        corb_sts: next() as u8,
        rirb_lbase: next(),
        rirb_ubase: next(),
        rirb_wp: next() as u16 & 0xff,
        rintcnt: next() as u16,
        rirb_ctl: next() as u8,
        rirb_sts: next() as u8,
        responses: next() as u16,
        ic: next(),
        ir: next(),
        ics: next() as u16,
        dpl: next(),
        dpu: next(),
        streams: [Stream::default(); STREAMS],
    };
    for s in &mut regs.streams {
        *s = Stream {
            ctl: next() & 0x00ff_ffff,
            sts: next() as u8,
            lpib: next(),
            cbl: next().min(MAX_CBL),
            lvi: next() as u16 & 0xff,
            fmt: next() as u16,
            bdpl: next(),
            bdpu: next(),
            entry: next() % MAX_BDL_ENTRIES,
            entry_offset: next(),
        };
    }
    let codec_bytes: &[u8; 12] = rest[words * 4..].try_into().ok()?;
    Some((regs, Codec::load(codec_bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn memory() -> Arc<GuestMem> {
        Arc::new(GuestMem::from_ranges(&[(GuestAddress(0), 1 << 20)]).unwrap())
    }

    fn controller() -> (Hda, Arc<GuestMem>, Arc<AtomicUsize>) {
        let hda = Hda::new();
        let mem = memory();
        let raised = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&raised);
        hda.connect(
            Arc::clone(&mem),
            Arc::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            }),
        );
        (hda, mem, raised)
    }

    fn w32(hda: &Hda, off: u64, v: u32) {
        hda.mmio_write(off, &v.to_le_bytes());
    }
    fn w16(hda: &Hda, off: u64, v: u16) {
        hda.mmio_write(off, &v.to_le_bytes());
    }
    fn w8(hda: &Hda, off: u64, v: u8) {
        hda.mmio_write(off, &[v]);
    }
    fn r32(hda: &Hda, off: u64) -> u32 {
        let mut b = [0; 4];
        hda.mmio_read(off, &mut b);
        u32::from_le_bytes(b)
    }
    fn r16(hda: &Hda, off: u64) -> u16 {
        let mut b = [0; 2];
        hda.mmio_read(off, &mut b);
        u16::from_le_bytes(b)
    }

    #[test]
    fn leaving_reset_announces_the_codec() {
        let (hda, _, _) = controller();
        assert_eq!(r16(&hda, 0x00) >> 12, OSS as u16);
        assert_eq!(r16(&hda, 0x0e), 0, "nothing while in reset");
        w32(&hda, 0x08, 1);
        assert_eq!(r32(&hda, 0x08) & 1, 1);
        assert_eq!(r16(&hda, 0x0e), 1, "codec 0 present");
        w16(&hda, 0x0e, 1);
        assert_eq!(r16(&hda, 0x0e), 0, "write-1-to-clear");
    }

    #[test]
    fn corb_commands_come_back_in_the_rirb_with_an_interrupt() {
        let (hda, mem, raised) = controller();
        w32(&hda, 0x08, 1);
        w32(&hda, 0x20, intctl::GIE | intctl::CIE);
        w32(&hda, 0x40, 0x1000); // CORB
        w32(&hda, 0x50, 0x2000); // RIRB
        w16(&hda, 0x5a, 1);
        w8(&hda, 0x5c, rirbctl::DMAEN | rirbctl::RINTCTL);
        w8(&hda, 0x4c, corbctl::RUN);
        // GetParameter(VendorID) on the root node, in slot 1.
        mem.write_slice(&0x000f_0000u32.to_le_bytes(), GuestAddress(0x1004))
            .unwrap();
        w16(&hda, 0x48, 1);
        assert_eq!(r16(&hda, 0x4a), 1, "read pointer advanced");
        assert_eq!(r16(&hda, 0x58), 1, "write pointer advanced");
        let mut entry = [0u8; 8];
        mem.read_slice(&mut entry, GuestAddress(0x2008)).unwrap();
        assert_eq!(
            u32::from_le_bytes(entry[0..4].try_into().unwrap()),
            crate::codec::VENDOR_DEVICE_ID
        );
        assert!(raised.load(Ordering::SeqCst) >= 1);
        assert_ne!(r32(&hda, 0x24) & intctl::CIE, 0);
        w8(&hda, 0x5d, rirbsts::RINTFL);
        assert_eq!(r32(&hda, 0x24), 0);
    }

    #[test]
    fn the_immediate_interface_answers_too() {
        let (hda, _, _) = controller();
        w32(&hda, 0x08, 1);
        w32(&hda, 0x60, 0x000f_0000);
        w16(&hda, 0x68, ics::ICB);
        assert_eq!(r16(&hda, 0x68) & (ics::ICB | ics::IRV), ics::IRV);
        assert_eq!(r32(&hda, 0x64), crate::codec::VENDOR_DEVICE_ID);
    }

    #[test]
    fn a_running_stream_walks_its_list_and_interrupts_on_ioc() {
        let (hda, mem, raised) = controller();
        w32(&hda, 0x08, 1);
        w32(&hda, 0x20, intctl::GIE | (1 << OUT));
        let sd = SD_BASE + OUT as u64 * SD_STRIDE;
        // Two 960-byte buffers (5 ms of 48 kHz stereo 16-bit each).
        for (i, addr) in [0x4000u64, 0x5000].iter().enumerate() {
            let at = 0x3000 + i as u64 * 16;
            mem.write_slice(&addr.to_le_bytes(), GuestAddress(at))
                .unwrap();
            mem.write_slice(&960u32.to_le_bytes(), GuestAddress(at + 8))
                .unwrap();
            mem.write_slice(&1u32.to_le_bytes(), GuestAddress(at + 12))
                .unwrap();
        }
        w32(&hda, sd + 0x18, 0x3000);
        w32(&hda, sd + 0x08, 1920);
        w16(&hda, sd + 0x0c, 1);
        w16(&hda, sd + 0x12, 0x0011);
        w32(&hda, 0x70, 0x6001); // position buffer
        w32(&hda, sd, sdctl::RUN | sdctl::IOCE | (1 << 20));
        let mut state = hda.lock();
        let step = next_step(&mut state).expect("a step");
        assert_eq!(step.data.len(), 960, "the whole first buffer (< 10 ms)");
        assert!(step.raise);
        assert_eq!(state.regs.streams[OUT].lpib, 960);
        assert_eq!(state.regs.streams[OUT].entry, 1);
        drop(state);
        let mut pos = [0u8; 4];
        mem.read_slice(&mut pos, GuestAddress(0x6000 + OUT as u64 * 8))
            .unwrap();
        assert_eq!(u32::from_le_bytes(pos), 960);
        assert_eq!(
            r32(&hda, sd) >> 24 & u32::from(sdsts::BCIS),
            u32::from(sdsts::BCIS)
        );
        let _ = raised;
    }

    #[test]
    fn hostile_lists_stop_rather_than_wander() {
        let (hda, _, _) = controller();
        w32(&hda, 0x08, 1);
        let sd = SD_BASE + OUT as u64 * SD_STRIDE;
        // A list far outside guest memory, and a zero-length entry.
        w32(&hda, sd + 0x18, 0xffff_ff80);
        w32(&hda, sd + 0x1c, 0xffff_ffff);
        w32(&hda, sd + 0x08, u32::MAX);
        w16(&hda, sd + 0x0c, 0xffff);
        w16(&hda, sd + 0x12, 0x0011);
        w32(&hda, sd, sdctl::RUN);
        assert!(next_step(&mut hda.lock()).is_none());
        let mut state = hda.lock();
        assert_eq!(state.regs.streams[OUT].cbl, MAX_CBL);
        assert_eq!(state.regs.streams[OUT].lvi, 0xff);
        // A list in RAM whose entry is empty: descriptor error, stopped.
        state.regs.streams[OUT].bdpl = 0x3000;
        state.regs.streams[OUT].bdpu = 0;
        assert!(next_step(&mut state).is_none());
        assert_eq!(state.regs.streams[OUT].ctl & sdctl::RUN, 0);
    }

    #[test]
    fn formats_decode_and_convert() {
        let f = decode_format(0x4011).unwrap();
        assert_eq!((f.rate_hz, f.channels, f.sample_bytes), (44_100, 2, 2));
        let f = decode_format(0x0031).unwrap();
        assert_eq!(f.sample_bytes, 4);
        let mut out = Vec::new();
        to_s16(f, &[1, 2, 3, 4, 5, 6, 7, 8], &mut out);
        assert_eq!(out, [3, 4, 7, 8]);
        assert!(decode_format(0x0071).is_none());
    }

    #[test]
    fn state_round_trips_through_a_snapshot() {
        let (hda, _, _) = controller();
        w32(&hda, 0x08, 1);
        w32(&hda, 0x40, 0x1000);
        w16(&hda, 0x5a, 7);
        let saved = hda.save();
        let other = Hda::new();
        assert!(other.load(&saved));
        assert_eq!(other.save(), saved);
        assert!(!other.load(&saved[1..]));
    }
}
