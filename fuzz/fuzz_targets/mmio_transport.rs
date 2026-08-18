//! Fuzzes the virtio-mmio register interface (backlog MVP-1402, MVP-301/303).
//!
//! Plays an arbitrary sequence of register reads and writes at the transport —
//! any offset, any width, any value, in any order — against a mock device, i.e.
//! exactly what a hostile or simply broken driver can do. The mock device also
//! flips between accepting and refusing activation and notification, so the
//! transport's error handling is on the fuzzed path too.
//!
//! Properties checked after every operation:
//!
//! * no panic anywhere (the whole point: this runs on a guest-controlled path);
//! * the device status word never contains bits outside the spec's set;
//! * a device is never `activated` without both FEATURES_OK and DRIVER_OK;
//! * `INTERRUPT_STATUS` only ever carries the two defined bits;
//! * `reset()` returns the transport to the pristine state, from any state.

#![no_main]

use std::sync::Arc;

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use virtio_core::device::{DeviceError, DeviceResources, DeviceType, VirtioDevice};
use virtio_core::testing::{guest_memory, TestIrqLine};
use virtio_core::{mmio, status, MmioTransport, VIRTIO_F_VERSION_1};

const MEM_SIZE: u64 = 0x2_0000;

/// Every status bit the spec defines; anything else must never be stored.
const KNOWN_STATUS: u32 = status::ACKNOWLEDGE
    | status::DRIVER
    | status::DRIVER_OK
    | status::FEATURES_OK
    | status::DEVICE_NEEDS_RESET
    | status::FAILED;

#[derive(Debug, Arbitrary)]
enum Op {
    /// Register write; `wide` picks an 8-byte access, `narrow` a 1-byte one, to
    /// exercise the width rejection.
    Write {
        offset: u16,
        value: u32,
        wide: bool,
        narrow: bool,
    },
    Read {
        offset: u16,
        wide: bool,
        narrow: bool,
    },
    /// Config-space access, which the transport forwards to the device.
    ConfigWrite { offset: u16, bytes: Vec<u8> },
    ConfigRead { offset: u16, len: u8 },
    /// A kick delivered the way a worker thread does it (MVP-307).
    WorkerNotify { queue: u32 },
    Offload { queue: u16 },
    Restore { queue: u16 },
    Reset,
}

#[derive(Debug, Arbitrary)]
struct Input {
    queue_count: u8,
    veto_features: bool,
    fail_activate: bool,
    fail_notify: bool,
    config_len: u8,
    ops: Vec<Op>,
}

/// The device behind the fuzzed registers: records nothing, refuses what the
/// input tells it to refuse.
struct MockDevice {
    queues: Vec<u16>,
    veto_features: bool,
    fail_activate: bool,
    fail_notify: bool,
    config: Vec<u8>,
}

impl VirtioDevice for MockDevice {
    fn device_type(&self) -> DeviceType {
        DeviceType::Block
    }
    fn queue_max_sizes(&self) -> &[u16] {
        &self.queues
    }
    fn device_features(&self) -> u64 {
        VIRTIO_F_VERSION_1 | 0xff
    }
    fn ack_features(&mut self, _negotiated: u64) -> bool {
        !self.veto_features
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        for (i, byte) in data.iter_mut().enumerate() {
            let at = offset.saturating_add(i as u64);
            *byte = usize::try_from(at)
                .ok()
                .and_then(|at| self.config.get(at))
                .copied()
                .unwrap_or(0);
        }
    }
    fn write_config(&mut self, offset: u64, data: &[u8]) {
        for (i, byte) in data.iter().enumerate() {
            let at = offset.saturating_add(i as u64);
            if let Some(slot) = usize::try_from(at)
                .ok()
                .and_then(|at| self.config.get_mut(at))
            {
                *slot = *byte;
            }
        }
    }
    fn activate(&mut self, _resources: DeviceResources) -> Result<(), DeviceError> {
        if self.fail_activate {
            return Err(DeviceError::Backend("fuzz".into()));
        }
        Ok(())
    }
    fn notify(&mut self, queue_index: u16) -> Result<(), DeviceError> {
        if self.fail_notify {
            return Err(DeviceError::Backend("fuzz".into()));
        }
        if usize::from(queue_index) >= self.queues.len() {
            return Err(DeviceError::UnknownQueue(queue_index));
        }
        Ok(())
    }
    fn reset(&mut self) {}
}

