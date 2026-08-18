//! vCPU register state for entering a 64-bit kernel directly
//! (backlog MVP-105): boot GDT, identity-map page tables, control registers
//! and the initial general-purpose registers per the Linux boot protocol.

use kvm_bindings::{kvm_regs, kvm_segment};
use kvm_ioctls::VcpuFd;
use thiserror::Error;
use vm_memory::{Bytes, GuestAddress, GuestMemory};

use crate::layout;

#[derive(Debug, Error)]
pub enum BootSetupError {
    #[error("failed to write boot structures to guest memory: {0}")]
    GuestMemory(String),

    #[error("KVM register setup failed: {0}")]
    Kvm(#[from] kvm_ioctls::Error),
}

// Page table entry flags.
const PTE_PRESENT: u64 = 1;
const PTE_RW: u64 = 1 << 1;
const PTE_PS: u64 = 1 << 7; // 2 MiB page (in a PD entry)

// Control register bits.
const CR0_PE: u64 = 1;
const CR0_MP: u64 = 1 << 1;
const CR0_ET: u64 = 1 << 4;
const CR0_NE: u64 = 1 << 5;
const CR0_WP: u64 = 1 << 16;
const CR0_AM: u64 = 1 << 18;
const CR0_PG: u64 = 1 << 31;
const CR4_PAE: u64 = 1 << 5;
const EFER_LME: u64 = 1 << 8;
const EFER_LMA: u64 = 1 << 10;

/// Packs a GDT descriptor from access/granularity flags, base and limit.
const fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 {
    ((base as u64 & 0xff00_0000) << 32)
        | ((base as u64 & 0x00ff_ffff) << 16)
        | (limit as u64 & 0x0000_ffff)
        | ((limit as u64 & 0x000f_0000) << 32)
        | ((flags as u64) << 40)
}

/// Boot GDT: null, 64-bit code, data, TSS — the layout Linux expects from a
/// 64-bit boot protocol loader.
const BOOT_GDT: [u64; 4] = [
    0,
    gdt_entry(0xa09b, 0, 0xfffff), // code: present, exec, L=1, G=1
    gdt_entry(0xc093, 0, 0xfffff), // data: present, writable, DB=1, G=1
    gdt_entry(0x808b, 0, 0xfffff), // TSS (64-bit available)
];

fn segment_from_gdt(entry: u64, table_index: u8) -> kvm_segment {
    let g = ((entry >> 55) & 1) as u8;
    let raw_limit = (((entry >> 32) & 0x000f_0000) | (entry & 0xffff)) as u32;
    kvm_segment {
        base: ((entry >> 16) & 0x00ff_ffff) | (((entry >> 56) & 0xff) << 24),
        limit: if g == 0 {
            raw_limit
        } else {
            (raw_limit << 12) | 0xfff
        },
        selector: u16::from(table_index) * 8,
        type_: ((entry >> 40) & 0xf) as u8,
        present: ((entry >> 47) & 1) as u8,
        dpl: ((entry >> 45) & 3) as u8,
        db: ((entry >> 54) & 1) as u8,
        s: ((entry >> 44) & 1) as u8,
        l: ((entry >> 53) & 1) as u8,
        g,
        avl: ((entry >> 52) & 1) as u8,
        unusable: 0,
        padding: 0,
    }
}

/// Builds identity-mapped page tables covering the first 4 GiB with 2 MiB
/// pages: PML4 → PDPT → 4 page directories. Covers all MVP guest RAM (which
/// stays below the MMIO hole) plus the 32-bit MMIO window.
pub fn setup_page_tables<M: GuestMemory>(mem: &M) -> Result<(), BootSetupError> {
    let gm = |e: vm_memory::GuestMemoryError| BootSetupError::GuestMemory(e.to_string());
    mem.write_obj(
        layout::PDPTE_START | PTE_PRESENT | PTE_RW,
        GuestAddress(layout::PML4_START),
    )
    .map_err(gm)?;
    for i in 0..4u64 {
        let pd_base = layout::PD_START + i * 0x1000;
        mem.write_obj(
            pd_base | PTE_PRESENT | PTE_RW,
            GuestAddress(layout::PDPTE_START + i * 8),
        )
        .map_err(gm)?;
        for j in 0..512u64 {
            let phys = (i << 30) + (j << 21);
            mem.write_obj(
                phys | PTE_PRESENT | PTE_RW | PTE_PS,
                GuestAddress(pd_base + j * 8),
            )
            .map_err(gm)?;
        }
    }
    Ok(())
}

/// Writes the boot GDT/IDT into guest memory and configures the vCPU's
/// segment and control registers for 64-bit (long mode) execution with
/// paging enabled.
pub fn setup_long_mode_sregs<M: GuestMemory>(mem: &M, vcpu: &VcpuFd) -> Result<(), BootSetupError> {
    let gm = |e: vm_memory::GuestMemoryError| BootSetupError::GuestMemory(e.to_string());
    let mut sregs = vcpu.get_sregs()?;

    for (i, entry) in BOOT_GDT.iter().enumerate() {
        mem.write_obj(*entry, GuestAddress(layout::BOOT_GDT_START + i as u64 * 8))
            .map_err(gm)?;
    }
    sregs.gdt.base = layout::BOOT_GDT_START;
    sregs.gdt.limit = (BOOT_GDT.len() * 8 - 1) as u16;

    mem.write_obj(0u64, GuestAddress(layout::BOOT_IDT_START))
        .map_err(gm)?;
    sregs.idt.base = layout::BOOT_IDT_START;
    sregs.idt.limit = 7;

    let code = segment_from_gdt(BOOT_GDT[1], 1);
    let data = segment_from_gdt(BOOT_GDT[2], 2);
    sregs.cs = code;
    sregs.ds = data;
    sregs.es = data;
    sregs.fs = data;
    sregs.gs = data;
    sregs.ss = data;
    sregs.tr = segment_from_gdt(BOOT_GDT[3], 3);

    setup_page_tables(mem)?;
    sregs.cr3 = layout::PML4_START;
    sregs.cr4 |= CR4_PAE;
    sregs.cr0 |= CR0_PE | CR0_MP | CR0_ET | CR0_NE | CR0_WP | CR0_AM | CR0_PG;
    sregs.efer |= EFER_LME | EFER_LMA;

    vcpu.set_sregs(&sregs)?;
    Ok(())
}

/// Sets the general-purpose registers for the 64-bit kernel entry point:
/// `rip` at the entry, `rsi` pointing at `boot_params`, per the Linux x86
/// boot protocol.
pub fn setup_boot_regs(
    vcpu: &VcpuFd,
    entry_point: u64,
    boot_params: u64,
) -> Result<(), BootSetupError> {
    let regs = kvm_regs {
        rflags: 2, // reserved bit 1 must be set
        rip: entry_point,
        rsp: layout::BOOT_STACK_POINTER,
        rbp: layout::BOOT_STACK_POINTER,
        rsi: boot_params,
        ..Default::default()
    };
    vcpu.set_regs(&regs)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::GuestMemoryMmap;

    fn mem() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 16 << 20)]).unwrap()
    }

    #[test]
    fn page_tables_identity_map_first_4gib() {
        let m = mem();
        setup_page_tables(&m).unwrap();
        let pml4: u64 = m.read_obj(GuestAddress(layout::PML4_START)).unwrap();
        assert_eq!(pml4, layout::PDPTE_START | 0x3);
        let pdpte0: u64 = m.read_obj(GuestAddress(layout::PDPTE_START)).unwrap();
        assert_eq!(pdpte0, layout::PD_START | 0x3);
        // First 2 MiB page maps physical 0 with PS set.
        let pd0: u64 = m.read_obj(GuestAddress(layout::PD_START)).unwrap();
        assert_eq!(pd0, 0x83);
        // Last entry of the last PD maps 4 GiB - 2 MiB.
        let last: u64 = m
            .read_obj(GuestAddress(layout::PD_START + 3 * 0x1000 + 511 * 8))
            .unwrap();
        assert_eq!(last, ((3u64 << 30) + (511u64 << 21)) | 0x83);
    }

    #[test]
    fn gdt_code_segment_is_64bit() {
        let code = segment_from_gdt(BOOT_GDT[1], 1);
        assert_eq!(code.selector, 8);
        assert_eq!(code.l, 1, "code segment must be long-mode");
        assert_eq!(code.present, 1);
        assert_eq!(code.limit, 0xffff_ffff);
        let data = segment_from_gdt(BOOT_GDT[2], 2);
        assert_eq!(data.selector, 16);
        assert_eq!(data.l, 0);
        assert_eq!(data.db, 1);
    }
}
