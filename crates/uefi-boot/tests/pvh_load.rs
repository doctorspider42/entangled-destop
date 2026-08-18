//! PVH firmware loading into guest memory (backlog UEFI-1802). No hypervisor
//! needed: the assertions are about what ends up in guest RAM.

#![cfg(target_os = "linux")]

use std::path::Path;

use uefi_boot::pvh;
use uefi_boot::{FirmwareImage, FirmwareKind};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

const XEN_ELFNOTE_PHYS32_ENTRY: u32 = 18;
const PT_LOAD: u32 = 1;
const PT_NOTE: u32 = 4;

/// Builds a PVH ELF shaped like EDK2's `CLOUDHV.fd`: an ELF64 header, one
/// `PT_LOAD` covering the whole file at `paddr`, one `PT_NOTE` with Xen's
/// 32-bit entry note. `payload` is the recognisable body we later look for in
/// guest memory.
fn cloudhv_like(paddr: u64, entry: u32, payload: &[u8]) -> Vec<u8> {
    let phoff = 0x40usize;
    let phentsize = 0x38usize;
    let notes_at = 0xb0usize;
    let note_len = 12 + 4 + 4; // namesz("Xen\0") + type + 4-byte desc
    let body_at = 0x1000usize;
    let total = body_at + payload.len();

    let mut v = vec![0u8; body_at];
    v[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    v[4] = 2; // ELFCLASS64
    v[5] = 1; // ELFDATA2LSB
    v[6] = 1; // EV_CURRENT
    v[0x10..0x12].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    v[0x12..0x14].copy_from_slice(&3u16.to_le_bytes()); // EM_386, like CloudHv
    v[0x18..0x20].copy_from_slice(&u64::from(entry).to_le_bytes()); // e_entry
    v[0x20..0x28].copy_from_slice(&(phoff as u64).to_le_bytes());
    v[0x34..0x36].copy_from_slice(&0x40u16.to_le_bytes()); // e_ehsize
    v[0x36..0x38].copy_from_slice(&(phentsize as u16).to_le_bytes());
    v[0x38..0x3a].copy_from_slice(&2u16.to_le_bytes()); // e_phnum

    let load = phoff;
    v[load..load + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
    v[load + 4..load + 8].copy_from_slice(&7u32.to_le_bytes()); // RWE
    v[load + 0x08..load + 0x10].copy_from_slice(&0u64.to_le_bytes()); // p_offset
    v[load + 0x10..load + 0x18].copy_from_slice(&paddr.to_le_bytes()); // p_vaddr
    v[load + 0x18..load + 0x20].copy_from_slice(&paddr.to_le_bytes()); // p_paddr
    v[load + 0x20..load + 0x28].copy_from_slice(&(total as u64).to_le_bytes()); // p_filesz
    v[load + 0x28..load + 0x30].copy_from_slice(&(total as u64).to_le_bytes()); // p_memsz
    v[load + 0x30..load + 0x38].copy_from_slice(&4u64.to_le_bytes()); // p_align

    let note = phoff + phentsize;
    v[note..note + 4].copy_from_slice(&PT_NOTE.to_le_bytes());
    v[note + 4..note + 8].copy_from_slice(&4u32.to_le_bytes()); // R
    v[note + 0x08..note + 0x10].copy_from_slice(&(notes_at as u64).to_le_bytes());
    v[note + 0x10..note + 0x18].copy_from_slice(&(paddr + notes_at as u64).to_le_bytes());
    v[note + 0x18..note + 0x20].copy_from_slice(&(paddr + notes_at as u64).to_le_bytes());
    v[note + 0x20..note + 0x28].copy_from_slice(&(note_len as u64).to_le_bytes());
    v[note + 0x28..note + 0x30].copy_from_slice(&(note_len as u64).to_le_bytes());
    v[note + 0x30..note + 0x38].copy_from_slice(&4u64.to_le_bytes());

    v[notes_at..notes_at + 4].copy_from_slice(&4u32.to_le_bytes()); // n_namesz
    v[notes_at + 4..notes_at + 8].copy_from_slice(&4u32.to_le_bytes()); // n_descsz
    v[notes_at + 8..notes_at + 12].copy_from_slice(&XEN_ELFNOTE_PHYS32_ENTRY.to_le_bytes());
    v[notes_at + 12..notes_at + 16].copy_from_slice(b"Xen\0");
    v[notes_at + 16..notes_at + 20].copy_from_slice(&entry.to_le_bytes());

    v.extend_from_slice(payload);
    v
}

fn guest_memory(mib: u64) -> GuestMemoryMmap {
    GuestMemoryMmap::from_ranges(&[(GuestAddress(0), (mib << 20) as usize)]).unwrap()
}

#[test]
fn pvh_firmware_lands_at_its_program_header_address() {
    // The addresses EDK2's CloudHv actually uses.
    const LOAD_ADDR: u64 = 0x0010_0000;
    const ENTRY: u32 = 0x004f_ffd0;
    let payload = b"ENTANGLED-FIRMWARE-BODY";
    let image = FirmwareImage::from_bytes(
        Path::new("CLOUDHV.fd"),
        cloudhv_like(LOAD_ADDR, ENTRY, payload),
    )
    .unwrap();
    assert_eq!(
        image.kind(),
        FirmwareKind::PvhElf {
            entry: u64::from(ENTRY)
        }
    );

    let mem_size = 512u64 << 20;
    let mem = guest_memory(512);
    let boot = uefi_boot::load_pvh(&mem, &image, mem_size).unwrap();

    assert_eq!(boot.entry, u64::from(ENTRY));
    assert_eq!(
        boot.start_info_addr,
        machine_x86::layout::PVH_START_INFO_START
    );

    // The body must be readable at load address + its file offset.
    let mut body = vec![0u8; payload.len()];
    mem.read_slice(&mut body, GuestAddress(LOAD_ADDR + 0x1000))
        .unwrap();
    assert_eq!(&body, payload, "firmware body not at its p_paddr");

    // hvm_start_info: magic, version 1, and a memmap that describes RAM.
    let mut si = [0u8; pvh::START_INFO_SIZE];
    mem.read_slice(&mut si, GuestAddress(boot.start_info_addr))
        .unwrap();
    assert_eq!(
        u32::from_le_bytes(si[0..4].try_into().unwrap()),
        pvh::XEN_HVM_START_MAGIC_VALUE
    );
    assert_eq!(u32::from_le_bytes(si[4..8].try_into().unwrap()), 1);
    let memmap_paddr = u64::from_le_bytes(si[40..48].try_into().unwrap());
    let memmap_entries = u32::from_le_bytes(si[48..52].try_into().unwrap()) as usize;
    assert_eq!(memmap_paddr, machine_x86::layout::PVH_MEMMAP_START);
    assert_eq!(memmap_entries, pvh::memmap_for(mem_size).len());

    // The first memmap entry is low RAM starting at 0, and no entry may reach
    // into the reset-vector ROM window.
    let mut table = vec![0u8; memmap_entries * pvh::MEMMAP_ENTRY_SIZE];
    mem.read_slice(&mut table, GuestAddress(memmap_paddr))
        .unwrap();
    assert_eq!(u64::from_le_bytes(table[0..8].try_into().unwrap()), 0);
    assert_eq!(
        u32::from_le_bytes(table[16..20].try_into().unwrap()),
        pvh::XEN_HVM_MEMMAP_TYPE_RAM
    );
    let ceiling = uefi_boot::rom::place_at_top_of_32bit(4 << 20)
        .unwrap()
        .guest_addr;
    for i in 0..memmap_entries {
        let at = i * pvh::MEMMAP_ENTRY_SIZE;
        let addr = u64::from_le_bytes(table[at..at + 8].try_into().unwrap());
        let size = u64::from_le_bytes(table[at + 8..at + 16].try_into().unwrap());
        assert!(
            addr + size <= ceiling,
            "memmap entry {i} at {addr:#x}+{size:#x} reaches the firmware ROM window"
        );
    }
}

#[test]
fn refuses_a_firmware_that_does_not_fit_in_ram() {
    // Loads at 1 MiB with a body large enough to exceed a 2 MiB VM.
    let image = FirmwareImage::from_bytes(
        Path::new("big.fd"),
        cloudhv_like(0x0010_0000, 0x0010_0000, &vec![0xaa; 4 << 20]),
    )
    .unwrap();
    let mem = guest_memory(128);
    let err = uefi_boot::load_pvh(&mem, &image, 2 << 20).unwrap_err();
    assert!(
        matches!(
            err,
            uefi_boot::FirmwareError::NoRoomForFirmware { .. } | uefi_boot::FirmwareError::Load(_)
        ),
        "{err}"
    );
}

#[test]
fn reset_vector_images_are_rejected_by_the_pvh_loader() {
    let image = FirmwareImage::from_bytes(Path::new("flash.fd"), vec![0xff; 0x1000]).unwrap();
    assert_eq!(image.kind(), FirmwareKind::ResetVector);
    let mem = guest_memory(64);
    assert!(matches!(
        uefi_boot::load_pvh(&mem, &image, 64 << 20),
        Err(uefi_boot::FirmwareError::MalformedElf(_))
    ));
}
