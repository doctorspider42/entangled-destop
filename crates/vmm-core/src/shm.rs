//! Host memory exposed to the guest that is **not** guest RAM (EPIC 20,
//! VEN-2001).
//!
//! Everything else this VMM shows a guest is either guest RAM (allocated once
//! in [`crate::memory`], mapped once, never moved) or a device register
//! decoded by an MMIO exit. A virtio shared-memory region is neither: it is a
//! window of ordinary host pages that the guest maps with its own page tables
//! and touches at full speed, with no exit and no host code in the path. That
//! is the whole point — a Venus guest puts its Vulkan command ring and its
//! `VkDeviceMemory` there — and it is also why this module states its
//! ownership rules out loud.
//!
//! # Who owns the pages, and how they are freed
//!
//! [`HostShmRegion`] owns one anonymous host allocation — `mmap` on Linux,
//! `VirtualAlloc` on Windows, both through `vm_memory::MmapRegion`, so there
//! is no new `unsafe` here and no second guest-memory type (CLAUDE.md's rule).
//! [`SharedWindow`] pairs that allocation with the hypervisor mapping and owns
//! **both**: dropping it unmaps the range from the guest first and releases
//! the pages second, in that order, because the reverse would leave a
//! hypervisor slot pointing at freed host memory for as long as the VM lives.
//!
//! A window can be [`place`](SharedWindow::place)d, moved and
//! [`unplace`](SharedWindow::unplace)d while the VM runs, which is not
//! optional: the guest's firmware reassigns PCI BARs during enumeration
//! (ADR-0003), so the window's guest-physical address is decided by the guest,
//! not by us.
//!
//! # What the guest can and cannot do with it
//!
//! * **Read and write, at memory speed.** That is the feature.
//! * **Not execute.** WHP is told `Read | Write` and nothing else, so an
//!   instruction fetch from the window faults. KVM has no per-slot execute
//!   permission — a memory slot is whatever the guest's own page tables make
//!   it — so on that host the guest could, in principle, execute from a window
//!   it has mapped executable. It gains nothing by doing so: the pages are its
//!   own writable memory either way, exactly like guest RAM.
//! * **Never reach past the window.** The size handed to the hypervisor is the
//!   size of the allocation; a guest access one byte past the end is an
//!   ordinary unmapped-GPA exit.
//! * **Never see another VM's bytes.** The allocation is fresh, and both
//!   backends hand out zeroed pages; on top of that the device zero-fills every
//!   span it hands to a guest blob (`virtio_gpu::blob`).
//!
//! # Why the hypervisor half is a trait
//!
//! ADR-0002: `machine-x86` may not name a `kvm_bindings` or `WHV_*` type. The
//! two backends implement [`GpaMapper`] — four lines each, one call apiece —
//! and everything above the seam moves a window by asking a trait.

use std::sync::{Arc, Mutex};

use vm_memory::{MmapRegion, VolatileMemory};

use crate::hv::HvError;
use crate::VmmError;

/// Page size every shared-memory window is a multiple of, and the alignment
/// both hypervisors require of a mapped range.
pub const SHM_PAGE_SIZE: u64 = 4096;

/// Largest window this VMM will allocate, as a sanity bound on a host
/// configuration value: 4 GiB. A window is committed host memory, so a typo in
/// a renderer's `host_visible_bytes` should be a typed error and not an
/// out-of-memory kill.
pub const MAX_SHM_WINDOW_BYTES: u64 = 4 << 30;

/// One anonymous host allocation destined to be shown to a guest.
///
/// Deliberately *not* a `GuestMemoryMmap` region: this is not guest RAM, it
/// must never appear in the E820 or PVH memory map, and no device may reach it
/// through the `GuestMem` handle. Keeping it a separate type is what makes
/// that impossible rather than merely discouraged.
#[derive(Debug)]
pub struct HostShmRegion {
    mapping: MmapRegion<()>,
    len: u64,
}

