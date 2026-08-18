//! Guest memory creation (backlog MVP-102).

use vm_memory::{GuestAddress, GuestMemoryMmap};

use crate::VmmError;

/// The concrete guest memory type used across the VMM.
pub type GuestMem = GuestMemoryMmap;

/// Allocates anonymous mmap-backed guest RAM as a single region starting at
/// guest physical 0. MVP guests stay below the 32-bit MMIO hole, so one
/// region suffices; the E820 map (machine-x86) is what tells the guest which
/// parts are usable.
pub fn create_guest_memory(mem_size_bytes: u64) -> Result<GuestMem, VmmError> {
    if mem_size_bytes == 0 {
        return Err(VmmError::GuestMemory("guest memory size is zero".into()));
    }
    GuestMem::from_ranges(&[(GuestAddress(0), mem_size_bytes as usize)])
        .map_err(|e| VmmError::GuestMemory(e.to_string()))
}
