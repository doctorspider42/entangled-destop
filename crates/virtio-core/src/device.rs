//! The transport-agnostic device contract.

use std::sync::Arc;

use thiserror::Error;
use virtio_queue::Queue;

use crate::chain::ChainError;
use crate::interrupt::{Interrupt, InterruptError};
use crate::quiesce::Quiesce;
use crate::GuestMem;

/// VirtIO device ids used by the MVP (VirtIO spec 1.2, section 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum DeviceType {
    Net = 1,
    Block = 2,
    Gpu = 16,
    Input = 18,
    Sound = 25,
}

impl DeviceType {
    /// The value the transport reports in its `DEVICE_ID` register.
    pub const fn id(self) -> u32 {
        self as u32
    }
}

/// Everything a device needs to start serving its queues, handed over by the
/// transport when the driver sets `DRIVER_OK`.
///
/// This is the crux of the trait redesign for EPIC 3: before the transport
/// existed, `activate()` took no arguments and a device had no way to reach a
/// queue, guest memory or the interrupt line. Now the transport — the only
/// component that knows the register layout — validates the guest-programmed
/// geometry, builds real virtqueues and hands them over together with the
/// guest memory handle and an [`Interrupt`]. Nothing mmio-specific crosses the
/// boundary, so virtio-pci can construct the same struct post-MVP.
pub struct DeviceResources {
    /// Guest RAM, shared with the VM. Devices must only access it through the
    /// checked `vm-memory` APIs.
    pub mem: Arc<GuestMem>,

    /// One configured and validated virtqueue per entry of
    /// [`VirtioDevice::queue_max_sizes`], in the same order.
    pub queues: Vec<Queue>,

    /// Used-buffer and config-change notifications.
    pub interrupt: Arc<dyn Interrupt>,

    /// The VM's pause gate (ADR-0005). A device that runs a worker thread of
    /// its own must call [`Quiesce::wait_while_paused`] before it touches guest
    /// memory from that thread, or a paused VM is not actually stopped. A device
    /// that only works inside `notify()` needs nothing: it is already on a
    /// parked vCPU thread, or behind a queue worker that took the gate for it.
    pub quiesce: Arc<Quiesce>,
}

impl std::fmt::Debug for DeviceResources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceResources")
            .field("queues", &self.queues.len())
            .finish_non_exhaustive()
    }
}

/// Errors a device reports to the transport.
///
/// Everything here is recoverable from the host's point of view: the transport
/// logs the error and sets `DEVICE_NEEDS_RESET`. Guest-caused failures of a
/// *single request* never reach this type — devices answer those in-band (a
/// virtio-blk error status byte, for instance), because failing one request
/// must not take the whole device down.
#[derive(Debug, Error)]
pub enum DeviceError {
    #[error("device has no queue with index {0}")]
    UnknownQueue(u16),

    #[error("device is not activated")]
    NotActivated,

    #[error("device expects {expected} queues, transport offered {actual}")]
    QueueCount { expected: usize, actual: usize },

