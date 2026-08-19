//! Interrupt injection into WHP's emulated local APIC, and the gate a halted
//! vCPU waits on (backlog WHP-1703).
//!
//! # The division of labour
//!
//! WHP emulates each vCPU's **local** APIC — LVT, timer, IRR/ISR, EOI — and
//! nothing above it. So the machine's IOAPIC lives in userspace
//! (`machine_x86::irqchip::ioapic`) and hands each decoded redirection-table
//! entry to [`WhpInterruptDelivery`], which is one `WHvRequestInterrupt` call.
//! That is the whole of the WHP-specific half of interrupt delivery; everything
//! else — pins, masks, vectors, trigger modes — is portable machine code.
//!
//! Local APIC emulation must be switched on before `WHvSetupPartition`
//! ([`crate::whp::WhpOptions::local_apic`]); `WHvRequestInterrupt` fails without
//! it, because there is no APIC to request anything of.
//!
//! # Why a halt gate is needed at all
//!
//! With KVM's in-kernel irqchip, `hlt` is emulated in the kernel: the vCPU
//! blocks inside `KVM_RUN` until an interrupt arrives and userspace never sees
//! it. WHP has no such thing — `hlt` always exits with
//! `WHvRunVpExitReasonX64Halt`, and a Linux guest executes `hlt` on *every* trip
//! through its idle loop. Re-entering immediately would spin a host core at
//! 100%; blocking forever would hang the guest. So the run loop waits here, and
//! every injection wakes it.
//!
//! The wait is a wake-up *hint*, not the interrupt itself: the vector is already
//! in WHP's local APIC by the time [`HaltGate::notify`] runs, so a spurious wake
//! costs one extra `WHvRunVirtualProcessor` round trip and a missed wake costs at
//! most [`HALT_POLL`]. The run loop snapshots the epoch *before* entering the
//! guest, which is what closes the real race — an interrupt requested between
//! `hlt` executing and the exit being observed.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use windows::Win32::System::Hypervisor::{
    WHvRequestInterrupt, WHvX64InterruptDestinationModeLogical,
    WHvX64InterruptDestinationModePhysical, WHvX64InterruptTriggerModeEdge,
    WHvX64InterruptTriggerModeLevel, WHvX64InterruptTypeFixed, WHvX64InterruptTypeLowestPriority,
    WHvX64InterruptTypeNmi, WHV_INTERRUPT_CONTROL,
};

use crate::hv::{
    DestinationMode, HvError, InterruptDelivery, InterruptKind, InterruptRequest, TriggerMode,
};
use crate::whp::partition::Partition;

/// Longest a halted vCPU sleeps without being woken.
///
/// A safety net, not the mechanism: it bounds how long a lost wake-up or a
/// stop request can go unnoticed. Ten milliseconds is well inside the tens of
/// milliseconds `check_timer()` allows and cheap enough to ignore when the guest
/// is genuinely idle.
pub const HALT_POLL: Duration = Duration::from_millis(10);

/// `WHV_INTERRUPT_CONTROL::_bitfield` layout, from WinHvPlatformDefs.h:
/// `Type:8, DestinationMode:4, TriggerMode:4, Reserved:48`. The `windows` crate
/// exposes the bitfield only as an opaque `u64`, so we pack it, the same way
/// `whp::regs` packs the segment attributes.
const CONTROL_TYPE_SHIFT: u64 = 0;
const CONTROL_DESTINATION_MODE_SHIFT: u64 = 8;
const CONTROL_TRIGGER_MODE_SHIFT: u64 = 12;

fn pack_control(interrupt: &InterruptRequest) -> WHV_INTERRUPT_CONTROL {
    let kind = match interrupt.kind {
        InterruptKind::Fixed => WHvX64InterruptTypeFixed,
        InterruptKind::LowestPriority => WHvX64InterruptTypeLowestPriority,
        InterruptKind::Nmi => WHvX64InterruptTypeNmi,
    };
    let destination_mode = match interrupt.destination_mode {
        DestinationMode::Physical => WHvX64InterruptDestinationModePhysical,
        DestinationMode::Logical => WHvX64InterruptDestinationModeLogical,
    };
    let trigger = match interrupt.trigger {
        TriggerMode::Edge => WHvX64InterruptTriggerModeEdge,
        TriggerMode::Level => WHvX64InterruptTriggerModeLevel,
    };
    let bitfield = ((kind.0 as u64 & 0xff) << CONTROL_TYPE_SHIFT)
        | ((destination_mode.0 as u64 & 0xf) << CONTROL_DESTINATION_MODE_SHIFT)
        | ((trigger.0 as u64 & 0xf) << CONTROL_TRIGGER_MODE_SHIFT);
    WHV_INTERRUPT_CONTROL {
        _bitfield: bitfield,
        Destination: interrupt.destination,
        Vector: u32::from(interrupt.vector),
    }
}

/// Lets a halted vCPU thread sleep until something asks for an interrupt.
///
/// An epoch counter rather than a flag: the run loop reads it before entering the
/// guest and compares afterwards, so an injection that lands *during* the run
/// cannot be lost between the `hlt` and the wait.
#[derive(Default)]
pub struct HaltGate {
    epoch: AtomicU64,
    /// Only ever held for the moment it takes to notify or to arm the wait; the
    /// epoch itself is atomic so the fast path (`epoch()`) takes no lock.
    lock: Mutex<()>,
    woken: Condvar,
}

impl HaltGate {
    /// The current epoch. Snapshot this before entering the guest.
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    /// Records that an interrupt was requested and wakes every halted vCPU.
    pub fn notify(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        // The lock is taken so a waiter that has just tested the epoch and is
        // about to block cannot miss this notification.
        let _guard = self.lock.lock();
        self.woken.notify_all();
    }

