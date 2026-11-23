//! The boot framebuffer: a PCI display function with a linear framebuffer,
//! for guests that draw into what the firmware's GOP reported (ADR-0008 §1).
//!
//! Linux never needed one. It talks to the firmware over ttyS0 and draws
//! nothing until its own `virtio_gpu` driver binds. Windows draws from the
//! first instruction of `bootmgfw.efi`: the boot manager, Windows Setup and
//! the in-box `BasicDisplay` driver all write into the **linear framebuffer
//! GOP reported**, and keep writing into it after `ExitBootServices`. A
//! `PixelBltOnly` GOP such as `VirtioGpuDxe`'s gives them no address to
//! write to, so the guest boots to a black window.
//!
//! # The interface
//!
//! The Bochs "DISPI" register set (VBE 0xB0C5), MMIO flavour, because that is
//! what EDK2's `QemuVideoDxe` binds and what Linux's `bochs-drm` drives:
//!
//! | BAR | Contents |
//! |---|---|
//! | 0 (64-bit, prefetchable) | VRAM: host pages mapped straight into the guest, through the same [`crate::shm::ShmWindow`] that backs a virtio shared-memory BAR |
//! | 3 (32-bit) | registers: EDID at `0x000`, VGA ports `0x3c0..0x3df` at `0x400` (absorbed), DISPI at `0x500`, the extended block at `0x600` (absent) |
//!
//! Register 3, not 2, because EDK2 numbers BARs *logically*: `PciBusDxe`
//! gives a 64-bit pair one index, so with VRAM in BARs 0–1 the BAR
//! `QemuVideoDxe` asks for as `PCI_BAR_IDX2` is the register at `0x1c`. Put the
//! page at `0x18` and the driver reports "No mmio bar" and falls back to port
//! I/O this machine does not decode. Linux's `bochs-drm` numbers by register
//! and would look at `0x18`; Linux guests use virtio-gpu, so nothing is lost.
//!
//! The identity is `1234:1111`, class `0300`, the de-facto id of this
//! register interface. Using an id is not using QEMU: no code, process or
//! library of it is involved, and the register semantics are the published
//! Bochs VBE ones.
//!
//! VRAM writes never exit: the window is guest memory. Only the register
//! page traps, and a guest touches it on a mode set and never per frame.
//! Something on the host reads the VRAM at the display's refresh rate
//! ([`Framebuffer::scanout`] and [`Framebuffer::read_vram`]), which is the
//! whole presentation path: the guest has no "flush" to tell us when it drew.
//!
//! # On a virtio-gpu ("virtio-vga")
//!
//! The same VRAM and DISPI page can instead belong to the machine's
//! virtio-gpu function ([`FramebufferConfig::on_virtio_gpu`]): one function
//! of class VGA, VRAM in its BAR 0, and the register page as the first page
//! ([`VGA_WINDOW`]) of the virtio register BAR, the virtio regions after it
//! (`virtio_core::pci::RegsPlacement::VGA`). The firmware and `BasicDisplay` draw into VRAM as
//! above; once virtio-win's `viogpudo` binds the very same function, the
//! guest draws through virtio-gpu, resizes with the window, and the VRAM
//! simply stops changing. One adapter, one monitor, whichever driver has it.
//!
//! Only 32 bpp is presented. The other depths are accepted as register values,
//! because a driver probing them must not be refused, but a mode in one of
//! them shows nothing; GOP and `BasicDisplay` only use 32 bpp.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use virtio_core::{ShmAccessError, ShmBacking};

use crate::pci::{ConfigSpace, PciError};

/// The vendor and device id `QemuVideoDxe` and `bochs-drm` both match on.
pub const VENDOR_ID: u16 = 0x1234;
/// See [`VENDOR_ID`].
pub const DEVICE_ID: u16 = 0x1111;
/// Class `03`, subclass `00` (VGA-compatible display), interface `00`.
///
/// VGA rather than "other display" because `BasicDisplay` and the firmware
/// both treat a VGA-class function as the console; the FADT still says
/// `VGA_NOT_PRESENT`, so no guest goes looking for the legacy ports.
pub const CLASS_DISPLAY_VGA: u32 = 0x0300_0000;
/// Revision 2: the one that has the MMIO register BAR.
pub const REVISION: u8 = 2;

/// The BAR that carries VRAM (a 64-bit pair, so register 1 is its high half).
pub const VRAM_BAR_INDEX: u8 = 0;
/// The BAR register that carries the register page: EDK2's logical BAR 2
/// (see the module docs).
pub const REGS_BAR_INDEX: u8 = 3;
/// Size of the register BAR.
pub const REGS_BAR_SIZE: u32 = 0x1000;

/// Default VRAM: 16 MiB, room for two 1920×1080×32 pages.
pub const DEFAULT_VRAM_BYTES: u64 = 16 << 20;
/// Smallest VRAM accepted: one 1024×768×32 page and change.
pub const MIN_VRAM_BYTES: u64 = 4 << 20;
/// Largest VRAM accepted: one 3840×2160×32 page fits in 32 MiB twice over.
pub const MAX_VRAM_BYTES: u64 = 256 << 20;

/// Where the register page puts each block.
mod regs {
    /// The monitor's EDID at offset 0: 128 bytes, the rest of the 1 KiB reads zero.
    pub const EDID_END: u64 = 0x400;
    /// DISPI: index `n` at `0x500 + 2n`, 16 bits each.
    pub const DISPI: u64 = 0x500;
    pub const DISPI_END: u64 = DISPI + 2 * super::dispi::COUNT as u64;
    /// The accelerator's registers (ADR-0013), 32 bits each.
    pub const ACCEL: u64 = 0x600;
    pub const ACCEL_END: u64 = ACCEL + 4 * super::accel::COUNT as u64;
    /// One byte, write-only: a character of the guest drivers' log. A line
    /// feed ends a line, which the host then logs (ADR-0013).
    pub const LOG: u64 = 0x700;
}