/// Access width for a register operation.
fn width(wide: bool, narrow: bool) -> usize {
    match (wide, narrow) {
        (true, _) => 8,
        (false, true) => 1,
        (false, false) => 4,
    }
}

fn check_invariants(t: &MmioTransport) {
    let status_word = t.status();
    assert_eq!(
        status_word & !KNOWN_STATUS,
        0,
        "status {status_word:#x} carries bits outside the spec"
    );
    if t.is_activated() {
        assert_ne!(
            status_word & status::FEATURES_OK,
            0,
            "activated without FEATURES_OK (status {status_word:#x})"
        );
        assert_ne!(
            status_word & status::DRIVER_OK,
            0,
            "activated without DRIVER_OK (status {status_word:#x})"
        );
    }
    let interrupts = t.interrupt_status();
    assert_eq!(
        interrupts & !(mmio::INT_VRING | mmio::INT_CONFIG),
        0,
        "interrupt status {interrupts:#x} carries undefined bits"
    );
}

fuzz_target!(|input: Input| {
    // One to four queues; the transport rejects zero at construction, which is a
    // host-side contract rather than something a guest can trigger.
    let queue_count = usize::from(input.queue_count % 4) + 1;
    let device = MockDevice {
        queues: vec![16; queue_count],
        veto_features: input.veto_features,
        fail_activate: input.fail_activate,
        fail_notify: input.fail_notify,
        config: vec![0xa5; usize::from(input.config_len)],
    };
    let mem = Arc::new(guest_memory(MEM_SIZE));
    let line = Arc::new(TestIrqLine::default());
    let Ok(mut t) = MmioTransport::new(0, Box::new(device), mem, line) else {
        return;
    };

    for op in input.ops.iter().take(512) {
        match op {
            Op::Write {
                offset,
                value,
                wide,
                narrow,
            } => {
                let mut bytes = [0u8; 8];
                bytes[..4].copy_from_slice(&value.to_le_bytes());
                t.write(u64::from(*offset), &bytes[..width(*wide, *narrow)]);
            }
            Op::Read {
                offset,
                wide,
                narrow,
            } => {
                let mut buf = [0u8; 8];
                let len = width(*wide, *narrow);
                t.read(u64::from(*offset), &mut buf[..len]);
            }
            Op::ConfigWrite { offset, bytes } => {
                let len = bytes.len().min(64);
                t.write(
                    mmio::CONFIG_SPACE.saturating_add(u64::from(*offset)),
                    &bytes[..len],
                );
            }
            Op::ConfigRead { offset, len } => {
                let mut buf = vec![0u8; usize::from(*len)];
                t.read(
                    mmio::CONFIG_SPACE.saturating_add(u64::from(*offset)),
                    &mut buf,
                );
            }
            Op::WorkerNotify { queue } => t.queue_notify(*queue),
            Op::Offload { queue } => {
                let accepted = t.offload_queue_notify(*queue);
                assert_eq!(
                    accepted,
                    usize::from(*queue) < queue_count,
                    "offload acceptance must match the queue count"
                );
                assert_eq!(t.is_queue_notify_offloaded(*queue), accepted);
            }
            Op::Restore { queue } => {
                t.restore_queue_notify(*queue);
                assert!(!t.is_queue_notify_offloaded(*queue));
            }
            Op::Reset => t.reset(),
        }
        check_invariants(&t);
    }

    // From wherever the operations left it, a reset must produce the pristine
    // state a fresh driver expects (EPIC 3 acceptance criterion).
    t.reset();
    assert_eq!(t.status(), 0);
    assert!(!t.is_activated());
    assert_eq!(t.interrupt_status(), 0);
    let mut magic = [0u8; 4];
    t.read(mmio::MAGIC_VALUE, &mut magic);
    assert_eq!(u32::from_le_bytes(magic), mmio::MAGIC);
    let mut version = [0u8; 4];
    t.read(mmio::VERSION_REG, &mut version);
    assert_eq!(u32::from_le_bytes(version), mmio::VERSION);
});
