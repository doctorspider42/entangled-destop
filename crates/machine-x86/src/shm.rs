//! Backing a virtio shared-memory region with real host memory (EPIC 20,
//! VEN-2001 phase 2).
//!
//! Phase 1 taught both transports to *answer* for a shared-memory region and
//! taught virtio-gpu to reserve mappings inside one. Nothing allocated the
//! window, so `RESOURCE_MAP_BLOB` could not succeed anywhere. This module is
//! the missing half: it allocates the host pages, decides where in the guest's
//! physical address space they go, follows the BAR when the guest's firmware
//! moves it, and hands the device a bounded accessor for the same bytes.
//!
//! # The one rule
//!
//! **A window is mapped only where the machine itself would put it.** The
//! guest programs the BAR, so the guest chooses the address — and a guest that
//! parks a 256 MiB prefetchable window on top of its own RAM, on top of the
//! LAPIC, or on top of another device is not a configuration to honour. Every
//! placement therefore goes through [`ShmWindow::follow`], which refuses
//! anything outside the 64-bit aperture the machine published in the DSDT
//! ([`layout::pci_mmio64_base`]) and leaves the window unmapped instead. An
//! unmapped window costs the guest a failed `mmap`; a mapped one over its own
//! page tables costs it everything.
//!
//! Note what that check is *not*: it is not a hypervisor-level protection. KVM
//! and WHP would both happily map host pages over guest RAM if asked. It is
//! the machine layer refusing to ask.
//!
//! # Ownership
//!
//! The pages belong to `vmm_core::shm::SharedWindow`, which unmaps before it
//! frees. This module holds an `Arc` on one, the device holds another through
//! [`virtio_core::ShmBacking`], and the last one out frees the memory — after
//! the hypervisor mapping is gone, because that is what `SharedWindow::drop`
//! guarantees.

use std::sync::Arc;

use virtio_core::pci::{self as vpci, ShmPlacement};
use virtio_core::{ShmAccessError, ShmBacking, ShmRegion};
use vmm_core::shm::SharedWindow;

use crate::layout;

/// Errors from laying a device's shared-memory regions out in a BAR.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ShmLayoutError {
    #[error("shared-memory regions total {total} bytes, more than the {max}-byte maximum window")]
    TooLarge { total: u64, max: u64 },

    #[error("a shared-memory region of {len} bytes is not a whole number of {page}-byte pages")]
    Unaligned { len: u64, page: u64 },

    #[error("a shared-memory region must not be zero length")]
    ZeroLength,

    #[error("the {size}-byte shared-memory windows do not fit the 64-bit MMIO aperture")]
    ApertureFull { size: u64 },
}

/// How big a BAR a device's declared regions need, and where each region sits
/// inside it.
///
/// Kept apart from the allocation so the arithmetic — the part a test can
/// exercise on any host without a hypervisor — has no host memory in it.
pub fn plan(regions: &[ShmRegion]) -> Result<(u64, Vec<ShmPlacement>), ShmLayoutError> {
    let page = vmm_core::shm::SHM_PAGE_SIZE;
    let mut total = 0u64;
    for region in regions {
        if region.len == 0 {
            return Err(ShmLayoutError::ZeroLength);
        }
        if region.len % page != 0 {
            return Err(ShmLayoutError::Unaligned {
                len: region.len,
                page,
            });
        }
        total = total
            .checked_add(region.len)
            .ok_or(ShmLayoutError::TooLarge {
                total: u64::MAX,
                max: layout::MAX_SHM_BAR_BYTES,
            })?;
    }
    // A BAR's size is a power of two — the sizing protocol cannot express
    // anything else — and at least a page, because that is the granularity the
    // guest maps it at.
    let bar_size = total.max(page).next_power_of_two();
    if bar_size > layout::MAX_SHM_BAR_BYTES {
        return Err(ShmLayoutError::TooLarge {
            total,
            max: layout::MAX_SHM_BAR_BYTES,
        });
    }
    let placements = vpci::place_shm_regions(regions, bar_size, page).ok_or({
        // Unreachable given the sum above, but a `None` here would otherwise
        // become a silent "publish no capability", which is the one outcome
        // this whole module exists to avoid.
        ShmLayoutError::TooLarge {
            total,
            max: bar_size,
        }
    })?;
    Ok((bar_size, placements))
}

