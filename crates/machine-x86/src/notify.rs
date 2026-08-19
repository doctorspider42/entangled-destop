//! Host-side queue-notify offload, for both virtio transports (backlog MVP-307,
//! extended to virtio-pci in EPIC 19).
//!
//! A guest kick is a write to a transport-defined address. Served from the MMIO
//! exit path it costs a `KVM_EXIT_MMIO` round-trip *and* runs the device — disk
//! I/O, TAP writes, scanout blits — on the vCPU thread, which cannot re-enter
//! the guest until the device is done.
//!
//! This module removes both costs:
//!
//! * one `EventFd` per (device, queue) is registered with KVM as an
//!   **ioeventfd**, so KVM completes the guest write inside the kernel and
//!   signals the eventfd instead of exiting to userspace;
//! * one **worker thread per device** epolls its queue eventfds plus a kill
//!   eventfd and calls [`QueueNotifyTarget::queue_notify`] under the same
//!   `Arc<Mutex<T>>` the vCPUs use for register access, so device work runs
//!   concurrently with guest execution.
//!
//! # The two addressing schemes
//!
//! The transports disagree about *where* a kick lands, and only about that, so
//! [`NotifyAddressing`] is the whole difference:
//!
//! * **virtio-mmio** has one `QUEUE_NOTIFY` register for every queue and the
//!   queue index is the written value, so all queues share one address and KVM
//!   is given a 4-byte **datamatch** on the index. A write of any other value
//!   still exits to userspace, where the transport drops it as an unknown queue.
//! * **virtio-pci** has a notification *area* with a `notify_off_multiplier`, so
//!   every queue has its own address and **no datamatch is needed** — which is
//!   strictly better: a kick of any width (Linux writes 2 bytes, some drivers 4)
//!   is completed in the kernel, whereas a datamatch is width-sensitive.
//!
//! Everything KVM-specific stays here, in the Linux-gated machine layer
//! (ADR-0002): `virtio-core` only learns *that* a queue is offloaded, through
//! [`MmioTransport::offload_queue_notify`].
//!
//! # Fallback
//!
//! Offload is best-effort and per queue. If `KVM_IOEVENTFD` is unavailable or
//! the address is already claimed, the queue is left un-offloaded and its kicks
//! keep going through the synchronous MMIO path — the VM still boots, just
//! slower. [`QueueNotifyMode::Synchronous`] (or `ENTANGLED_QUEUE_NOTIFY=sync`)
//! disables the offload wholesale, which is also how the before/after
//! measurement is taken.
//!
//! # Shutdown
//!
//! [`DeviceNotifier::shutdown`] writes the kill eventfd, joins the worker and
//! deassigns every ioeventfd; it is idempotent and also runs from `Drop`, so a
//! VM that stops (or a construction error half way through) never leaks a
//! thread or a kernel-side registration. A *guest-driven device reset* does not
//! stop the worker: the registration describes host wiring, and the transport's
//! own `activated` flag already drops kicks that arrive while the device is
//! down.

use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use kvm_ioctls::{IoEventAddress, NoDatamatch, VmFd};
use thiserror::Error;
use virtio_core::{MmioTransport, PciTransport};
use vmm_sys_util::epoll::{ControlOperation, Epoll, EpollEvent, EventSet};
use vmm_sys_util::eventfd::{EventFd, EFD_NONBLOCK};

/// What the offload needs from a transport: how many queues it has, and the two
/// halves of handing a queue's kicks over to a host primitive.
///
/// Implemented for both transports; nothing else in this module knows which one
/// it is serving.
pub trait QueueNotifyTarget: Send + 'static {
    fn num_queues(&self) -> usize;
    fn offload_queue_notify(&mut self, index: u16) -> bool;
    fn restore_queue_notify(&mut self, index: u16);
    fn queue_notify(&mut self, value: u32);
    /// Transport name, for log records.
    fn transport_name() -> &'static str;
}

