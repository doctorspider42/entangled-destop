//! Queue kicks served off the vCPU thread, for machines without ioeventfds.
//!
//! On KVM a queue kick never reaches userspace on the vCPU: an ioeventfd
//! swallows it and a worker serves the queue ([`crate::notify`]). WHP has no
//! working equivalent here. Its doorbell API fails fast inside
//! `winhvplatform.dll` on the second registration on Windows 10 22H2
//! (ADR-0008), so every kick was a full exit **and** the device ran inline on
//! the vCPU that made it: a disk request held the guest's CPU for as long as
//! the host took to read the disk.
//!
//! This keeps the exit and moves the work. The bus's MMIO handler recognises
//! a write to an offloaded queue's notification address, sets that queue's
//! bit and wakes the device's worker, and returns to the guest at once; the
//! worker takes the pause gate (ADR-0005) and then the transport lock and
//! runs the queue. Several kicks before the worker gets there coalesce into
//! one pass, which is what a virtqueue wants anyway.
//!
//! Portable: nothing here is host-specific, so it is tested everywhere.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use virtio_core::{PciTransport, Quiesce};

/// Queues one device can offload: one bit each in the pending mask.
pub const MAX_OFFLOADED_QUEUES: usize = 16;

/// How long the worker sleeps before re-checking its stop flag.
const IDLE_WAIT: Duration = Duration::from_millis(500);

/// What the worker waits on: kicked queues and the stop request.
#[derive(Default)]
struct Pending {
    queues: u16,
    stop: bool,
}

#[derive(Default)]
struct Shared {
    pending: Mutex<Pending>,
    wake: Condvar,
}

/// One device's offloaded kicks: where they land, and the worker serving them.
pub struct KickWorker {
    slot: usize,
    queues: usize,
    /// Bytes between two queues' notification addresses.
    stride: u64,
    /// The notification base kicks are recognised at; `None` while the BAR
    /// decodes nothing and kicks take the synchronous path.
    base: Mutex<Option<u64>>,
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
    stopping: Arc<AtomicBool>,
    quiesce_handle: Arc<Mutex<Arc<Quiesce>>>,
}

impl KickWorker {
    /// Starts the worker for the device in `slot`, recognising kicks at
    /// `base` (queue *n* at `base + n * stride`). `None` for a device with no
    /// queues or when the thread cannot start; the device keeps the
    /// synchronous path.
    pub fn attach(
        slot: usize,
        base: u64,
        stride: u64,
        transport: &Arc<Mutex<PciTransport>>,
    ) -> Option<Self> {
        let queues = transport
            .lock()
            .ok()?
            .num_queues()
            .min(MAX_OFFLOADED_QUEUES);
        if queues == 0 {
            return None;
        }
        let shared = Arc::new(Shared::default());
        let stopping = Arc::new(AtomicBool::new(false));
        let quiesce_handle = Arc::new(Mutex::new(Quiesce::new()));
        let worker = {
            let shared = Arc::clone(&shared);
            let transport = Arc::clone(transport);
            let stopping = Arc::clone(&stopping);
            let quiesce = Arc::clone(&quiesce_handle);
            std::thread::Builder::new()
                .name(format!("virtio-q{slot}"))
                .spawn(move || worker_loop(slot, &shared, &transport, &stopping, &quiesce))
        };
        let worker = match worker {
            Ok(handle) => handle,
            Err(error) => {
                tracing::warn!(slot, %error, "cannot start the queue worker; synchronous kicks");
                return None;
            }
        };
        let kicks = Self {
            slot,
            queues,
            stride,
            base: Mutex::new(None),
            shared,
            worker: Mutex::new(Some(worker)),
            stopping,
            quiesce_handle,
        };
        kicks.rebase(Some(base));
        tracing::info!(
            slot,
            queues,
            base = format_args!("{base:#x}"),
            "queue kicks served by a worker thread"
        );
        Some(kicks)
    }

    /// Recognises kicks at `base` from now on; `None` stops recognising any
    /// (the BAR decodes nothing), so kicks fall through to the transport.
    pub fn rebase(&self, base: Option<u64>) {
        *self.base.lock().unwrap_or_else(PoisonError::into_inner) = base;
    }

