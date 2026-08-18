//! Test-only helpers for driving virtio devices from the *driver* side.
//!
//! Enabled by the `test-utils` feature (and automatically inside this crate's
//! own tests) so device crates can build split-ring layouts by hand — including
//! deliberately malicious ones — without duplicating the ring maths. This
//! module is not part of the shipped VMM: it may panic on host programming
//! errors, which is why nothing outside `#[cfg(test)]` may call it.

use std::sync::atomic::{AtomicUsize, Ordering};

use vm_memory::{Bytes, GuestAddress};

use crate::chain::DESC_SIZE;
use crate::interrupt::{Interrupt, InterruptError, IrqLine};
use crate::queue::QueueConfig;
use crate::GuestMem;

/// Allocates `size` bytes of anonymous guest memory starting at address 0.
pub fn guest_memory(size: u64) -> GuestMem {
    let len = usize::try_from(size).expect("test memory size fits in usize");
    GuestMem::from_ranges(&[(GuestAddress(0), len)]).expect("test guest memory can be mapped")
}

/// A split virtqueue laid out by hand in guest memory, addressed the way a
/// real driver would program the `QUEUE_*` registers.
///
/// Layout (VirtIO spec 1.2, section 2.7): descriptor table (16 bytes per
/// entry, 16-byte aligned), then the available ring (`flags`, `idx`,
/// `ring[size]`, `used_event`; 2-byte aligned), then the used ring (`flags`,
/// `idx`, `ring[size]` of `{id: u32, len: u32}`, `avail_event`; 4-byte
/// aligned).
#[derive(Debug, Clone, Copy)]
pub struct SplitRing {
    size: u16,
    desc_table: u64,
    driver_area: u64,
    device_area: u64,
}

impl SplitRing {
    /// Lays out a ring of `size` entries starting at `base`, which must be
    /// 16-byte aligned.
    pub fn layout(base: u64, size: u16) -> Self {
        assert_eq!(base % 16, 0, "descriptor tables must be 16-byte aligned");
        assert!(size > 0 && size.is_power_of_two(), "invalid ring size");
        let entries = u64::from(size);
        let driver_area = base + entries * DESC_SIZE;
        let avail_bytes = 4 + 2 * entries + 2;
        let device_area = (driver_area + avail_bytes + 3) & !3;
        Self {
            size,
            desc_table: base,
            driver_area,
            device_area,
        }
    }

    pub fn size(&self) -> u16 {
        self.size
    }

    pub fn desc_table(&self) -> u64 {
        self.desc_table
    }

    pub fn driver_area(&self) -> u64 {
        self.driver_area
    }

    pub fn device_area(&self) -> u64 {
        self.device_area
    }

    /// First guest address after the ring.
    pub fn end(&self) -> u64 {
        self.device_area + 4 + 8 * u64::from(self.size) + 2
    }

    /// The register state a driver would have programmed for this ring.
    pub fn queue_config(&self, max_size: u16) -> QueueConfig {
        let mut cfg = QueueConfig::new(max_size);
        cfg.set_size(self.size);
        cfg.set_desc_table_low(self.desc_table as u32);
        cfg.set_desc_table_high((self.desc_table >> 32) as u32);
        cfg.set_driver_area_low(self.driver_area as u32);
        cfg.set_driver_area_high((self.driver_area >> 32) as u32);
        cfg.set_device_area_low(self.device_area as u32);
        cfg.set_device_area_high((self.device_area >> 32) as u32);
        cfg.set_ready(true);
        cfg
    }

    /// Writes one descriptor table entry. `index` may exceed the ring size on
    /// purpose — malicious-guest tests need that.
    pub fn write_desc(
        &self,
        mem: &GuestMem,
        index: u16,
        addr: u64,
        len: u32,
        flags: u16,
        next: u16,
    ) {
        let at = self.desc_table + u64::from(index) * DESC_SIZE;
        write_u64(mem, at, addr);
        write_u32(mem, at + 8, len);
        write_u16(mem, at + 12, flags);
        write_u16(mem, at + 14, next);
    }

    /// Appends `head` to the available ring and publishes it (`idx += 1`).
    pub fn publish(&self, mem: &GuestMem, head: u16) {
        let idx = self.avail_idx(mem);
        self.set_avail_entry(mem, idx % self.size, head);
        self.set_avail_idx(mem, idx.wrapping_add(1));
    }

    pub fn avail_idx(&self, mem: &GuestMem) -> u16 {
        read_u16(mem, self.driver_area + 2)
    }

    pub fn set_avail_idx(&self, mem: &GuestMem, idx: u16) {
        write_u16(mem, self.driver_area + 2, idx);
    }