/// The accelerator's registers: how a guest driver hands the host a buffer
/// of commands (ADR-0013). The buffer lives at the top of VRAM — memory both
/// sides already share — so the registers only say where, and when.
pub mod accel {
    /// Reads [`MAGIC`] when the host has an accelerator, 0 when not.
    pub const ID: usize = 0;
    /// Offset in VRAM of the command area.
    pub const COMMANDS: usize = 1;
    /// Its size in bytes.
    pub const COMMANDS_BYTES: usize = 2;
    /// Offset in VRAM of the response area.
    pub const RESPONSE: usize = 3;
    /// Its size in bytes.
    pub const RESPONSE_BYTES: usize = 4;
    /// Write: run this many bytes of the command area, now; the write
    /// returns when they have run. Read: bytes of response they produced.
    pub const RUN: usize = 5;
    pub const COUNT: usize = 6;

    /// "ENGL".
    pub const MAGIC: u32 = 0x4c47_4e45;
    /// The top of VRAM set aside: commands, then the response.
    pub const AREA_BYTES: u64 = 4 << 20;
    pub const RESPONSE_AREA_BYTES: u64 = 1 << 20;
    /// VRAM smaller than this has no room to set aside.
    pub const MIN_VRAM_BYTES: u64 = 16 << 20;
}

/// What runs the buffers a guest driver hands over through the accelerator
/// registers. The device knows nothing of what is in them.
pub trait Accelerator: Send + Sync {
    /// Runs `commands` to completion.
    fn run(&self, commands: &[u8]) -> Accelerated;
    /// The guest was reset: forget everything it had set up.
    fn reset(&self);
}

/// What a buffer produced.
#[derive(Debug, Default)]
pub struct Accelerated {
    /// Bytes for the guest's driver; cut to the response area's size.
    pub response: Vec<u8>,
    /// Pictures to put on the guest's screen.
    pub blits: Vec<Blit>,
}

/// `width`x`height` pixels of B, G, R, X, top row first, for the place
/// (`x`, `y`) on the guest's screen.
#[derive(Debug)]
pub struct Blit {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    /// Where on the screen it may land: `[left, top, right, bottom]` each,
    /// right and bottom exclusive. Empty means anywhere.
    pub clip: Vec<[i32; 4]>,
}

/// The DISPI register indices and the bits that mean something.
mod dispi {
    pub const ID: usize = 0;
    pub const XRES: usize = 1;
    pub const YRES: usize = 2;
    pub const BPP: usize = 3;
    pub const ENABLE: usize = 4;
    pub const BANK: usize = 5;
    pub const VIRT_WIDTH: usize = 6;
    pub const VIRT_HEIGHT: usize = 7;
    pub const X_OFFSET: usize = 8;
    pub const Y_OFFSET: usize = 9;
    pub const VIDEO_MEMORY_64K: usize = 10;
    pub const COUNT: usize = 11;

    /// The newest interface id; a driver writes the id it speaks and reads
    /// back what the device accepted.
    pub const ID5: u16 = 0xb0c5;
    pub const ID0: u16 = 0xb0c0;

    pub const ENABLED: u16 = 0x01;
    pub const GETCAPS: u16 = 0x02;
    pub const LFB_ENABLED: u16 = 0x40;
    pub const NOCLEARMEM: u16 = 0x80;

    /// What `GETCAPS` reports as the largest mode.
    pub const MAX_XRES: u16 = 3840;
    pub const MAX_YRES: u16 = 2160;
    pub const MAX_BPP: u16 = 32;
}

/// How the device is built: its VRAM and the EDID it shows.
#[derive(Debug, Clone)]
pub struct FramebufferConfig {
    /// VRAM size in bytes: a power of two from [`MIN_VRAM_BYTES`] to
    /// [`MAX_VRAM_BYTES`], because it is a BAR.
    pub vram_bytes: u64,
    /// The EDID block at register offset 0, whose preferred timing is the
    /// mode `QemuVideoDxe` makes the GOP default. `None` reads as zeroes, and
    /// the firmware falls back to its own default mode.
    pub edid: Option<[u8; 128]>,
    /// Part of the virtio-gpu function rather than a function of its own
    /// (the module docs' "virtio-vga").
    pub on_virtio_gpu: bool,
    /// Put VRAM at this address, below 4 GiB, instead of in the 64-bit
    /// aperture: for a 32-bit guest that reaches it through the BIOS's VBE
    /// (ADR-0012), which can name no address above.
    pub low_vram_base: Option<u64>,
}

impl Default for FramebufferConfig {
    fn default() -> Self {
        Self {
            vram_bytes: DEFAULT_VRAM_BYTES,
            edid: None,
            on_virtio_gpu: false,
            low_vram_base: None,
        }
    }
}

/// Where the register page shows through a virtio-vga function's register
/// BAR: its first page, offsets as in the standalone BAR. After it is
/// virtio's.
pub const VGA_WINDOW: std::ops::Range<u64> = 0..REGS_BAR_SIZE as u64;

/// Errors building the device.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FramebufferError {
    #[error(
        "framebuffer VRAM of {bytes} bytes must be a power of two from {MIN_VRAM_BYTES} to \
         {MAX_VRAM_BYTES} bytes"
    )]
    BadVramSize { bytes: u64 },
    #[error("the framebuffer's VRAM backing is {got} bytes, not the {want} it was built for")]
    BackingSize { got: u64, want: u64 },
    #[error(transparent)]
    Pci(#[from] PciError),
}

/// What the guest currently shows, in VRAM terms.
///
/// Every field is already validated against the VRAM: a host reading
/// `height` rows of `width * 4` bytes, `stride` apart from `offset`, stays
/// inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanoutMode {
    pub width: u32,
    pub height: u32,
    /// Bytes from one row to the next.
    pub stride: u32,
    /// Byte offset of the top-left visible pixel.
    pub offset: u64,
}