    /// Where kicks are recognised now.
    pub fn notify_base(&self) -> Option<u64> {
        *self.base.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A guest write at `addr`: when it is one of this device's queue
    /// kicks, hands it to the worker and returns `true`; otherwise `false`
    /// and the caller serves the write as usual.
    pub fn kick(&self, addr: u64) -> bool {
        let Some(base) = self.notify_base() else {
            return false;
        };
        let Some(offset) = addr.checked_sub(base) else {
            return false;
        };
        if self.stride == 0 || offset % self.stride != 0 {
            return false;
        }
        let Ok(queue) = usize::try_from(offset / self.stride) else {
            return false;
        };
        if queue >= self.queues {
            return false;
        }
        let mut pending = self
            .shared
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        pending.queues |= 1 << queue;
        drop(pending);
        self.shared.wake.notify_one();
        true
    }

    /// Shares the VM's pause gate with the worker (ADR-0005).
    pub fn set_quiesce(&self, quiesce: Arc<Quiesce>) {
        *self
            .quiesce_handle
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = quiesce;
    }

    /// Stops the worker. Idempotent; also run from `Drop`.
    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::Release);
        self.rebase(None);
        self.quiesce_handle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .wake();
        self.shared
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .stop = true;
        self.shared.wake.notify_one();
        let handle = self
            .worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            if handle.join().is_err() {
                tracing::error!(slot = self.slot, "queue worker thread panicked");
            }
        }
    }
}

impl Drop for KickWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker_loop(
    slot: usize,
    shared: &Shared,
    transport: &Mutex<PciTransport>,
    stopping: &AtomicBool,
    quiesce: &Mutex<Arc<Quiesce>>,
) {
    loop {
        let kicked = {
            let mut pending = shared
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            while pending.queues == 0 && !pending.stop {
                pending = shared
                    .wake
                    .wait_timeout(pending, IDLE_WAIT)
                    .map(|(guard, _)| guard)
                    .unwrap_or_else(|poisoned| poisoned.into_inner().0);
            }
            if pending.stop {
                return;
            }
            std::mem::take(&mut pending.queues)
        };
        // The pause gate before the transport lock: a paused VM makes no
        // progress, and parking while holding the lock would deadlock the
        // reset that runs on a quiesced VM (ADR-0005).
        let gate = Arc::clone(&quiesce.lock().unwrap_or_else(PoisonError::into_inner));
        let Some(_pass) = gate.wait_while_paused(|| !stopping.load(Ordering::Acquire)) else {
            return;
        };
        let Ok(mut transport) = transport.lock() else {
            tracing::error!(slot, "transport lock is poisoned; dropping queue kicks");
            continue;
        };
        for queue in 0..MAX_OFFLOADED_QUEUES as u32 {
            if kicked & (1 << queue) != 0 {
                transport.queue_notify(queue);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker(queues: usize) -> KickWorker {
        let shared = Arc::new(Shared::default());
        KickWorker {
            slot: 0,
            queues,
            stride: 4,
            base: Mutex::new(Some(0x1000)),
            shared,
            worker: Mutex::new(None),
            stopping: Arc::new(AtomicBool::new(false)),
            quiesce_handle: Arc::new(Mutex::new(Quiesce::new())),
        }
    }

    fn pending(kicks: &KickWorker) -> u16 {
        kicks.shared.pending.lock().unwrap().queues
    }

    #[test]
    fn a_kick_at_a_queue_address_is_taken_and_marked() {
        let kicks = worker(2);
        assert!(kicks.kick(0x1000));
        assert!(kicks.kick(0x1004));
        assert_eq!(pending(&kicks), 0b11);
    }

    #[test]
    fn anything_else_falls_through() {
        let kicks = worker(2);
        assert!(!kicks.kick(0x0ffc), "below the base");
        assert!(!kicks.kick(0x1002), "between two queues");
        assert!(!kicks.kick(0x1008), "past the last queue");
        assert!(!kicks.kick(u64::MAX));
        kicks.rebase(None);
        assert!(!kicks.kick(0x1000), "nothing is recognised without a base");
        assert_eq!(pending(&kicks), 0);
    }

    #[test]
    fn shutdown_without_a_thread_is_fine_and_idempotent() {
        let kicks = worker(1);
        kicks.shutdown();
        kicks.shutdown();
        assert_eq!(kicks.notify_base(), None);
    }
}
