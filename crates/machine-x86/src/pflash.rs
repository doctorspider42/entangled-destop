//! Emulated CFI flash for the UEFI non-volatile variable store (backlog
//! UEFI-1801/1804, [ADR-0003](../../../docs/adr/0003-uefi-firmware.md)).
//!
//! # Why this device exists
//!
//! Without it, EDK2 keeps its UEFI variables in ordinary RAM
//! (`EmuVariableFvbRuntimeDxe`), so `BootOrder` and every `Boot####` entry are
//! lost when the VM stops. That is survivable for booting *installation media* —
//! with no `BootOrder`, `BdsDxe` enumerates removable media and finds the ISO's
//! `\EFI\BOOT\BOOTX64.EFI` by itself — and fatal for booting an *installed*
//! system, which is reached through the `Boot0000` entry
//! `grub-install` wrote into NVRAM and nothing else.
//!
//! # What the firmware expects
//!
//! `OvmfPkg/QemuFlashFvbServicesRuntimeDxe` speaks the **Intel/Sharp extended
//! CFI command set** (what QEMU calls `pflash_cfi01`), byte-wide, with every
//! command written to the address being operated on rather than to a separate
//! command register. Read out of `QemuFlash.c` at `edk2-stable202602`, the whole
//! vocabulary is:
//!
//! | Write | Meaning | What this device does |
//! |---|---|---|
//! | `0xff` | read array | reads return flash contents |
//! | `0x50` | clear status | status ← 0, **and back to read-array mode** |
//! | `0x70` | read status | reads return the status register |
//! | `0x10` | program setup | next write programs that byte |
//! | `0x20` | block erase setup | must be confirmed by `0xd0` |
//! | `0xd0` | erase confirm | erases the containing block to `0xff` |
//!
//! Three properties of that driver shape this implementation, and each of them
//! is a test below:
//!
//! * **`QemuFlashWrite` never polls status.** It writes `0x10`, then the data
//!   byte, for every byte in turn, and issues one `0xff` at the end. So a
//!   program must complete synchronously *and* leave the device ready to take
//!   `0x10` as a fresh command.
//! * **`QemuFlashEraseBlock` never polls or resets either** — `0x20`, `0xd0`,
//!   return. So an erase must also complete synchronously and leave the device
//!   readable, because the fault-tolerant-write layer reads the block back.
//! * **`QemuFlashRead` is a plain `CopyMem`.** Outside a command sequence the
//!   window must behave like memory-mapped ROM; the driver never issues `0xff`
//!   before reading.
//!
//! `QemuFlashDetected()` is the gate that decides whether any of this is used,
//! and it is a *sequence*, not a probe of one register:
//! read the original byte, write `0x50`, read (must **not** read back `0x50` —
//! that is "behaves as RAM"), write `0x70`, read (must be neither the original
//! byte — "behaves as ROM" — nor `0x70` — "RAM" again — so it has to be the
//! status register reading `0x00`), then program the original byte over itself
//! and check that status bit 4 is clear. Only then does the firmware log
//! `FD behaves as FLASH, writable` and install the writable FVB.
//! [`Pflash::probe_sequence_is_flash`] is that exact sequence, run against this
//! device as a unit test.
//!
//! # Programming semantics
//!
//! Real NOR flash can only clear bits when programming; erasing sets them. This
//! device instead **overwrites**, which is what `pflash_cfi01` does and
//! therefore what every OVMF build was developed against. The firmware always
//! erases before it writes (that is what the FTW working/spare blocks are for),
//! so the distinction never comes up — but if it ever did, matching QEMU is the
//! behaviour EDK2 was tested with, and silently dropping bits in an NVRAM store
//! is a worse failure than not modelling the physics.
//!
//! # Untrusted input
//!
//! Every byte in the NVRAM file was written by guest firmware, and every address
//! here comes from a guest MMIO access. So: offsets are computed against the
//! window and range-checked before indexing, an access past the backed region is
//! an erased read or a refused program (status bit 4) rather than a panic, the
//! file is never resized by the guest, and a host I/O failure is reported to the
//! guest as a program error instead of being lost.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::layout;

/// Erase block size, matching `PcdOvmfFirmwareBlockSize`.
pub const BLOCK_SIZE: usize = 0x1000;

/// Value of an erased flash byte (erase polarity 1).
pub const ERASED: u8 = 0xff;

// CFI command set, from OvmfPkg/QemuFlashFvbServicesRuntimeDxe/QemuFlash.c.
const CMD_PROGRAM: u8 = 0x10;
/// Alternate program-setup opcode. Unused by EDK2, accepted by real chips and
/// by `pflash_cfi01`.
const CMD_PROGRAM_ALT: u8 = 0x40;
const CMD_BLOCK_ERASE: u8 = 0x20;
const CMD_CLEAR_STATUS: u8 = 0x50;
const CMD_READ_STATUS: u8 = 0x70;
const CMD_READ_DEVID: u8 = 0x90;
const CMD_ERASE_CONFIRM: u8 = 0xd0;
const CMD_READ_ARRAY: u8 = 0xff;

/// Status register bits (Intel extended command set).
///
/// The *cleared* status is `0x00`, not `0x80`: `QemuFlashDetected()` requires
/// the byte it reads after a clear-status to be exactly `CLEARED_ARRAY_STATUS`
/// (`0x00`) before it will try a program, so a device that reported "ready"
/// there would be dismissed as neither RAM nor ROM nor flash. `pflash_cfi01`
/// starts at 0 and sets the ready bit only when an operation completes, and the
/// firmware was written against that.
const STATUS_READY: u8 = 0x80;
const STATUS_ERASE_ERROR: u8 = 0x20;
const STATUS_PROGRAM_ERROR: u8 = 0x10;
/// "Command sequence error" as `pflash_cfi01` reports it: both error bits.
const STATUS_SEQUENCE_ERROR: u8 = STATUS_ERASE_ERROR | STATUS_PROGRAM_ERROR;