/// The register file, as the guest last wrote it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Dispi {
    regs: [u16; dispi::COUNT],
}

impl Dispi {
    fn power_on(vram_bytes: u64) -> Self {
        let mut regs = [0u16; dispi::COUNT];
        regs[dispi::ID] = dispi::ID5;
        regs[dispi::VIDEO_MEMORY_64K] = u16::try_from(vram_bytes >> 16).unwrap_or(u16::MAX);
        Self { regs }
    }
}

/// The boot framebuffer device: the register page and the VRAM behind BAR 0.
pub struct Framebuffer {
    dispi: Mutex<Dispi>,
    edid: [u8; 128],
    vram: Arc<dyn ShmBacking>,
    vram_bytes: u64,
    /// Bumped on every register write that can change what is shown, so a
    /// presenter can tell "the guest set a mode" from "the guest drew".
    generation: AtomicU64,
    accelerator: std::sync::OnceLock<Arc<dyn Accelerator>>,
    /// Bytes of response the last buffer produced.
    accel_response: std::sync::atomic::AtomicU32,
    /// The line of guest log being written.
    log: Mutex<Vec<u8>>,
}

impl std::fmt::Debug for Framebuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Framebuffer")
            .field("vram_bytes", &self.vram_bytes)
            .field("mode", &self.scanout())
            .finish()
    }
}

impl Framebuffer {
    /// Checks a VRAM size before anything is allocated for it.
    pub fn validate(config: &FramebufferConfig) -> Result<(), FramebufferError> {
        let bytes = config.vram_bytes;
        if !bytes.is_power_of_two() || !(MIN_VRAM_BYTES..=MAX_VRAM_BYTES).contains(&bytes) {
            return Err(FramebufferError::BadVramSize { bytes });
        }
        Ok(())
    }

    /// The device over `vram`, which must be exactly `config.vram_bytes` long.
    pub fn new(
        config: &FramebufferConfig,
        vram: Arc<dyn ShmBacking>,
    ) -> Result<Self, FramebufferError> {
        Self::validate(config)?;
        if vram.len() != config.vram_bytes {
            return Err(FramebufferError::BackingSize {
                got: vram.len(),
                want: config.vram_bytes,
            });
        }
        Ok(Self {
            dispi: Mutex::new(Dispi::power_on(config.vram_bytes)),
            edid: config.edid.unwrap_or([0; 128]),
            vram,
            vram_bytes: config.vram_bytes,
            generation: AtomicU64::new(0),
            accelerator: std::sync::OnceLock::new(),
            accel_response: std::sync::atomic::AtomicU32::new(0),
            log: Mutex::new(Vec::new()),
        })
    }

    /// One character of the guest drivers' log. Lines are cut at 200 bytes
    /// and logged with their unprintable bytes replaced: the text is the
    /// guest's.
    fn log_byte(&self, byte: u8) {
        const LINE: usize = 200;
        let mut line = self
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if byte != b'\n' {
            line.push(if byte.is_ascii_graphic() || byte == b' ' {
                byte
            } else {
                b'?'
            });
        }
        if byte == b'\n' || line.len() >= LINE {
            tracing::info!(target: "guest_driver", "{}", String::from_utf8_lossy(&line));
            line.clear();
        }
    }

    /// Gives the device an accelerator (ADR-0013); `false` if it has one, or
    /// its VRAM is too small to set a transfer area aside.
    pub fn set_accelerator(&self, accelerator: Arc<dyn Accelerator>) -> bool {
        self.vram_bytes >= accel::MIN_VRAM_BYTES && self.accelerator.set(accelerator).is_ok()
    }

    fn read_accel(&self, index: usize) -> u32 {
        if self.accelerator.get().is_none() {
            return 0;
        }
        let area = self.vram_bytes - accel::AREA_BYTES;
        let commands = accel::AREA_BYTES - accel::RESPONSE_AREA_BYTES;
        let value = match index {
            accel::ID => return accel::MAGIC,
            accel::COMMANDS => area,
            accel::COMMANDS_BYTES => commands,
            accel::RESPONSE => area + commands,
            accel::RESPONSE_BYTES => accel::RESPONSE_AREA_BYTES,
            accel::RUN => return self.accel_response.load(Ordering::Acquire),
            _ => 0,
        };
        u32::try_from(value).unwrap_or(0)
    }

    /// Runs `bytes` bytes of the command area and leaves the response in the
    /// response area. On the vCPU thread: the guest's driver is waiting.
    fn run_accel(&self, bytes: u32) {
        let Some(accelerator) = self.accelerator.get() else {
            return;
        };
        self.accel_response.store(0, Ordering::Release);
        let area = self.vram_bytes - accel::AREA_BYTES;
        let capacity = accel::AREA_BYTES - accel::RESPONSE_AREA_BYTES;
        let bytes = u64::from(bytes).min(capacity);
        // A copy, so the guest cannot change a command after it was checked.
        let mut commands = vec![0u8; bytes as usize];
        if self.vram.read(area, &mut commands).is_err() {
            return;
        }
        let outcome = accelerator.run(&commands);
        for blit in &outcome.blits {
            self.blit(blit);
        }
        let kept = outcome
            .response
            .len()
            .min(accel::RESPONSE_AREA_BYTES as usize);
        let response = &outcome.response[..kept];
        if self.vram.write(area + capacity, response).is_ok() {
            self.accel_response
                .store(response.len() as u32, Ordering::Release);
        }
    }