/// One device's shared-memory BAR: the host pages, where they are published
/// inside the BAR, and where the BAR currently decodes.
pub struct ShmWindow {
    placements: Vec<ShmPlacement>,
    bar_size: u64,
    initial_base: u64,
    /// The guest's RAM size, which is what decides the aperture a placement
    /// has to stay inside.
    mem_bytes: u64,
    window: Arc<SharedWindow>,
}

impl std::fmt::Debug for ShmWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShmWindow")
            .field("bar_size", &self.bar_size)
            .field("initial_base", &format_args!("{:#x}", self.initial_base))
            .field("placed_at", &self.window.placed_at())
            .field("regions", &self.placements.len())
            .finish()
    }
}

impl ShmWindow {
    /// Ties an allocated window to a BAR layout and an initial address.
    ///
    /// `window.len()` must be the BAR size: the guest maps the BAR, so any host
    /// pages the BAR covers but the allocation does not would be an
    /// unbacked hole inside a range the guest was told it may touch.
    pub fn new(
        window: Arc<SharedWindow>,
        bar_size: u64,
        placements: Vec<ShmPlacement>,
        initial_base: u64,
        mem_bytes: u64,
    ) -> Result<Self, ShmLayoutError> {
        if window.len() != bar_size {
            return Err(ShmLayoutError::TooLarge {
                total: window.len(),
                max: bar_size,
            });
        }
        Ok(Self {
            placements,
            bar_size,
            initial_base,
            mem_bytes,
            window,
        })
    }

    /// Size of the BAR that carries this window.
    pub fn bar_size(&self) -> u64 {
        self.bar_size
    }

    /// Where the host first programmed the BAR, before any guest touched it.
    pub fn initial_base(&self) -> u64 {
        self.initial_base
    }

    /// Where each region sits inside the BAR, for the capability records.
    pub fn placements(&self) -> &[ShmPlacement] {
        &self.placements
    }

    /// Where the window is mapped in guest physical memory, if it is.
    pub fn placed_at(&self) -> Option<u64> {
        self.window.placed_at()
    }

    /// The guest physical base of region `id`, once the window is placed.
    ///
    /// This is the number the **mmio** transport publishes in `SHM_BASE`, and
    /// the one a snapshot records (ADR-0006): on PCI the driver derives it from
    /// the BAR itself, so nothing has to tell it.
    pub fn region_base(&self, id: u8) -> Option<u64> {
        let at = self.window.placed_at()?;
        let placement = self.placements.iter().find(|p| p.id == id)?;
        at.checked_add(placement.offset)
    }

    /// A handle the device can use to read and write the window's host pages.
    pub fn backing(&self) -> Arc<dyn ShmBacking> {
        Arc::new(ShmBackingHandle {
            window: Arc::clone(&self.window),
        })
    }

    /// Follows the BAR: maps the window at `bar_base`, or unmaps it when the
    /// BAR decodes nothing (`None`).
    ///
    /// Called from the same sweep that reconciles ioeventfd addresses after a
    /// configuration write, and for the same reason — EDK2's `PciBusDxe`
    /// reassigns every BAR during enumeration, so a host object wired to a
    /// BAR-relative address has to move with it or stop pointing at the device
    /// (`crate::notify`). The difference is what "stop pointing" costs: a stale
    /// ioeventfd loses a kick; a stale *memory mapping* leaves host pages
    /// visible at an address the guest has since given to something else.
    ///
    /// Returns `true` when the window is mapped afterwards.
    pub fn follow(&self, bar_base: Option<u64>) -> bool {
        let Some(base) = bar_base else {
            self.unplace("the BAR decodes nothing");
            return false;
        };
        if !self.is_placeable(base) {
            tracing::warn!(
                at = format_args!("{base:#x}"),
                len = self.bar_size,
                aperture = format_args!(
                    "{:#x}..{:#x}",
                    layout::pci_mmio64_base(self.mem_bytes),
                    layout::pci_mmio64_end(self.mem_bytes)
                ),
                "refusing to map a shared-memory window outside the 64-bit MMIO \
                 aperture; the guest moved its BAR somewhere the host will not \
                 follow it"
            );
            self.unplace("the guest moved the BAR out of the aperture");
            return false;
        }
        if self.window.placed_at() == Some(base) {
            return true;
        }
        match self.window.place(base) {
            Ok(()) => {
                tracing::info!(
                    at = format_args!("{base:#x}"),
                    len = self.bar_size,
                    "shared-memory window mapped into the guest"
                );
                true
            }
            Err(error) => {
                tracing::error!(
                    at = format_args!("{base:#x}"),
                    %error,
                    "could not map the shared-memory window; blob mappings will fail"
                );
                false
            }
        }
    }