    /// Blocks until the epoch moves past `seen` or `timeout` elapses. Returns the
    /// epoch it observed, so a caller can chain waits without missing a bump.
    pub fn wait_since(&self, seen: u64, timeout: Duration) -> u64 {
        let Ok(guard) = self.lock.lock() else {
            // A poisoned gate must not wedge a vCPU: fall back to a plain sleep,
            // which still bounds the wait.
            std::thread::sleep(timeout);
            return self.epoch();
        };
        if self.epoch() != seen {
            return self.epoch();
        }
        let (_guard, _timed_out) = self
            .woken
            .wait_timeout(guard, timeout)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.epoch()
    }
}

/// Injects interrupts into the partition's local APICs over
/// `WHvRequestInterrupt`, and wakes halted vCPUs.
///
/// The WHP implementation of [`InterruptDelivery`]; `machine_x86::irqchip` builds
/// its IOAPIC on top of one of these and needs no other WHP knowledge.
pub struct WhpInterruptDelivery {
    partition: Arc<Partition>,
    gate: Arc<HaltGate>,
}

impl WhpInterruptDelivery {
    pub(super) fn new(partition: Arc<Partition>, gate: Arc<HaltGate>) -> Self {
        Self { partition, gate }
    }
}

impl InterruptDelivery for WhpInterruptDelivery {
    fn request(&self, interrupt: &InterruptRequest) -> Result<(), HvError> {
        let control = pack_control(interrupt);
        let size = u32::try_from(size_of::<WHV_INTERRUPT_CONTROL>()).unwrap_or(16);
        // SAFETY: `control` is a live, fully initialised `WHV_INTERRUPT_CONTROL`
        // of exactly `size` bytes which WHP only reads, and the partition is kept
        // alive by our `Arc`. `WHvRequestInterrupt` is documented as callable
        // from any thread, which matters: the callers are vCPU threads inside an
        // exit and the PIT's timer thread.
        let result = unsafe { WHvRequestInterrupt(self.partition.handle(), &control, size) };
        match result {
            Ok(()) => {
                // Wake any halted vCPU only after the vector is actually in the
                // APIC, so a woken vCPU always finds it there.
                self.gate.notify();
                Ok(())
            }
            Err(e) => Err(HvError::Interrupt(format!(
                "WHvRequestInterrupt(vector {:#x}, destination {}) failed: {e} ({:#010x})",
                interrupt.vector,
                interrupt.destination,
                e.code().0 as u32
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bit packing is the one thing here that cannot be checked against a
    /// live WHP call without a partition, so pin it against the header layout.
    #[test]
    fn interrupt_control_bit_positions() {
        let control = pack_control(&InterruptRequest {
            vector: 0x31,
            destination: 3,
            kind: InterruptKind::Fixed,
            destination_mode: DestinationMode::Physical,
            trigger: TriggerMode::Edge,
        });
        assert_eq!(control.Vector, 0x31);
        assert_eq!(control.Destination, 3);
        assert_eq!(control._bitfield, 0, "fixed/physical/edge is all zeroes");

        let control = pack_control(&InterruptRequest {
            vector: 0xff,
            destination: 0xff,
            kind: InterruptKind::Nmi,
            destination_mode: DestinationMode::Logical,
            trigger: TriggerMode::Level,
        });
        assert_eq!(control._bitfield & 0xff, WHvX64InterruptTypeNmi.0 as u64);
        assert_eq!(
            (control._bitfield >> 8) & 0xf,
            1,
            "logical destination mode"
        );
        assert_eq!((control._bitfield >> 12) & 0xf, 1, "level trigger mode");
        assert_eq!(control._bitfield >> 16, 0, "reserved bits must be zero");
    }

    #[test]
    fn lowest_priority_maps_to_its_own_whp_type() {
        let control = pack_control(&InterruptRequest {
            kind: InterruptKind::LowestPriority,
            ..Default::default()
        });
        assert_eq!(
            control._bitfield & 0xff,
            WHvX64InterruptTypeLowestPriority.0 as u64
        );
    }

    /// A notification that arrives before the wait must not be slept through:
    /// that is the race the epoch exists for.
    #[test]
    fn a_notification_before_the_wait_returns_immediately() {
        let gate = HaltGate::default();
        let seen = gate.epoch();
        gate.notify();
        let start = std::time::Instant::now();
        let now = gate.wait_since(seen, Duration::from_secs(5));
        assert!(now > seen);
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "wait_since blocked despite a pending notification"
        );
    }

    #[test]
    fn a_notification_during_the_wait_wakes_the_waiter() {
        let gate = Arc::new(HaltGate::default());
        let seen = gate.epoch();
        let waker = Arc::clone(&gate);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            waker.notify();
        });
        let start = std::time::Instant::now();
        let now = gate.wait_since(seen, Duration::from_secs(5));
        let elapsed = start.elapsed();
        handle.join().unwrap();
        assert!(now > seen);
        assert!(elapsed < Duration::from_secs(2), "woken late: {elapsed:?}");
    }

    /// With nothing to wake it, the wait must still end — a lost wake-up may
    /// cost latency but must never wedge a vCPU.
    #[test]
    fn the_wait_times_out_on_its_own() {
        let gate = HaltGate::default();
        let seen = gate.epoch();
        let start = std::time::Instant::now();
        assert_eq!(gate.wait_since(seen, Duration::from_millis(30)), seen);
        assert!(start.elapsed() >= Duration::from_millis(20));
    }
}
