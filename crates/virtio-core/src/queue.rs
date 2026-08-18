//! Transport-side virtqueue configuration (backlog MVP-301/305).
//!
//! [`QueueConfig`] mirrors exactly what the guest can program through the
//! `QUEUE_*` registers: size, ready flag and the three 64-bit ring addresses,
//! each written as two 32-bit halves. It is pure register state — no guest
//! memory is touched until [`QueueConfig::build`] turns it into a real
//! `virtio_queue::Queue` at activation time, which is also where every value
//! the guest supplied is validated.

use thiserror::Error;
use virtio_queue::{Queue, QueueT};
use vm_memory::GuestAddress;

use crate::GuestMem;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum QueueError {
    #[error(
        "queue size {size} is invalid (must be a non-zero power of two, at most the maximum {max_size})"
    )]
    InvalidSize { size: u16, max_size: u16 },

    #[error("queue is not marked ready by the driver")]
    NotReady,

    #[error(
        "queue rings are unset or outside guest memory \
         (desc {desc_table:#x}, driver {driver_area:#x}, device {device_area:#x}, size {size})"
    )]
    RingOutOfBounds {
        desc_table: u64,
        driver_area: u64,
        device_area: u64,
        size: u16,
    },

    #[error("virtqueue rejected the driver's configuration: {0}")]
    Rejected(String),
}

/// Guest-programmable configuration of one virtqueue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueConfig {
    max_size: u16,
    size: u16,
    ready: bool,
    desc_table: u64,
    driver_area: u64,
    device_area: u64,
}

fn set_low(dst: &mut u64, value: u32) {
    *dst = (*dst & 0xffff_ffff_0000_0000) | u64::from(value);
}

fn set_high(dst: &mut u64, value: u32) {
    *dst = (*dst & 0x0000_0000_ffff_ffff) | (u64::from(value) << 32);
}

impl QueueConfig {
    /// Pristine state, as after a device reset: the driver's size defaults to
    /// the maximum, the queue is not ready and the rings are unset.
    pub fn new(max_size: u16) -> Self {
        Self {
            max_size,
            size: max_size,
            ready: false,
            desc_table: 0,
            driver_area: 0,
            device_area: 0,
        }
    }

    /// Returns the queue to its pristine state (device reset, MVP-303).
    pub fn reset(&mut self) {
        *self = Self::new(self.max_size);
    }

    pub fn max_size(&self) -> u16 {
        self.max_size
    }

    pub fn size(&self) -> u16 {
        self.size
    }

    pub fn set_size(&mut self, size: u16) {
        self.size = size;
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }

    pub fn set_ready(&mut self, ready: bool) {
        self.ready = ready;
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

    pub fn set_desc_table_low(&mut self, value: u32) {
        set_low(&mut self.desc_table, value);
    }

    pub fn set_desc_table_high(&mut self, value: u32) {
        set_high(&mut self.desc_table, value);
    }

    pub fn set_driver_area_low(&mut self, value: u32) {
        set_low(&mut self.driver_area, value);
    }

    pub fn set_driver_area_high(&mut self, value: u32) {
        set_high(&mut self.driver_area, value);
    }

    pub fn set_device_area_low(&mut self, value: u32) {
        set_low(&mut self.device_area, value);
    }

    pub fn set_device_area_high(&mut self, value: u32) {
        set_high(&mut self.device_area, value);
    }

    /// Turns guest-programmed register state into a usable virtqueue.
    ///
    /// This is the single place where the guest's queue geometry is validated:
    /// the size must be a non-zero power of two no larger than the advertised
    /// maximum, the queue must be marked ready, no ring may sit at guest
    /// physical 0 (a driver never puts rings over the real-mode IVT — an unset
    /// register reads as 0), and all three rings must fit entirely inside
    /// guest RAM. Anything else is a typed error, and the transport turns it
    /// into `DEVICE_NEEDS_RESET` rather than a failed host allocation.
    pub fn build(&self, mem: &GuestMem) -> Result<Queue, QueueError> {
        if !self.ready {
            return Err(QueueError::NotReady);
        }
        if self.size == 0 || !self.size.is_power_of_two() || self.size > self.max_size {
            return Err(QueueError::InvalidSize {
                size: self.size,
                max_size: self.max_size,
            });
        }
        let out_of_bounds = || QueueError::RingOutOfBounds {
            desc_table: self.desc_table,
            driver_area: self.driver_area,
            device_area: self.device_area,
            size: self.size,
        };
        if self.desc_table == 0 || self.driver_area == 0 || self.device_area == 0 {
            return Err(out_of_bounds());
        }

        let mut queue =
            Queue::new(self.max_size).map_err(|e| QueueError::Rejected(e.to_string()))?;
        queue
            .try_set_size(self.size)
            .map_err(|e| QueueError::Rejected(e.to_string()))?;
        queue
            .try_set_desc_table_address(GuestAddress(self.desc_table))
            .map_err(|e| QueueError::Rejected(e.to_string()))?;
        queue
            .try_set_avail_ring_address(GuestAddress(self.driver_area))
            .map_err(|e| QueueError::Rejected(e.to_string()))?;
        queue
            .try_set_used_ring_address(GuestAddress(self.device_area))
            .map_err(|e| QueueError::Rejected(e.to_string()))?;
        queue.set_ready(true);

        // `is_valid` re-checks that every ring fits in guest RAM at the size
        // the driver chose. Without this a guest could point the used ring
        // just past the end of RAM and have the device write there.
        if !queue.is_valid(mem) {
            return Err(out_of_bounds());
        }
        Ok(queue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    fn ready_config(ring: &testing::SplitRing) -> QueueConfig {
        ring.queue_config(256)
    }

    #[test]
    fn address_halves_compose_a_64_bit_value() {
        let mut cfg = QueueConfig::new(256);
        cfg.set_desc_table_low(0x1234_5678);
        cfg.set_desc_table_high(0x9abc_def0);
        assert_eq!(cfg.desc_table(), 0x9abc_def0_1234_5678);
        // Rewriting one half must leave the other alone.
        cfg.set_desc_table_low(0);
        assert_eq!(cfg.desc_table(), 0x9abc_def0_0000_0000);
        cfg.set_desc_table_high(0);
        assert_eq!(cfg.desc_table(), 0);
    }

    #[test]
    fn reset_restores_pristine_state() {
        let mut cfg = QueueConfig::new(256);
        cfg.set_size(16);
        cfg.set_ready(true);
        cfg.set_desc_table_low(0x1000);
        cfg.set_driver_area_low(0x2000);
        cfg.set_device_area_low(0x3000);
        cfg.reset();
        assert_eq!(cfg, QueueConfig::new(256));
        assert_eq!(cfg.size(), 256);
        assert!(!cfg.is_ready());
    }

    #[test]
    fn builds_a_valid_queue() {
        let mem = testing::guest_memory(0x2_0000);
        let ring = testing::SplitRing::layout(0x1000, 16);
        let queue = ready_config(&ring).build(&mem).expect("valid geometry");
        assert_eq!(queue.size(), 16);
        assert!(queue.ready());
    }

    #[test]
    fn not_ready_is_rejected() {
        let mem = testing::guest_memory(0x2_0000);
        let ring = testing::SplitRing::layout(0x1000, 16);
        let mut cfg = ready_config(&ring);
        cfg.set_ready(false);
        assert_eq!(cfg.build(&mem), Err(QueueError::NotReady));
    }

    #[test]
    fn non_power_of_two_size_is_rejected() {
        let mem = testing::guest_memory(0x2_0000);
        let ring = testing::SplitRing::layout(0x1000, 16);
        for bad in [0u16, 3, 17, 255] {
            let mut cfg = ready_config(&ring);
            cfg.set_size(bad);
            assert_eq!(
                cfg.build(&mem),
                Err(QueueError::InvalidSize {
                    size: bad,
                    max_size: 256
                }),
                "size {bad} must be rejected"
            );
        }
    }

    #[test]
    fn size_above_maximum_is_rejected() {
        let mem = testing::guest_memory(0x2_0000);
        let ring = testing::SplitRing::layout(0x1000, 16);
        let mut cfg = ring.queue_config(16);
        cfg.set_size(32);
        assert_eq!(
            cfg.build(&mem),
            Err(QueueError::InvalidSize {
                size: 32,
                max_size: 16
            })
        );
    }

    #[test]
    fn unset_rings_are_rejected() {
        let mem = testing::guest_memory(0x2_0000);
        let mut cfg = QueueConfig::new(256);
        cfg.set_size(16);
        cfg.set_ready(true);
        assert!(matches!(
            cfg.build(&mem),
            Err(QueueError::RingOutOfBounds { .. })
        ));
    }

    #[test]
    fn rings_outside_guest_memory_are_rejected() {
        let mem = testing::guest_memory(0x2_0000);
        let ring = testing::SplitRing::layout(0x1000, 16);

        for mutate in [
            (|c: &mut QueueConfig| c.set_desc_table_low(0x1f_fff0)) as fn(&mut QueueConfig),
            |c: &mut QueueConfig| c.set_driver_area_low(0x1f_fff0),
            |c: &mut QueueConfig| c.set_device_area_low(0x1f_fff0),
            |c: &mut QueueConfig| c.set_desc_table_high(1),
        ] {
            let mut cfg = ready_config(&ring);
            mutate(&mut cfg);
            assert!(
                matches!(cfg.build(&mem), Err(QueueError::RingOutOfBounds { .. })),
                "out-of-bounds ring must be rejected: {cfg:?}"
            );
        }
    }

    #[test]
    fn misaligned_descriptor_table_is_rejected() {
        let mem = testing::guest_memory(0x2_0000);
        let ring = testing::SplitRing::layout(0x1000, 16);
        let mut cfg = ready_config(&ring);
        cfg.set_desc_table_low(0x1001);
        assert!(matches!(cfg.build(&mem), Err(QueueError::Rejected(_))));
    }
}