impl HostShmRegion {
    /// Allocates `len` bytes of zeroed host memory.
    ///
    /// `len` must be non-zero, a whole number of [`SHM_PAGE_SIZE`] pages and
    /// at most [`MAX_SHM_WINDOW_BYTES`]. All three are host configuration
    /// errors — nothing a guest can influence — so they are checked once, here.
    pub fn new(len: u64) -> Result<Self, VmmError> {
        if len == 0 || len % SHM_PAGE_SIZE != 0 {
            return Err(VmmError::GuestMemory(format!(
                "a shared-memory window must be a non-zero multiple of {SHM_PAGE_SIZE} bytes \
                 (got {len})"
            )));
        }
        if len > MAX_SHM_WINDOW_BYTES {
            return Err(VmmError::GuestMemory(format!(
                "a shared-memory window of {len} bytes exceeds the {MAX_SHM_WINDOW_BYTES}-byte \
                 maximum"
            )));
        }
        let size = usize::try_from(len)
            .map_err(|_| VmmError::GuestMemory(format!("window of {len} bytes overflows usize")))?;
        let mapping = MmapRegion::<()>::new(size).map_err(|e| {
            VmmError::GuestMemory(format!("cannot allocate a {len}-byte shared-memory window: {e}"))
        })?;
        Ok(Self { mapping, len })
    }

    /// Length of the window in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Never true: a zero-length region is refused at construction, because a
    /// zero-length shared-memory region is exactly what makes Linux'
    /// `virtio_gpu` fail its probe (see `virtio_core::ShmRegion`).
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Host virtual address of the first byte, for the hypervisor call.
    ///
    /// Only [`GpaMapper`] implementations have any business with this: it is a
    /// raw address whose only safe use is to be handed straight back to the
    /// hypervisor together with [`len`](Self::len).
    pub fn host_addr(&self) -> u64 {
        self.mapping.as_ptr() as u64
    }

    /// Copies `buf.len()` bytes out of the window at `offset`.
    ///
    /// Goes through `vm-memory`'s volatile slices, because the guest may be
    /// writing the same bytes at the same time: this is shared memory, and a
    /// non-volatile read of it would be undefined behaviour rather than merely
    /// racy. Callers get whatever the guest last wrote, torn or not, which is
    /// the only honest contract for a window like this.
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), ShmAccessError> {
        let slice = self.slice(offset, buf.len() as u64)?;
        slice.copy_to(buf);
        Ok(())
    }

    /// Copies `data` into the window at `offset`.
    pub fn write(&self, offset: u64, data: &[u8]) -> Result<(), ShmAccessError> {
        let slice = self.slice(offset, data.len() as u64)?;
        slice.copy_from(data);
        Ok(())
    }

    /// Fills `[offset, offset + len)` with `byte`, in bounded chunks so a
    /// gigabyte-sized fill does not allocate a gigabyte-sized source.
    pub fn fill(&self, offset: u64, len: u64, byte: u8) -> Result<(), ShmAccessError> {
        // Validate the whole span before writing any of it: a partially
        // completed fill would leave stale host bytes inside a region the
        // caller believes it has cleared.
        self.slice(offset, len)?;
        const CHUNK: usize = 64 * 1024;
        let chunk = vec![byte; CHUNK.min(usize::try_from(len).unwrap_or(CHUNK))];
        let mut done = 0u64;
        while done < len {
            let take = usize::try_from(len - done).unwrap_or(CHUNK).min(chunk.len());
            self.write(offset + done, &chunk[..take])?;
            done += take as u64;
        }
        Ok(())
    }

    fn slice(&self, offset: u64, len: u64) -> Result<vm_memory::VolatileSlice<'_, ()>, ShmAccessError> {
        let end = offset
            .checked_add(len)
            .ok_or(ShmAccessError::OutOfBounds { offset, len, window: self.len })?;
        if end > self.len {
            return Err(ShmAccessError::OutOfBounds { offset, len, window: self.len });
        }
        let (offset, len) = (
            usize::try_from(offset).map_err(|_| ShmAccessError::OutOfBounds { offset, len, window: self.len })?,
            usize::try_from(len).map_err(|_| ShmAccessError::OutOfBounds { offset, len, window: self.len })?,
        );
        self.mapping
            .get_slice(offset, len)
            .map_err(|_| ShmAccessError::OutOfBounds {
                offset: offset as u64,
                len: len as u64,
                window: self.len,
            })
    }
}

/// Errors from host-side access to a shared-memory window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ShmAccessError {
    #[error("shared-memory access at {offset:#x}+{len} leaves the {window}-byte window")]
    OutOfBounds { offset: u64, len: u64, window: u64 },
}