impl QueueNotifyTarget for MmioTransport {
    fn num_queues(&self) -> usize {
        self.num_queues()
    }
    fn offload_queue_notify(&mut self, index: u16) -> bool {
        self.offload_queue_notify(index)
    }
    fn restore_queue_notify(&mut self, index: u16) {
        self.restore_queue_notify(index)
    }
    fn queue_notify(&mut self, value: u32) {
        self.queue_notify(value)
    }
    fn transport_name() -> &'static str {
        "virtio-mmio"
    }
}

impl QueueNotifyTarget for PciTransport {
    fn num_queues(&self) -> usize {
        self.num_queues()
    }
    fn offload_queue_notify(&mut self, index: u16) -> bool {
        self.offload_queue_notify(index)
    }
    fn restore_queue_notify(&mut self, index: u16) {
        self.restore_queue_notify(index)
    }
    fn queue_notify(&mut self, value: u32) {
        self.queue_notify(value)
    }
    fn transport_name() -> &'static str {
        "virtio-pci"
    }
}

/// Where a transport's queue kicks land in the guest's address space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyAddressing {
    /// Every queue kicks the same address and identifies itself by the value it
    /// writes: KVM matches on the queue index (virtio-mmio's `QUEUE_NOTIFY`).
    SharedWithDatamatch { addr: u64 },
    /// Queue *n* kicks `base + n * stride`, so the address alone identifies it
    /// and no datamatch is required (virtio-pci's notification area).
    PerQueue { base: u64, stride: u64 },
}

impl NotifyAddressing {
    /// Guest physical address queue `index` is kicked at.
    fn addr_of(self, index: u16) -> u64 {
        match self {
            Self::SharedWithDatamatch { addr } => addr,
            Self::PerQueue { base, stride } => {
                base.saturating_add(u64::from(index).saturating_mul(stride))
            }
        }
    }

    fn is_datamatched(self) -> bool {
        matches!(self, Self::SharedWithDatamatch { .. })
    }

    /// The same addressing moved to a new base.
    ///
    /// Only [`Self::PerQueue`] can move, and only because a PCI BAR is *guest*
    /// programmable: a virtio-mmio slot lives at a host constant that nothing in
    /// the guest can change, so rebasing one is a host bug rather than a
    /// situation to handle.
    fn with_base(self, base: u64) -> Self {
        match self {
            Self::SharedWithDatamatch { .. } => self,
            Self::PerQueue { stride, .. } => Self::PerQueue { base, stride },
        }
    }

    /// The base a rebase would replace, for comparing "where the registrations
    /// are" against "where the BAR now decodes".
    pub fn base(self) -> u64 {
        match self {
            Self::SharedWithDatamatch { addr } => addr,
            Self::PerQueue { base, .. } => base,
        }
    }
}

/// Upper bound on queues we will offload per device.
///
/// The MVP's widest device has two queues (net, gpu, input); the cap keeps the
/// number of host fds a device can demand bounded and independent of anything
/// the guest does.
pub const MAX_OFFLOADED_QUEUES: usize = 16;

/// epoll token of the kill eventfd. Queue tokens are the queue index, which is
/// always below [`MAX_OFFLOADED_QUEUES`], so the two never collide.
const KILL_TOKEN: u64 = u64::MAX;

/// How long the worker blocks in `epoll_wait` before looping. A timeout rather
/// than an infinite wait so a worker whose kill eventfd write somehow failed
/// still notices the stop flag instead of hanging the join forever.
const EPOLL_TIMEOUT_MS: i32 = 250;

/// Environment variable that forces a notification mode, for benchmarking and
/// for working around a host where the offload misbehaves.
pub const NOTIFY_MODE_ENV: &str = "ENTANGLED_QUEUE_NOTIFY";

/// Whether queue kicks are offloaded to ioeventfds and worker threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueueNotifyMode {
    /// Register ioeventfds and run one worker thread per device (MVP-307).
    #[default]
    Ioeventfd,
    /// Handle every kick inline on the vCPU thread that took the MMIO exit.
    Synchronous,
}