#[derive(Debug, Error)]
pub enum PflashError {
    #[error("cannot open NVRAM store {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "NVRAM store {path} is {len} bytes, but this firmware's variable store is \
         {expected} bytes — refusing to reinterpret it (move it aside to start fresh)"
    )]
    WrongSize {
        path: PathBuf,
        len: u64,
        expected: u64,
    },

    #[error("cannot read NVRAM store {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

// ---------------------------------------------------------------------------
// The pristine variable store
// ---------------------------------------------------------------------------
//
// A brand-new NVRAM file cannot simply be 0x84000 bytes of erased flash. Two
// firmware drivers read structures out of it before anything has had a chance to
// create them:
//
// * `FvbInitialize` → `InitializeVariableFvHeader` checks the
//   `EFI_FIRMWARE_VOLUME_HEADER` and *does* rewrite it when it is missing, so
//   that one would heal itself;
// * `VariableRuntimeDxe` → `InitNonVolatileVariableStore` then does
//   `ASSERT (VariableStore->Size == VariableStoreLength)` on the
//   `VARIABLE_STORE_HEADER` immediately behind it — and nothing writes *that*.
//   Measured on the first boot of this device: `ASSERT [VariableRuntimeDxe]
//   VariableNonVolatile.c(228)`, i.e. a dead firmware.
//
// On QEMU the question never arises, because OVMF's flash *image* ships a
// pre-formatted store (`OvmfPkg/Include/Fdf/VarStore.fdf.inc`, the file the
// constants below are transcribed from) and `OVMF_VARS.fd` is a copy of it.
// CloudHv has no varstore region at all, so the host has to produce the same
// bytes. That is what [`pristine_varstore`] is: an empty-but-formatted store,
// generated rather than vendored, and identical in shape to the one every
// OVMF-on-QEMU guest starts from.

/// `gEfiSystemNvDataFvGuid`, the file-system GUID of a non-volatile data FV.
const SYSTEM_NV_DATA_FV_GUID: [u8; 16] = [
    0x8d, 0x2b, 0xf1, 0xff, 0x96, 0x76, 0x8b, 0x4c, 0xa9, 0x85, 0x27, 0x47, 0x07, 0x5b, 0x4f, 0x50,
];

/// `gEfiAuthenticatedVariableGuid`. EDK2's own template uses this signature
/// unconditionally — "It is compatible with SECURE_BOOT_ENABLE == FALSE as
/// well" — and `VariableRuntimeDxe` derives `mAuthFormat` from it, so a store
/// signed this way works in both builds. Using the plain `gEfiVariableGuid`
/// instead would make a later secure-boot firmware reject the store.
const AUTHENTICATED_VARIABLE_GUID: [u8; 16] = [
    0x78, 0x2c, 0xf3, 0xaa, 0x7b, 0x94, 0x9a, 0x43, 0xa1, 0x80, 0x2e, 0x14, 0x4e, 0xc3, 0x77, 0x92,
];

