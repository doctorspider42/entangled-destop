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
        // Bounded here, before the sum reaches `next_power_of_two` below, and
        // not only after it: that method **panics in debug and wraps to zero in
        // release** above 2^63. A region length is not a guest value, but it is
        // not this crate's value either — for an isolated renderer (GPU-012) it
        // is decoded off the helper's pipe, and the helper is the one component
        // this design assumes can misbehave. A refusal is the contract; a panic
        // is not.
        if total > layout::MAX_SHM_BAR_BYTES {
            return Err(ShmLayoutError::TooLarge {
                total,
                max: layout::MAX_SHM_BAR_BYTES,
            });
        }
    }
    // A BAR's size is a power of two — the sizing protocol cannot express
    // anything else — and at least a page, because that is the granularity the
    // guest maps it at. `total` is at most `MAX_SHM_BAR_BYTES` by the loop
    // above, so the rounding cannot overflow.
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

/// How a bus gets host memory for the shared-memory regions its devices
/// declare (EPIC 20, VEN-2001).
///
/// Borrowed for the duration of an attach and never stored, which is what lets
/// the caller hand over a closure over its own `Vm` or `WhpPartition` without
/// either type appearing in the machine crate — ADR-0002's seam, at the one
/// place a machine has to name a hypervisor to get host pages.
///
/// Optional everywhere it appears, and `None` keeps every pre-EPIC-20 caller
/// producing byte-identical registers and byte-identical configuration space:
/// no BAR 2, no shared-memory capability, no `SHM_BASE`, and a device that
/// declared a region told once, in the log, that this machine cannot back it.
pub struct ShmSupport<'a> {
    /// Guest RAM in bytes. Fixes the 64-bit MMIO aperture, because that
    /// aperture begins at the top of RAM ([`layout::pci_mmio64_base`]).
    pub mem_bytes: u64,
    /// Allocates `len` bytes of host memory already tied to this VM's
    /// hypervisor, unplaced.
    ///
    /// The `bool` is [`ShmRegion::host_mapped`]: `false` gives a window of its
    /// own pages, `true` one whose bytes the device's 3D renderer supplies per
    /// blob mapping (VEN-2003). It reaches the hypervisor because the two are
    /// different objects there — a slot pool on KVM against a single slot —
    /// and not because either backend chooses between them.
    #[allow(clippy::type_complexity)]
    pub allocate: &'a dyn Fn(u64, bool) -> Result<Arc<SharedWindow>, vmm_core::VmmError>,
}