    /// Puts a picture on the guest's screen, clipped to the mode it shows
    /// and to the picture's own clip rectangles.
    fn blit(&self, blit: &Blit) {
        let Some(mode) = self.scanout() else {
            return;
        };
        let row = blit.width as usize * 4;
        if row == 0 || blit.pixels.len() != row * blit.height as usize {
            return;
        }
        let screen = [0, 0, i64::from(mode.width), i64::from(mode.height)];
        if blit.clip.is_empty() {
            self.blit_within(blit, &mode, screen);
        }
        for clip in &blit.clip {
            let [left, top, right, bottom] = clip.map(i64::from);
            self.blit_within(
                blit,
                &mode,
                [
                    left.max(0),
                    top.max(0),
                    right.min(screen[2]),
                    bottom.min(screen[3]),
                ],
            );
        }
    }

    /// The part of `blit` inside `[left, top, right, bottom)`, which is
    /// inside the screen.
    fn blit_within(&self, blit: &Blit, mode: &ScanoutMode, within: [i64; 4]) {
        let row = blit.width as usize * 4;
        let left = i64::from(blit.x).max(within[0]);
        let right = (i64::from(blit.x) + i64::from(blit.width)).min(within[2]);
        if left >= right {
            return;
        }
        let skip = (left - i64::from(blit.x)) as usize * 4;
        let take = (right - left) as usize * 4;
        for (line, pixels) in blit.pixels.chunks_exact(row).enumerate() {
            let y = i64::from(blit.y) + line as i64;
            if y < within[1] || y >= within[3] {
                continue;
            }
            let at = mode.offset + y as u64 * u64::from(mode.stride) + left as u64 * 4;
            if self.vram.write(at, &pixels[skip..skip + take]).is_err() {
                return;
            }
        }
    }

    /// The function's configuration space: VRAM in a 64-bit prefetchable BAR
    /// at `vram_base`, the register page in a 32-bit one at `regs_base`, no
    /// interrupt.
    pub fn config_space(
        vram_bytes: u64,
        vram_base: u64,
        regs_base: u32,
    ) -> Result<ConfigSpace, PciError> {
        ConfigSpace::type0(VENDOR_ID, DEVICE_ID, CLASS_DISPLAY_VGA, REVISION)
            .with_subsystem(VENDOR_ID, DEVICE_ID)
            .with_memory_bar64(VRAM_BAR_INDEX, vram_base, vram_bytes)?
            .with_memory_bar(REGS_BAR_INDEX, regs_base, REGS_BAR_SIZE)
    }

    pub fn vram_bytes(&self) -> u64 {
        self.vram_bytes
    }

    /// Changes whenever the guest writes a register that can change the mode.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn lock(&self) -> MutexGuard<'_, Dispi> {
        // The register file is plain data; a panic elsewhere cannot have left
        // it half-written in a way worse than the guest could write it.
        self.dispi
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The mode the guest is showing, or `None` when it shows nothing this
    /// host can present: the device disabled, the linear framebuffer off, a
    /// depth other than 32 bpp, or a geometry that leaves the VRAM.
    pub fn scanout(&self) -> Option<ScanoutMode> {
        let dispi = *self.lock();
        let r = &dispi.regs;
        let enable = r[dispi::ENABLE];
        if enable & dispi::ENABLED == 0 || enable & dispi::LFB_ENABLED == 0 {
            return None;
        }
        if r[dispi::BPP] != 32 {
            return None;
        }
        let width = u32::from(r[dispi::XRES]);
        let height = u32::from(r[dispi::YRES]);
        let virt_width = u32::from(r[dispi::VIRT_WIDTH]).max(width);
        if width == 0 || height == 0 {
            return None;
        }
        let stride = virt_width.checked_mul(4)?;
        let offset = (u64::from(r[dispi::Y_OFFSET]) * u64::from(virt_width)
            + u64::from(r[dispi::X_OFFSET]))
            * 4;
        let last_row = offset.checked_add(u64::from(stride) * u64::from(height - 1))?;
        let end = last_row.checked_add(u64::from(width) * 4)?;
        (end <= self.vram_bytes).then_some(ScanoutMode {
            width,
            height,
            stride,
            offset,
        })
    }

    /// Copies VRAM bytes out, for the presenter.
    pub fn read_vram(&self, offset: u64, buf: &mut [u8]) -> Result<(), ShmAccessError> {
        self.vram.read(offset, buf)
    }