    /// Whether `base` is somewhere this machine is willing to put the window.
    ///
    /// The whole window must lie inside the 64-bit aperture. Because that
    /// aperture starts at the top of RAM ([`layout::pci_mmio64_base`]), one
    /// containment test rules out every collision that matters at once: guest
    /// RAM low and high, the 32-bit MMIO hole with its PCI BARs and
    /// virtio-mmio slots, the LAPIC, the IOAPIC and the pflash window.
    fn is_placeable(&self, base: u64) -> bool {
        let Some(end) = base.checked_add(self.bar_size) else {
            return false;
        };
        base >= layout::pci_mmio64_base(self.mem_bytes)
            && end <= layout::pci_mmio64_end(self.mem_bytes)
            && base % vmm_core::shm::SHM_PAGE_SIZE == 0
    }

    fn unplace(&self, why: &str) {
        if self.window.placed_at().is_none() {
            return;
        }
        match self.window.unplace() {
            Ok(()) => tracing::info!(why, "shared-memory window unmapped"),
            Err(error) => tracing::error!(
                why,
                %error,
                "could not unmap the shared-memory window; the guest may still reach host pages"
            ),
        }
    }
}

/// The device's view of the window: bounded host access, nothing else.
struct ShmBackingHandle {
    window: Arc<SharedWindow>,
}

impl ShmBacking for ShmBackingHandle {
    fn len(&self) -> u64 {
        self.window.len()
    }

    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), ShmAccessError> {
        self.window.read(offset, buf).map_err(convert)
    }

    fn write(&self, offset: u64, data: &[u8]) -> Result<(), ShmAccessError> {
        self.window.write(offset, data).map_err(convert)
    }

    fn fill(&self, offset: u64, len: u64, byte: u8) -> Result<(), ShmAccessError> {
        self.window.fill(offset, len, byte).map_err(convert)
    }
}

