//! UEFI firmware boot (backlog EPIC 18, UEFI-1801/1802).
//!
//! Two entry protocols, chosen by looking at the firmware image itself
//! ([`ADR-0003`](../../../docs/adr/0003-uefi-firmware.md)):
//!
//! * **PVH** — the image is an ELF carrying `XEN_ELFNOTE_PHYS32_ENTRY`. It is
//!   loaded into guest RAM at the physical addresses its program headers ask
//!   for and entered in 32-bit protected mode with `%ebx` pointing at an
//!   [`pvh::StartInfo`]. This is what EDK2 `OvmfPkg/CloudHv` and
//!   `rust-hypervisor-firmware` use.
//! * **Reset vector** — anything else is treated as a flash image, mapped as a
//!   ROM whose last byte sits at 4 GiB − 1 so that the architectural reset
//!   vector `0xffff_fff0` lands inside it, and entered with the vCPU left in
//!   the state KVM already gives us.
//!
//! Image inspection, placement arithmetic and the PVH structure layout are
//! host-OS independent and tested everywhere; only the guest-memory loading is
//! Linux-gated.

use std::path::{Path, PathBuf};

use thiserror::Error;

pub mod image;
pub mod pvh;
pub mod rom;

#[cfg(target_os = "linux")]
mod load;

#[cfg(target_os = "linux")]
pub use load::{load_pvh, PvhBoot};

pub use image::{FirmwareImage, FirmwareKind};
pub use rom::RomPlacement;

/// What to boot in `mode = "uefi"` (resolved from the VM config).
#[derive(Debug, Clone)]
pub struct FirmwareConfig {
    /// Path to the firmware image (`CLOUDHV.fd`, `OVMF.fd`, …).
    pub firmware: PathBuf,
}

#[derive(Debug, Error)]
pub enum FirmwareError {
    #[error("cannot read firmware {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("firmware image {path} is empty")]
    Empty { path: PathBuf },

    #[error(
        "firmware image is {len} bytes; a reset-vector ROM must be a non-zero \
         multiple of the {page:#x}-byte page size"
    )]
    NotPageAligned { len: u64, page: u64 },

    #[error(
        "firmware image of {len:#x} bytes placed at the top of the 32-bit \
         address space would start at {start:#x} and swallow the APIC/MMIO \
         window above {limit:#x}"
    )]
    RomTooLarge { len: u64, start: u64, limit: u64 },

    #[error("firmware ELF is malformed: {0}")]
    MalformedElf(&'static str),

    #[error(
        "PVH firmware needs {needed:#x} bytes of guest RAM at {addr:#x}, but the VM has {have:#x}"
    )]
    NoRoomForFirmware { addr: u64, needed: u64, have: u64 },

    #[error("the PVH memory map needs {needed} entries, but only {max} fit")]
    MemmapTooLarge { needed: usize, max: usize },

    #[error("failed to load firmware into guest memory: {0}")]
    Load(String),

    #[error("failed to write PVH boot data to guest memory: {0}")]
    GuestMemory(String),
}

impl FirmwareConfig {
    /// Reads and classifies the firmware image without touching guest memory.
    pub fn open(&self) -> Result<FirmwareImage, FirmwareError> {
        FirmwareImage::read(&self.firmware)
    }
}

/// Reads a firmware image from disk.
pub(crate) fn read_image(path: &Path) -> Result<Vec<u8>, FirmwareError> {
    let bytes = std::fs::read(path).map_err(|source| FirmwareError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    if bytes.is_empty() {
        return Err(FirmwareError::Empty {
            path: path.to_path_buf(),
        });
    }
    Ok(bytes)
}