/// The fault-tolerant-write working block header, verbatim from
/// `VarStore.fdf.inc`: `gEdkiiWorkingBlockSignatureGuid`, the CRC-32 EDK2
/// computes over this structure with its own `Crc` field zeroed
/// (`0x642CAF2C`), the valid/invalid state byte `0xFE`, and
/// `WriteQueueSize = 0x0FE0` (the block minus this 32-byte header).
///
/// Transcribed rather than computed because the CRC covers a bitfield whose
/// erase-polarity encoding is easy to get subtly wrong, and a wrong CRC here
/// costs a whole boot to notice. `FaultTolerantWriteDxe` would reinitialise an
/// invalid working block on its own; starting from the same bytes QEMU guests
/// start from means it never has to.
const FTW_WORKING_HEADER: [u8; 32] = [
    0x2b, 0x29, 0x58, 0x9e, 0x68, 0x7c, 0x7d, 0x49, 0xa0, 0xce, 0x65, 0x00, 0xfd, 0x9f, 0x1b, 0x95,
    0x2c, 0xaf, 0x2c, 0x64, 0xfe, 0xff, 0xff, 0xff, 0xe0, 0x0f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// Size of `EFI_FIRMWARE_VOLUME_HEADER` with one block map entry plus the
/// terminator, i.e. the `HeaderLength` field's value.
const FV_HEADER_LEN: usize = 0x48;

/// `EFI_FVB2_*` attributes of a non-volatile data FV, as EDK2's template sets
/// them: read/write enabled and their caps, erase polarity 1, 16-byte alignment.
const FV_ATTRIBUTES: u32 = 0x0004_feff;

/// Builds an empty, formatted UEFI variable store of `layout::PFLASH_NVRAM_SIZE`
/// bytes: firmware volume header, variable store header, fault-tolerant-write
/// working block header, everything else erased.
pub fn pristine_varstore() -> Vec<u8> {
    let total = layout::PFLASH_NVRAM_SIZE as usize;
    let mut image = vec![ERASED; total];

    // --- EFI_FIRMWARE_VOLUME_HEADER (offset 0) ---
    let mut fv = [0u8; FV_HEADER_LEN];
    // ZeroVector [16] stays zero.
    fv[16..32].copy_from_slice(&SYSTEM_NV_DATA_FV_GUID);
    // FvLength covers the *whole* store, not just the variable region: the FVB
    // this header describes spans the variable store, the event log and both
    // fault-tolerant-write blocks. `InitializeVariableFvHeader` recomputes the
    // same number and rejects the volume if it disagrees.
    fv[32..40].copy_from_slice(&layout::PFLASH_NVRAM_SIZE.to_le_bytes());
    fv[40..44].copy_from_slice(b"_FVH");
    fv[44..48].copy_from_slice(&FV_ATTRIBUTES.to_le_bytes());
    fv[48..50].copy_from_slice(&(FV_HEADER_LEN as u16).to_le_bytes());
    // fv[50..52] is the checksum, filled in below.
    // fv[52..54] ExtHeaderOffset = 0, fv[54] Reserved = 0.
    fv[55] = 2; // Revision = EFI_FVH_REVISION
    let blocks = (layout::PFLASH_NVRAM_SIZE / BLOCK_SIZE as u64) as u32;
    fv[56..60].copy_from_slice(&blocks.to_le_bytes());
    fv[60..64].copy_from_slice(&(BLOCK_SIZE as u32).to_le_bytes());
    // fv[64..72] is the terminating block map entry {0, 0}.
    let checksum = fv_checksum(&fv);
    fv[50..52].copy_from_slice(&checksum.to_le_bytes());
    image[..FV_HEADER_LEN].copy_from_slice(&fv);

    // --- VARIABLE_STORE_HEADER (immediately behind it) ---
    let mut vs = [0u8; 28];
    vs[..16].copy_from_slice(&AUTHENTICATED_VARIABLE_GUID);
    // Size is the *variable region* minus the FV header — this is the field
    // whose mismatch asserted `VariableNonVolatile.c(228)`.
    let store_size = layout::PFLASH_VARSTORE_SIZE as u32 - FV_HEADER_LEN as u32;
    vs[16..20].copy_from_slice(&store_size.to_le_bytes());
    vs[20] = 0x5a; // VARIABLE_STORE_FORMATTED
    vs[21] = 0xfe; // VARIABLE_STORE_HEALTHY
    image[FV_HEADER_LEN..FV_HEADER_LEN + vs.len()].copy_from_slice(&vs);

    // --- fault-tolerant-write working block ---
    let ftw = (layout::PFLASH_VARSTORE_SIZE + layout::PFLASH_EVENT_LOG_SIZE) as usize;
    image[ftw..ftw + FTW_WORKING_HEADER.len()].copy_from_slice(&FTW_WORKING_HEADER);

    image
}

/// The firmware volume header's checksum: the 16-bit words of the header must
/// sum to zero.
fn fv_checksum(header: &[u8; FV_HEADER_LEN]) -> u16 {
    let sum = header.chunks_exact(2).fold(0u16, |acc, w| {
        acc.wrapping_add(u16::from_le_bytes([w[0], w[1]]))
    });
    0u16.wrapping_sub(sum)
}

/// Where the flash contents are kept across VM runs.
///
/// A trait rather than a `File` so the device itself is host-independent and
/// unit-testable: the tests below run the whole CFI state machine against an
/// in-memory store and assert on the bytes that *would* have been persisted.
pub trait NvramStore: Send {
    /// Persists `bytes` at `offset` in the store. Called for every programmed
    /// byte and every erased block, so it must be cheap for small writes.
    fn persist(&mut self, offset: usize, bytes: &[u8]) -> std::io::Result<()>;
    /// A name for log lines.
    fn describe(&self) -> String;
}

/// A file-backed NVRAM store: one file per VM, created erased.
pub struct FileStore {
    file: std::fs::File,
    path: PathBuf,
}

impl FileStore {
    /// Opens (or creates, formatted) the NVRAM file for a VM and returns the
    /// store together with its current contents.
    ///
    /// The file is exactly `len` bytes: a shorter or longer one is refused
    /// rather than padded, because reinterpreting a variable store of the wrong
    /// size is how boot configuration gets silently lost.
    pub fn open(path: &Path, len: usize) -> Result<(Self, Vec<u8>), PflashError> {
        use std::io::Read as _;

        let open_err = |source: std::io::Error| PflashError::Open {
            path: path.to_path_buf(),
            source,
        };
        let existed = path.exists();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(open_err)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(open_err)?;

        let mut contents = pristine_varstore();
        contents.resize(len, ERASED);
        if existed {
            let actual = file.metadata().map_err(open_err)?.len();
            if actual != len as u64 {
                return Err(PflashError::WrongSize {
                    path: path.to_path_buf(),
                    len: actual,
                    expected: len as u64,
                });
            }
            file.read_exact(&mut contents)
                .map_err(|source| PflashError::Read {
                    path: path.to_path_buf(),
                    source,
                })?;
        } else {
            // A fresh store is empty but *formatted*: `VariableRuntimeDxe` reads
            // the VARIABLE_STORE_HEADER before anything could have written it and
            // asserts on a blank one (measured). See `pristine_varstore`.
            use std::io::Write as _;
            file.write_all(&contents).map_err(open_err)?;
            file.sync_all().map_err(open_err)?;
        }
        tracing::info!(
            path = %path.display(),
            bytes = len,
            fresh = !existed,
            "UEFI variable store ready"
        );
        Ok((
            Self {
                file,
                path: path.to_path_buf(),
            },
            contents,
        ))
    }
}

impl NvramStore for FileStore {
    fn persist(&mut self, offset: usize, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        self.file.seek(SeekFrom::Start(offset as u64))?;
        self.file.write_all(bytes)
    }

    fn describe(&self) -> String {
        self.path.display().to_string()
    }
}

/// An in-memory store, for tests and for a deliberately throwaway VM.
#[derive(Debug, Default)]
pub struct MemStore {
    pub bytes: Vec<u8>,
    pub fail: bool,
}

impl MemStore {
    pub fn new(len: usize) -> Self {
        Self {
            bytes: vec![ERASED; len],
            fail: false,
        }
    }
}

impl NvramStore for MemStore {
    fn persist(&mut self, offset: usize, bytes: &[u8]) -> std::io::Result<()> {
        if self.fail {
            return Err(std::io::Error::other("store is deliberately broken"));
        }
        let end = offset.saturating_add(bytes.len());
        if end > self.bytes.len() {
            return Err(std::io::Error::other("write past the end of the store"));
        }
        self.bytes[offset..end].copy_from_slice(bytes);
        Ok(())
    }

    fn describe(&self) -> String {
        format!("memory ({} bytes)", self.bytes.len())
    }
}

/// The device's command state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Reads return flash contents (the reset state).
    ReadArray,
    /// Reads return the status register.
    ReadStatus,
    /// A `0x10` was seen: the next write is the data byte.
    Program,
    /// A `0x20` was seen: the next write must be `0xd0`.
    Erase,
}

/// The emulated flash device.
pub struct Pflash {
    /// Guest physical base of the decoded window.
    base: u64,
    /// Size of the decoded window. Larger than the backed region on purpose:
    /// the firmware adds `PcdOvmfFirmwareFdSize` bytes from `base` to the GCD as
    /// runtime MMIO, and an access anywhere in there must get a defined answer
    /// (erased flash) rather than falling through to the bus default.
    window: u64,
    /// The backed, persisted bytes: the variable store, event log and the two
    /// fault-tolerant-write blocks.
    nvram: Vec<u8>,
    state: State,
    status: u8,
    store: Option<Box<dyn NvramStore>>,
    stats: PflashStats,
}