/// Two crates, one bound, two error types: `virtio-core` may not depend on
/// `vmm-core` (the dependency points the other way), so the same refusal is
/// spelled once in each and translated here.
fn convert(error: vmm_core::shm::ShmAccessError) -> ShmAccessError {
    let vmm_core::shm::ShmAccessError::OutOfBounds { offset, len, window } = error;
    ShmAccessError {
        offset,
        len,
        window,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use vmm_core::shm::UnmappedGpaMapper;

    use super::*;

    const MIB: u64 = 1 << 20;
    const GUEST: u64 = 2048 * MIB;

    fn window(bar_size: u64, regions: &[ShmRegion]) -> ShmWindow {
        let (planned, placements) = plan(regions).expect("plan");
        assert_eq!(planned, bar_size);
        let shared = Arc::new(
            SharedWindow::new(bar_size, Arc::new(UnmappedGpaMapper)).expect("host pages"),
        );
        let base = layout::Mmio64Allocator::for_guest(GUEST)
            .allocate(bar_size)
            .expect("aperture");
        ShmWindow::new(shared, bar_size, placements, base, GUEST).expect("window")
    }

    #[test]
    fn a_plan_rounds_the_bar_up_to_a_power_of_two() {
        let (size, placements) = plan(&[ShmRegion { id: 1, len: 3 << 20 }]).unwrap();
        assert_eq!(size, 4 << 20, "a BAR size must be a power of two");
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].offset, 0);
        assert_eq!(placements[0].len, 3 << 20);
    }

    #[test]
    fn a_plan_refuses_what_no_bar_could_carry() {
        assert_eq!(plan(&[ShmRegion { id: 1, len: 0 }]), Err(ShmLayoutError::ZeroLength));
        assert!(matches!(
            plan(&[ShmRegion { id: 1, len: 4095 }]),
            Err(ShmLayoutError::Unaligned { .. })
        ));
        assert!(matches!(
            plan(&[ShmRegion {
                id: 1,
                len: layout::MAX_SHM_BAR_BYTES + 4096
            }]),
            Err(ShmLayoutError::TooLarge { .. })
        ));
        // Two regions that each fit but together do not.
        assert!(matches!(
            plan(&[
                ShmRegion { id: 1, len: layout::MAX_SHM_BAR_BYTES },
                ShmRegion { id: 2, len: 4096 },
            ]),
            Err(ShmLayoutError::TooLarge { .. })
        ));
    }

    #[test]
    fn a_window_follows_its_bar_and_reports_the_region_base() {
        let regions = [ShmRegion { id: 1, len: 256 * MIB }];
        let w = window(256 * MIB, &regions);
        assert_eq!(w.placed_at(), None, "unplaced until the driver enables the BAR");
        assert_eq!(w.region_base(1), None);

        let base = layout::pci_mmio64_base(GUEST);
        assert!(w.follow(Some(base)));
        assert_eq!(w.placed_at(), Some(base));
        assert_eq!(w.region_base(1), Some(base));
        assert_eq!(w.region_base(2), None, "no such region");

        // A firmware reassignment inside the aperture is followed.
        let moved = base + 512 * MIB;
        assert!(w.follow(Some(moved)));
        assert_eq!(w.placed_at(), Some(moved));
        assert_eq!(w.region_base(1), Some(moved));

        // Memory decoding off, or the BAR parked at 0.
        assert!(!w.follow(None));
        assert_eq!(w.placed_at(), None);
    }

    /// The untrusted-guest case: a BAR the guest points at its own RAM, at the
    /// LAPIC, or past the aperture must leave the window unmapped.
    #[test]
    fn a_bar_outside_the_aperture_is_never_mapped() {
        let regions = [ShmRegion { id: 1, len: 4096 }];
        let w = window(4096, &regions);
        let aperture = layout::pci_mmio64_base(GUEST);

        for evil in [
            0,                      // guest RAM at zero
            0x1000,                 // the boot page tables
            u64::from(layout::LAPIC_ADDR),
            u64::from(layout::IOAPIC_ADDR),
            layout::PFLASH_BASE,
            layout::PCI_MMIO_BASE,
            layout::VIRTIO_MMIO_BASE,
            aperture - 0x1000,      // one page below the aperture
            layout::pci_mmio64_end(GUEST), // one window past the end
            u64::MAX - 0x1000,      // wraps
        ] {
            assert!(!w.follow(Some(evil)), "{evil:#x} must not be mapped");
            assert_eq!(w.placed_at(), None, "{evil:#x} left a mapping behind");
        }

        // And a legitimate address still works afterwards, so the refusal is
        // not a latch.
        assert!(w.follow(Some(aperture)));
        assert_eq!(w.placed_at(), Some(aperture));

        // A refused address after a good one takes the mapping *down* rather
        // than leaving the old one live at an address the guest has moved on
        // from.
        assert!(!w.follow(Some(0x1000)));
        assert_eq!(w.placed_at(), None);
    }

    /// The last byte of the aperture is usable; one byte more is not.
    #[test]
    fn the_aperture_bound_is_inclusive_at_the_top() {
        let regions = [ShmRegion { id: 1, len: 4096 }];
        let w = window(4096, &regions);
        let last = layout::pci_mmio64_end(GUEST) - 4096;
        assert!(w.follow(Some(last)));
        assert!(!w.follow(Some(last + 4096)));
    }

    #[test]
    fn the_device_backing_is_bounded_by_the_window() {
        let regions = [ShmRegion { id: 1, len: 8192 }];
        let w = window(8192, &regions);
        let backing = w.backing();
        assert_eq!(backing.len(), 8192);
        assert!(!backing.is_empty());
        backing.write(0, b"host-wrote-this").unwrap();
        let mut buf = [0u8; 15];
        backing.read(0, &mut buf).unwrap();
        assert_eq!(&buf, b"host-wrote-this");
        assert!(backing.write(8192, b"x").is_err());
        assert!(backing.fill(8188, 8, 0).is_err());
        backing.fill(0, 8192, 0xa5).unwrap();
        backing.read(8191, &mut buf[..1]).unwrap();
        assert_eq!(buf[0], 0xa5);
    }

    #[test]
    fn a_window_whose_pages_do_not_match_the_bar_is_refused() {
        let shared =
            Arc::new(SharedWindow::new(4096, Arc::new(UnmappedGpaMapper)).expect("host pages"));
        assert!(ShmWindow::new(shared, 8192, Vec::new(), 0x1_0000_0000, GUEST).is_err());
    }
}