/// The hypervisor half of a shared-memory window: putting host pages into the
/// guest's physical address space, and taking them out again.
///
/// One implementation per backend (`Vm` on KVM, `WhpPartition` on Windows),
/// each holding whatever bookkeeping its host needs — a memory-slot number on
/// KVM, nothing at all on WHP. Everything above this trait moves a window
/// without naming a hypervisor type, which is ADR-0002's rule.
pub trait GpaMapper: Send + Sync {
    /// Maps `region` at guest physical `gpa`, read/write, not executable where
    /// the host can say so. Replacing an existing mapping of the same window is
    /// the caller's job, not this method's.
    fn map(&self, gpa: u64, region: &HostShmRegion) -> Result<(), HvError>;

    /// Removes a mapping of `len` bytes at `gpa` previously made by
    /// [`map`](Self::map).
    fn unmap(&self, gpa: u64, len: u64) -> Result<(), HvError>;

    /// A name for logs, so a failure says which backend refused.
    fn backend(&self) -> &'static str;
}

/// A [`GpaMapper`] that maps nothing: host pages with no guest behind them.
///
/// Not a stub for production — it is what makes the whole window path
/// unit-testable on a machine with no hypervisor at all, on either host, which
/// is how the bounds and the rebase bookkeeping get tested everywhere rather
/// than only where `/dev/kvm` exists.
#[derive(Debug, Default)]
pub struct UnmappedGpaMapper;

impl GpaMapper for UnmappedGpaMapper {
    fn map(&self, _gpa: u64, _region: &HostShmRegion) -> Result<(), HvError> {
        Ok(())
    }

    fn unmap(&self, _gpa: u64, _len: u64) -> Result<(), HvError> {
        Ok(())
    }

    fn backend(&self) -> &'static str {
        "unmapped"
    }
}

/// Host pages plus their placement in the guest's physical address space.
///
/// The one object that knows both halves, and therefore the one that can keep
/// them consistent: it never has two mappings live at once, it unmaps before
/// it frees, and it refuses to place a window at an address a hypervisor
/// cannot map.
pub struct SharedWindow {
    region: HostShmRegion,
    mapper: Arc<dyn GpaMapper>,
    /// Guest physical address the window is currently mapped at, if any.
    /// Behind a `Mutex` because a BAR write on any vCPU thread can move it.
    placed: Mutex<Option<u64>>,
}

impl std::fmt::Debug for SharedWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedWindow")
            .field("len", &self.region.len())
            .field("backend", &self.mapper.backend())
            .field("placed_at", &self.placed_at())
            .finish()
    }
}

impl SharedWindow {
    /// A window of `len` bytes, allocated but not yet placed.
    ///
    /// Unplaced is the correct initial state on both hosts and in both boot
    /// modes: a PCI function decodes nothing until its driver sets the
    /// memory-space-enable bit, so a window mapped before that would be
    /// reachable through an address the guest has not been told about.
    pub fn new(len: u64, mapper: Arc<dyn GpaMapper>) -> Result<Self, VmmError> {
        Ok(Self {
            region: HostShmRegion::new(len)?,
            mapper,
            placed: Mutex::new(None),
        })
    }

    /// Length of the window in bytes.
    pub fn len(&self) -> u64 {
        self.region.len()
    }

    /// Never true — see [`HostShmRegion::is_empty`].
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Where the window is mapped in guest physical memory, if it is.
    pub fn placed_at(&self) -> Option<u64> {
        self.placed.lock().ok().and_then(|p| *p)
    }

    /// Maps the window at `gpa`, moving it if it was somewhere else.
    ///
    /// Idempotent for the address it is already at, so the BAR-reconcile sweep
    /// can call it after every configuration write without churning a mapping
    /// the guest is actively using.
    pub fn place(&self, gpa: u64) -> Result<(), HvError> {
        if gpa % SHM_PAGE_SIZE != 0 {
            return Err(HvError::Registers(format!(
                "a shared-memory window cannot be placed at {gpa:#x}: not page aligned"
            )));
        }
        if gpa.checked_add(self.len()).is_none() {
            return Err(HvError::Registers(format!(
                "a shared-memory window of {} bytes at {gpa:#x} runs off the address space",
                self.len()
            )));
        }
        let mut placed = self
            .placed
            .lock()
            .map_err(|_| HvError::Registers("shared-memory placement lock poisoned".into()))?;
        if *placed == Some(gpa) {
            return Ok(());
        }
        // Old mapping first: leaving two live mappings of the same host pages
        // would let a guest see the window twice and, on KVM, would need a
        // second memory slot we have not reserved.
        if let Some(old) = placed.take() {
            if let Err(error) = self.mapper.unmap(old, self.len()) {
                tracing::warn!(
                    backend = self.mapper.backend(),
                    at = format_args!("{old:#x}"),
                    %error,
                    "could not unmap a shared-memory window before moving it"
                );
            }
        }
        self.mapper.map(gpa, &self.region)?;
        *placed = Some(gpa);
        tracing::debug!(
            backend = self.mapper.backend(),
            at = format_args!("{gpa:#x}"),
            len = self.len(),
            "shared-memory window placed"
        );
        Ok(())
    }