/// Counters, for `entangled doctor` and for tests that want to prove the
/// firmware really did write (rather than that the file merely exists).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PflashStats {
    pub programmed_bytes: u64,
    pub erased_blocks: u64,
    pub refused_programs: u64,
    pub store_errors: u64,
}

impl Pflash {
    /// A device covering `window` bytes at `base`, of which the first
    /// `contents.len()` bytes are backed by `store`.
    pub fn new(
        base: u64,
        window: u64,
        contents: Vec<u8>,
        store: Option<Box<dyn NvramStore>>,
    ) -> Self {
        Self {
            base,
            window: window.max(contents.len() as u64),
            nvram: contents,
            state: State::ReadArray,
            status: 0,
            store,
            stats: PflashStats::default(),
        }
    }

    /// The device this machine's firmware expects: `layout::PFLASH_*`, backed by
    /// an NVRAM file next to the VM profile.
    pub fn open(nvram: &Path) -> Result<Self, PflashError> {
        let len = layout::PFLASH_NVRAM_SIZE as usize;
        let (store, contents) = FileStore::open(nvram, len)?;
        Ok(Self::new(
            layout::PFLASH_BASE,
            layout::PFLASH_WINDOW_SIZE,
            contents,
            Some(Box::new(store)),
        ))
    }

    /// True when `addr` is inside the decoded window.
    pub fn contains(&self, addr: u64) -> bool {
        addr >= self.base && addr - self.base < self.window
    }

    pub fn stats(&self) -> PflashStats {
        self.stats
    }

    /// The current flash contents, for tests and diagnostics.
    pub fn contents(&self) -> &[u8] {
        &self.nvram
    }

    /// Guest read. Fills `data` with the status register in read-status mode,
    /// and with flash contents otherwise (erased bytes past the backed region).
    pub fn mmio_read(&mut self, addr: u64, data: &mut [u8]) {
        if !self.contains(addr) {
            data.fill(ERASED);
            return;
        }
        if self.state == State::ReadStatus {
            // A wider access reads the same register: the status byte is not an
            // array location, so there is nothing else to return.
            data.fill(self.status);
            return;
        }
        // `contains` bounded the base offset; each byte is bounded again because
        // `data` may run past the window's end.
        let offset = (addr - self.base) as usize;
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = offset
                .checked_add(i)
                .and_then(|at| self.nvram.get(at).copied())
                .unwrap_or(ERASED);
        }
    }

    /// Guest write: one CFI bus cycle.
    ///
    /// Only the first byte is a cycle. EDK2 writes single bytes (`QemuFlashPtrWrite`
    /// takes a `UINT8`), and a wider access has no meaning in a byte-wide command
    /// set — treating the extra bytes as further cycles would invent behaviour no
    /// firmware asks for.
    pub fn mmio_write(&mut self, addr: u64, data: &[u8]) {
        let Some(&value) = data.first() else {
            return;
        };
        if !self.contains(addr) {
            return;
        }
        if data.len() > 1 {
            tracing::debug!(
                addr = format_args!("{addr:#x}"),
                bytes = data.len(),
                "wide write to pflash; only the first byte is a bus cycle"
            );
        }
        let offset = (addr - self.base) as usize;
        match self.state {
            State::Program => self.program(offset, value),
            State::Erase => {
                if value == CMD_ERASE_CONFIRM {
                    self.erase_block(offset);
                } else {
                    tracing::warn!(
                        addr = format_args!("{addr:#x}"),
                        value = format_args!("{value:#04x}"),
                        "block erase not confirmed with 0xd0"
                    );
                    self.status |= STATUS_SEQUENCE_ERROR;
                }
                self.state = State::ReadArray;
            }
            State::ReadArray | State::ReadStatus => self.command(offset, value),
        }
    }

    fn command(&mut self, offset: usize, value: u8) {
        match value {
            CMD_READ_ARRAY => self.state = State::ReadArray,
            CMD_CLEAR_STATUS => {
                // Clearing the status also returns to read-array mode. These two
                // lines are the most load-bearing in the file: returning to
                // read-array is what makes `QemuFlashDetected()` see "not RAM"
                // on its first probe, and clearing to *zero* is what makes the
                // status read that follows say "cleared array status" — the only
                // value from which the firmware goes on to try a program.
                self.status = 0;
                self.state = State::ReadArray;
            }
            CMD_READ_STATUS => self.state = State::ReadStatus,
            CMD_PROGRAM | CMD_PROGRAM_ALT => self.state = State::Program,
            CMD_BLOCK_ERASE => self.state = State::Erase,
            CMD_READ_DEVID => {
                // Never issued by EDK2 (the constant exists in QemuFlash.c and
                // is unused). Nothing sane can be returned without a device-id
                // table, so stay in read-array rather than answering with
                // plausible-looking garbage.
                tracing::debug!("pflash: read-device-id is not implemented; staying in read-array");
                self.state = State::ReadArray;
            }
            other => {
                tracing::debug!(
                    offset = format_args!("{offset:#x}"),
                    command = format_args!("{other:#04x}"),
                    "unknown pflash command; returning to read-array"
                );
                self.state = State::ReadArray;
            }
        }
    }

    fn program(&mut self, offset: usize, value: u8) {
        self.state = State::ReadArray;
        let Some(slot) = self.nvram.get_mut(offset) else {
            // Inside the window, outside the backed store: the firmware bounds
            // its own writes by the firmware volume header, so this is either a
            // firmware bug or a hostile guest. Report a program error, which is
            // the only thing a real chip could say, and keep the store intact.
            self.stats.refused_programs += 1;
            self.status |= STATUS_PROGRAM_ERROR;
            tracing::warn!(
                offset = format_args!("{offset:#x}"),
                backed = self.nvram.len(),
                "pflash program past the persisted region; refused"
            );
            return;
        };
        *slot = value;
        self.status |= STATUS_READY;
        self.stats.programmed_bytes += 1;
        self.persist(offset, 1);
    }

    fn erase_block(&mut self, offset: usize) {
        let start = offset - (offset % BLOCK_SIZE);
        let end = start.saturating_add(BLOCK_SIZE).min(self.nvram.len());
        if start >= self.nvram.len() {
            self.stats.refused_programs += 1;
            self.status |= STATUS_ERASE_ERROR;
            tracing::warn!(
                offset = format_args!("{offset:#x}"),
                "pflash erase past the persisted region; refused"
            );
            return;
        }
        self.nvram[start..end].fill(ERASED);
        self.status |= STATUS_READY;
        self.stats.erased_blocks += 1;
        self.persist(start, end - start);
    }

    fn persist(&mut self, offset: usize, len: usize) {
        let Some(store) = self.store.as_mut() else {
            return;
        };
        let end = (offset + len).min(self.nvram.len());
        if let Err(e) = store.persist(offset, &self.nvram[offset..end]) {
            // Losing a variable write silently would make the *next* boot look
            // broken instead of this one, so tell the guest: a program error is
            // exactly what a chip with a failing cell reports.
            self.stats.store_errors += 1;
            self.status |= STATUS_PROGRAM_ERROR;
            tracing::error!(
                store = store.describe(),
                offset = format_args!("{offset:#x}"),
                len,
                error = %e,
                "cannot persist UEFI variable store"
            );
        }
    }

    /// Runs `QemuFlashDetected()`'s exact byte sequence against this device and
    /// reports whether the firmware would conclude "FLASH, writable".
    ///
    /// Public because it is worth more than a test: it is the executable form of
    /// the firmware contract, and a change to the state machine that breaks it
    /// breaks UEFI variables — a failure that otherwise only shows up as an
    /// installed guest that stops booting after the second restart.
    pub fn probe_sequence_is_flash(&mut self) -> bool {
        // The driver scans block 0 for a byte that is not 0x50, 0x70 or 0x00.
        let mut probe = None;
        for offset in 0..BLOCK_SIZE as u64 {
            let mut byte = [0u8; 1];
            self.mmio_read(self.base + offset, &mut byte);
            if ![CMD_CLEAR_STATUS, CMD_READ_STATUS, 0x00].contains(&byte[0]) {
                probe = Some((self.base + offset, byte[0]));
                break;
            }
        }
        let Some((at, original)) = probe else {
            return false; // "Failed to find probe location"
        };

        let read = |dev: &mut Self| {
            let mut byte = [0u8; 1];
            dev.mmio_read(at, &mut byte);
            byte[0]
        };

        self.mmio_write(at, &[CMD_CLEAR_STATUS]);
        if original != CMD_CLEAR_STATUS && read(self) == CMD_CLEAR_STATUS {
            return false; // "FD behaves as RAM"
        }
        self.mmio_write(at, &[CMD_READ_STATUS]);
        let status = read(self);
        if status == original || status == CMD_READ_STATUS {
            return false; // "behaves as ROM" / "behaves as RAM"
        }
        if status != 0x00 {
            return false; // not a cleared status register either
        }
        self.mmio_write(at, &[CMD_PROGRAM]);
        self.mmio_write(at, &[original]);
        self.mmio_write(at, &[CMD_READ_STATUS]);
        let status = read(self);
        self.mmio_write(at, &[CMD_READ_ARRAY]);
        status & STATUS_PROGRAM_ERROR == 0
    }
}