/// Allocates and hands over the host memory behind `device`'s declared
/// shared-memory regions, if it declared any and this machine can back them.
///
/// Shared by both transports, because none of it is transport knowledge: the
/// regions come from the device, the size arithmetic is
/// [`plan`], the address comes from the aperture allocator, and the pages come
/// from the hypervisor through [`ShmSupport`]. What the two buses then *do*
/// with the window differs — PCI puts it in a BAR the guest may move, mmio
/// pins it at one address and publishes it in `SHM_BASE` — and that part stays
/// with them.
///
/// Everything about this is "or nothing": a device with no regions, a machine
/// with no [`ShmSupport`], an aperture with no room, a plan that does not fit
/// and a failed allocation all end the same way — no window, and the device
/// refusing the operations that would have needed one. That is phase 1's
/// behaviour, and it is the right failure: an unbacked window advertised to a
/// driver is a guest that faults on its first `mmap`.
pub fn back_regions(
    slot: usize,
    device: &mut dyn virtio_core::VirtioDevice,
    shm: Option<(&ShmSupport<'_>, &mut layout::Mmio64Allocator)>,
) -> Option<Arc<ShmWindow>> {
    let regions = device.shm_regions();
    if regions.is_empty() {
        return None;
    }
    let Some((support, aperture)) = shm else {
        tracing::warn!(
            slot,
            regions = regions.len(),
            "device declares a shared-memory region but this machine cannot back one; \
             the region will not be published"
        );
        return None;
    };
    let (bar_size, placements) = match plan(&regions) {
        Ok(plan) => plan,
        Err(error) => {
            tracing::error!(slot, %error, "cannot lay out the device's shared-memory regions");
            return None;
        }
    };
    let Some(base) = aperture.allocate(bar_size) else {
        tracing::error!(
            slot,
            bar_size,
            "the 64-bit MMIO aperture has no room for this device's shared-memory window"
        );
        return None;
    };
    // One window per device, so one mode per device: a device that declared a
    // renderer-mapped region and an ordinary one at the same time is asking
    // for two windows in one BAR, and there is no sensible half-answer.
    let host_mapped = regions.iter().any(|r| r.host_mapped);
    if host_mapped && !regions.iter().all(|r| r.host_mapped) {
        tracing::error!(
            slot,
            "a device may not mix renderer-mapped and device-backed shared-memory regions"
        );
        return None;
    }
    let pages = match (support.allocate)(bar_size, host_mapped) {
        Ok(pages) => pages,
        Err(error) => {
            tracing::error!(slot, bar_size, %error, "cannot allocate the shared-memory window");
            return None;
        }
    };
    let window = match ShmWindow::new(pages, bar_size, placements, base, support.mem_bytes) {
        Ok(window) => Arc::new(window),
        Err(error) => {
            tracing::error!(slot, %error, "cannot build the shared-memory window");
            return None;
        }
    };
    for region in &regions {
        if let Some(backing) = window.backing_for(region.id) {
            device.set_shm_backing(region.id, backing);
        }
    }
    tracing::info!(
        slot,
        at = format_args!("{base:#x}"),
        bar_size,
        regions = regions.len(),
        "backed the device's shared-memory regions with host memory"
    );
    Some(window)
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

    /// A handle the device can use to read and write region `id`'s host pages.
    ///
    /// Scoped to that region's span inside the BAR, not to the whole BAR: the
    /// device validates guest offsets against the length it *declared*, so a
    /// backing that reached past its region would turn a correct bounds check
    /// into an incorrect one the moment a second region existed.
    pub fn backing_for(&self, id: u8) -> Option<Arc<dyn ShmBacking>> {
        let placement = self.placements.iter().find(|p| p.id == id)?;
        Some(Arc::new(ShmBackingHandle {
            window: Arc::clone(&self.window),
            offset: placement.offset,
            len: placement.len,
        }))
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
    ///
    /// One-shot: the release and the claim happen back to back, which is all a
    /// bus with a single window needs. A sweep over **several** windows must
    /// use [`Self::release_for`] and [`Self::claim`] instead — see the note
    /// there.
    pub fn follow(&self, bar_base: Option<u64>) -> bool {
        match self.release_for(bar_base) {
            Some(base) => self.claim(base),
            None => false,
        }
    }

    /// The release half of [`Self::follow`]: gives up the mapping unless it is
    /// already the right one, and returns the address still to be claimed.
    ///
    /// Split out because a sweep over several windows has to release **all** of
    /// them before it claims **any** — exactly as `crate::notify`'s ioeventfd
    /// sweep does, and for a sharper reason. A firmware that permutes two
    /// functions' BARs asks this machine to map A where B still is; KVM refuses
    /// an overlapping memory slot and WHP fails the `WHvMapGpaRange`, so a
    /// one-pass sweep would leave A **unmapped** with nothing scheduled to
    /// retry it, and the guest's `mmap` of the region would read nothing.
    ///
    /// `None` means there is nothing to claim — either the BAR decodes nothing,
    /// or it decodes somewhere this machine will not follow it — and in both
    /// cases the window is unmapped by the time this returns.
    pub fn release_for(&self, bar_base: Option<u64>) -> Option<u64> {
        let Some(base) = bar_base else {
            self.unplace("the BAR decodes nothing");
            return None;
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
            return None;
        }
        if self.window.placed_at() != Some(base) {
            self.unplace("the guest moved the BAR");
        }
        Some(base)
    }

    /// The claim half of [`Self::follow`]: maps the window at `base`, which
    /// [`Self::release_for`] has already vetted against the aperture.
    /// Idempotent for an address the window already has.
    ///
    /// Returns `true` when the window is mapped afterwards.
    pub fn claim(&self, base: u64) -> bool {
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

/// The device's view of one region: bounded host access, nothing else.
///
/// Every access is checked twice — once here against the region's own span,
/// once inside `SharedWindow` against the allocation — and the first check is
/// the one that matters, because the offset came from the guest.
struct ShmBackingHandle {
    window: Arc<SharedWindow>,
    offset: u64,
    len: u64,
}

impl ShmBackingHandle {
    /// Translates a region-relative span into a window-relative one, refusing
    /// anything that would leave the region. `u64` throughout: `offset` is a
    /// guest-chosen value and `len` follows from a guest-chosen blob size.
    fn at(&self, offset: u64, len: u64) -> Result<u64, ShmAccessError> {
        let out_of_bounds = ShmAccessError {
            offset,
            len,
            window: self.len,
        };
        let end = offset.checked_add(len).ok_or(out_of_bounds)?;
        if end > self.len {
            return Err(out_of_bounds);
        }
        self.offset.checked_add(offset).ok_or(out_of_bounds)
    }
}

impl ShmBacking for ShmBackingHandle {
    fn len(&self) -> u64 {
        self.len
    }

    fn host_mapped(&self) -> bool {
        self.window.is_host_mapped()
    }

    unsafe fn map_host(
        &self,
        offset: u64,
        host_addr: u64,
        len: u64,
    ) -> Result<(), virtio_core::ShmMapError> {
        if !self.window.is_host_mapped() {
            return Err(virtio_core::ShmMapError::Unsupported);
        }
        let at = self
            .at(offset, len)
            .map_err(|e| virtio_core::ShmMapError::Refused(e.to_string()))?;
        // SAFETY: the caller's obligation — `host_addr`..+`len` is live host
        // memory that stays mapped until `unmap_host` — is exactly the one
        // `HostRange::new` asks for, and it is passed straight through to the
        // hypervisor seam. Nothing here dereferences the address.
        let range = unsafe { vmm_core::HostRange::new(host_addr, len) };
        self.window
            .map_host_range(at, range)
            .map_err(|e| virtio_core::ShmMapError::Refused(e.to_string()))
    }

    fn unmap_host(&self, offset: u64) {
        // `self.at(offset, 0)` is *not* the right check, and the `venus_window`
        // fuzzer found out why on its second minute: a zero-length span is
        // "inside" the region at `offset == len` too, which is the *next*
        // region's offset 0 — so region 1 could unmap region 2's mapping and
        // leave the guest reading host memory the renderer had freed. An
        // unmap names a byte, so the byte has to be one of ours.
        if offset >= self.len {
            return;
        }
        // An offset outside the region never named a mapping of ours, so
        // there is nothing to take down and nothing to report.
        if let Ok(at) = self.at(offset, 1) {
            self.window.unmap_host_range(at);
        }
    }

    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), ShmAccessError> {
        let at = self.at(offset, buf.len() as u64)?;
        self.window.read(at, buf).map_err(convert)
    }

    fn write(&self, offset: u64, data: &[u8]) -> Result<(), ShmAccessError> {
        let at = self.at(offset, data.len() as u64)?;
        self.window.write(at, data).map_err(convert)
    }

    fn fill(&self, offset: u64, len: u64, byte: u8) -> Result<(), ShmAccessError> {
        let at = self.at(offset, len)?;
        self.window.fill(at, len, byte).map_err(convert)
    }
}

/// Two crates, one bound, two error types: `virtio-core` may not depend on
/// `vmm-core` (the dependency points the other way), so the same refusal is
/// spelled once in each and translated here.
fn convert(error: vmm_core::shm::ShmAccessError) -> ShmAccessError {
    match error {
        vmm_core::shm::ShmAccessError::OutOfBounds {
            offset,
            len,
            window,
        } => ShmAccessError {
            offset,
            len,
            window,
        },
        // A device touching a renderer-mapped window's own pages is a wiring
        // bug, not a bounds error, and there is no field in the portable type
        // to say so — `window: 0` is the closest honest answer and the log
        // above it names the real reason.
        vmm_core::shm::ShmAccessError::HostMapped { offset, len } => ShmAccessError {
            offset,
            len,
            window: 0,
        },
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
        let shared =
            Arc::new(SharedWindow::new(bar_size, Arc::new(UnmappedGpaMapper)).expect("host pages"));
        let base = layout::Mmio64Allocator::for_guest(GUEST)
            .allocate(bar_size)
            .expect("aperture");
        ShmWindow::new(shared, bar_size, placements, base, GUEST).expect("window")
    }

    #[test]
    fn a_plan_rounds_the_bar_up_to_a_power_of_two() {
        let (size, placements) = plan(&[ShmRegion {
            id: 1,
            len: 3 << 20,
            host_mapped: false,
        }])
        .unwrap();
        assert_eq!(size, 4 << 20, "a BAR size must be a power of two");
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].offset, 0);
        assert_eq!(placements[0].len, 3 << 20);
    }

    #[test]
    fn a_plan_refuses_what_no_bar_could_carry() {
        assert_eq!(
            plan(&[ShmRegion {
                id: 1,
                len: 0,
                host_mapped: false
            }]),
            Err(ShmLayoutError::ZeroLength)
        );
        assert!(matches!(
            plan(&[ShmRegion {
                id: 1,
                len: 4095,
                host_mapped: false
            }]),
            Err(ShmLayoutError::Unaligned { .. })
        ));
        assert!(matches!(
            plan(&[ShmRegion {
                id: 1,
                len: layout::MAX_SHM_BAR_BYTES + 4096,
                host_mapped: false,
            }]),
            Err(ShmLayoutError::TooLarge { .. })
        ));
        // Two regions that each fit but together do not.
        assert!(matches!(
            plan(&[
                ShmRegion {
                    id: 1,
                    len: layout::MAX_SHM_BAR_BYTES,
                    host_mapped: false,
                },
                ShmRegion {
                    id: 2,
                    len: 4096,
                    host_mapped: false
                },
            ]),
            Err(ShmLayoutError::TooLarge { .. })
        ));
        // A length above 2^63 must come back as a refusal. `next_power_of_two`
        // panics on one in a debug build and wraps to zero in a release build,
        // so an unbounded sum reaching it is a crash in tests and a nonsense
        // BAR size in production. The value is page aligned so it gets past the
        // earlier arms and reaches the rounding.
        assert!(matches!(
            plan(&[ShmRegion {
                id: 1,
                len: (1u64 << 63) + 4096,
                host_mapped: false,
            }]),
            Err(ShmLayoutError::TooLarge { .. })
        ));
        // And the same total split across regions, so the bound is on the sum
        // and not only on one entry.
        assert!(matches!(
            plan(&[
                ShmRegion {
                    id: 1,
                    len: 1 << 62,
                    host_mapped: false,
                },
                ShmRegion {
                    id: 2,
                    len: 1 << 62,
                    host_mapped: false,
                },
                ShmRegion {
                    id: 3,
                    len: 1 << 62,
                    host_mapped: false,
                },
            ]),
            Err(ShmLayoutError::TooLarge { .. })
        ));
    }

    #[test]
    fn a_window_follows_its_bar_and_reports_the_region_base() {
        let regions = [ShmRegion {
            id: 1,
            len: 256 * MIB,
            host_mapped: false,
        }];
        let w = window(256 * MIB, &regions);
        assert_eq!(
            w.placed_at(),
            None,
            "unplaced until the driver enables the BAR"
        );
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
        let regions = [ShmRegion {
            id: 1,
            len: 4096,
            host_mapped: false,
        }];
        let w = window(4096, &regions);
        let aperture = layout::pci_mmio64_base(GUEST);

        for evil in [
            0,      // guest RAM at zero
            0x1000, // the boot page tables
            u64::from(layout::LAPIC_ADDR),
            u64::from(layout::IOAPIC_ADDR),
            layout::PFLASH_BASE,
            layout::PCI_MMIO_BASE,
            layout::VIRTIO_MMIO_BASE,
            aperture - 0x1000,             // one page below the aperture
            layout::pci_mmio64_end(GUEST), // one window past the end
            u64::MAX - 0x1000,             // wraps
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
        let regions = [ShmRegion {
            id: 1,
            len: 4096,
            host_mapped: false,
        }];
        let w = window(4096, &regions);
        let last = layout::pci_mmio64_end(GUEST) - 4096;
        assert!(w.follow(Some(last)));
        assert!(!w.follow(Some(last + 4096)));
    }

    #[test]
    fn the_device_backing_is_bounded_by_the_window() {
        let regions = [ShmRegion {
            id: 1,
            len: 8192,
            host_mapped: false,
        }];
        let w = window(8192, &regions);
        let backing = w.backing_for(1).expect("region 1");
        assert!(w.backing_for(2).is_none(), "no such region");
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

    /// A second region's backing must not be able to reach the first one's
    /// bytes — the offset a guest names is region-relative, and the device
    /// bounds it against the length it declared.
    #[test]
    fn a_region_backing_cannot_reach_another_region() {
        let regions = [
            ShmRegion {
                id: 1,
                len: 8192,
                host_mapped: false,
            },
            ShmRegion {
                id: 2,
                len: 4096,
                host_mapped: false,
            },
        ];
        let w = window(16384, &regions);
        let first = w.backing_for(1).expect("region 1");
        let second = w.backing_for(2).expect("region 2");
        assert_eq!(first.len(), 8192);
        assert_eq!(second.len(), 4096);

        first.fill(0, 8192, 0x11).unwrap();
        second.fill(0, 4096, 0x22).unwrap();
        // Region 2 starts where region 1 ends, and neither can see the other.
        let mut probe = [0u8; 1];
        first.read(8191, &mut probe).unwrap();
        assert_eq!(probe[0], 0x11);
        second.read(0, &mut probe).unwrap();
        assert_eq!(probe[0], 0x22);
        assert!(
            first.read(8192, &mut probe).is_err(),
            "region 1 must stop at its own end"
        );
        assert!(second.read(4096, &mut probe).is_err());
        assert!(second.fill(4090, 16, 0).is_err());
        assert!(second.write(u64::MAX, b"x").is_err(), "no wrap");
    }

    /// A mapper that remembers where it was asked to put host memory, so a
    /// test can check the *guest* address a renderer range lands at rather
    /// than only that a call happened.
    #[derive(Default)]
    struct RecordingMapper {
        live: std::sync::Mutex<std::collections::BTreeMap<u64, (u64, u64)>>,
    }

    impl vmm_core::shm::GpaMapper for RecordingMapper {
        fn map_range(
            &self,
            gpa: u64,
            range: vmm_core::shm::HostRange,
        ) -> Result<(), vmm_core::hv::HvError> {
            self.live
                .lock()
                .expect("mapper")
                .insert(gpa, (range.addr(), range.len()));
            Ok(())
        }
        fn unmap_range(&self, gpa: u64, _len: u64) -> Result<(), vmm_core::hv::HvError> {
            self.live.lock().expect("mapper").remove(&gpa);
            Ok(())
        }
        fn backend(&self) -> &'static str {
            "recording"
        }
    }

    /// The Venus mode end to end at this layer (VEN-2003): the window's own
    /// pages never reach the guest, the renderer's do, and each region's
    /// offsets are its own.
    #[test]
    fn a_renderer_mapped_window_places_renderer_pages_at_the_regions_offset() {
        let regions = [
            ShmRegion {
                id: 1,
                len: 8192,
                host_mapped: true,
            },
            ShmRegion {
                id: 2,
                len: 4096,
                host_mapped: true,
            },
        ];
        let (bar_size, placements) = plan(&regions).expect("plan");
        let mapper = Arc::new(RecordingMapper::default());
        let shared = Arc::new(
            vmm_core::shm::SharedWindow::new_host_mapped(bar_size, mapper.clone())
                .expect("host-mapped window"),
        );
        let base = layout::Mmio64Allocator::for_guest(GUEST)
            .allocate(bar_size)
            .expect("aperture");
        let w = ShmWindow::new(shared, bar_size, placements, base, GUEST).expect("window");

        let first = w.backing_for(1).expect("region 1");
        let second = w.backing_for(2).expect("region 2");
        assert!(first.host_mapped() && second.host_mapped());
        // The device must not be able to write bytes it thinks a guest reads.
        assert!(first.write(0, b"x").is_err());
        assert!(first.fill(0, 4096, 0).is_err());
        let mut buf = [0u8; 4];
        assert!(first.read(0, &mut buf).is_err());

        // Enabling the BAR maps nothing at all: there is no renderer memory in
        // the window yet, and the window's own pages are not the guest's.
        assert!(w.follow(Some(base)));
        assert!(mapper.live.lock().expect("mapper").is_empty());

        // Two pages of stand-in "renderer" memory.
        let pages = vmm_core::shm::HostShmRegion::new(8192).expect("renderer pages");
        let addr = pages.range().addr();
        // SAFETY: `pages` is alive for the whole test and is exactly 8192
        // bytes; both sub-ranges below lie inside it.
        let first_page = unsafe { vmm_core::shm::HostRange::new(addr, 4096) };
        // SAFETY: as above, the second page of the same live allocation.
        let second_page = unsafe { vmm_core::shm::HostRange::new(addr + 4096, 4096) };

        // SAFETY: both ranges are inside `pages`, which outlives every mapping
        // made here (they are all torn down before it drops).
        unsafe {
            first
                .map_host(4096, first_page.addr(), 4096)
                .expect("inside region 1");
            second
                .map_host(0, second_page.addr(), 4096)
                .expect("inside region 2");
            // Region 2 is 4096 long; its offset 4096 is region 1's business.
            assert!(second.map_host(4096, first_page.addr(), 4096).is_err());
            assert!(second.map_host(u64::MAX, first_page.addr(), 4096).is_err());
        }

        // Region 1 sits at BAR offset 0 and region 2 at 8192, so the two
        // guest addresses are base+4096 and base+8192 — the region-relative
        // offset translated once, and never mixed up.
        let live = mapper.live.lock().expect("mapper").clone();
        assert_eq!(live.len(), 2);
        assert_eq!(live.get(&(base + 4096)), Some(&(first_page.addr(), 4096)));
        assert_eq!(live.get(&(base + 8192)), Some(&(second_page.addr(), 4096)));

        // A firmware reassignment moves both.
        let moved = base + 64 * MIB;
        assert!(w.follow(Some(moved)));
        let live = mapper.live.lock().expect("mapper").clone();
        assert_eq!(live.len(), 2);
        assert!(live.contains_key(&(moved + 4096)) && live.contains_key(&(moved + 8192)));

        // One region must not be able to unmap another's span. Region 1 is
        // 8192 bytes long, so its offset 8192 is region 2's offset 0 in
        // window terms — and a zero-length bounds check calls that "inside".
        // The `venus_window` fuzzer found exactly this; the consequence is a
        // guest still reading host memory the renderer is about to free.
        first.unmap_host(8192);
        first.unmap_host(u64::MAX);
        assert_eq!(
            mapper.live.lock().expect("mapper").len(),
            2,
            "region 1 reached past its own end and took region 2's mapping down"
        );

        first.unmap_host(4096);
        second.unmap_host(0);
        assert!(mapper.live.lock().expect("mapper").is_empty());
    }

    /// A device-backed window has nowhere to put renderer memory, and says so
    /// rather than mapping it somewhere plausible.
    #[test]
    fn a_device_backed_window_refuses_renderer_memory() {
        let regions = [ShmRegion {
            id: 1,
            len: 4096,
            host_mapped: false,
        }];
        let w = window(4096, &regions);
        let backing = w.backing_for(1).expect("region 1");
        assert!(!backing.host_mapped());
        // SAFETY: the call is refused on the mode before the address is used;
        // the value never reaches a hypervisor.
        let refused = unsafe { backing.map_host(0, 0x1000, 4096) };
        assert!(matches!(
            refused,
            Err(virtio_core::ShmMapError::Unsupported)
        ));
    }

    #[test]
    fn a_window_whose_pages_do_not_match_the_bar_is_refused() {
        let shared =
            Arc::new(SharedWindow::new(4096, Arc::new(UnmappedGpaMapper)).expect("host pages"));
        assert!(ShmWindow::new(shared, 8192, Vec::new(), 0x1_0000_0000, GUEST).is_err());
    }
}
