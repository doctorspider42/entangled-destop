//! Guest memory creation (backlog MVP-102, EPIC 17 / ADR-0002).
//!
//! # Why this module is portable
//!
//! ADR-0002's first amendment recorded `vm-memory`'s mmap backend as
//! "unix-only". That measurement was off by one feature: the backend itself
//! ships a Windows implementation (`vm_memory::mmap::windows`) that reserves
//! and commits guest RAM with `VirtualAlloc(MEM_COMMIT, PAGE_READWRITE)` and
//! releases it with `VirtualFree(MEM_RELEASE)` — precisely the allocation the
//! WHP backend needs, with `get_host_address()` handing out the base pointer
//! for `WHvMapGpaRange`. What is unix-only is vm-memory's **default `rawfd`
//! feature** (fd-based `ReadVolatile`/`WriteVolatile`), which upstream rejects
//! on Windows with a `compile_error!`. Nothing in this workspace uses those
//! impls, so the workspace manifest turns `rawfd` off and
//! [`GuestMem`] is one and the same type on both hosts.
//!
//! Keeping the type identical (rather than a hand-rolled `GuestMemoryWindows`)
//! means zero new `unsafe` in this crate and no per-OS divergence for every
//! consumer of guest memory. [`GuestMem`] stays an alias so a future
//! divergence — huge pages on Linux, `MEM_WRITE_WATCH` dirty tracking on
//! Windows — is a one-line change here instead of a refactor.

use vm_memory::{GuestAddress, GuestMemoryMmap};

use crate::VmmError;

/// The concrete guest memory type used across the VMM.
///
/// Backed by an anonymous `mmap` on Linux and by `VirtualAlloc` on Windows;
/// both go through `vm-memory`'s checked `Bytes`/`GuestMemory` APIs, which is
/// what the "guest is untrusted" hard rule requires.
pub type GuestMem = GuestMemoryMmap;

/// Where RAM stops on its way up the 32-bit address space: the PC-convention
/// MMIO hole. A guest bigger than this gets the remainder as a second region
/// at [`HIGH_RAM_START`].
///
/// This is a deliberate copy of `machine_x86::layout::MMIO_HOLE_START` —
/// vmm-core does not depend on the machine crate (the dependency points the
/// other way), so machine-x86 carries the test that keeps the two numbers
/// equal (`layout_agrees_with_vmm_core_memory_split`).
pub const LOW_RAM_END: u64 = 0xc000_0000;

/// Where RAM resumes above the hole: 4 GiB, one past the 32-bit space. The
/// hole between [`LOW_RAM_END`] and here belongs to MMIO (PCI BARs, the
/// virtio-mmio window, LAPIC/IOAPIC, the pflash window) and is never RAM.
pub const HIGH_RAM_START: u64 = 0x1_0000_0000;

/// Allocates guest RAM starting at guest physical 0, split around the 32-bit
/// MMIO hole in the PC convention: one region up to [`LOW_RAM_END`], and —
/// for guests bigger than that — a second region from [`HIGH_RAM_START`] up.
/// The E820 map (machine-x86) tells the guest the same shape.
pub fn create_guest_memory(mem_size_bytes: u64) -> Result<GuestMem, VmmError> {
    if mem_size_bytes == 0 {
        return Err(VmmError::GuestMemory("guest memory size is zero".into()));
    }
    let to_usize = |bytes: u64| {
        usize::try_from(bytes)
            .map_err(|_| VmmError::GuestMemory(format!("guest memory size {bytes} overflows")))
    };
    let mut ranges = vec![(GuestAddress(0), to_usize(mem_size_bytes.min(LOW_RAM_END))?)];
    if mem_size_bytes > LOW_RAM_END {
        ranges.push((
            GuestAddress(HIGH_RAM_START),
            to_usize(mem_size_bytes - LOW_RAM_END)?,
        ));
    }
    GuestMem::from_ranges(&ranges).map_err(|e| VmmError::GuestMemory(e.to_string()))
}

#[cfg(test)]
mod tests {
    use vm_memory::{Address, Bytes, GuestMemory, GuestMemoryRegion, MemoryRegionAddress};

    use super::*;

    const MIB: u64 = 1 << 20;

    #[test]
    fn rejects_zero_size() {
        let err = create_guest_memory(0).unwrap_err();
        assert!(matches!(err, VmmError::GuestMemory(_)), "{err}");
    }

    /// One region at GPA 0 of exactly the requested length — the shape both
    /// `KVM_SET_USER_MEMORY_REGION` and `WHvMapGpaRange` are handed.
    #[test]
    fn single_region_at_zero() {
        let mem = create_guest_memory(16 * MIB).unwrap();
        assert_eq!(mem.num_regions(), 1);
        let region = mem.find_region(GuestAddress(0)).expect("region at 0");
        assert_eq!(region.start_addr().raw_value(), 0);
        assert_eq!(region.len(), 16 * MIB);
        assert_eq!(mem.last_addr().raw_value(), 16 * MIB - 1);
    }