    /// Takes the window out of the guest's address space. A no-op when it is
    /// not placed.
    pub fn unplace(&self) -> Result<(), HvError> {
        let mut placed = self
            .placed
            .lock()
            .map_err(|_| HvError::Registers("shared-memory placement lock poisoned".into()))?;
        let Some(at) = placed.take() else {
            return Ok(());
        };
        self.mapper.unmap(at, self.len())?;
        tracing::debug!(
            backend = self.mapper.backend(),
            at = format_args!("{at:#x}"),
            "shared-memory window unplaced"
        );
        Ok(())
    }

    /// Host-side read out of the window (see [`HostShmRegion::read`]).
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), ShmAccessError> {
        self.region.read(offset, buf)
    }

    /// Host-side write into the window.
    pub fn write(&self, offset: u64, data: &[u8]) -> Result<(), ShmAccessError> {
        self.region.write(offset, data)
    }

    /// Host-side fill, used to clear a span before a guest is allowed to see it.
    pub fn fill(&self, offset: u64, len: u64, byte: u8) -> Result<(), ShmAccessError> {
        self.region.fill(offset, len, byte)
    }
}

impl Drop for SharedWindow {
    /// Unmap before free, always.
    ///
    /// The reverse order leaves the hypervisor holding a slot that points at
    /// host memory this process has returned to the allocator — a guest write
    /// then lands in whatever the host reused those pages for, which is the
    /// worst bug this whole module could have.
    fn drop(&mut self) {
        if let Err(error) = self.unplace() {
            tracing::error!(
                backend = self.mapper.backend(),
                %error,
                "could not unmap a shared-memory window while dropping it; \
                 the hypervisor may still reference freed host pages"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const PAGE: u64 = SHM_PAGE_SIZE;

    #[test]
    fn a_window_must_be_whole_pages_and_bounded() {
        assert!(HostShmRegion::new(0).is_err());
        assert!(HostShmRegion::new(PAGE - 1).is_err());
        assert!(HostShmRegion::new(PAGE + 1).is_err());
        assert!(HostShmRegion::new(MAX_SHM_WINDOW_BYTES + PAGE).is_err());
        let region = HostShmRegion::new(2 * PAGE).expect("two pages");
        assert_eq!(region.len(), 2 * PAGE);
        assert!(!region.is_empty());
        assert_ne!(region.host_addr(), 0);
        assert_eq!(region.host_addr() % PAGE, 0, "hypervisors demand alignment");
    }

    /// Fresh pages read as zero on both backends, and the round trip works at
    /// the very last byte.
    #[test]
    fn host_access_round_trips_and_starts_zeroed() {
        let region = HostShmRegion::new(4 * PAGE).unwrap();
        let mut buf = [0xffu8; 16];
        region.read(0, &mut buf).unwrap();
        assert_eq!(buf, [0u8; 16]);

        region.write(PAGE, b"venus").unwrap();
        let mut back = [0u8; 5];
        region.read(PAGE, &mut back).unwrap();
        assert_eq!(&back, b"venus");

        region.write(4 * PAGE - 1, &[0x5a]).unwrap();
        let mut last = [0u8; 1];
        region.read(4 * PAGE - 1, &mut last).unwrap();
        assert_eq!(last, [0x5a]);
    }

    /// Every host-side access is bounded in `u64` before it becomes a pointer.
    #[test]
    fn host_access_outside_the_window_is_refused() {
        let region = HostShmRegion::new(PAGE).unwrap();
        let mut buf = [0u8; 8];
        assert!(matches!(
            region.read(PAGE - 4, &mut buf),
            Err(ShmAccessError::OutOfBounds { .. })
        ));
        assert!(region.write(PAGE, b"x").is_err());
        assert!(region.read(u64::MAX, &mut buf).is_err(), "no wrap");
        assert!(region.fill(PAGE / 2, PAGE, 0).is_err());
        // …and a refused fill leaves nothing behind.
        let mut zero = [0xffu8; 8];
        region.read(PAGE / 2, &mut zero).unwrap();
        assert_eq!(zero, [0u8; 8]);
    }

    #[test]
    fn fill_covers_exactly_its_span() {
        let region = HostShmRegion::new(3 * PAGE).unwrap();
        region.fill(0, 3 * PAGE, 0xa5).unwrap();
        region.fill(PAGE, PAGE, 0).unwrap();
        let mut probe = [0u8; 3];
        region.read(PAGE - 1, &mut probe).unwrap();
        assert_eq!(probe, [0xa5, 0, 0]);
        region.read(2 * PAGE - 1, &mut probe).unwrap();
        assert_eq!(probe, [0, 0xa5, 0xa5]);
    }

    /// A mapper that counts calls, so placement bookkeeping can be asserted on
    /// a host with no hypervisor.
    #[derive(Default)]
    struct CountingMapper {
        maps: AtomicUsize,
        unmaps: AtomicUsize,
        last_map: Mutex<Option<u64>>,
    }

    impl GpaMapper for CountingMapper {
        fn map(&self, gpa: u64, _region: &HostShmRegion) -> Result<(), HvError> {
            self.maps.fetch_add(1, Ordering::SeqCst);
            *self.last_map.lock().unwrap() = Some(gpa);
            Ok(())
        }
        fn unmap(&self, _gpa: u64, _len: u64) -> Result<(), HvError> {
            self.unmaps.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn backend(&self) -> &'static str {
            "counting"
        }
    }

    #[test]
    fn placement_moves_the_mapping_exactly_once_per_move() {
        let mapper = Arc::new(CountingMapper::default());
        let window = SharedWindow::new(PAGE, mapper.clone()).unwrap();
        assert_eq!(window.placed_at(), None, "a fresh window decodes nothing");

        window.place(0x1_0000_0000).unwrap();
        assert_eq!(window.placed_at(), Some(0x1_0000_0000));
        assert_eq!(mapper.maps.load(Ordering::SeqCst), 1);

        // Idempotent: the reconcile sweep runs after every config write.
        window.place(0x1_0000_0000).unwrap();
        assert_eq!(mapper.maps.load(Ordering::SeqCst), 1);
        assert_eq!(mapper.unmaps.load(Ordering::SeqCst), 0);

        // A move unmaps the old address before mapping the new one.
        window.place(0x2_0000_0000).unwrap();
        assert_eq!(mapper.maps.load(Ordering::SeqCst), 2);
        assert_eq!(mapper.unmaps.load(Ordering::SeqCst), 1);
        assert_eq!(*mapper.last_map.lock().unwrap(), Some(0x2_0000_0000));

        window.unplace().unwrap();
        assert_eq!(window.placed_at(), None);
        assert_eq!(mapper.unmaps.load(Ordering::SeqCst), 2);
        window.unplace().unwrap();
        assert_eq!(mapper.unmaps.load(Ordering::SeqCst), 2, "no double unmap");
    }

    #[test]
    fn a_misaligned_or_wrapping_placement_is_refused() {
        let window = SharedWindow::new(PAGE, Arc::new(UnmappedGpaMapper)).unwrap();
        assert!(window.place(0x1_0000_0800).is_err(), "not page aligned");
        assert!(window.place(u64::MAX - 0x100).is_err(), "wraps");
        assert_eq!(window.placed_at(), None);
    }

    #[test]
    fn dropping_a_placed_window_unmaps_it() {
        let mapper = Arc::new(CountingMapper::default());
        {
            let window = SharedWindow::new(PAGE, mapper.clone()).unwrap();
            window.place(0x4_0000_0000).unwrap();
        }
        assert_eq!(
            mapper.unmaps.load(Ordering::SeqCst),
            1,
            "the hypervisor must not outlive the host pages"
        );
    }

    #[test]
    fn window_host_access_goes_through_the_same_bounds() {
        let window = SharedWindow::new(2 * PAGE, Arc::new(UnmappedGpaMapper)).unwrap();
        window.write(8, b"host").unwrap();
        let mut buf = [0u8; 4];
        window.read(8, &mut buf).unwrap();
        assert_eq!(&buf, b"host");
        assert!(window.write(2 * PAGE, b"x").is_err());
        assert_eq!(window.len(), 2 * PAGE);
        assert!(!window.is_empty());
    }
}