impl QueueNotifyMode {
    /// Reads [`NOTIFY_MODE_ENV`]; anything unrecognised keeps the default.
    pub fn from_env() -> Self {
        match std::env::var(NOTIFY_MODE_ENV) {
            Ok(value) => Self::parse(&value).unwrap_or_else(|| {
                tracing::warn!(
                    var = NOTIFY_MODE_ENV,
                    value = %value,
                    "unrecognised queue-notify mode, using the default"
                );
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Parses the accepted spellings; `None` for anything else.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.eq_ignore_ascii_case("ioeventfd") || value.eq_ignore_ascii_case("async") {
            Some(Self::Ioeventfd)
        } else if value.eq_ignore_ascii_case("sync") || value.eq_ignore_ascii_case("synchronous") {
            Some(Self::Synchronous)
        } else {
            None
        }
    }

    pub fn is_offloaded(self) -> bool {
        matches!(self, Self::Ioeventfd)
    }
}

#[derive(Debug, Error)]
pub enum NotifyError {
    #[error("failed to create the queue-notify eventfd for slot {slot} queue {queue}: {source}")]
    EventFd {
        slot: usize,
        queue: u16,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to create the epoll set for slot {slot}: {source}")]
    Epoll {
        slot: usize,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to spawn the queue worker thread for slot {slot}: {source}")]
    Spawn {
        slot: usize,
        #[source]
        source: std::io::Error,
    },
}

/// One queue's eventfd. The *address* it is registered at is not here: on
/// virtio-pci that address follows a guest-programmable BAR, so it lives in
/// [`Registration`] where it can change without disturbing the fds the worker
/// thread is already epolling.
struct QueueEvent {
    index: u16,
    event: EventFd,
}

/// Where a device's queue eventfds are registered with KVM *right now*.
///
/// Separate from the eventfds themselves because it moves. On virtio-pci the
/// notification area is at `BAR0 + 0x2000`, and BAR0 is whatever the guest last
/// wrote — EDK2's `PciBusDxe` reassigns every BAR during resource allocation, so
/// a registration made at attach time is at the wrong address by the time any
/// driver kicks a queue.
struct Registration {
    /// `None` when nothing is registered: either the offload has been taken down,
    /// or every registration was refused (in which case the queues are back on
    /// the synchronous path and a later rebase may pick them up).
    addressing: Option<NotifyAddressing>,
    /// Bit *n* set means `events[n]`'s eventfd is registered at `addressing` and
    /// the transport has been told that queue is offloaded.
    ///
    /// A bitmask rather than a list so reconciling costs no allocation on a path
    /// a guest can drive (one config-space write per reconcile).
    mask: u16,
}

impl Registration {
    const fn none() -> Self {
        Self {
            addressing: None,
            mask: 0,
        }
    }

    fn holds(&self, position: usize) -> bool {
        position < MAX_OFFLOADED_QUEUES && self.mask & (1u16 << position) != 0
    }
}

/// The host side of one device's offloaded queue notifications: the eventfds,
/// their KVM registrations and the worker thread draining them.
///
/// Generic over the transport so there is one implementation of the offload
/// rather than one per transport; everything transport-specific is
/// [`NotifyAddressing`] plus the [`QueueNotifyTarget`] impl.
pub struct DeviceNotifier<T: QueueNotifyTarget> {
    slot: usize,
    vm: Arc<VmFd>,
    /// The *shape* of this device's addressing — datamatched or per-queue, and
    /// with what stride. Fixed at attach: only the base moves.
    shape: NotifyAddressing,
    /// The eventfds and the worker thread: fixed for the notifier's lifetime.
    events: Vec<QueueEvent>,
    /// The KVM registrations: guest-movable, hence the `Mutex`. Any vCPU thread
    /// can take the configuration-space exit that moves a BAR, so this is
    /// reached through `&self`.
    registration: Mutex<Registration>,
    kill: EventFd,
    /// `None` before the worker starts and again once it has been joined.
    worker: Mutex<Option<JoinHandle<()>>>,
    /// Set by the first [`Self::shutdown`], which makes it idempotent: it is
    /// called explicitly on VM stop and again from `Drop`.
    torn_down: AtomicBool,
    /// A notifier never stores a `T`; the parameter only selects whose
    /// `queue_notify` the worker calls.
    _target: std::marker::PhantomData<fn() -> T>,
}

impl<T: QueueNotifyTarget> DeviceNotifier<T> {
    /// Offloads every queue of the device in `slot` whose ioeventfd could be
    /// registered, and starts the worker thread that serves them.
    ///
    /// Returns `Ok(None)` when there is nothing to offload — a device with no
    /// queues — and the caller simply keeps the synchronous path. A notifier is
    /// returned even if *no* registration succeeded: on virtio-pci the addresses
    /// move, so "refused today" is not "refused forever" (see [`Self::rebase`]).
    /// Only queues actually registered are reported to the transport as
    /// offloaded, so a kick can never be lost to a worker that will not run.
    pub fn attach(
        vm: Arc<VmFd>,
        slot: usize,
        addressing: NotifyAddressing,
        transport: &Arc<Mutex<T>>,
    ) -> Result<Option<Self>, NotifyError> {
        // The transport lock is held only for the bookkeeping, never across the
        // thread spawn: the worker takes the same lock.
        let queue_count = {
            let Ok(t) = transport.lock() else {
                tracing::error!(slot, "transport lock poisoned; not offloading queue notify");
                return Ok(None);
            };
            t.num_queues().min(MAX_OFFLOADED_QUEUES)
        };
        if queue_count == 0 {
            return Ok(None);
        }

        // Built empty first so that *every* early return from here on drops a
        // `DeviceNotifier` that owns whatever was registered so far, and its
        // `Drop` deassigns it. Registering an ioeventfd and then bailing out
        // without deassigning would leave KVM swallowing that queue's kicks for
        // the lifetime of the VM fd, which wedges the device silently.
        let kill = EventFd::new(EFD_NONBLOCK).map_err(|source| NotifyError::EventFd {
            slot,
            queue: u16::MAX,
            source,
        })?;
        let mut notifier = Self {
            slot,
            vm,
            shape: addressing,
            events: Vec::with_capacity(queue_count),
            registration: Mutex::new(Registration::none()),
            kill,
            worker: Mutex::new(None),
            torn_down: AtomicBool::new(false),
            _target: std::marker::PhantomData,
        };

        for index in 0..queue_count {
            // `queue_count` is capped at MAX_OFFLOADED_QUEUES, so this fits.
            let queue = u16::try_from(index).unwrap_or(u16::MAX);
            let event = EventFd::new(EFD_NONBLOCK).map_err(|source| NotifyError::EventFd {
                slot,
                queue,
                source,
            })?;
            notifier.events.push(QueueEvent {
                index: queue,
                event,
            });
        }

        // The worker exists before any registration does, so a kick can never
        // arrive on an eventfd nobody is watching.
        let handle = notifier.spawn_worker(Arc::clone(transport))?;
        if let Ok(mut slot_handle) = notifier.worker.lock() {
            *slot_handle = Some(handle);
        }

        let registered = notifier.rebase(addressing.base(), transport);
        tracing::info!(
            transport = T::transport_name(),
            slot,
            queues = registered,
            of = queue_count,
            ?addressing,
            "queue notify offloaded to ioeventfds"
        );
        Ok(Some(notifier))
    }

    /// Points this device's queue eventfds at `base`, moving them if they are
    /// somewhere else. Returns how many queues are registered afterwards.
    ///
    /// This exists because a **PCI BAR belongs to the guest**. The notification
    /// area sits at `BAR0 + 0x2000`, and EDK2's `PciBusDxe` reassigns every BAR
    /// during resource allocation — on this machine it hands out exactly our own
    /// aperture slots in reverse order, so every device's kicks land on a
    /// *different* device's ioeventfd. The symptom is not a crash: the firmware
    /// reads a disk's capacity correctly (that is plain MMIO, which the config
    /// space decodes wherever the BAR now points), kicks its request queue, and
    /// the log shows some unrelated device logging "queue notify before
    /// DRIVER_OK, ignoring" while the disk waits forever.
    ///
    /// Rules, in the order they matter:
    ///
    /// * a queue is reported to the transport as offloaded **only** while its
    ///   eventfd is registered. Deassign first, tell the transport second — never
    ///   the other way round, or a kick in the gap is dropped by both paths;
    /// * a refused registration is not fatal. The queue goes back to the
    ///   synchronous MMIO path, which still works, and the next rebase retries
    ///   it. That is what makes a *permutation* of BAR addresses resolvable at
    ///   all: the first device to move collides with a stale registration, keeps
    ///   the synchronous path for a moment, and is picked up once the other
    ///   device has moved away (see `VirtioPciBus::reconcile_notify`).
    pub fn rebase(&self, base: u64, transport: &Arc<Mutex<T>>) -> usize {
        if self.torn_down.load(Ordering::Acquire) {
            return 0;
        }
        let addressing = self.shape.with_base(base);
        let Ok(mut registration) = self.registration.lock() else {
            tracing::error!(slot = self.slot, "registration lock poisoned; not rebasing");
            return 0;
        };
        if registration.addressing == Some(addressing) {
            return registration.mask.count_ones() as usize;
        }
        self.unregister_locked(&mut registration, transport);

        let mut mask = 0u16;
        for (position, queue) in self.events.iter().enumerate() {
            if self.register_one(addressing, queue) && self.mark_offloaded(transport, queue.index) {
                mask |= 1u16 << position;
            } else {
                // Either KVM refused the address or the transport refused the
                // queue; both mean nothing would serve a kick here, so the
                // registration must not stay.
                self.deassign_one(addressing, queue);
            }
        }
        let count = mask.count_ones() as usize;
        registration.mask = mask;
        registration.addressing = (mask != 0).then_some(addressing);
        if count < self.events.len() {
            tracing::warn!(
                transport = T::transport_name(),
                slot = self.slot,
                registered = count,
                of = self.events.len(),
                base = format_args!("{:#x}", addressing.base()),
                "some queues could not be offloaded at this address and keep the \
                 synchronous notify path"
            );
        } else {
            tracing::debug!(
                transport = T::transport_name(),
                slot = self.slot,
                queues = count,
                base = format_args!("{:#x}", addressing.base()),
                "queue-notify ioeventfds now registered"
            );
        }
        count
    }

    /// Where the registrations currently sit, or `None` when none do.
    pub fn notify_base(&self) -> Option<u64> {
        self.registration
            .lock()
            .ok()
            .and_then(|r| r.addressing)
            .map(|a| a.base())
    }

    /// Deassigns everything and puts every queue back on the synchronous path.
    pub fn unregister(&self, transport: &Arc<Mutex<T>>) {
        if let Ok(mut registration) = self.registration.lock() {
            self.unregister_locked(&mut registration, transport);
        }
    }

    fn unregister_locked(&self, registration: &mut Registration, transport: &Arc<Mutex<T>>) {
        let Some(addressing) = registration.addressing.take() else {
            registration.mask = 0;
            return;
        };
        for (position, queue) in self.events.iter().enumerate() {
            if !registration.holds(position) {
                continue;
            }
            self.deassign_one(addressing, queue);
            if let Ok(mut t) = transport.lock() {
                t.restore_queue_notify(queue.index);
            }
        }
        registration.mask = 0;
    }

    /// Registers one queue's eventfd. `false` means KVM refused it — most often
    /// because a stale registration from another device still owns the address.
    fn register_one(&self, addressing: NotifyAddressing, queue: &QueueEvent) -> bool {
        let notify_addr = addressing.addr_of(queue.index);
        let addr = IoEventAddress::Mmio(notify_addr);
        // With a shared address the datamatch on the queue index makes KVM
        // swallow exactly this queue's kicks; a write of any other value still
        // exits to userspace, where the transport drops it as an unknown queue.
        // With a per-queue address the address itself says which queue it is, so
        // no datamatch — and therefore no width sensitivity — is needed.
        let result = if addressing.is_datamatched() {
            self.vm
                .register_ioevent(&queue.event, &addr, u32::from(queue.index))
        } else {
            self.vm.register_ioevent(&queue.event, &addr, NoDatamatch)
        };
        match result {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(
                    slot = self.slot,
                    queue = queue.index,
                    addr = format_args!("{notify_addr:#x}"),
                    %error,
                    "ioeventfd registration refused; this queue keeps the synchronous notify path"
                );
                false
            }
        }
    }

    fn mark_offloaded(&self, transport: &Arc<Mutex<T>>, queue: u16) -> bool {
        match transport.lock() {
            Ok(mut t) => t.offload_queue_notify(queue),
            Err(_) => {
                tracing::error!(
                    slot = self.slot,
                    queue,
                    "transport lock poisoned; skipping offload"
                );
                false
            }
        }
    }

    /// Removes one queue's ioeventfd registration, with the same datamatch it
    /// was registered with — KVM matches registrations on the whole tuple, so a
    /// mismatched deassign silently leaves the kernel swallowing kicks.
    fn deassign_one(&self, addressing: NotifyAddressing, queue: &QueueEvent) {
        let addr = IoEventAddress::Mmio(addressing.addr_of(queue.index));
        let result = if addressing.is_datamatched() {
            self.vm
                .unregister_ioevent(&queue.event, &addr, u32::from(queue.index))
        } else {
            self.vm.unregister_ioevent(&queue.event, &addr, NoDatamatch)
        };
        if let Err(error) = result {
            tracing::debug!(
                slot = self.slot,
                queue = queue.index,
                %error,
                "deassigning a queue-notify ioeventfd failed (it may never have been registered)"
            );
        }
    }

    /// Queue indices this notifier currently serves.
    pub fn offloaded_queues(&self) -> Vec<u16> {
        let Ok(registration) = self.registration.lock() else {
            return Vec::new();
        };
        self.events
            .iter()
            .enumerate()
            .filter(|(position, _)| registration.holds(*position))
            .map(|(_, e)| e.index)
            .collect()
    }

    /// Test/diagnostic hook: signals queue `index`'s eventfd exactly as KVM
    /// would, so the worker path can be exercised without a guest.
    pub fn kick(&self, index: u16) -> Result<(), std::io::Error> {
        match self.events.iter().find(|e| e.index == index) {
            Some(queue) => queue.event.write(1),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("queue {index} is not offloaded"),
            )),
        }
    }

    fn spawn_worker(&self, transport: Arc<Mutex<T>>) -> Result<JoinHandle<()>, NotifyError> {
        let epoll = Epoll::new().map_err(|source| NotifyError::Epoll {
            slot: self.slot,
            source,
        })?;
        let add = |fd, token| {
            epoll.ctl(
                ControlOperation::Add,
                fd,
                EpollEvent::new(EventSet::IN, token),
            )
        };
        add(self.kill.as_raw_fd(), KILL_TOKEN).map_err(|source| NotifyError::Epoll {
            slot: self.slot,
            source,
        })?;

        // The worker needs its own handles on the eventfds: `self` stays owned
        // by the bus and must remain droppable independently of the thread.
        let mut queues = Vec::with_capacity(self.events.len());
        for queue in &self.events {
            let clone = queue
                .event
                .try_clone()
                .map_err(|source| NotifyError::EventFd {
                    slot: self.slot,
                    queue: queue.index,
                    source,
                })?;
            add(clone.as_raw_fd(), u64::from(queue.index)).map_err(|source| {
                NotifyError::Epoll {
                    slot: self.slot,
                    source,
                }
            })?;
            queues.push((queue.index, clone));
        }

        let slot = self.slot;
        std::thread::Builder::new()
            .name(format!("virtio-q{slot}"))
            .spawn(move || worker_loop(slot, epoll, queues, transport))
            .map_err(|source| NotifyError::Spawn { slot, source })
    }

    /// Stops the worker and deassigns every ioeventfd. Idempotent, and correct
    /// even when no worker was ever started (a half-built notifier still owns
    /// registrations that must go back).
    pub fn shutdown(&self) {
        if self.torn_down.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(handle) = self.worker.lock().ok().and_then(|mut h| h.take()) {
            if let Err(error) = self.kill.write(1) {
                tracing::error!(
                    slot = self.slot,
                    %error,
                    "failed to signal the queue worker to stop; falling back to the epoll timeout"
                );
            }
            if handle.join().is_err() {
                // A device panic on a guest-controlled path is a bug; report it
                // loudly rather than swallowing it, but never propagate the
                // unwind through the VM teardown path.
                tracing::error!(slot = self.slot, "queue worker thread panicked");
            }
        }
        // Deassign wherever the registrations *currently* are, which after a
        // guest BAR reassignment is not where `attach` put them.
        if let Ok(mut registration) = self.registration.lock() {
            if let Some(addressing) = registration.addressing.take() {
                for (position, queue) in self.events.iter().enumerate() {
                    if registration.holds(position) {
                        self.deassign_one(addressing, queue);
                    }
                }
            }
            registration.mask = 0;
        }
        tracing::debug!(
            slot = self.slot,
            queues = self.events.len(),
            "queue worker stopped and ioeventfds deassigned"
        );
    }
}

impl<T: QueueNotifyTarget> Drop for DeviceNotifier<T> {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The per-device worker: turn eventfd signals into `queue_notify` calls until
/// the kill eventfd fires.
///
/// Never panics and never returns early on a guest-triggered condition: a
/// malformed request is the device's business (it answers in-band), and a device
/// that reports a host-level failure makes the transport set
/// `DEVICE_NEEDS_RESET` — the worker keeps serving the remaining queues either
/// way, because a stopped worker would silently wedge the VM.
fn worker_loop<T: QueueNotifyTarget>(
    slot: usize,
    epoll: Epoll,
    queues: Vec<(u16, EventFd)>,
    transport: Arc<Mutex<T>>,
) {
    // One slot per registered fd (queues + kill) so a single wait drains them.
    let mut ready = vec![EpollEvent::default(); queues.len() + 1];
    loop {
        let count = match epoll.wait(EPOLL_TIMEOUT_MS, &mut ready) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                tracing::error!(slot, %error, "queue worker epoll failed; stopping");
                return;
            }
        };
        // `epoll_wait` never reports more events than the buffer holds, but
        // clamp anyway rather than trusting the count for an index.
        let count = count.min(ready.len());
        for event in &ready[..count] {
            if event.data() == KILL_TOKEN {
                tracing::debug!(slot, "queue worker asked to stop");
                return;
            }
            let Some((index, fd)) = queues.iter().find(|(i, _)| u64::from(*i) == event.data())
            else {
                continue;
            };
            // Reading clears the counter; several kicks coalesce into one
            // drain, which is exactly what a virtqueue wants.
            match fd.read() {
                Ok(0) => continue,
                Ok(_) => (),
                // EAGAIN: another wakeup already consumed the count.
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(error) => {
                    tracing::error!(slot, queue = index, %error, "queue eventfd read failed");
                    continue;
                }
            }
            match transport.lock() {
                Ok(mut t) => t.queue_notify(u32::from(*index)),
                Err(_) => tracing::error!(
                    transport = T::transport_name(),
                    slot,
                    queue = index,
                    "transport lock is poisoned; dropping queue notification"
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parsing_accepts_both_spellings_and_rejects_junk() {
        assert_eq!(
            QueueNotifyMode::parse("ioeventfd"),
            Some(QueueNotifyMode::Ioeventfd)
        );
        assert_eq!(
            QueueNotifyMode::parse(" ASYNC "),
            Some(QueueNotifyMode::Ioeventfd)
        );
        assert_eq!(
            QueueNotifyMode::parse("sync"),
            Some(QueueNotifyMode::Synchronous)
        );
        assert_eq!(
            QueueNotifyMode::parse("Synchronous"),
            Some(QueueNotifyMode::Synchronous)
        );
        assert_eq!(QueueNotifyMode::parse("maybe"), None);
        assert_eq!(QueueNotifyMode::parse(""), None);
    }

    #[test]
    fn default_mode_is_offloaded() {
        assert!(QueueNotifyMode::default().is_offloaded());
        assert!(!QueueNotifyMode::Synchronous.is_offloaded());
    }

    /// Queue tokens must never collide with the kill token.
    #[test]
    fn kill_token_is_outside_the_queue_token_range() {
        assert!(u64::try_from(MAX_OFFLOADED_QUEUES).unwrap_or(u64::MAX) < KILL_TOKEN);
    }
}
