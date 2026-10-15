//! `virtio-balloon` with free page reporting (2026-10-13): the guest tells the
//! host which of its pages are free, and the host gives the memory back.
//!
//! A guest's memory is the host's from the first time the guest touches it;
//! a browser session that peaked at 6 GiB keeps 6 GiB of host RAM long after
//! the tabs are closed, because nothing tells the host they are free. Linux's
//! *free page reporting* (`CONFIG_PAGE_REPORTING`, on in every distribution
//! kernel since 5.7) does: once free blocks of at least a page block (2 MiB)
//! have stayed free a moment, the balloon driver posts them on the reporting
//! queue, and the device discards them — `madvise(MADV_DONTNEED)` on Linux,
//! `MADV_FREE` on macOS, `DiscardVirtualMemory` on Windows. The guest may use a reported page again
//! at any time; it reads as zeros (Linux) or as whatever the page held
//! (macOS/Windows, where discarded contents are undefined), and a free page's
//! contents are not the guest's concern.
//!
//! Nothing else of the balloon is used: the host never asks the guest to
//! inflate (`num_pages` stays 0), so the inflate and deflate queues are served
//! only to give back whatever buffers a driver posts on them.
//!
//! **Untrusted guest.** A reported range is a descriptor the guest wrote: its
//! address and length are taken to the host only through `vm-memory`'s
//! `get_slice`, which refuses anything outside one guest RAM region, and only
//! whole host pages inside it are discarded. The chain walk is
//! `virtio_core::chain`'s. A range that fails is skipped and counted — the
//! worst a lying guest gets is to keep its own memory.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use virtio_core::chain;
use virtio_core::device::{DeviceError, DeviceResources, DeviceType, VirtioDevice};
use virtio_core::interrupt::Interrupt;
use virtio_core::{GuestMem, MAX_QUEUE_SIZE, VIRTIO_F_VERSION_1};
use virtio_queue::{Queue, QueueT};
use vm_memory::{GuestAddress, GuestMemory};

/// `VIRTIO_BALLOON_F_REPORTING`: the free page reporting queue exists.
pub const VIRTIO_BALLOON_F_REPORTING: u64 = 1 << 5;
/// The host page size discards are aligned to.
pub const PAGE: u64 = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    16384
} else {
    4096
};
/// Chains served per kick on any queue.
pub const CHAINS_PER_NOTIFY: usize = 4 * MAX_QUEUE_SIZE as usize;

const INFLATE: u16 = 0;
const DEFLATE: u16 = 1;
const REPORTING: u16 = 2;
static QUEUE_MAX_SIZES: [u16; 3] = [MAX_QUEUE_SIZE, MAX_QUEUE_SIZE, MAX_QUEUE_SIZE];

/// What the device has done, for the log and for tests.
#[derive(Debug, Default)]
pub struct BalloonStats {
    /// Bytes given back to the host.
    pub reported_bytes: AtomicU64,
    /// Ranges given back.
    pub reported_ranges: AtomicU64,
    /// Ranges refused (outside guest RAM, or the host would not discard).
    pub refused_ranges: AtomicU64,
}

/// The free-page-reporting balloon.
pub struct BalloonDevice {
    stats: Arc<BalloonStats>,
    acked: u64,
    mem: Option<Arc<GuestMem>>,
    queues: Vec<Queue>,
    interrupt: Option<Arc<dyn Interrupt>>,
}

impl Default for BalloonDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl BalloonDevice {
    pub fn new() -> Self {
        Self {
            stats: Arc::new(BalloonStats::default()),
            acked: 0,
            mem: None,
            queues: Vec::new(),
            interrupt: None,
        }
    }

    /// The device's counters, shared.
    pub fn stats(&self) -> Arc<BalloonStats> {
        Arc::clone(&self.stats)
    }

