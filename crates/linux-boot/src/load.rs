//! Loading a bzImage + initramfs + cmdline into guest memory and building
//! `boot_params` with the E820 map (backlog MVP-201..204).

use std::io::Cursor;

use linux_loader::bootparam::{boot_e820_entry, boot_params};
use linux_loader::cmdline::Cmdline;
use linux_loader::configurator::linux::LinuxBootConfigurator;
use linux_loader::configurator::{BootConfigurator, BootParams};
use linux_loader::loader::bzimage::BzImage;
use linux_loader::loader::{load_cmdline, KernelLoader};
use machine_x86::layout;
use vm_memory::{Bytes, GuestAddress, GuestMemory};

use crate::{BootConfig, BootError};

/// Offset of the 64-bit entry point from the kernel load address, per the
/// Linux x86 boot protocol.
const ENTRY_64_OFFSET: u64 = 0x200;

/// Result of loading a kernel: where vCPU0 must start and where the zero
/// page lives (goes into `rsi`).
#[derive(Debug, Clone, Copy)]
pub struct LoadedKernel {
    pub entry: u64,
    pub boot_params_addr: u64,
}

fn load_err(what: &'static str) -> impl FnOnce(&dyn std::fmt::Display) -> BootError + 'static {
    move |e| BootError::Load {
        what,
        message: e.to_string(),
    }
}

/// Loads the kernel, optional initramfs and command line into `mem` and
/// writes the zero page. `mem_size` is the guest RAM size in bytes (used for
/// the E820 map and initramfs placement).
pub fn load<M: GuestMemory>(
    mem: &M,
    cfg: &BootConfig,
    mem_size: u64,
) -> Result<LoadedKernel, BootError> {
    cfg.validate()?;

    // Read the image into memory rather than handing `BzImage::load` the `File`.
    // `KernelLoader::load` wants `Read + ReadVolatile + Seek`, and vm-memory only
    // implements `ReadVolatile` for `File` behind its unix-only `rawfd` feature —
    // the feature ADR-0002 keeps switched off workspace-wide so the virtio crates
    // and this one build natively on Windows. `Cursor<Vec<u8>>` satisfies all
    // three portably, at the cost of one transient copy of a kernel image
    // (single-digit MiB), which the initramfs path already pays anyway.
    let image = std::fs::read(&cfg.kernel).map_err(|e| BootError::Load {
        what: "kernel",
        message: format!("{}: {e}", cfg.kernel.display()),
    })?;
    let loaded = BzImage::load(
        mem,
        None,
        &mut Cursor::new(image),
        Some(GuestAddress(layout::HIGH_RAM_START)),
    )
    .map_err(|e| load_err("kernel")(&e))?;
    let setup_header = loaded
        .setup_header
        .ok_or_else(|| load_err("kernel")(&"bzImage has no setup header"))?;

    let mut params = boot_params {
        hdr: setup_header,
        ..Default::default()
    };
    params.hdr.type_of_loader = 0xff;

    // Command line.
    let mut cmdline = Cmdline::new(layout::CMDLINE_MAX_LEN).map_err(|e| load_err("cmdline")(&e))?;
    cmdline
        .insert_str(&cfg.cmdline)
        .map_err(|e| load_err("cmdline")(&e))?;
    load_cmdline(mem, GuestAddress(layout::CMDLINE_START), &cmdline)
        .map_err(|e| load_err("cmdline")(&e))?;
    params.hdr.cmd_line_ptr = layout::CMDLINE_START as u32;
    params.hdr.cmdline_size = cfg.cmdline.len() as u32;

    // Initramfs, placed as high as the setup header and RAM allow.
    if let Some(path) = &cfg.initramfs {
        let data = std::fs::read(path).map_err(|e| BootError::Load {
            what: "initramfs",
            message: format!("{}: {e}", path.display()),
        })?;
        let kernel_end = loaded.kernel_end;
        let addr = initramfs_address(&setup_header, mem_size, data.len() as u64, kernel_end)?;
        mem.write_slice(&data, GuestAddress(addr))
            .map_err(|e| load_err("initramfs")(&e))?;
        params.hdr.ramdisk_image = addr as u32;
        params.hdr.ramdisk_size = data.len() as u32;
    }

    // Where the ACPI tables are (`machine_x86::acpi`). Handing the RSDP address
    // over in `boot_params` is the modern boot-protocol way and saves the kernel
    // the legacy EBDA/0xE0000 scan; `acpi_os_get_root_pointer()` prefers it.
    //
    // Advertised unconditionally, whether or not the machine actually published
    // the tables: `machine_x86::acpi::write` is a separate call, and if it was
    // not made the region is zeroed, the RSDP signature check fails and the
    // guest falls back to the MP table. Advertising garbage is not a risk — an
    // unsigned RSDP is rejected, not misread.
    params.acpi_rsdp_addr = layout::ACPI_RSDP_START;

    // E820 memory map. Includes the ACPI region as ACPI-reclaimable, which is
    // what keeps Linux from allocating over the tables (`e820__memblock_setup`
    // only adds RAM ranges to memblock).
    let e820 = machine_x86::e820_map(mem_size);
    for (i, entry) in e820.iter().enumerate() {
        params.e820_table[i] = boot_e820_entry {
            addr: entry.addr,
            size: entry.size,
            r#type: entry.kind as u32,
        };
    }
    params.e820_entries = e820.len() as u8;

    LinuxBootConfigurator::write_bootparams(
        &BootParams::new(&params, GuestAddress(layout::ZERO_PAGE_START)),
        mem,
    )
    .map_err(|e| load_err("boot_params")(&e))?;

    Ok(LoadedKernel {
        entry: loaded.kernel_load.0 + ENTRY_64_OFFSET,
        boot_params_addr: layout::ZERO_PAGE_START,
    })
}

/// Picks the highest page-aligned address where the initramfs fits below
/// the setup header's `initrd_addr_max`, below the end of *low* RAM (a guest
/// bigger than the 32-bit MMIO hole continues at 4 GiB, but the initramfs must
/// stay 32-bit addressable — `initrd_addr_max` itself is a `u32`), and above
/// the loaded kernel.
fn initramfs_address(
    hdr: &linux_loader::bootparam::setup_header,
    mem_size: u64,
    initramfs_size: u64,
    kernel_end: u64,
) -> Result<u64, BootError> {
    let low_ram_end = mem_size.min(layout::MMIO_HOLE_START);
    let ceiling = u64::from(hdr.initrd_addr_max).min(low_ram_end.saturating_sub(1));
    let addr = (ceiling + 1)
        .checked_sub(initramfs_size)
        .map(|a| a & !0xfff)
        .filter(|&a| a > kernel_end)
        .ok_or_else(|| BootError::Load {
            what: "initramfs",
            message: format!(
                "no room: size {initramfs_size:#x}, ceiling {ceiling:#x}, kernel ends at {kernel_end:#x}"
            ),
        })?;
    Ok(addr)
}