    #[error("descriptor chain rejected: {0}")]
    Chain(#[from] ChainError),

    #[error("guest memory access failed: {0}")]
    Memory(String),

    #[error("virtqueue error: {0}")]
    Queue(String),

    #[error("interrupt delivery failed: {0}")]
    Interrupt(#[from] InterruptError),

    #[error("device backend failed: {0}")]
    Backend(String),
}

/// A queue the driver programmed badly is a device-level failure. Available so
/// devices that build queues themselves can propagate it.
impl From<crate::queue::QueueError> for DeviceError {
    fn from(value: crate::queue::QueueError) -> Self {
        DeviceError::Queue(value.to_string())
    }
}

/// One shared-memory region a device exposes to its driver (VirtIO spec 1.2
/// §4.1.4.7 for PCI, §4.2.2 for MMIO) — backlog VEN-2001.
///
/// A shared-memory region is a window of *host* memory the guest maps
/// directly, which is how virtio-gpu's host-visible blob resources
/// (`RESOURCE_MAP_BLOB`) reach a guest at all: the guest gets an address
/// range, not a copy. It is deliberately a plain `{id, len}` pair here —
/// *where* the window lands is the transport's and the machine layer's
/// business (a BAR offset on PCI, a guest-physical base on MMIO), and a device
/// must never learn either.
///
/// A device that returns none of these keeps the pre-existing, spec-mandated
/// "no such region" behaviour on both transports: MMIO reads all-ones for
/// `SHM_LEN`/`SHM_BASE`, PCI publishes no shared-memory capability. That
/// distinction is load-bearing — Linux' `virtio_gpu` treats a zero-length
/// region at address 0 as *present* and fails its probe on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShmRegion {
    /// `shmid`: the selector the driver uses to ask about this region.
    /// Device-specific (virtio-gpu's host-visible region is 1).
    pub id: u8,
    /// Length of the window in bytes. Never zero for a region that exists.
    pub len: u64,
}

/// A host-side wakeup a device may hold to ask for service from the
/// transport's worker context (ADR-0004 phase 2, real fences).
///
/// Some devices finish work asynchronously on the host — a GPU renderer
/// retiring a fence after the command that created it was already answered
/// pending. The completion must run where every other device call runs (the
/// queue worker's `notify` path), so the device cannot act on it directly
/// from the host thread that observed it; instead it calls [`Self::wake`],
/// and the machine layer arranges for the device's **queue 0** to be
/// notified from its ordinary worker context, exactly as if the guest had
/// kicked it (a spurious queue-0 notify is harmless by construction — every
/// device drains an empty ring as a no-op).
///
/// Portable by design: the trait carries no fd. The Linux machine layer
/// implements it as an eventfd write into the device's queue-notify worker;
/// a host (or notify mode) with no worker simply never installs a waker, and
/// [`VirtioDevice::set_host_waker`]'s default keeps such devices on their
/// synchronous paths.
pub trait HostWaker: Send + Sync {
    /// Requests a queue-0 `notify` from the device's worker context. Must be
    /// cheap, non-blocking and callable from any thread.
    fn wake(&self);
}

/// A [`HostWaker`] that can be handed to a device **before** the host
/// primitive that serves it exists.
///
/// The ordering problem it solves: a device is moved into its transport
/// before the queue-notify worker (which owns the eventfds a wake writes to)
/// is built around that transport. So the machine layer installs one of these
/// while it still holds the device, then fills in the real waker once the
/// worker is up — and a wake that arrives in the gap is remembered and
/// delivered by [`Self::install`] instead of being lost.
///
/// A `DeferredWaker` that is never filled in is inert, which is exactly what
/// a host with no worker thread needs: the device sees a waker it can hold,
/// its wakes go nowhere, and every path that depends on one must therefore
/// keep a synchronous fallback (`Renderer3d::create_fence` returning
/// `Signalled`).
#[derive(Default)]
pub struct DeferredWaker {
    inner: std::sync::Mutex<Option<std::sync::Arc<dyn HostWaker>>>,
    /// A wake that arrived before (or without) an installed waker.
    missed: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for DeferredWaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeferredWaker")
            .field(
                "installed",
                &self.inner.lock().map(|w| w.is_some()).unwrap_or(false),
            )
            .finish()
    }
}

impl DeferredWaker {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::default())
    }

    /// Points this waker at the real one. Any wake that happened before now
    /// is delivered immediately, so a completion can never be dropped just
    /// because it raced the host wiring.
    pub fn install(&self, waker: std::sync::Arc<dyn HostWaker>) {
        let missed = self.missed.swap(false, std::sync::atomic::Ordering::AcqRel);
        match self.inner.lock() {
            Ok(mut slot) => *slot = Some(std::sync::Arc::clone(&waker)),
            Err(_) => {
                tracing::error!("host waker slot is poisoned; wakeups will be dropped");
                return;
            }
        }
        if missed {
            waker.wake();
        }
    }

    /// True once a real waker is behind this one — i.e. a `wake` will reach a
    /// worker thread.
    pub fn is_live(&self) -> bool {
        self.inner.lock().map(|w| w.is_some()).unwrap_or(false)
    }
}

impl HostWaker for DeferredWaker {
    fn wake(&self) {
        // Cloned out from under the lock: `wake` may be called from any
        // thread, and the real waker's own `wake` must not run with our lock
        // held (it writes an eventfd, which can block on a full counter).
        let waker = match self.inner.lock() {
            Ok(slot) => slot.as_ref().map(std::sync::Arc::clone),
            Err(_) => None,
        };
        match waker {
            Some(waker) => waker.wake(),
            None => self
                .missed
                .store(true, std::sync::atomic::Ordering::Release),
        }
    }
}

/// Contract between the transport (`virtio-mmio` today, virtio-pci later)
/// and a device implementation.
///
/// The transport owns register handling, feature/status negotiation state and
/// queue plumbing; the device sees negotiated features, its config space and
/// activation with ready queues. Implementations must treat all queue content
/// as untrusted (see crate docs and [`crate::chain`]).
pub trait VirtioDevice: Send {
    /// Device id advertised to the guest.
    fn device_type(&self) -> DeviceType;

    /// Maximum size of each virtqueue this device exposes, in queue order.
    /// The length of the slice is the number of queues.
    fn queue_max_sizes(&self) -> &[u16];

    /// Number of virtqueues this device exposes.
    fn num_queues(&self) -> u16 {
        u16::try_from(self.queue_max_sizes().len()).unwrap_or(u16::MAX)
    }

    /// Feature bits the device offers (must include
    /// [`crate::VIRTIO_F_VERSION_1`]).
    fn device_features(&self) -> u64;

    /// Called when the driver has written FEATURES_OK; the device records
    /// the negotiated subset. Returns false to veto the negotiation (the
    /// transport then refuses FEATURES_OK per spec).
    fn ack_features(&mut self, negotiated: u64) -> bool;