    /// Power-on (ADR-0005): the registers back to "disabled", VRAM cleared.
    pub fn reset(&self) {
        *self.lock() = Dispi::power_on(self.vram_bytes);
        if let Some(accelerator) = self.accelerator.get() {
            accelerator.reset();
        }
        self.accel_response.store(0, Ordering::Release);
        self.clear_vram();
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    fn clear_vram(&self) {
        if let Err(error) = self.vram.fill(0, self.vram_bytes, 0) {
            tracing::warn!(%error, "could not clear the framebuffer's VRAM");
        }
    }

    /// Sets a mode as the BIOS's VBE does on a guest's behalf: `width` x
    /// `height` at `bpp`, with the linear framebuffer on.
    pub fn set_mode(&self, width: u16, height: u16, bpp: u16, clear: bool) {
        let register = |index: usize| regs::DISPI + 2 * index as u64;
        let mut enable = dispi::ENABLED | dispi::LFB_ENABLED;
        if !clear {
            enable |= dispi::NOCLEARMEM;
        }
        for (index, value) in [
            (dispi::ENABLE, 0),
            (dispi::XRES, width),
            (dispi::YRES, height),
            (dispi::BPP, bpp),
            (dispi::VIRT_WIDTH, width),
            (dispi::ENABLE, enable),
        ] {
            self.mmio_write(register(index), &value.to_le_bytes());
        }
    }

    /// Turns the framebuffer off: the VGA has the screen again.
    pub fn disable(&self) {
        self.mmio_write(regs::DISPI + 2 * dispi::ENABLE as u64, &0u16.to_le_bytes());
    }

    /// Guest read inside the register BAR. Unclaimed offsets read zero.
    pub fn mmio_read(&self, offset: u64, data: &mut [u8]) {
        data.fill(0);
        let Some(end) = offset.checked_add(data.len() as u64) else {
            return;
        };
        if end <= regs::EDID_END {
            let edid = &self.edid;
            for (i, byte) in data.iter_mut().enumerate() {
                let at = offset as usize + i;
                *byte = edid.get(at).copied().unwrap_or(0);
            }
            return;
        }
        if offset >= regs::ACCEL && end <= regs::ACCEL_END {
            if data.len() == 4 && offset % 4 == 0 {
                let value = self.read_accel(((offset - regs::ACCEL) / 4) as usize);
                data.copy_from_slice(&value.to_le_bytes());
            }
            return;
        }
        if offset >= regs::DISPI && end <= regs::DISPI_END {
            let dispi = *self.lock();
            for (i, byte) in data.iter_mut().enumerate() {
                let at = (offset - regs::DISPI) as usize + i;
                let value = self.read_dispi(&dispi, at / 2);
                *byte = value.to_le_bytes()[at % 2];
            }
        }
    }

    fn read_dispi(&self, dispi: &Dispi, index: usize) -> u16 {
        let r = &dispi.regs;
        let caps = r[dispi::ENABLE] & dispi::GETCAPS != 0;
        match index {
            dispi::XRES if caps => dispi::MAX_XRES,
            dispi::YRES if caps => dispi::MAX_YRES,
            dispi::BPP if caps => dispi::MAX_BPP,
            _ => r.get(index).copied().unwrap_or(0),
        }
    }

    /// Guest write inside the register BAR. Only whole, aligned 16-bit DISPI
    /// writes do anything; everything else (the VGA ports, the EDID, stray
    /// bytes) is absorbed.
    pub fn mmio_write(&self, offset: u64, data: &[u8]) {
        if offset == regs::LOG {
            if let [byte] = data {
                self.log_byte(*byte);
            }
            return;
        }
        if offset == regs::ACCEL + 4 * accel::RUN as u64 {
            if let Ok(bytes) = <[u8; 4]>::try_from(data) {
                self.run_accel(u32::from_le_bytes(bytes));
            }
            return;
        }
        if !(regs::DISPI..regs::DISPI_END).contains(&offset) {
            return;
        }
        let [lo, hi] = match data {
            [lo, hi] => [*lo, *hi],
            // A 32-bit write covers this register and the next one.
            [lo, hi, lo2, hi2] if offset + 4 <= regs::DISPI_END && offset % 2 == 0 => {
                self.mmio_write(offset, &[*lo, *hi]);
                self.mmio_write(offset + 2, &[*lo2, *hi2]);
                return;
            }
            _ => return,
        };
        if offset % 2 != 0 {
            return;
        }
        let index = ((offset - regs::DISPI) / 2) as usize;
        let value = u16::from_le_bytes([lo, hi]);
        let clear = {
            let mut dispi = self.lock();
            Self::write_dispi(&mut dispi, index, value, self.vram_bytes)
        };
        if clear {
            self.clear_vram();
        }
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Applies one register write. Returns whether VRAM must be cleared.
    fn write_dispi(dispi: &mut Dispi, index: usize, value: u16, vram_bytes: u64) -> bool {
        let r = &mut dispi.regs;
        match index {
            dispi::ID => {
                // The driver announces the newest interface it speaks; we
                // speak all of them up to ID5.
                if (dispi::ID0..=dispi::ID5).contains(&value) {
                    r[dispi::ID] = value;
                }
                false
            }
            dispi::XRES | dispi::YRES | dispi::BPP | dispi::BANK => {
                r[index] = value;
                false
            }
            dispi::ENABLE => {
                let was_enabled = r[dispi::ENABLE] & dispi::ENABLED != 0;
                r[dispi::ENABLE] = value;
                let enabling = value & dispi::ENABLED != 0 && !was_enabled;
                if enabling {
                    // What the Bochs VBE does on enable: the virtual screen is
                    // at least the visible one, as tall as the VRAM allows,
                    // and the view starts at the top.
                    let bytes = u64::from(r[dispi::BPP].div_ceil(8).max(1));
                    let width = r[dispi::XRES].max(r[dispi::VIRT_WIDTH]);
                    r[dispi::VIRT_WIDTH] = width;
                    let row = u64::from(width.max(1)) * bytes;
                    r[dispi::VIRT_HEIGHT] = u16::try_from(vram_bytes / row).unwrap_or(u16::MAX);
                    r[dispi::X_OFFSET] = 0;
                    r[dispi::Y_OFFSET] = 0;
                }
                enabling && value & dispi::NOCLEARMEM == 0
            }
            dispi::VIRT_WIDTH | dispi::X_OFFSET | dispi::Y_OFFSET => {
                r[index] = value;
                false
            }
            // VIRT_HEIGHT and VIDEO_MEMORY_64K are the device's to say.
            _ => false,
        }
    }

    /// All of VRAM, for a snapshot (ADR-0006): what the guest drew is guest
    /// state, and a resumed `BasicDisplay` would not redraw it.
    pub fn save_vram(&self) -> Result<Vec<u8>, ShmAccessError> {
        let mut vram = vec![0; self.vram_bytes as usize];
        self.vram.read(0, &mut vram)?;
        Ok(vram)
    }

    /// Puts saved VRAM back. It must be exactly this device's size.
    pub fn load_vram(&self, vram: &[u8]) -> Result<(), crate::state::StateError> {
        if vram.len() as u64 != self.vram_bytes {
            return Err(crate::state::StateError::Count {
                what: "framebuffer VRAM bytes",
                snapshot: vram.len(),
                current: self.vram_bytes as usize,
            });
        }
        self.vram
            .write(0, vram)
            .map_err(|_| crate::state::StateError::Poisoned("the framebuffer's VRAM"))?;
        self.generation.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// The register file, for a snapshot (ADR-0006).
    pub fn save_registers(&self) -> Vec<u16> {
        self.lock().regs.to_vec()
    }

    /// Puts a saved register file back.
    pub fn load_registers(&self, regs: &[u16]) -> Result<(), crate::state::StateError> {
        let regs: [u16; dispi::COUNT] =
            regs.try_into()
                .map_err(|_| crate::state::StateError::Count {
                    what: "framebuffer registers",
                    snapshot: regs.len(),
                    current: dispi::COUNT,
                })?;
        self.lock().regs = regs;
        self.generation.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// VRAM as plain host memory, for tests that need no hypervisor.
    struct Vram(Mutex<Vec<u8>>);

    impl ShmBacking for Vram {
        fn len(&self) -> u64 {
            self.0.lock().unwrap().len() as u64
        }
        fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), ShmAccessError> {
            let vram = self.0.lock().unwrap();
            let at = offset as usize;
            buf.copy_from_slice(&vram[at..at + buf.len()]);
            Ok(())
        }
        fn write(&self, offset: u64, data: &[u8]) -> Result<(), ShmAccessError> {
            let mut vram = self.0.lock().unwrap();
            let at = offset as usize;
            vram[at..at + data.len()].copy_from_slice(data);
            Ok(())
        }
        fn fill(&self, offset: u64, len: u64, byte: u8) -> Result<(), ShmAccessError> {
            let mut vram = self.0.lock().unwrap();
            vram[offset as usize..(offset + len) as usize].fill(byte);
            Ok(())
        }
    }

    fn device() -> (Framebuffer, Arc<Vram>) {
        let vram = Arc::new(Vram(Mutex::new(vec![0; MIN_VRAM_BYTES as usize])));
        let config = FramebufferConfig {
            vram_bytes: MIN_VRAM_BYTES,
            edid: Some([0xab; 128]),
            ..FramebufferConfig::default()
        };
        let backing: Arc<dyn ShmBacking> = vram.clone();
        (Framebuffer::new(&config, backing).unwrap(), vram)
    }

    fn write(fb: &Framebuffer, index: usize, value: u16) {
        fb.mmio_write(regs::DISPI + 2 * index as u64, &value.to_le_bytes());
    }

    fn read(fb: &Framebuffer, index: usize) -> u16 {
        let mut data = [0u8; 2];
        fb.mmio_read(regs::DISPI + 2 * index as u64, &mut data);
        u16::from_le_bytes(data)
    }

    /// The sequence `QemuVideoDxe`'s `InitializeBochsGraphicsMode` writes.
    fn set_mode(fb: &Framebuffer, width: u16, height: u16) {
        write(fb, dispi::ENABLE, 0);
        write(fb, dispi::BANK, 0);
        write(fb, dispi::X_OFFSET, 0);
        write(fb, dispi::Y_OFFSET, 0);
        write(fb, dispi::BPP, 32);
        write(fb, dispi::XRES, width);
        write(fb, dispi::YRES, height);
        write(fb, dispi::VIRT_WIDTH, width);
        write(fb, dispi::VIRT_HEIGHT, height);
        write(fb, dispi::ENABLE, dispi::ENABLED | dispi::LFB_ENABLED);
    }

    /// Answers every buffer with its length, and one 2x2 picture at the
    /// place the buffer's first two bytes name.
    struct Echo(Mutex<Vec<Vec<u8>>>, AtomicU64);

    impl Accelerator for Echo {
        fn run(&self, commands: &[u8]) -> Accelerated {
            self.0.lock().unwrap().push(commands.to_vec());
            let at = |i: usize| i32::from(commands.get(i).copied().unwrap_or(0) as i8);
            Accelerated {
                response: (commands.len() as u32).to_le_bytes().to_vec(),
                blits: vec![Blit {
                    x: at(0),
                    y: at(1),
                    width: 2,
                    height: 2,
                    pixels: (1..=16).collect(),
                    // A third byte asks for the right column only.
                    clip: match commands.get(2) {
                        Some(1) => vec![[at(0) + 1, -5, 4000, 4000]],
                        _ => Vec::new(),
                    },
                }],
            }
        }
        fn reset(&self) {
            self.1.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn accelerated() -> (Framebuffer, Arc<Vram>, Arc<Echo>) {
        let bytes = accel::MIN_VRAM_BYTES;
        let vram = Arc::new(Vram(Mutex::new(vec![0; bytes as usize])));
        let config = FramebufferConfig {
            vram_bytes: bytes,
            ..FramebufferConfig::default()
        };
        let backing: Arc<dyn ShmBacking> = vram.clone();
        let fb = Framebuffer::new(&config, backing).unwrap();
        let echo = Arc::new(Echo(Mutex::new(Vec::new()), AtomicU64::new(0)));
        assert!(fb.set_accelerator(echo.clone()));
        (fb, vram, echo)
    }

    fn accel_read(fb: &Framebuffer, index: usize) -> u32 {
        let mut data = [0u8; 4];
        fb.mmio_read(regs::ACCEL + 4 * index as u64, &mut data);
        u32::from_le_bytes(data)
    }

    fn accel_run(fb: &Framebuffer, bytes: u32) {
        fb.mmio_write(regs::ACCEL + 4 * accel::RUN as u64, &bytes.to_le_bytes());
    }

    #[test]
    fn a_device_without_an_accelerator_says_so() {
        let (fb, _) = device();
        assert_eq!(accel_read(&fb, accel::ID), 0);
        assert_eq!(accel_read(&fb, accel::COMMANDS), 0);
        // Nothing to run them: the write is absorbed.
        accel_run(&fb, 64);
        assert_eq!(accel_read(&fb, accel::RUN), 0);
        // And 4 MiB of VRAM has no room for the transfer area.
        struct Never;
        impl Accelerator for Never {
            fn run(&self, _: &[u8]) -> Accelerated {
                unreachable!()
            }
            fn reset(&self) {}
        }
        assert!(!fb.set_accelerator(Arc::new(Never)));
    }

    #[test]
    fn a_buffer_at_the_top_of_vram_is_run_and_answered() {
        let (fb, vram, echo) = accelerated();
        assert_eq!(accel_read(&fb, accel::ID), accel::MAGIC);
        let commands = u64::from(accel_read(&fb, accel::COMMANDS));
        let response = u64::from(accel_read(&fb, accel::RESPONSE));
        assert_eq!(commands, (12 << 20));
        assert_eq!(accel_read(&fb, accel::COMMANDS_BYTES), 3 << 20);
        assert_eq!(response, 15 << 20);
        assert_eq!(accel_read(&fb, accel::RESPONSE_BYTES), 1 << 20);

        set_mode(&fb, 640, 480);
        vram.write(commands, &[3, 1, 9, 9, 9]).unwrap();
        accel_run(&fb, 5);
        assert_eq!(echo.0.lock().unwrap()[0], [3, 1, 9, 9, 9]);
        // The answer: its length in the register, its bytes in the area.
        assert_eq!(accel_read(&fb, accel::RUN), 4);
        let mut answer = [0u8; 4];
        vram.read(response, &mut answer).unwrap();
        assert_eq!(u32::from_le_bytes(answer), 5);
        // The picture, at (3, 1) of a 640-wide screen.
        let mut row = [0u8; 8];
        vram.read((640 + 3) * 4, &mut row).unwrap();
        assert_eq!(row, [1, 2, 3, 4, 5, 6, 7, 8]);
        vram.read((2 * 640 + 3) * 4, &mut row).unwrap();
        assert_eq!(row, [9, 10, 11, 12, 13, 14, 15, 16]);
    }

    #[test]
    fn a_length_past_the_command_area_is_cut_to_it() {
        let (fb, _, echo) = accelerated();
        accel_run(&fb, u32::MAX);
        assert_eq!(echo.0.lock().unwrap()[0].len(), 3 << 20);
    }

    #[test]
    fn pictures_are_clipped_to_the_screen() {
        let (fb, vram, _) = accelerated();
        let commands = u64::from(accel_read(&fb, accel::COMMANDS));
        set_mode(&fb, 640, 480);
        // At (-1, -1): only the picture's last pixel lands, at (0, 0).
        vram.write(commands, &[0xff, 0xff]).unwrap();
        accel_run(&fb, 2);
        let mut pixels = [0u8; 8];
        vram.read(0, &mut pixels).unwrap();
        assert_eq!(pixels, [13, 14, 15, 16, 0, 0, 0, 0]);
        // With no mode set there is no screen to put it on.
        let (fb, vram, _) = accelerated();
        accel_run(&fb, 2);
        let mut pixels = [0u8; 8];
        vram.read(0, &mut pixels).unwrap();
        assert_eq!(pixels, [0; 8]);
    }

    #[test]
    fn pictures_keep_to_their_clip_rectangles() {
        let (fb, vram, _) = accelerated();
        let commands = u64::from(accel_read(&fb, accel::COMMANDS));
        set_mode(&fb, 640, 480);
        // At (3, 1), right column only: (4, 1) and (4, 2) are written,
        // (3, 1) and (3, 2) are not.
        vram.write(commands, &[3, 1, 1]).unwrap();
        accel_run(&fb, 3);
        let mut row = [0u8; 8];
        vram.read((640 + 3) * 4, &mut row).unwrap();
        assert_eq!(row, [0, 0, 0, 0, 5, 6, 7, 8]);
        vram.read((2 * 640 + 3) * 4, &mut row).unwrap();
        assert_eq!(row, [0, 0, 0, 0, 13, 14, 15, 16]);
    }

    #[test]
    fn a_reset_reaches_the_accelerator() {
        let (fb, _, echo) = accelerated();
        accel_run(&fb, 1);
        assert_eq!(accel_read(&fb, accel::RUN), 4);
        fb.reset();
        assert_eq!(echo.1.load(Ordering::Acquire), 1);
        assert_eq!(accel_read(&fb, accel::RUN), 0);
    }

    #[test]
    fn identifies_as_the_newest_dispi_and_reports_its_vram() {
        let (fb, _) = device();
        assert_eq!(read(&fb, dispi::ID), dispi::ID5);
        assert_eq!(
            read(&fb, dispi::VIDEO_MEMORY_64K),
            (MIN_VRAM_BYTES >> 16) as u16
        );
        write(&fb, dispi::ID, dispi::ID0 + 4);
        assert_eq!(read(&fb, dispi::ID), dispi::ID0 + 4);
        write(&fb, dispi::ID, 0x1234);
        assert_eq!(
            read(&fb, dispi::ID),
            dispi::ID0 + 4,
            "a bogus id is ignored"
        );
    }

    #[test]
    fn getcaps_reports_the_largest_mode() {
        let (fb, _) = device();
        write(&fb, dispi::ENABLE, dispi::GETCAPS);
        assert_eq!(read(&fb, dispi::XRES), dispi::MAX_XRES);
        assert_eq!(read(&fb, dispi::YRES), dispi::MAX_YRES);
        assert_eq!(read(&fb, dispi::BPP), dispi::MAX_BPP);
        write(&fb, dispi::ENABLE, 0);
        assert_eq!(read(&fb, dispi::XRES), 0);
    }

    #[test]
    fn a_disabled_device_shows_nothing() {
        let (fb, _) = device();
        assert_eq!(fb.scanout(), None);
    }

    #[test]
    fn a_mode_set_shows_that_mode_and_clears_vram() {
        let (fb, vram) = device();
        vram.0.lock().unwrap().fill(0x5a);
        set_mode(&fb, 1024, 768);
        assert_eq!(
            fb.scanout(),
            Some(ScanoutMode {
                width: 1024,
                height: 768,
                stride: 4096,
                offset: 0
            })
        );
        assert!(vram.0.lock().unwrap().iter().all(|&b| b == 0));
        assert_eq!(
            read(&fb, dispi::VIRT_HEIGHT),
            (MIN_VRAM_BYTES / 4096) as u16
        );
    }

    #[test]
    fn noclearmem_keeps_vram() {
        let (fb, vram) = device();
        vram.0.lock().unwrap().fill(0x5a);
        write(&fb, dispi::BPP, 32);
        write(&fb, dispi::XRES, 800);
        write(&fb, dispi::YRES, 600);
        write(
            &fb,
            dispi::ENABLE,
            dispi::ENABLED | dispi::LFB_ENABLED | dispi::NOCLEARMEM,
        );
        assert!(vram.0.lock().unwrap().iter().all(|&b| b == 0x5a));
    }

    #[test]
    fn a_mode_larger_than_vram_shows_nothing() {
        let (fb, _) = device();
        set_mode(&fb, 3840, 2160);
        assert_eq!(fb.scanout(), None, "33 MB does not fit in 4 MiB");
    }

    #[test]
    fn a_panned_view_that_leaves_vram_shows_nothing() {
        let (fb, _) = device();
        set_mode(&fb, 1024, 768);
        write(&fb, dispi::Y_OFFSET, 400);
        assert_eq!(fb.scanout(), None);
        write(&fb, dispi::Y_OFFSET, 100);
        assert_eq!(fb.scanout().unwrap().offset, 100 * 4096);
    }

    #[test]
    fn only_32_bpp_is_presented() {
        let (fb, _) = device();
        set_mode(&fb, 640, 480);
        write(&fb, dispi::BPP, 16);
        assert_eq!(fb.scanout(), None);
    }

    #[test]
    fn the_edid_reads_back_and_everything_else_reads_zero() {
        let (fb, _) = device();
        let mut data = [0u8; 4];
        fb.mmio_read(0, &mut data);
        assert_eq!(data, [0xab; 4]);
        fb.mmio_read(0x80, &mut data);
        assert_eq!(data, [0; 4], "past the 128-byte block");
        fb.mmio_read(0x600, &mut data);
        assert_eq!(data, [0; 4], "no extended register block");
        fb.mmio_read(u64::MAX - 1, &mut data);
        assert_eq!(data, [0; 4], "an overflowing offset is not a panic");
    }

    #[test]
    fn a_32_bit_write_sets_two_registers() {
        let (fb, _) = device();
        let mut data = [0u8; 4];
        data[..2].copy_from_slice(&640u16.to_le_bytes());
        data[2..].copy_from_slice(&480u16.to_le_bytes());
        fb.mmio_write(regs::DISPI + 2 * dispi::XRES as u64, &data);
        assert_eq!(read(&fb, dispi::XRES), 640);
        assert_eq!(read(&fb, dispi::YRES), 480);
    }

    #[test]
    fn stray_writes_are_absorbed() {
        let (fb, _) = device();
        fb.mmio_write(0x400, &[0x20]);
        fb.mmio_write(0, &[1, 2, 3, 4]);
        fb.mmio_write(regs::DISPI + 1, &[1, 2]);
        fb.mmio_write(regs::DISPI, &[1]);
        fb.mmio_write(regs::DISPI_END, &[1, 2]);
        assert_eq!(read(&fb, dispi::ID), dispi::ID5);
        let mut edid = [0u8; 1];
        fb.mmio_read(0, &mut edid);
        assert_eq!(edid, [0xab]);
    }

    #[test]
    fn reset_returns_to_power_on() {
        let (fb, vram) = device();
        set_mode(&fb, 800, 600);
        vram.0.lock().unwrap().fill(0x11);
        let before = fb.generation();
        fb.reset();
        assert_eq!(fb.scanout(), None);
        assert!(vram.0.lock().unwrap().iter().all(|&b| b == 0));
        assert_ne!(fb.generation(), before);
    }

    #[test]
    fn registers_round_trip_through_a_snapshot() {
        let (fb, _) = device();
        set_mode(&fb, 800, 600);
        let saved = fb.save_registers();
        let (other, _) = device();
        other.load_registers(&saved).unwrap();
        assert_eq!(other.scanout(), fb.scanout());
        assert!(other.load_registers(&saved[1..]).is_err());
    }

    #[test]
    fn vram_sizes_are_validated() {
        for bytes in [
            0,
            MIN_VRAM_BYTES / 2,
            MIN_VRAM_BYTES + 1,
            MAX_VRAM_BYTES * 2,
        ] {
            let config = FramebufferConfig {
                vram_bytes: bytes,
                ..FramebufferConfig::default()
            };
            assert!(Framebuffer::validate(&config).is_err(), "{bytes}");
        }
        assert!(Framebuffer::validate(&FramebufferConfig::default()).is_ok());
    }

    #[test]
    fn the_config_space_is_what_qemuvideodxe_matches() {
        let config = Framebuffer::config_space(DEFAULT_VRAM_BYTES, 1 << 34, 0xc000_8000).unwrap();
        assert_eq!(
            config.read_dword(0x00),
            u32::from(VENDOR_ID) | (u32::from(DEVICE_ID) << 16)
        );
        assert_eq!(config.read_dword(0x08) >> 8, CLASS_DISPLAY_VGA >> 8);
        assert_eq!(config.read_dword(0x08) & 0xff, u32::from(REVISION));
        // BAR0/1: 64-bit prefetchable memory; BAR2 empty; BAR3: 32-bit
        // memory, which is EDK2's logical BAR 2.
        assert_eq!(config.read_dword(0x10) & 0xf, 0b1100);
        assert_eq!(config.read_dword(0x14), 1 << 2);
        assert_eq!(config.read_dword(0x18), 0);
        assert_eq!(config.read_dword(0x1c), 0xc000_8000);
    }
}