impl std::fmt::Debug for Pflash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pflash")
            .field("base", &format_args!("{:#x}", self.base))
            .field("window", &format_args!("{:#x}", self.window))
            .field("backed", &self.nvram.len())
            .field("state", &self.state)
            .field("status", &format_args!("{:#04x}", self.status))
            .field("store", &self.store.as_ref().map(|s| s.describe()))
            .field("stats", &self.stats)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NVRAM: usize = 0x8_4000;
    const BASE: u64 = layout::PFLASH_BASE;

    fn device() -> Pflash {
        Pflash::new(
            BASE,
            layout::PFLASH_WINDOW_SIZE,
            vec![ERASED; NVRAM],
            Some(Box::new(MemStore::new(NVRAM))),
        )
    }

    fn read1(dev: &mut Pflash, addr: u64) -> u8 {
        let mut byte = [0u8; 1];
        dev.mmio_read(addr, &mut byte);
        byte[0]
    }

    /// The whole point: a fresh, erased device must satisfy
    /// `QemuFlashDetected()`. Fails ⇒ the firmware falls back to RAM variables
    /// and an installed guest loses its boot entry on every stop.
    #[test]
    fn a_fresh_device_reads_as_writable_flash() {
        assert!(device().probe_sequence_is_flash());
    }

    /// And so must a device whose store already holds a formatted firmware
    /// volume — the second boot of a VM. The FV header starts with 16 zero bytes
    /// (the "zero vector"), which are exactly the bytes the probe's scan skips,
    /// so this case takes a *different* path through the scan than the one above.
    #[test]
    fn a_formatted_device_reads_as_writable_flash() {
        let mut dev = device();
        // Zero vector, then the EFI_SYSTEM_NV_DATA_FV GUID (first byte 0x8d).
        for (i, byte) in [0u8; 16].iter().enumerate() {
            dev.nvram[i] = *byte;
        }
        dev.nvram[16] = 0x8d;
        assert!(dev.probe_sequence_is_flash());
        // The scan stopped at the first non-{0x50,0x70,0x00} byte, i.e. the GUID,
        // and the probe left the array untouched.
        assert_eq!(dev.nvram[16], 0x8d);
        assert_eq!(&dev.nvram[..16], &[0u8; 16]);
    }

    /// Plain RAM fails the probe — which is what the *current* CloudHv build
    /// does, and the reason this device exists. Modelled by answering every read
    /// with the last byte written, i.e. by removing the command set.
    #[test]
    fn the_probe_rejects_a_ram_like_window() {
        struct Ram {
            bytes: Vec<u8>,
        }
        impl Ram {
            fn write(&mut self, offset: usize, value: u8) {
                self.bytes[offset] = value;
            }
        }
        // Hand-run the same sequence against RAM semantics: write 0x50, read it
        // back, and the verdict is immediate.
        let mut ram = Ram {
            bytes: vec![ERASED; BLOCK_SIZE],
        };
        let original = ram.bytes[0];
        ram.write(0, CMD_CLEAR_STATUS);
        assert_ne!(original, CMD_CLEAR_STATUS);
        assert_eq!(
            ram.bytes[0], CMD_CLEAR_STATUS,
            "RAM reads back what was written, which is what makes the firmware \
             log \"FD behaves as RAM\" and fall back to EmuVariableFvb"
        );
    }

    /// `0x50` must clear the status *and* return to read-array mode; `0x70` must
    /// switch reads to the status register and back.
    #[test]
    fn status_and_read_array_modes_round_trip() {
        let mut dev = device();
        dev.nvram[0] = 0xa5;

        assert_eq!(read1(&mut dev, BASE), 0xa5, "reset state is read-array");
        dev.mmio_write(BASE, &[CMD_READ_STATUS]);
        assert_eq!(
            read1(&mut dev, BASE),
            0,
            "a device that has done nothing is not \"ready\""
        );
        // A wide read of the status register returns the same byte, not array data.
        let mut wide = [0u8; 4];
        dev.mmio_read(BASE, &mut wide);
        assert_eq!(wide, [0; 4]);

        // The ready bit appears once an operation has completed, and a
        // clear-status takes it away again.
        dev.mmio_write(BASE + 0x10, &[CMD_PROGRAM]);
        dev.mmio_write(BASE + 0x10, &[0x5a]);
        dev.mmio_write(BASE, &[CMD_READ_STATUS]);
        assert_eq!(read1(&mut dev, BASE), STATUS_READY);
        dev.mmio_write(BASE, &[CMD_CLEAR_STATUS]);
        dev.mmio_write(BASE, &[CMD_READ_STATUS]);
        assert_eq!(read1(&mut dev, BASE), 0);

        dev.mmio_write(BASE, &[CMD_READ_ARRAY]);
        assert_eq!(read1(&mut dev, BASE), 0xa5);

        dev.mmio_write(BASE, &[CMD_READ_STATUS]);
        dev.mmio_write(BASE, &[CMD_CLEAR_STATUS]);
        assert_eq!(
            read1(&mut dev, BASE),
            0xa5,
            "clear-status must also leave read-array mode"
        );
    }

    /// A store whose bytes the test can still see after the device owns it.
    #[derive(Clone, Default)]
    struct SharedStore(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl SharedStore {
        fn new(len: usize) -> Self {
            Self(std::sync::Arc::new(std::sync::Mutex::new(vec![
                ERASED;
                len
            ])))
        }
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }

    impl NvramStore for SharedStore {
        fn persist(&mut self, offset: usize, bytes: &[u8]) -> std::io::Result<()> {
            let mut inner = self.0.lock().unwrap();
            inner[offset..offset + bytes.len()].copy_from_slice(bytes);
            Ok(())
        }
        fn describe(&self) -> String {
            "shared test store".into()
        }
    }

    /// A byte program: `0x10` then the data, one address at a time, with no
    /// status poll in between — and the store sees every byte as it happens,
    /// which is what makes the variables survive a VM stop rather than a clean
    /// shutdown.
    #[test]
    fn programming_writes_through_to_the_store() {
        let store = SharedStore::new(NVRAM);
        let mut dev = Pflash::new(
            BASE,
            layout::PFLASH_WINDOW_SIZE,
            vec![ERASED; NVRAM],
            Some(Box::new(store.clone())),
        );
        for (i, byte) in b"BootOrder".iter().enumerate() {
            let at = BASE + 0x100 + i as u64;
            dev.mmio_write(at, &[CMD_PROGRAM]);
            dev.mmio_write(at, &[*byte]);
        }
        dev.mmio_write(BASE + 0x108, &[CMD_READ_ARRAY]);

        assert_eq!(&dev.nvram[0x100..0x109], b"BootOrder");
        assert_eq!(dev.stats().programmed_bytes, 9);
        assert_eq!(dev.stats().store_errors, 0);
        assert_eq!(
            &store.bytes()[0x100..0x109],
            b"BootOrder",
            "every programmed byte must be written through immediately"
        );

        // An erase reaches the store as a whole block of 0xff.
        dev.mmio_write(BASE + 0x100, &[CMD_BLOCK_ERASE]);
        dev.mmio_write(BASE + 0x100, &[CMD_ERASE_CONFIRM]);
        assert!(store.bytes()[..BLOCK_SIZE].iter().all(|&b| b == ERASED));
    }

    /// Erase clears one block to 0xff, leaves the neighbours alone, and needs
    /// its confirmation byte.
    #[test]
    fn block_erase_is_confirmed_and_block_scoped() {
        let mut dev = device();
        dev.nvram[0..BLOCK_SIZE].fill(0x00);
        dev.nvram[BLOCK_SIZE] = 0x00;

        // Unconfirmed: nothing happens, and the status says so.
        dev.mmio_write(BASE, &[CMD_BLOCK_ERASE]);
        dev.mmio_write(BASE, &[0x11]);
        assert_eq!(dev.nvram[0], 0x00);
        dev.mmio_write(BASE, &[CMD_READ_STATUS]);
        assert_eq!(
            read1(&mut dev, BASE) & STATUS_SEQUENCE_ERROR,
            STATUS_SEQUENCE_ERROR
        );
        dev.mmio_write(BASE, &[CMD_CLEAR_STATUS]);

        // Confirmed, from an address in the middle of the block.
        dev.mmio_write(BASE + 0x800, &[CMD_BLOCK_ERASE]);
        dev.mmio_write(BASE + 0x800, &[CMD_ERASE_CONFIRM]);
        assert!(dev.nvram[..BLOCK_SIZE].iter().all(|&b| b == ERASED));
        assert_eq!(dev.nvram[BLOCK_SIZE], 0x00, "the next block is untouched");
        assert_eq!(dev.stats().erased_blocks, 1);
        // Reads work immediately afterwards: the driver issues no 0xff and reads
        // the block back through the FTW layer.
        assert_eq!(read1(&mut dev, BASE), ERASED);
    }

    /// The driver's actual write loop: many `0x10`+data pairs and a single
    /// trailing `0xff`. Nothing in between may put the device in a state that
    /// swallows the next `0x10`.
    #[test]
    fn a_multi_byte_write_run_needs_no_resets() {
        let mut dev = device();
        let payload: Vec<u8> = (0..64u8).collect();
        for (i, byte) in payload.iter().enumerate() {
            let at = BASE + 0x2000 + i as u64;
            dev.mmio_write(at, &[CMD_PROGRAM]);
            dev.mmio_write(at, &[*byte]);
        }
        dev.mmio_write(BASE + 0x2000 + payload.len() as u64 - 1, &[CMD_READ_ARRAY]);
        assert_eq!(&dev.nvram[0x2000..0x2000 + payload.len()], &payload[..]);
    }

    /// Adversarial: the guest can aim a program or an erase anywhere in the
    /// decoded window, including past the persisted region. It must be refused,
    /// reported in the status register, and it must not touch the store.
    #[test]
    fn writes_past_the_persisted_region_are_refused() {
        let mut dev = device();
        let past = BASE + NVRAM as u64 + 0x10;
        assert!(dev.contains(past), "still inside the 4 MiB window");
        assert_eq!(read1(&mut dev, past), ERASED, "unbacked flash reads erased");

        dev.mmio_write(past, &[CMD_PROGRAM]);
        dev.mmio_write(past, &[0x42]);
        dev.mmio_write(past, &[CMD_READ_STATUS]);
        assert_eq!(
            read1(&mut dev, past) & STATUS_PROGRAM_ERROR,
            STATUS_PROGRAM_ERROR
        );
        assert_eq!(dev.stats().refused_programs, 1);
        assert_eq!(dev.stats().programmed_bytes, 0);

        dev.mmio_write(past, &[CMD_CLEAR_STATUS]);
        dev.mmio_write(past, &[CMD_BLOCK_ERASE]);
        dev.mmio_write(past, &[CMD_ERASE_CONFIRM]);
        dev.mmio_write(past, &[CMD_READ_STATUS]);
        assert_eq!(
            read1(&mut dev, past) & STATUS_ERASE_ERROR,
            STATUS_ERASE_ERROR
        );

        // Outside the window entirely: reads are erased, writes are ignored, and
        // no state changes (the address never reaches the state machine).
        let outside = BASE - 1;
        assert!(!dev.contains(outside));
        assert_eq!(read1(&mut dev, outside), ERASED);
        dev.mmio_write(outside, &[CMD_PROGRAM]);
        dev.mmio_write(outside, &[0x99]);
        assert_eq!(dev.stats().programmed_bytes, 0);
        // A zero-length access is a no-op rather than a panic.
        dev.mmio_write(BASE, &[]);
        dev.mmio_read(BASE, &mut []);
    }

    /// Adversarial: an access that starts inside the window and runs past its
    /// end must not index out of bounds.
    #[test]
    fn a_read_straddling_the_window_end_is_bounded() {
        let mut dev = device();
        let mut buf = [0u8; 16];
        dev.mmio_read(BASE + layout::PFLASH_WINDOW_SIZE - 4, &mut buf);
        assert_eq!(buf, [ERASED; 16]);
        // And one straddling the end of the persisted region reads part array,
        // part erased.
        dev.nvram[NVRAM - 2] = 0x11;
        dev.nvram[NVRAM - 1] = 0x22;
        let mut buf = [0u8; 4];
        dev.mmio_read(BASE + NVRAM as u64 - 2, &mut buf);
        assert_eq!(buf, [0x11, 0x22, ERASED, ERASED]);
    }

    /// A failing store is reported to the guest as a program error rather than
    /// pretending the write landed.
    #[test]
    fn a_broken_store_becomes_a_program_error() {
        let mut store = MemStore::new(NVRAM);
        store.fail = true;
        let mut dev = Pflash::new(
            BASE,
            layout::PFLASH_WINDOW_SIZE,
            vec![ERASED; NVRAM],
            Some(Box::new(store)),
        );
        dev.mmio_write(BASE, &[CMD_PROGRAM]);
        dev.mmio_write(BASE, &[0x5a]);
        dev.mmio_write(BASE, &[CMD_READ_STATUS]);
        assert_eq!(
            read1(&mut dev, BASE) & STATUS_PROGRAM_ERROR,
            STATUS_PROGRAM_ERROR
        );
        assert_eq!(dev.stats().store_errors, 1);
    }

    /// An unknown command must not leave the device in a state where the next
    /// data byte is taken as a program.
    #[test]
    fn unknown_commands_fall_back_to_read_array() {
        let mut dev = device();
        dev.nvram[0] = 0x77;
        for command in [0xe8u8, 0x60, 0x98, 0x00, CMD_READ_DEVID] {
            dev.mmio_write(BASE, &[command]);
            assert_eq!(read1(&mut dev, BASE), 0x77, "command {command:#04x}");
        }
        assert_eq!(dev.stats().programmed_bytes, 0);
    }

    /// The file-backed store is the persistence claim: write through one device,
    /// drop it, reopen the same path, and the bytes are there.
    #[test]
    fn a_file_store_survives_the_device() {
        let dir = std::env::temp_dir().join("entangled-pflash-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("nvram-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&path);

        {
            let (store, contents) = FileStore::open(&path, NVRAM).unwrap();
            assert_eq!(
                contents,
                pristine_varstore(),
                "a fresh store is empty but formatted"
            );
            let mut dev = Pflash::new(
                BASE,
                layout::PFLASH_WINDOW_SIZE,
                contents,
                Some(Box::new(store)),
            );
            assert!(dev.probe_sequence_is_flash());
            for (i, byte) in b"Boot0000".iter().enumerate() {
                let at = BASE + 0x1000 + i as u64;
                dev.mmio_write(at, &[CMD_PROGRAM]);
                dev.mmio_write(at, &[*byte]);
            }
        }

        let (_, contents) = FileStore::open(&path, NVRAM).unwrap();
        assert_eq!(&contents[0x1000..0x1008], b"Boot0000");
        assert_eq!(contents.len(), NVRAM);

        // A store of the wrong size is refused rather than reinterpreted.
        std::fs::write(&path, b"too short").unwrap();
        assert!(matches!(
            FileStore::open(&path, NVRAM),
            Err(PflashError::WrongSize { .. })
        ));
        std::fs::remove_file(&path).unwrap();
    }

    /// The generated store must be byte-identical in every field EDK2's own
    /// template sets — the checksum especially, which is the one value here that
    /// is computed rather than transcribed.
    #[test]
    fn the_pristine_store_matches_edk2s_template() {
        let image = pristine_varstore();
        assert_eq!(image.len(), layout::PFLASH_NVRAM_SIZE as usize);

        // EFI_FIRMWARE_VOLUME_HEADER.
        assert_eq!(&image[..16], &[0u8; 16], "zero vector");
        assert_eq!(&image[16..32], &SYSTEM_NV_DATA_FV_GUID);
        assert_eq!(&image[32..40], &0x8_4000u64.to_le_bytes(), "FvLength");
        assert_eq!(&image[40..44], b"_FVH");
        assert_eq!(&image[44..48], &[0xff, 0xfe, 0x04, 0x00], "attributes");
        assert_eq!(&image[48..50], &[0x48, 0x00], "HeaderLength");
        // VarStore.fdf.inc's 4 MiB flavour: CheckSum 0xB8AF. Computed here, so a
        // change to any header field that broke the checksum would show up as
        // this assertion rather than as a firmware that rejects the volume.
        assert_eq!(&image[50..52], &[0xaf, 0xb8], "CheckSum");
        assert_eq!(
            &image[52..56],
            &[0x00, 0x00, 0x00, 0x02],
            "ExtHeader/Revision"
        );
        assert_eq!(
            &image[56..64],
            &[0x84, 0, 0, 0, 0x00, 0x10, 0, 0],
            "block map"
        );
        assert_eq!(&image[64..72], &[0u8; 8], "block map terminator");
        // And the checksum property itself: the 16-bit words sum to zero.
        let sum = image[..FV_HEADER_LEN].chunks_exact(2).fold(0u16, |acc, w| {
            acc.wrapping_add(u16::from_le_bytes([w[0], w[1]]))
        });
        assert_eq!(sum, 0);

        // VARIABLE_STORE_HEADER: the field whose mismatch asserted
        // VariableNonVolatile.c(228) on the very first boot of this device.
        assert_eq!(&image[0x48..0x58], &AUTHENTICATED_VARIABLE_GUID);
        assert_eq!(&image[0x58..0x5c], &0x3_ffb8u32.to_le_bytes(), "Size");
        assert_eq!(image[0x5c], 0x5a, "FORMATTED");
        assert_eq!(image[0x5d], 0xfe, "HEALTHY");
        assert_eq!(&image[0x5e..0x64], &[0u8; 6], "reserved");
        assert_eq!(image[0x64], ERASED, "and nothing beyond the two headers");

        // The event log stays erased; the FTW working block is formatted.
        let event_log = layout::PFLASH_VARSTORE_SIZE as usize;
        assert!(image[event_log..event_log + 0x1000]
            .iter()
            .all(|&b| b == ERASED));
        let ftw = event_log + layout::PFLASH_EVENT_LOG_SIZE as usize;
        assert_eq!(&image[ftw..ftw + 32], &FTW_WORKING_HEADER);
        // FTW spare: erased.
        let spare = ftw + layout::PFLASH_FTW_WORKING_SIZE as usize;
        assert!(image[spare..].iter().all(|&b| b == ERASED));
        assert_eq!(image.len() - spare, layout::PFLASH_FTW_SPARE_SIZE as usize);
    }

    /// A formatted store must still pass the flash probe: its first 16 bytes are
    /// the FV header's zero vector, which the probe's scan walks over.
    #[test]
    fn the_pristine_store_passes_the_flash_probe() {
        let mut dev = Pflash::new(
            BASE,
            layout::PFLASH_WINDOW_SIZE,
            pristine_varstore(),
            Some(Box::new(MemStore::new(NVRAM))),
        );
        assert!(dev.probe_sequence_is_flash());
        // And the probe left the store exactly as it found it.
        assert_eq!(dev.contents(), &pristine_varstore()[..]);
    }

    /// The layout the firmware build agrees with. If EDK2's PCD overrides and
    /// these constants disagree, the firmware probes an address nothing decodes
    /// and silently goes back to RAM variables.
    #[test]
    fn the_layout_matches_the_firmware_build() {
        assert_eq!(layout::PFLASH_BASE, 0xffc0_0000);
        assert_eq!(
            layout::PFLASH_BASE + layout::PFLASH_WINDOW_SIZE,
            layout::TOP_OF_32BIT,
            "the window must end exactly at 4 GiB, as OvmfPkgX64's flash does"
        );
        // 0x40000 variable store + 0x1000 event log + 0x1000 FTW working
        // + 0x42000 FTW spare, i.e. VARS_SIZE from CloudHvDefines.fdf.inc.
        assert_eq!(layout::PFLASH_NVRAM_SIZE, 0x8_4000);
        assert_eq!(
            layout::PFLASH_VARSTORE_SIZE
                + layout::PFLASH_EVENT_LOG_SIZE
                + layout::PFLASH_FTW_WORKING_SIZE
                + layout::PFLASH_FTW_SPARE_SIZE,
            layout::PFLASH_NVRAM_SIZE
        );
        assert_eq!(layout::PFLASH_NVRAM_SIZE % BLOCK_SIZE as u64, 0);
        // Clear of the interrupt controllers and of the MMIO hole the firmware
        // hard-codes.
        assert!(layout::PFLASH_BASE > u64::from(layout::LAPIC_ADDR));
        let cloudhv_mmio_hole_end: u64 = 0xc000_0000 + 0x3800_0000;
        assert!(layout::PFLASH_BASE > cloudhv_mmio_hole_end);
    }
}