    /// Writes the available-ring slot directly; `slot` is taken modulo nothing,
    /// so tests can also write outside the ring.
    pub fn set_avail_entry(&self, mem: &GuestMem, slot: u16, head: u16) {
        write_u16(mem, self.driver_area + 4 + 2 * u64::from(slot), head);
    }

    pub fn used_idx(&self, mem: &GuestMem) -> u16 {
        read_u16(mem, self.device_area + 2)
    }

    pub fn set_used_idx(&self, mem: &GuestMem, idx: u16) {
        write_u16(mem, self.device_area + 2, idx);
    }

    /// Zeroes both ring indices, the way a driver does after a device reset.
    pub fn rewind(&self, mem: &GuestMem) {
        self.set_avail_idx(mem, 0);
        self.set_used_idx(mem, 0);
    }

    /// Returns the `(id, len)` pair of used-ring entry `slot`.
    pub fn used_elem(&self, mem: &GuestMem, slot: u16) -> (u32, u32) {
        let at = self.device_area + 4 + 8 * u64::from(slot);
        (read_u32(mem, at), read_u32(mem, at + 4))
    }
}

fn write_u16(mem: &GuestMem, addr: u64, value: u16) {
    mem.write_slice(&value.to_le_bytes(), GuestAddress(addr))
        .expect("test ring write inside guest memory");
}

fn write_u32(mem: &GuestMem, addr: u64, value: u32) {
    mem.write_slice(&value.to_le_bytes(), GuestAddress(addr))
        .expect("test ring write inside guest memory");
}

fn write_u64(mem: &GuestMem, addr: u64, value: u64) {
    mem.write_slice(&value.to_le_bytes(), GuestAddress(addr))
        .expect("test ring write inside guest memory");
}

fn read_u16(mem: &GuestMem, addr: u64) -> u16 {
    let mut buf = [0u8; 2];
    mem.read_slice(&mut buf, GuestAddress(addr))
        .expect("test ring read inside guest memory");
    u16::from_le_bytes(buf)
}

fn read_u32(mem: &GuestMem, addr: u64) -> u32 {
    let mut buf = [0u8; 4];
    mem.read_slice(&mut buf, GuestAddress(addr))
        .expect("test ring read inside guest memory");
    u32::from_le_bytes(buf)
}

/// An [`IrqLine`] that counts triggers instead of poking KVM.
#[derive(Debug, Default)]
pub struct TestIrqLine {
    count: AtomicUsize,
    fail: bool,
}

impl TestIrqLine {
    /// A line whose `trigger` always fails, to test error propagation.
    pub fn failing() -> Self {
        Self {
            count: AtomicUsize::new(0),
            fail: true,
        }
    }

    pub fn count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }
}

impl IrqLine for TestIrqLine {
    fn trigger(&self) -> Result<(), InterruptError> {
        self.count.fetch_add(1, Ordering::AcqRel);
        if self.fail {
            return Err(InterruptError::Signal("test line always fails".into()));
        }
        Ok(())
    }
}

/// An [`Interrupt`] that records signals, for activating a device directly
/// (without a transport) in device unit tests.
#[derive(Debug, Default)]
pub struct TestInterrupt {
    used: AtomicUsize,
    config: AtomicUsize,
}

impl TestInterrupt {
    pub fn used_signals(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    pub fn config_signals(&self) -> usize {
        self.config.load(Ordering::Acquire)
    }
}

impl Interrupt for TestInterrupt {
    fn signal_used_queue(&self, _queue_index: u16) -> Result<(), InterruptError> {
        self.used.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn signal_config_change(&self) -> Result<(), InterruptError> {
        self.config.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_layout_matches_the_spec_sizes() {
        let ring = SplitRing::layout(0x1000, 16);
        assert_eq!(ring.desc_table(), 0x1000);
        assert_eq!(ring.driver_area(), 0x1000 + 16 * 16);
        // avail = 4 + 2*16 + 2 = 38 bytes, rounded up to a 4-byte boundary.
        assert_eq!(ring.device_area(), 0x1000 + 256 + 40);
        assert_eq!(ring.end(), ring.device_area() + 4 + 8 * 16 + 2);
    }

    #[test]
    fn publish_advances_the_available_index() {
        let mem = guest_memory(0x1_0000);
        let ring = SplitRing::layout(0x1000, 8);
        assert_eq!(ring.avail_idx(&mem), 0);
        ring.publish(&mem, 3);
        ring.publish(&mem, 5);
        assert_eq!(ring.avail_idx(&mem), 2);
    }
}