    /// Reads from the device-specific config space at `offset`. Reads past the
    /// end of the config space must produce zeroes, never a panic.
    fn read_config(&self, offset: u64, data: &mut [u8]);

    /// Writes to the device-specific config space at `offset`. Devices with a
    /// read-only config space ignore this.
    fn write_config(&mut self, offset: u64, data: &[u8]);

    /// Driver wrote DRIVER_OK: take the validated queues, guest memory handle
    /// and interrupt, and get ready to process requests.
    fn activate(&mut self, resources: DeviceResources) -> Result<(), DeviceError>;

    /// The driver kicked `queue_index`. Process everything available on it and
    /// signal the interrupt if used buffers were added.
    ///
    /// Malformed guest requests must be failed in-band (status byte / dropped
    /// chain) and reported as `Ok`; only host-level trouble returns `Err`,
    /// which makes the transport set `DEVICE_NEEDS_RESET`.
    fn notify(&mut self, queue_index: u16) -> Result<(), DeviceError>;

    /// Device reset: drop in-flight work, release queue state, return to the
    /// pre-ACKNOWLEDGE state. Must be infallible.
    fn reset(&mut self);

    /// Hands the device a [`HostWaker`] it may use to request service from
    /// its worker context (see the trait docs). Called at most once, before
    /// the device is attached to a transport, and only on hosts whose notify
    /// mode runs a worker. The default ignores it — a device with no
    /// asynchronous host work needs nothing here, and a device that *would*
    /// use one must keep a synchronous fallback for when none arrives.
    fn set_host_waker(&mut self, waker: std::sync::Arc<dyn HostWaker>) {
        let _ = waker;
    }

    /// Shared-memory regions this device exposes ([`ShmRegion`], VEN-2001).
    ///
    /// The default is none, which is what every device except virtio-gpu with
    /// a host-visible renderer returns — and what keeps both transports'
    /// absent-region behaviour byte-identical to what they did before regions
    /// existed. Constant for the life of the device: the transport reads it
    /// once when it publishes its capability list.
    fn shm_regions(&self) -> Vec<ShmRegion> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_ids_match_the_spec() {
        assert_eq!(DeviceType::Net.id(), 1);
        assert_eq!(DeviceType::Block.id(), 2);
        assert_eq!(DeviceType::Gpu.id(), 16);
        assert_eq!(DeviceType::Input.id(), 18);
        assert_eq!(DeviceType::Sound.id(), 25);
    }

    #[derive(Default)]
    struct Counting(std::sync::atomic::AtomicUsize);

    impl HostWaker for Counting {
        fn wake(&self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Release);
        }
    }

    impl Counting {
        fn count(&self) -> usize {
            self.0.load(std::sync::atomic::Ordering::Acquire)
        }
    }

    /// The whole point of [`DeferredWaker`]: a wake that happens before the
    /// host wiring exists is delivered when it appears, not dropped — a lost
    /// wakeup is a device that never completes what it deferred.
    #[test]
    fn a_wake_before_installation_is_delivered_on_installation() {
        let deferred = DeferredWaker::new();
        assert!(!deferred.is_live());
        deferred.wake();
        deferred.wake(); // Coalesces: one wakeup is enough to make a poll run.

        let real = std::sync::Arc::new(Counting::default());
        deferred.install(std::sync::Arc::clone(&real) as std::sync::Arc<dyn HostWaker>);
        assert!(deferred.is_live());
        assert_eq!(
            real.count(),
            1,
            "the missed wake was delivered exactly once"
        );

        deferred.wake();
        assert_eq!(real.count(), 2, "later wakes pass straight through");
    }

    /// A never-installed waker is inert — which is what a host with no queue
    /// worker gets, and why every user needs a synchronous fallback.
    #[test]
    fn a_never_installed_waker_is_inert_but_safe() {
        let deferred = DeferredWaker::new();
        for _ in 0..100 {
            deferred.wake();
        }
        assert!(!deferred.is_live());
        // Installing later still delivers exactly one wake.
        let real = std::sync::Arc::new(Counting::default());
        deferred.install(std::sync::Arc::clone(&real) as std::sync::Arc<dyn HostWaker>);
        assert_eq!(real.count(), 1);
    }

    /// Wakes from other threads are the normal case (a renderer's monitor
    /// thread), so the waker must be `Send + Sync` and usable concurrently.
    #[test]
    fn wakes_from_many_threads_all_arrive() {
        let deferred = DeferredWaker::new();
        let real = std::sync::Arc::new(Counting::default());
        deferred.install(std::sync::Arc::clone(&real) as std::sync::Arc<dyn HostWaker>);
        let mut threads = Vec::new();
        for _ in 0..4 {
            let waker = std::sync::Arc::clone(&deferred);
            threads.push(std::thread::spawn(move || {
                for _ in 0..250 {
                    waker.wake();
                }
            }));
        }
        for thread in threads {
            thread.join().expect("waker thread");
        }
        assert_eq!(real.count(), 1000);
    }
}
