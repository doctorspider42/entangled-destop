//! vCPU register state for entering a 64-bit kernel directly
//! (backlog MVP-105): boot GDT, identity-map page tables, control registers
//! and the initial general-purpose registers per the Linux boot protocol.

use thiserror::Error;
use vm_memory::{Bytes, GuestAddress, GuestMemory};
use vmm_core::hv::{HvError, VcpuRegisters, X86Registers, X86Segment, X86SpecialRegisters};

use crate::layout;

#[derive(Debug, Error)]
pub enum BootSetupError {
    #[error("failed to write boot structures to guest memory: {0}")]
    GuestMemory(String),

    #[error("register setup failed: {0}")]
    Registers(#[from] HvError),
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

/// PVH boot GDT (ADR-0003): null, 32-bit flat code, 32-bit flat data, and a
/// 32-bit busy TSS with limit 0x67 — exactly the start-of-day descriptors the
/// PVH specification mandates. The firmware reloads its own GDT before it
/// touches a selector, but pointing GDTR at a real table keeps the state
/// self-consistent instead of relying on descriptor caches alone.
const PVH_GDT: [u64; 4] = [
    0,
    gdt_entry(0xc09b, 0, 0xfffff), // code: present, exec/read, DB=1, G=1
    gdt_entry(0xc093, 0, 0xfffff), // data: present, r/w, DB=1, G=1
    gdt_entry(0x008b, 0, 0x67),    // TSS: 32-bit busy, byte granular
];

fn segment_from_gdt(entry: u64, table_index: u8) -> X86Segment {
    let g = ((entry >> 55) & 1) as u8;
    let raw_limit = (((entry >> 32) & 0x000f_0000) | (entry & 0xffff)) as u32;
    X86Segment {
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
pub fn setup_long_mode_sregs<M: GuestMemory>(
    mem: &M,
    vcpu: &dyn VcpuRegisters,
) -> Result<(), BootSetupError> {
    let gm = |e: vm_memory::GuestMemoryError| BootSetupError::GuestMemory(e.to_string());
    let mut sregs: X86SpecialRegisters = vcpu.get_special_registers()?;

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

    vcpu.set_special_registers(&sregs)?;
    Ok(())
}

/// Puts the vCPU in the PVH start-of-day state: 32-bit protected mode, flat
/// segments, **paging disabled** (backlog UEFI-1802, ADR-0003).
///
/// Quoting Xen's `docs/misc/pvh.pandoc`: `cr0` bit 0 (PE) must be set and all
/// other writeable bits cleared, `cr4` all bits cleared, `cs` a 32-bit
/// read/execute segment with base 0 and limit `0xFFFFFFFF`, `ds`/`es`/`ss`
/// 32-bit read/write segments likewise, and `tr` an active 32-bit TSS with
/// base 0 and limit `0x67`.
///
/// Deliberately does *not* build page tables: unlike the direct-Linux path,
/// the firmware enters with paging off and installs its own.
pub fn setup_pvh_sregs<M: GuestMemory>(
    mem: &M,
    vcpu: &dyn VcpuRegisters,
) -> Result<(), BootSetupError> {
    let gm = |e: vm_memory::GuestMemoryError| BootSetupError::GuestMemory(e.to_string());
    let mut sregs: X86SpecialRegisters = vcpu.get_special_registers()?;

    for (i, entry) in PVH_GDT.iter().enumerate() {
        mem.write_obj(*entry, GuestAddress(layout::BOOT_GDT_START + i as u64 * 8))
            .map_err(gm)?;
    }
    sregs.gdt.base = layout::BOOT_GDT_START;
    sregs.gdt.limit = (PVH_GDT.len() * 8 - 1) as u16;

    mem.write_obj(0u64, GuestAddress(layout::BOOT_IDT_START))
        .map_err(gm)?;
    sregs.idt.base = layout::BOOT_IDT_START;
    sregs.idt.limit = 7;

    let data = segment_from_gdt(PVH_GDT[2], 2);
    sregs.cs = segment_from_gdt(PVH_GDT[1], 1);
    sregs.ds = data;
    sregs.es = data;
    sregs.fs = data;
    sregs.gs = data;
    sregs.ss = data;
    sregs.tr = segment_from_gdt(PVH_GDT[3], 3);

    // Assign rather than OR: KVM's reset CR0 has CD|NW set, and the PVH
    // contract says every writeable bit other than PE is clear. ET (bit 4) is
    // hardwired to 1 on every CPU that can run us, so keeping it avoids a
    // pointless KVM_SET_SREGS disagreement.
    sregs.cr0 = CR0_PE | CR0_ET;
    sregs.cr3 = 0;
    sregs.cr4 = 0;
    sregs.efer = 0; // no LME/LMA: this is 32-bit protected mode, not long mode

    vcpu.set_special_registers(&sregs)?;
    Ok(())
}

/// Sets the general-purpose registers for a PVH entry point: `eip` at the
/// firmware's `XEN_ELFNOTE_PHYS32_ENTRY`, `ebx` at the `hvm_start_info`
/// structure, interrupts off.
pub fn setup_pvh_regs(
    vcpu: &dyn VcpuRegisters,
    entry_point: u64,
    start_info: u64,
) -> Result<(), BootSetupError> {
    let regs = X86Registers {
        // Bit 1 is reserved-set; IF (9), TF (8) and VM (17) must all be clear.
        rflags: 2,
        rip: entry_point,
        rbx: start_info,
        ..Default::default()
    };
    vcpu.set_registers(&regs)?;
    Ok(())
}

/// Sets the general-purpose registers for the 64-bit kernel entry point:
/// `rip` at the entry, `rsi` pointing at `boot_params`, per the Linux x86
/// boot protocol.
pub fn setup_boot_regs(
    vcpu: &dyn VcpuRegisters,
    entry_point: u64,
    boot_params: u64,
) -> Result<(), BootSetupError> {
    let regs = X86Registers {
        rflags: 2, // reserved bit 1 must be set
        rip: entry_point,
        rsp: layout::BOOT_STACK_POINTER,
        rbp: layout::BOOT_STACK_POINTER,
        rsi: boot_params,
        ..Default::default()
    };
    vcpu.set_registers(&regs)?;
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

    /// ADR-0003: the PVH start-of-day descriptors are 32-bit and flat, and the
    /// TSS is a *byte-granular* 32-bit busy TSS with limit 0x67 — a G=1 TSS
    /// would describe a 0x67000-byte segment instead.
    #[test]
    fn pvh_gdt_matches_the_pvh_contract() {
        let code = segment_from_gdt(PVH_GDT[1], 1);
        assert_eq!(code.selector, 8);
        assert_eq!(code.l, 0, "PVH entry is 32-bit, not long mode");
        assert_eq!(code.db, 1, "32-bit default operand size");
        assert_eq!(code.base, 0);
        assert_eq!(code.limit, 0xffff_ffff);
        assert_eq!(code.present, 1);

        let data = segment_from_gdt(PVH_GDT[2], 2);
        assert_eq!(data.selector, 16);
        assert_eq!(data.base, 0);
        assert_eq!(data.limit, 0xffff_ffff);

        let tss = segment_from_gdt(PVH_GDT[3], 3);
        assert_eq!(tss.selector, 24);
        assert_eq!(tss.g, 0);
        assert_eq!(tss.limit, 0x67);
        assert_eq!(tss.type_, 0xb, "32-bit busy TSS");
        assert_eq!(tss.present, 1);
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