    /// Serves one queue: every chain returned; on the reporting queue, every
    /// device-writable range in it discarded first.
    fn drain(&mut self, index: u16) -> Result<(), DeviceError> {
        let mem = self.mem.clone().ok_or(DeviceError::NotActivated)?;
        let interrupt = self.interrupt.clone().ok_or(DeviceError::NotActivated)?;
        let queue = self
            .queues
            .get_mut(usize::from(index))
            .ok_or(DeviceError::UnknownQueue(index))?;
        let desc_table = queue.desc_table();
        let size = queue.size();
        let mut served = 0usize;
        let mut freed = 0u64;
        while let Some(head) = queue
            .pop_descriptor_chain(Arc::clone(&mem))
            .map(|c| c.head_index())
        {
            if index == REPORTING {
                match chain::walk(&mem, desc_table, size, head) {
                    Ok(segments) => {
                        for seg in segments.iter().filter(|s| s.writable) {
                            match discard(&mem, seg.addr, u64::from(seg.len)) {
                                Some(bytes) => {
                                    freed += bytes;
                                    self.stats.reported_ranges.fetch_add(1, Ordering::Relaxed);
                                }
                                None => {
                                    self.stats.refused_ranges.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(head, %error, "dropping a malformed balloon chain");
                    }
                }
            }
            queue
                .add_used(mem.as_ref(), head, 0)
                .map_err(|e| DeviceError::Queue(e.to_string()))?;
            served += 1;
            if served >= CHAINS_PER_NOTIFY {
                break;
            }
        }
        if freed > 0 {
            let total = self
                .stats
                .reported_bytes
                .fetch_add(freed, Ordering::Relaxed)
                + freed;
            tracing::debug!(freed, total, "guest memory given back to the host");
        }
        if served > 0
            && queue
                .needs_notification(mem.as_ref())
                .map_err(|e| DeviceError::Queue(e.to_string()))?
        {
            interrupt.signal_used_queue(index)?;
        }
        Ok(())
    }
}

/// Discards the whole host pages of guest range `addr..addr + len`; answers
/// the bytes discarded, or `None` if the range is not guest RAM or the host
/// refused.
fn discard(mem: &GuestMem, addr: u64, len: u64) -> Option<u64> {
    let start = addr.checked_add(PAGE - 1)? & !(PAGE - 1);
    let end = addr.checked_add(len)? & !(PAGE - 1);
    if end <= start {
        return Some(0);
    }
    let bytes = usize::try_from(end - start).ok()?;
    // The checked API: one region, bounds-checked, or nothing.
    let slice = mem.get_slice(GuestAddress(start), bytes).ok()?;
    let guard = slice.ptr_guard_mut();
    let ptr = guard.as_ptr();
    // Page-aligned on the host too: regions are page-aligned mappings, and
    // the offset inside them is a multiple of PAGE.
    if (ptr as usize) % PAGE as usize != 0 {
        return None;
    }
    // SAFETY: `get_slice` has bounds-checked `start..end` against one guest
    // RAM region, so `ptr..ptr + bytes` lies inside that region's own host
    // mapping (anonymous `mmap` / `VirtualAlloc(MEM_COMMIT)`), page-aligned at
    // both ends. Discarding it only changes what the pages read as — zeros on
    // Linux, undefined on macOS/Windows — never whether they are mapped, so every
    // other holder of a pointer into the region stays valid. The guest
    // reported these pages free; their contents are nobody's.
    host::discard(ptr, bytes).then_some(end - start)
}

#[cfg(target_os = "linux")]
mod host {
    /// `madvise(MADV_DONTNEED)`: the pages are freed and read back as zeros.
    pub fn discard(ptr: *mut u8, len: usize) -> bool {
        // SAFETY: the caller's contract — a page-aligned range of a private
        // anonymous mapping this process owns.
        unsafe { libc::madvise(ptr.cast(), len, libc::MADV_DONTNEED) == 0 }
    }
}

#[cfg(windows)]
mod host {
    /// `DiscardVirtualMemory`: the physical pages go back to the system and
    /// the range stays committed, its contents undefined until rewritten.
    pub fn discard(ptr: *mut u8, len: usize) -> bool {
        // SAFETY: the caller's contract — a page-aligned range of committed
        // memory this process allocated; the slice is valid for its length.
        let range = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
        // SAFETY: as above; the call only changes the pages' residency.
        unsafe { windows::Win32::System::Memory::DiscardVirtualMemory(range) == 0 }
    }
}

#[cfg(target_os = "macos")]
mod host {
    /// Darwin's lazy discard leaves the mapping accessible; contents may
    /// survive until reclaimed. Reported free pages permit either outcome.
    pub fn discard(ptr: *mut u8, len: usize) -> bool {
        // SAFETY: the caller supplies whole host pages within a checked
        // private anonymous guest RAM mapping. MADV_FREE does not unmap them.
        unsafe { libc::madvise(ptr.cast(), len, libc::MADV_FREE) == 0 }
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod host {
    pub fn discard(_: *mut u8, _: usize) -> bool {
        false
    }
}

impl VirtioDevice for BalloonDevice {
    fn device_type(&self) -> DeviceType {
        DeviceType::Balloon
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUE_MAX_SIZES
    }

    fn device_features(&self) -> u64 {
        VIRTIO_F_VERSION_1 | VIRTIO_BALLOON_F_REPORTING
    }

    /// The reporting queue is the whole point, and the queue count follows
    /// it: a driver without reporting would set up two queues where three are
    /// declared, so it is refused here rather than at activation.
    fn ack_features(&mut self, negotiated: u64) -> bool {
        if negotiated & VIRTIO_F_VERSION_1 == 0
            || negotiated & VIRTIO_BALLOON_F_REPORTING == 0
            || negotiated & !self.device_features() != 0
        {
            return false;
        }
        self.acked = negotiated;
        true
    }

    /// `num_pages` (the host's target, always 0) and `actual`, both u32.
    fn read_config(&self, _offset: u64, data: &mut [u8]) {
        data.fill(0);
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {
        // `actual`: the guest tells the host how far it inflated. Never
        // anything here, since the host never asks.
    }

    fn activate(&mut self, resources: DeviceResources) -> Result<(), DeviceError> {
        if resources.queues.len() != QUEUE_MAX_SIZES.len() {
            return Err(DeviceError::QueueCount {
                expected: QUEUE_MAX_SIZES.len(),
                actual: resources.queues.len(),
            });
        }
        self.queues = resources.queues;
        self.mem = Some(resources.mem);
        self.interrupt = Some(resources.interrupt);
        tracing::info!("virtio-balloon ready (free page reporting)");
        Ok(())
    }

    fn notify(&mut self, queue_index: u16) -> Result<(), DeviceError> {
        match queue_index {
            INFLATE | DEFLATE | REPORTING => self.drain(queue_index),
            other => Err(DeviceError::UnknownQueue(other)),
        }
    }

    /// Every chain is returned inside `notify`, so only positions remain.
    fn queue_positions(&self) -> Vec<virtio_core::QueuePosition> {
        self.queues
            .iter()
            .map(|q| virtio_core::QueuePosition {
                next_avail: q.next_avail(),
                next_used: q.next_used(),
            })
            .collect()
    }

    fn reset(&mut self) {
        self.queues.clear();
        self.mem = None;
        self.interrupt = None;
        self.acked = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::Bytes;

    fn mem() -> Arc<GuestMem> {
        Arc::new(GuestMem::from_ranges(&[(GuestAddress(0), 16 << 20)]).unwrap())
    }

    #[test]
    fn a_reported_range_is_discarded_page_aligned() {
        let mem = mem();
        mem.write_slice(&[0xab; 8192], GuestAddress(1 << 20))
            .unwrap();
        let far_edge = GuestAddress((1 << 20) + 3 * PAGE);
        mem.write_obj(0xcd_u8, far_edge).unwrap();
        // A range with ragged ends gives back only its whole pages.
        let freed = discard(&mem, (1 << 20) + 100, 3 * PAGE).unwrap();
        assert_eq!(freed, 2 * PAGE);
        assert_eq!(
            mem.read_obj::<u8>(GuestAddress(1 << 20)).unwrap(),
            0xab,
            "rounding must preserve the partial first page"
        );
        assert_eq!(
            mem.read_obj::<u8>(far_edge).unwrap(),
            0xcd,
            "rounding must preserve the partial last page"
        );
        if cfg!(target_os = "linux") {
            let mut page = [0u8; 4096];
            mem.read_slice(&mut page, GuestAddress((1 << 20) + PAGE))
                .unwrap();
            assert!(page.iter().all(|b| *b == 0), "a discarded page reads zeros");
        }
    }

    #[test]
    fn a_range_outside_guest_ram_is_refused() {
        let mem = mem();
        assert_eq!(discard(&mem, 15 << 20, 4 << 20), None, "runs off the end");
        assert_eq!(discard(&mem, u64::MAX - 10, 100), None, "overflows");
        assert_eq!(discard(&mem, 0, 10), Some(0), "less than a page: nothing");
    }

    #[test]
    fn a_driver_without_reporting_is_refused() {
        let mut device = BalloonDevice::new();
        assert!(!device.ack_features(VIRTIO_F_VERSION_1));
        assert!(device.ack_features(VIRTIO_F_VERSION_1 | VIRTIO_BALLOON_F_REPORTING));
        let mut config = [0xffu8; 8];
        device.read_config(0, &mut config);
        assert_eq!(config, [0; 8], "the host never asks for pages");
    }
}
