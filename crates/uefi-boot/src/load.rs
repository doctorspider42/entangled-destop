//! Loading a PVH firmware into guest memory (backlog UEFI-1802).
//!
//! The reset-vector path has nothing to load — the image becomes its own KVM
//! memory slot (`vmm_core::Vm::map_rom`) and the vCPU comes out of reset
//! pointing at it. Only PVH needs work here: ELF program headers copied to the
//! physical addresses they name, plus the start-of-day structures.

use std::io::Cursor;

use linux_loader::loader::elf::Elf;
use linux_loader::loader::{KernelLoader, PvhBootCapability};
use machine_x86::layout;
use vm_memory::{Bytes, GuestAddress, GuestMemory};

use crate::pvh;
use crate::{FirmwareError, FirmwareImage, FirmwareKind};

/// Where vCPU0 must start for a PVH firmware, and what goes in `%ebx`.
#[derive(Debug, Clone, Copy)]
pub struct PvhBoot {
    pub entry: u64,
    pub start_info_addr: u64,
    /// Highest guest address the firmware image occupies, for logging and for
    /// the "does this fit in RAM" check.
    pub image_end: u64,
}

/// Loads a PVH firmware image into `mem` and writes its start-of-day data.
///
/// `mem_size` is the guest RAM size in bytes; it defines the PVH memory map and
/// bounds the load. Returns an error (never a panic) if the image does not fit
/// or the memory map cannot be encoded.
pub fn load_pvh<M: GuestMemory>(
    mem: &M,
    image: &FirmwareImage,
    mem_size: u64,
) -> Result<PvhBoot, FirmwareError> {
    let FirmwareKind::PvhElf { entry } = image.kind() else {
        return Err(FirmwareError::MalformedElf(
            "load_pvh called with a reset-vector firmware image",
        ));
    };

    let mut cursor = Cursor::new(image.bytes());
    // No kernel_offset: PT_LOAD segments go exactly to their p_paddr (CloudHv
    // asks for 1 MiB). No highmem_start_address: the firmware's 32-bit entry
    // point legitimately sits below any "high memory" threshold, and passing
    // one only makes linux-loader reject it.
    let loaded = Elf::load(mem, None, &mut cursor, None)
        .map_err(|e| FirmwareError::Load(format!("{}: {e}", image.path().display())))?;

    // Cross-check our own note parser against the loader's: they read the same
    // note independently, and a disagreement means one of them is wrong.
    match loaded.pvh_boot_cap {
        PvhBootCapability::PvhEntryPresent(addr) if addr.0 == entry => {}
        PvhBootCapability::PvhEntryPresent(addr) => {
            return Err(FirmwareError::Load(format!(
                "PVH entry mismatch: image scan says {entry:#x}, ELF loader says {:#x}",
                addr.0
            )));
        }
        PvhBootCapability::PvhEntryNotPresent => {
            return Err(FirmwareError::MalformedElf(
                "the ELF loader found no PVH entry point in an image our scan accepted",
            ));
        }
        // `PvhEntryIgnored` is what the loader reports when the note was not
        // looked for at all; our own scan already found one, so trust it.
        PvhBootCapability::PvhEntryIgnored => {
            tracing::debug!("ELF loader ignored the PVH note; using the scanned entry");
        }
    }

    let image_end = loaded.kernel_end;
    if image_end > mem_size {
        return Err(FirmwareError::NoRoomForFirmware {
            addr: loaded.kernel_load.0,
            needed: image_end,
            have: mem_size,
        });
    }

    let gm = |e: vm_memory::GuestMemoryError| FirmwareError::GuestMemory(e.to_string());

    // Empty command line: the firmware and the guest bootloader own the boot
    // path in UEFI mode. The pointer is still valid, which is friendlier to
    // firmwares that dereference it unconditionally than a null would be.
    mem.write_slice(&[0u8], GuestAddress(layout::PVH_CMDLINE_START))
        .map_err(gm)?;

    let entries = pvh::memmap_for(mem_size);
    let memmap = pvh::encode_memmap(&entries)?;
    mem.write_slice(&memmap, GuestAddress(layout::PVH_MEMMAP_START))
        .map_err(gm)?;

    let start_info = pvh::StartInfo {
        cmdline_paddr: layout::PVH_CMDLINE_START,
        // No ACPI tables yet — see ADR-0003's phase 2 gap map.
        rsdp_paddr: 0,
        memmap_paddr: layout::PVH_MEMMAP_START,
        memmap_entries: entries.len() as u32,
    };
    mem.write_slice(
        &start_info.encode(),
        GuestAddress(layout::PVH_START_INFO_START),
    )
    .map_err(gm)?;

    tracing::info!(
        firmware = %image.path().display(),
        entry = format_args!("{entry:#x}"),
        load = format_args!("{:#x}", loaded.kernel_load.0),
        end = format_args!("{image_end:#x}"),
        memmap_entries = entries.len(),
        "loaded PVH firmware"
    );

    Ok(PvhBoot {
        entry,
        start_info_addr: layout::PVH_START_INFO_START,
        image_end,
    })
}