    /// The host pointer handed to the hypervisor must be non-null and page
    /// aligned: `WHvMapGpaRange` rejects unaligned source addresses outright,
    /// and KVM rejects them for the same reason.
    #[test]
    fn host_address_is_page_aligned() {
        let mem = create_guest_memory(4 * MIB).unwrap();
        let region = mem.find_region(GuestAddress(0)).expect("region at 0");
        let host = region.get_host_address(MemoryRegionAddress(0)).unwrap();
        assert!(!host.is_null());
        assert_eq!(
            host as usize % 0x1000,
            0,
            "host base {host:p} not page aligned"
        );
        // And the whole region must be addressable as one slice.
        assert_eq!(
            region
                .get_slice(MemoryRegionAddress(0), (4 * MIB) as usize)
                .unwrap()
                .len(),
            (4 * MIB) as usize
        );
    }

    /// Round-trip through the checked APIs, including at the very last byte,
    /// plus a rejected out-of-bounds access.
    #[test]
    fn checked_access_round_trip() {
        let mem = create_guest_memory(2 * MIB).unwrap();
        mem.write_slice(&[0xde, 0xad, 0xbe, 0xef], GuestAddress(0x1000))
            .unwrap();
        let mut buf = [0u8; 4];
        mem.read_slice(&mut buf, GuestAddress(0x1000)).unwrap();
        assert_eq!(buf, [0xde, 0xad, 0xbe, 0xef]);

        // Fresh guest RAM reads back as zeroes (both backends hand out
        // zero-filled pages; the boot path relies on it for page tables).
        let mut zero = [0xffu8; 8];
        mem.read_slice(&mut zero, GuestAddress(0x2000)).unwrap();
        assert_eq!(zero, [0u8; 8]);

        mem.write_obj(0x5au8, GuestAddress(2 * MIB - 1)).unwrap();
        assert_eq!(mem.read_obj::<u8>(GuestAddress(2 * MIB - 1)).unwrap(), 0x5a);
        assert!(mem.write_obj(0u8, GuestAddress(2 * MIB)).is_err());
        assert!(mem.read_slice(&mut buf, GuestAddress(2 * MIB - 2)).is_err());
    }

    /// A guest bigger than the 32-bit MMIO hole gets exactly two regions: low
    /// RAM stopping at the hole, and the remainder at 4 GiB — the same shape
    /// `machine_x86::e820_map` publishes and both hypervisors' register loops
    /// map region by region.
    #[test]
    fn big_guests_split_around_the_mmio_hole() {
        let mem = create_guest_memory(4096 * MIB).unwrap();
        assert_eq!(mem.num_regions(), 2);
        let low = mem.find_region(GuestAddress(0)).expect("low region");
        assert_eq!(low.start_addr().raw_value(), 0);
        assert_eq!(low.len(), LOW_RAM_END);
        let high = mem
            .find_region(GuestAddress(HIGH_RAM_START))
            .expect("high region");
        assert_eq!(high.start_addr().raw_value(), HIGH_RAM_START);
        assert_eq!(high.len(), 4096 * MIB - LOW_RAM_END);
        assert_eq!(
            mem.last_addr().raw_value(),
            HIGH_RAM_START + (4096 * MIB - LOW_RAM_END) - 1
        );
        // The hole itself is not guest memory: a checked access there fails.
        assert!(mem.find_region(GuestAddress(LOW_RAM_END)).is_none());
        assert!(mem
            .write_obj(0u8, GuestAddress(LOW_RAM_END + 0x1000))
            .is_err());
        // But both sides of it are writable through the checked APIs.
        mem.write_obj(0xa5u8, GuestAddress(LOW_RAM_END - 1)).unwrap();
        mem.write_obj(0x5au8, GuestAddress(HIGH_RAM_START)).unwrap();
        assert_eq!(mem.read_obj::<u8>(GuestAddress(HIGH_RAM_START)).unwrap(), 0x5a);
    }

    /// Exactly at the boundary stays a single region — the split must not
    /// manufacture an empty second region.
    #[test]
    fn a_guest_exactly_at_the_hole_is_one_region() {
        let mem = create_guest_memory(LOW_RAM_END).unwrap();
        assert_eq!(mem.num_regions(), 1);
        assert_eq!(mem.last_addr().raw_value(), LOW_RAM_END - 1);
    }

    /// Repeated create/drop must not leak the backing allocation — 64 × 32 MiB
    /// is 2 GiB of churn, which a leaked `VirtualAlloc`/`mmap` would not
    /// survive in a 32-bit-address-space-free process.
    #[test]
    fn create_drop_does_not_leak() {
        for i in 0..64 {
            let mem =
                create_guest_memory(32 * MIB).unwrap_or_else(|e| panic!("iteration {i}: {e}"));
            mem.write_obj(i as u8, GuestAddress(0x1000)).unwrap();
        }
    }
}
