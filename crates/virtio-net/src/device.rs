//! The virtio-net device (backlog MVP-501/502/504/508/509).
//!
//! Two queues, per VirtIO spec 1.2 section 5.1.2: **queue 0 is receive, queue 1
//! is transmit**, both from the guest's point of view. Every buffer in either
//! direction is one virtio-net header ([`VIRTIO_NET_HDR_LEN`] bytes) followed by
//! one whole Ethernet frame.
//!
//! # Threading
//!
//! TX is driver-driven and runs inline: [`VirtioDevice::notify`] for queue 1
//! drains the transmit queue on the thread that took the queue-notify exit.
//!
//! RX is host-driven — frames arrive whenever the host network stack feels like
//! it, with no guest event to hang the work on — so the device owns one worker
//! thread per activation:
//!
//! ```text
//!   activate()  ── spawns ──▶  RX worker ── blocks in backend.wait_readable()
//!                                  │
//!                                  ├─ frame ──▶ RX virtqueue ──▶ interrupt
//!                                  │
//!   reset()/drop ── stop flag ─────┤
//!                └─ backend.wake() ┘  (breaks the poll at once)
//!                └─ join() ─────────▶ worker returns, releases the RX queue
//! ```
//!
//! The worker is the *only* owner of the RX queue (an `Arc<Mutex<Queue>>` that
//! nothing else holds), so joining it is what proves the queue and the guest
//! memory handle are released. The stop flag alone would leave up to one poll
//! tick of shutdown latency; [`NetBackend::wake`] removes it.
//!
//! # Untrusted guest
//!
//! Chain walks go through [`virtio_core::chain`] (bounded, index-checked),
//! every guest buffer is touched only through checked `vm-memory` calls, and
//! anything malformed — a truncated header, a runt or oversized frame, a chain
//! whose RX buffers cannot hold the frame, a buffer outside guest RAM — drops
//! that one frame, counts it in [`NetStats`] and leaves the device running.
//! Nothing on a guest-controlled path panics, unwraps or fails the device.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use virtio_core::chain;
use virtio_core::device::{DeviceError, DeviceResources, DeviceType, VirtioDevice};
use virtio_core::interrupt::Interrupt;
use virtio_core::{GuestMem, Quiesce, MAX_QUEUE_SIZE, VIRTIO_F_VERSION_1};
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress};

use crate::backend::{NetBackend, Readiness};
use crate::frame::{validate_tx_buffer, NetHeader, MAX_BUFFER_LEN, MAX_FRAME_LEN};
use crate::frame::{FrameError, VIRTIO_NET_HDR_LEN};
use crate::MacAddr;

/// `VIRTIO_NET_F_MAC`: the config space carries a valid MAC address.
pub const VIRTIO_NET_F_MAC: u64 = 1 << 5;

/// The complete feature set Entangled Desktop's virtio-net offers.
///
/// Deliberately minimal (MVP-502): no checksum or segmentation offloads, no
/// mergeable RX buffers, no control queue, no multiqueue, no announced status
/// or MTU. Correct first, fast later — every one of those features adds a guest
/// controlled code path, and the MVP goal is a Debian desktop reaching the
/// network, not line rate.
pub const FEATURES: u64 = VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC;

/// Receive queue index (guest receives).
pub const RX_QUEUE: u16 = 0;
/// Transmit queue index (guest transmits).
pub const TX_QUEUE: u16 = 1;
/// Number of virtqueues: RX and TX, no control queue.
pub const NUM_QUEUES: usize = 2;

static QUEUE_MAX_SIZES: [u16; NUM_QUEUES] = [MAX_QUEUE_SIZE, MAX_QUEUE_SIZE];

/// Guest-visible config space: just the 6-byte MAC. `status`, `max_virtqueue_
/// pairs` and `mtu` all belong to features Entangled Desktop does not offer, so a
/// conforming driver never reads past byte 5.
const CONFIG_LEN: usize = 6;

/// Hard bound on chains processed per TX notification, so a guest refilling the
/// available ring from another vCPU cannot pin this thread forever. Leftovers
/// are picked up by the next notification.
pub const CHAINS_PER_NOTIFY: usize = 4 * MAX_QUEUE_SIZE as usize;

/// How long the RX worker blocks in one `wait_readable` call. Only a fallback:
/// [`NetBackend::wake`] normally ends the wait immediately on shutdown.
const RX_POLL_TICK: Duration = Duration::from_millis(100);

/// Frames the RX worker moves per readable event before re-checking the stop
/// flag. Bounds shutdown latency under a flood of incoming traffic.
const RX_FRAMES_PER_WAKE: usize = 64;

/// Counters for `entangled doctor`, logs and tests. Every drop reason is separate:
/// "the guest posted no RX buffers" and "the guest's RX buffer was too small"
/// are very different bugs.
#[derive(Debug, Default)]
pub struct NetStats {
    rx_frames: AtomicU64,
    rx_bytes: AtomicU64,
    rx_dropped_no_buffer: AtomicU64,
    rx_dropped_no_space: AtomicU64,
    rx_dropped_oversized: AtomicU64,
    rx_dropped_bad_chain: AtomicU64,
    tx_frames: AtomicU64,
    tx_bytes: AtomicU64,
    tx_dropped_invalid: AtomicU64,
    tx_dropped_backend: AtomicU64,
}

macro_rules! counters {
    ($($field:ident),+ $(,)?) => {
        impl NetStats {
            $(
                pub fn $field(&self) -> u64 {
                    self.$field.load(Ordering::Acquire)
                }
            )+
        }
    };
}

counters!(
    rx_frames,
    rx_bytes,
    rx_dropped_no_buffer,
    rx_dropped_no_space,
    rx_dropped_oversized,
    rx_dropped_bad_chain,
    tx_frames,
    tx_bytes,
    tx_dropped_invalid,
    tx_dropped_backend,
);

impl NetStats {
    /// Total frames dropped on the receive path, for any reason.
    pub fn rx_dropped(&self) -> u64 {
        self.rx_dropped_no_buffer()
            .saturating_add(self.rx_dropped_no_space())
            .saturating_add(self.rx_dropped_oversized())
            .saturating_add(self.rx_dropped_bad_chain())
    }

    /// Total frames dropped on the transmit path, for any reason.
    pub fn tx_dropped(&self) -> u64 {
        self.tx_dropped_invalid()
            .saturating_add(self.tx_dropped_backend())
    }

    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::AcqRel);
    }

    fn add(counter: &AtomicU64, value: u64) {
        counter.fetch_add(value, Ordering::AcqRel);
    }
}

/// Recovers a poisoned lock instead of propagating the panic.
///
/// A poisoned RX queue mutex means a thread panicked while holding it — a host
/// bug we must report, not one we may turn into a second panic on a vCPU
/// thread. The protected state (a `virtio_queue::Queue`) stays structurally
/// valid either way, and every use of it re-validates the guest's data.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Everything the RX worker owns for the lifetime of one activation.
struct RxContext {
    backend: Arc<dyn NetBackend>,
    mem: Arc<GuestMem>,
    queue: Arc<Mutex<Queue>>,
    interrupt: Arc<dyn Interrupt>,
    stats: Arc<NetStats>,
    stop: Arc<AtomicBool>,
    /// The VM's pause gate (ADR-0005). This worker is the one device thread
    /// that writes guest memory entirely on its own schedule — a frame arrives
    /// from the host and goes straight into the RX ring — so a paused VM is only
    /// really stopped if it parks here.
    quiesce: Arc<Quiesce>,
}

/// The device's handle on its RX worker.
struct RxWorker {
    stop: Arc<AtomicBool>,
    /// Kept so `stop_rx` can wake a worker parked on the pause gate: a device
    /// reset happens while the VM is quiesced, and joining a parked thread
    /// without waking it would deadlock the reset.
    quiesce: Arc<Quiesce>,
    thread: JoinHandle<()>,
}

/// A virtio-net device on top of a host [`NetBackend`].
pub struct NetDevice {
    backend: Arc<dyn NetBackend>,
    mac: MacAddr,
    acked_features: u64,
    stats: Arc<NetStats>,
    /// Reusable TX staging buffer, so steady-state transmission does not
    /// allocate. Exactly [`MAX_BUFFER_LEN`] bytes: a chain claiming more is
    /// rejected before anything is copied.
    tx_buf: Vec<u8>,

    // Set on activate(), cleared on reset().
    mem: Option<Arc<GuestMem>>,
    tx_queue: Option<Queue>,
    interrupt: Option<Arc<dyn Interrupt>>,
    rx: Option<RxWorker>,
}

impl std::fmt::Debug for NetDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetDevice")
            .field("backend", &self.backend.name())
            .field("mac", &self.mac.to_string())
            .field("activated", &self.tx_queue.is_some())
            .field("rx_worker", &self.rx.is_some())
            .finish_non_exhaustive()
    }
}

impl NetDevice {
    /// Builds a device around `backend` advertising `mac` to the guest.
    ///
    /// ```no_run
    /// # use virtio_net::{MacAddr, NetDevice};
    /// # #[cfg(target_os = "linux")]
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use virtio_net::TapBackend;
    ///
    /// let device = NetDevice::new(TapBackend::open("entangled0")?, MacAddr::derive("debian-demo"));
    /// # Ok(()) }
    /// # #[cfg(not(target_os = "linux"))] fn main() {}
    /// ```
    pub fn new<B: NetBackend + 'static>(backend: B, mac: MacAddr) -> Self {
        Self::with_backend(Arc::new(backend), mac)
    }

    /// Same, for a backend that is already shared (or a `dyn` one).
    pub fn with_backend(backend: Arc<dyn NetBackend>, mac: MacAddr) -> Self {
        Self {
            backend,
            mac,
            acked_features: 0,
            stats: Arc::new(NetStats::default()),
            tx_buf: vec![0u8; MAX_BUFFER_LEN],
            mem: None,
            tx_queue: None,
            interrupt: None,
            rx: None,
        }
    }

    /// The MAC the guest sees in the config space.
    pub fn mac(&self) -> MacAddr {
        self.mac
    }

    /// Label of the host backend, e.g. `tap:entangled0`.
    pub fn backend_name(&self) -> &str {
        self.backend.name()
    }

    /// Live counters, shareable with a monitoring thread or a test.
    pub fn stats(&self) -> &Arc<NetStats> {
        &self.stats
    }

    /// True while the RX worker is running.
    pub fn is_receiving(&self) -> bool {
        self.rx.is_some()
    }

    // -------------------------------------------------------------- transmit

    /// Drains the TX queue: one chain is one frame.
    fn drain_tx(
        &mut self,
        queue: &mut Queue,
        mem: &Arc<GuestMem>,
        interrupt: &dyn Interrupt,
    ) -> Result<(), DeviceError> {
        let desc_table = queue.desc_table();
        let queue_size = queue.size();
        let mut served = 0usize;

        while let Some(head) = queue
            .pop_descriptor_chain(Arc::clone(mem))
            .map(|chain| chain.head_index())
        {
            self.transmit(mem, desc_table, queue_size, head);
            // TX chains are device-readable only: nothing was written back, so
            // the used length is always 0.
            queue
                .add_used(mem.as_ref(), head, 0)
                .map_err(|e| DeviceError::Queue(e.to_string()))?;
            served += 1;
            if served >= CHAINS_PER_NOTIFY {
                tracing::warn!(
                    backend = self.backend.name(),
                    served,
                    "virtio-net TX budget exhausted; deferring the rest"
                );
                break;
            }
        }

        if served > 0
            && queue
                .needs_notification(mem.as_ref())
                .map_err(|e| DeviceError::Queue(e.to_string()))?
        {
            interrupt.signal_used_queue(TX_QUEUE)?;
        }
        Ok(())
    }

    /// Validates one TX chain and hands its frame to the backend.
    ///
    /// Never fails the device: a malformed chain is a dropped frame, because a
    /// guest bug on one packet must not take the interface down.
    fn transmit(&mut self, mem: &GuestMem, desc_table: u64, queue_size: u16, head: u16) {
        let Some(filled) = self.gather_tx(mem, desc_table, queue_size, head) else {
            return;
        };
        let frame_len = match validate_tx_buffer(&self.tx_buf[..filled]) {
            Ok(len) => len,
            Err(error) => {
                self.drop_tx(head, &error);
                return;
            }
        };
        // `validate_tx_buffer` proved the header and this many frame bytes are
        // present, so the range is inside the staging buffer.
        let frame = &self.tx_buf[VIRTIO_NET_HDR_LEN..VIRTIO_NET_HDR_LEN + frame_len];
        match self.backend.write_frame(frame) {
            Ok(0) => {
                NetStats::bump(&self.stats.tx_dropped_backend);
                tracing::debug!(
                    backend = self.backend.name(),
                    frame_len,
                    "host backend is congested; dropping a transmitted frame"
                );
            }
            Ok(written) => {
                NetStats::bump(&self.stats.tx_frames);
                NetStats::add(&self.stats.tx_bytes, written as u64);
                if written != frame_len {
                    tracing::warn!(
                        backend = self.backend.name(),
                        frame_len,
                        written,
                        "host backend accepted only part of a frame"
                    );
                }
            }
            Err(error) => {
                NetStats::bump(&self.stats.tx_dropped_backend);
                tracing::warn!(
                    backend = self.backend.name(),
                    frame_len,
                    %error,
                    "host backend refused a transmitted frame"
                );
            }
        }
    }

    /// Copies a TX chain's device-readable bytes into the staging buffer.
    ///
    /// Returns the number of bytes gathered, or `None` when the chain is
    /// unusable. The header may be split across descriptors — the guest is free
    /// to lay the chain out however it likes — so the whole thing is
    /// concatenated before anything is parsed.
    fn gather_tx(
        &mut self,
        mem: &GuestMem,
        desc_table: u64,
        queue_size: u16,
        head: u16,
    ) -> Option<usize> {
        let segments = match chain::walk(mem, desc_table, queue_size, head) {
            Ok(segments) => segments,
            Err(error) => {
                tracing::warn!(
                    backend = self.backend.name(),
                    head,
                    %error,
                    "dropping malformed virtio-net TX chain"
                );
                NetStats::bump(&self.stats.tx_dropped_invalid);
                return None;
            }
        };
        let (readable, writable) = match chain::split_rw(&segments) {
            Ok(split) => split,
            Err(error) => {
                tracing::warn!(
                    backend = self.backend.name(),
                    head,
                    %error,
                    "dropping virtio-net TX chain"
                );
                NetStats::bump(&self.stats.tx_dropped_invalid);
                return None;
            }
        };
        if !writable.is_empty() {
            // Not fatal — the spec says TX chains are device-readable only, so
            // trailing writable buffers are simply not frame data.
            tracing::debug!(
                backend = self.backend.name(),
                head,
                count = writable.len(),
                "ignoring device-writable buffers in a virtio-net TX chain"
            );
        }

        let mut filled = 0usize;
        for segment in readable {
            let len = segment.len as usize;
            if len == 0 {
                continue;
            }
            let end = filled.checked_add(len).filter(|end| *end <= MAX_BUFFER_LEN);
            let Some(end) = end else {
                tracing::warn!(
                    backend = self.backend.name(),
                    head,
                    claimed = len,
                    filled,
                    cap = MAX_BUFFER_LEN,
                    "dropping oversized virtio-net TX chain"
                );
                NetStats::bump(&self.stats.tx_dropped_invalid);
                return None;
            };
            if let Err(error) =
                mem.read_slice(&mut self.tx_buf[filled..end], GuestAddress(segment.addr))
            {
                tracing::warn!(
                    backend = self.backend.name(),
                    head,
                    addr = format_args!("{:#x}", segment.addr),
                    len,
                    %error,
                    "virtio-net TX buffer is not readable guest memory"
                );
                NetStats::bump(&self.stats.tx_dropped_invalid);
                return None;
            }
            filled = end;
        }
        Some(filled)
    }

    fn drop_tx(&self, head: u16, error: &FrameError) {
        NetStats::bump(&self.stats.tx_dropped_invalid);
        tracing::warn!(
            backend = self.backend.name(),
            head,
            %error,
            "dropping invalid virtio-net TX frame"
        );
    }

    // --------------------------------------------------------------- receive

    /// Stops and joins the RX worker. Idempotent and infallible, so both
    /// [`VirtioDevice::reset`] and `Drop` can call it.
    fn stop_rx(&mut self) {
        let Some(worker) = self.rx.take() else {
            return;
        };
        worker.stop.store(true, Ordering::Release);
        // Two places it could be waiting: the backend poll, and the pause gate
        // (a device reset runs on a quiesced VM — ADR-0005). Wake both, or the
        // join below is a deadlock.
        worker.quiesce.wake();
        // Break the worker out of its poll immediately; without this it would
        // notice the flag only after RX_POLL_TICK.
        if let Err(error) = self.backend.wake() {
            tracing::warn!(
                backend = self.backend.name(),
                %error,
                "cannot wake the virtio-net RX worker; waiting for its poll timeout"
            );
        }
        match worker.thread.join() {
            Ok(()) => tracing::debug!(
                backend = self.backend.name(),
                "virtio-net RX worker stopped"
            ),
            // A panicking device thread is a host bug, never guest-triggered;
            // log it loudly rather than re-raising it on the vCPU thread.
            Err(_) => tracing::error!(
                backend = self.backend.name(),
                "virtio-net RX worker panicked"
            ),
        }
    }
}

impl Drop for NetDevice {
    fn drop(&mut self) {
        // Guarantees "closing the VM leaves no worker threads behind" even if
        // the driver never reset the device. The backend descriptor is released
        // right after, when the last Arc goes.
        self.stop_rx();
    }
}

/// The RX worker: block, read whatever the host delivered, hand it to the guest.
fn rx_loop(ctx: RxContext) {
    // Header (all zeroes) up front, frames read straight behind it, so what
    // goes into guest memory is one contiguous buffer. One byte of slack lets
    // an oversized frame be detected instead of silently truncated.
    let mut staging = vec![0u8; MAX_BUFFER_LEN + 1];
    staging[..VIRTIO_NET_HDR_LEN].copy_from_slice(&NetHeader::rx());

    while !ctx.stop.load(Ordering::Acquire) {
        // Nothing below this line may touch guest memory while the VM is
        // paused. Parking here rather than after the read is deliberate: a frame
        // already taken off the host socket would have nowhere to go.
        let Some(_pass) = ctx
            .quiesce
            .wait_while_paused(|| !ctx.stop.load(Ordering::Acquire))
        else {
            return;
        };
        // Held for the whole poll-and-deliver round below: a pause is not
        // acknowledged while a frame is half-way into the RX ring.
        match ctx.backend.wait_readable(RX_POLL_TICK) {
            Ok(Readiness::Readable) => {}
            Ok(Readiness::TimedOut) | Ok(Readiness::WokenUp) => continue,
            Err(error) => {
                tracing::error!(
                    backend = ctx.backend.name(),
                    %error,
                    "virtio-net RX poll failed; receive path is down until reset"
                );
                return;
            }
        }

        for _ in 0..RX_FRAMES_PER_WAKE {
            if ctx.stop.load(Ordering::Acquire) {
                return;
            }
            let read = match ctx.backend.read_frame(&mut staging[VIRTIO_NET_HDR_LEN..]) {
                Ok(Some(len)) => len,
                // Nothing pending: back to the poll.
                Ok(None) => break,
                Err(error) => {
                    tracing::error!(
                        backend = ctx.backend.name(),
                        %error,
                        "virtio-net RX read failed; receive path is down until reset"
                    );
                    return;
                }
            };
            if read > MAX_FRAME_LEN {
                NetStats::bump(&ctx.stats.rx_dropped_oversized);
                tracing::warn!(
                    backend = ctx.backend.name(),
                    len = read,
                    cap = MAX_FRAME_LEN,
                    "dropping an oversized frame from the host"
                );
                continue;
            }
            if read == 0 {
                continue;
            }
            deliver_rx(&ctx, VIRTIO_NET_HDR_LEN + read, &staging);
        }
    }
}

/// Hands `staging[..len]` (header + frame) to the guest's RX queue.
fn deliver_rx(ctx: &RxContext, len: usize, staging: &[u8]) {
    let buffer = &staging[..len];
    let mut queue = lock(&ctx.queue);

    let Some(head) = queue
        .pop_descriptor_chain(Arc::clone(&ctx.mem))
        .map(|chain| chain.head_index())
    else {
        // The guest has not posted RX buffers. Dropping is the only option a
        // NIC has; the counter is how an operator notices.
        NetStats::bump(&ctx.stats.rx_dropped_no_buffer);
        tracing::debug!(
            backend = ctx.backend.name(),
            len,
            "no available RX buffer; dropping a received frame"
        );
        return;
    };
    let desc_table = queue.desc_table();
    let queue_size = queue.size();
    let written = write_rx_chain(ctx, desc_table, queue_size, head, buffer);

    if let Err(error) = queue.add_used(ctx.mem.as_ref(), head, written) {
        tracing::error!(
            backend = ctx.backend.name(),
            head,
            %error,
            "cannot publish a received frame on the RX used ring"
        );
        return;
    }
    match queue.needs_notification(ctx.mem.as_ref()) {
        Ok(false) => (),
        Ok(true) => {
            if let Err(error) = ctx.interrupt.signal_used_queue(RX_QUEUE) {
                tracing::warn!(
                    backend = ctx.backend.name(),
                    %error,
                    "cannot signal the RX interrupt"
                );
            }
        }
        Err(error) => tracing::warn!(
            backend = ctx.backend.name(),
            %error,
            "cannot evaluate the RX notification suppression state"
        ),
    }
}

/// Scatters `buffer` across one RX chain's device-writable segments and returns
/// how many bytes reached the guest (0 when the chain was unusable).
fn write_rx_chain(
    ctx: &RxContext,
    desc_table: u64,
    queue_size: u16,
    head: u16,
    buffer: &[u8],
) -> u32 {
    let segments = match chain::walk(ctx.mem.as_ref(), desc_table, queue_size, head) {
        Ok(segments) => segments,
        Err(error) => {
            NetStats::bump(&ctx.stats.rx_dropped_bad_chain);
            tracing::warn!(
                backend = ctx.backend.name(),
                head,
                %error,
                "dropping a received frame: malformed RX chain"
            );
            return 0;
        }
    };
    let (readable, writable) = match chain::split_rw(&segments) {
        Ok(split) => split,
        Err(error) => {
            NetStats::bump(&ctx.stats.rx_dropped_bad_chain);
            tracing::warn!(
                backend = ctx.backend.name(),
                head,
                %error,
                "dropping a received frame: malformed RX chain"
            );
            return 0;
        }
    };
    if !readable.is_empty() {
        tracing::debug!(
            backend = ctx.backend.name(),
            head,
            count = readable.len(),
            "ignoring device-readable buffers in a virtio-net RX chain"
        );
    }

    let capacity = writable
        .iter()
        .fold(0usize, |acc, s| acc.saturating_add(s.len as usize));
    if capacity < buffer.len() {
        // Without VIRTIO_NET_F_MRG_RXBUF a frame may not span chains, so a
        // chain too small for header+frame means the frame is lost.
        NetStats::bump(&ctx.stats.rx_dropped_no_space);
        tracing::warn!(
            backend = ctx.backend.name(),
            head,
            capacity,
            needed = buffer.len(),
            "dropping a received frame: RX chain too small"
        );
        return 0;
    }

    let mut offset = 0usize;
    for segment in writable {
        let remaining = buffer.len() - offset;
        if remaining == 0 {
            break;
        }
        let take = (segment.len as usize).min(remaining);
        if take == 0 {
            continue;
        }
        if let Err(error) = ctx
            .mem
            .write_slice(&buffer[offset..offset + take], GuestAddress(segment.addr))
        {
            NetStats::bump(&ctx.stats.rx_dropped_bad_chain);
            tracing::warn!(
                backend = ctx.backend.name(),
                head,
                addr = format_args!("{:#x}", segment.addr),
                len = segment.len,
                %error,
                "dropping a received frame: RX buffer is not writable guest memory"
            );
            // Report zero bytes: a partially filled buffer must not look like a
            // valid frame to the driver.
            return 0;
        }
        offset += take;
    }

    NetStats::bump(&ctx.stats.rx_frames);
    NetStats::add(
        &ctx.stats.rx_bytes,
        (offset.saturating_sub(VIRTIO_NET_HDR_LEN)) as u64,
    );
    u32::try_from(offset).unwrap_or(u32::MAX)
}

impl VirtioDevice for NetDevice {
    fn device_type(&self) -> DeviceType {
        DeviceType::Net
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUE_MAX_SIZES
    }

    fn device_features(&self) -> u64 {
        FEATURES
    }

    fn ack_features(&mut self, negotiated: u64) -> bool {
        if negotiated & VIRTIO_F_VERSION_1 == 0 {
            return false;
        }
        if negotiated & !FEATURES != 0 {
            tracing::warn!(
                backend = self.backend.name(),
                negotiated = format_args!("{negotiated:#x}"),
                offered = format_args!("{FEATURES:#x}"),
                "driver accepted virtio-net features the device never offered"
            );
            return false;
        }
        self.acked_features = negotiated;
        true
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let config = self.mac.0;
        debug_assert_eq!(config.len(), CONFIG_LEN);
        for (i, byte) in data.iter_mut().enumerate() {
            let index = offset.saturating_add(i as u64);
            *byte = usize::try_from(index)
                .ok()
                .and_then(|i| config.get(i))
                .copied()
                .unwrap_or(0);
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        // With VIRTIO_NET_F_MAC the MAC is read-only for the driver, and it is
        // the host's identity for the interface either way.
        tracing::warn!(
            backend = self.backend.name(),
            offset,
            len = data.len(),
            "ignoring guest write to the read-only virtio-net config space"
        );
    }

    fn activate(&mut self, resources: DeviceResources) -> Result<(), DeviceError> {
        if resources.queues.len() != NUM_QUEUES {
            return Err(DeviceError::QueueCount {
                expected: NUM_QUEUES,
                actual: resources.queues.len(),
            });
        }
        // Re-activation without a reset in between must not leak a worker.
        self.stop_rx();

        let mut queues = resources.queues.into_iter();
        let (Some(rx_queue), Some(tx_queue)) = (queues.next(), queues.next()) else {
            // Unreachable: the length was just checked.
            return Err(DeviceError::QueueCount {
                expected: NUM_QUEUES,
                actual: 0,
            });
        };

        let stop = Arc::new(AtomicBool::new(false));
        let context = RxContext {
            backend: Arc::clone(&self.backend),
            mem: Arc::clone(&resources.mem),
            queue: Arc::new(Mutex::new(rx_queue)),
            interrupt: Arc::clone(&resources.interrupt),
            stats: Arc::clone(&self.stats),
            stop: Arc::clone(&stop),
            quiesce: Arc::clone(&resources.quiesce),
        };
        let thread = std::thread::Builder::new()
            .name("entangled-net-rx".to_owned())
            .spawn(move || rx_loop(context))
            .map_err(|error| {
                DeviceError::Backend(format!("cannot spawn the virtio-net RX worker: {error}"))
            })?;

        self.rx = Some(RxWorker {
            stop,
            quiesce: Arc::clone(&resources.quiesce),
            thread,
        });
        self.tx_queue = Some(tx_queue);
        self.mem = Some(resources.mem);
        self.interrupt = Some(resources.interrupt);
        tracing::info!(
            backend = self.backend.name(),
            mac = %self.mac,
            "virtio-net ready"
        );
        Ok(())
    }

    fn notify(&mut self, queue_index: u16) -> Result<(), DeviceError> {
        match queue_index {
            RX_QUEUE => {
                if self.rx.is_none() {
                    return Err(DeviceError::NotActivated);
                }
                // The driver refilled the RX ring. Nothing to do: the worker
                // takes buffers as frames arrive, and a frame that arrived
                // while the ring was empty is already gone.
                Ok(())
            }
            TX_QUEUE => {
                let mem = self.mem.clone().ok_or(DeviceError::NotActivated)?;
                let interrupt = self.interrupt.clone().ok_or(DeviceError::NotActivated)?;
                // Taken out so `self` stays mutably usable while draining;
                // always put back, even on error.
                let mut queue = self.tx_queue.take().ok_or(DeviceError::NotActivated)?;
                let result = self.drain_tx(&mut queue, &mem, interrupt.as_ref());
                self.tx_queue = Some(queue);
                result
            }
            other => Err(DeviceError::UnknownQueue(other)),
        }
    }

    fn reset(&mut self) {
        // Order matters: stop the worker first so nothing touches the queues or
        // guest memory after they are dropped.
        self.stop_rx();
        self.tx_queue = None;
        self.mem = None;
        self.interrupt = None;
        self.acked_features = 0;
        // The backend (and with it the TAP descriptor) deliberately survives:
        // a driver may reset and re-initialise, and re-creating the interface
        // could need privileges the VMM does not have. It is released when the
        // device is dropped.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::NetError;

    /// A backend that swallows everything, for the pure-logic tests here. The
    /// queue round trips live in `tests/net_queue.rs`.
    struct NullBackend;

    impl NetBackend for NullBackend {
        fn name(&self) -> &str {
            "null"
        }

        fn write_frame(&self, frame: &[u8]) -> Result<usize, NetError> {
            Ok(frame.len())
        }

        fn read_frame(&self, _buf: &mut [u8]) -> Result<Option<usize>, NetError> {
            Ok(None)
        }

        fn wait_readable(&self, _timeout: Duration) -> Result<Readiness, NetError> {
            Ok(Readiness::TimedOut)
        }
    }

    fn device() -> NetDevice {
        NetDevice::new(NullBackend, MacAddr([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]))
    }

    #[test]
    fn advertises_exactly_version_1_and_mac() {
        let device = device();
        assert_eq!(device.device_features(), FEATURES);
        assert_eq!(device.device_features(), VIRTIO_F_VERSION_1 | (1 << 5));
        // Every feature Entangled Desktop must NOT offer in the MVP.
        for (bit, name) in [
            (0u64, "CSUM"),
            (1, "GUEST_CSUM"),
            (7, "GUEST_TSO4"),
            (8, "GUEST_TSO6"),
            (10, "GUEST_UFO"),
            (11, "HOST_TSO4"),
            (12, "HOST_TSO6"),
            (14, "HOST_UFO"),
            (15, "MRG_RXBUF"),
            (16, "STATUS"),
            (17, "CTRL_VQ"),
            (21, "GUEST_ANNOUNCE"),
            (22, "MQ"),
            (25, "MTU"),
        ] {
            assert_eq!(
                device.device_features() & (1 << bit),
                0,
                "VIRTIO_NET_F_{name} must not be offered"
            );
        }
    }

    #[test]
    fn exposes_two_queues_rx_first() {
        let device = device();
        assert_eq!(device.device_type(), DeviceType::Net);
        assert_eq!(device.num_queues(), 2);
        assert_eq!(device.queue_max_sizes(), &[MAX_QUEUE_SIZE, MAX_QUEUE_SIZE]);
        assert_eq!(RX_QUEUE, 0);
        assert_eq!(TX_QUEUE, 1);
    }

    #[test]
    fn feature_negotiation_is_exact() {
        let mut device = device();
        // The full offered set is fine, as is dropping the optional MAC bit.
        assert!(device.ack_features(FEATURES));
        assert!(device.ack_features(VIRTIO_F_VERSION_1));
        // Legacy drivers and drivers claiming features we never offered are not.
        assert!(!device.ack_features(0));
        assert!(!device.ack_features(VIRTIO_NET_F_MAC));
        assert!(!device.ack_features(FEATURES | (1 << 15)));
        assert!(!device.ack_features(u64::MAX));
    }

    #[test]
    fn config_space_is_the_mac_in_wire_order() {
        let mac = MacAddr([0x52, 0x54, 0x00, 0xab, 0xcd, 0xef]);
        let device = NetDevice::new(NullBackend, mac);

        let mut config = [0xffu8; CONFIG_LEN];
        device.read_config(0, &mut config);
        assert_eq!(config, mac.0, "byte 0 of the config space is mac[0]");

        // Byte-at-a-time reads, the way a driver may do it.
        for (i, expected) in mac.0.iter().enumerate() {
            let mut byte = [0xffu8; 1];
            device.read_config(i as u64, &mut byte);
            assert_eq!(byte[0], *expected);
        }

        // Reads past the end are zeroes, never a panic — including a read that
        // straddles the end and one at a wild offset.
        let mut straddle = [0xffu8; 8];
        device.read_config(2, &mut straddle);
        assert_eq!(straddle, [0x00, 0xab, 0xcd, 0xef, 0, 0, 0, 0]);
        let mut far = [0xffu8; 4];
        device.read_config(u64::MAX, &mut far);
        assert_eq!(far, [0u8; 4]);
    }

    #[test]
    fn config_writes_are_ignored() {
        let mac = MacAddr([0x52, 0, 0, 0, 0, 1]);
        let mut device = NetDevice::new(NullBackend, mac);
        device.write_config(0, &[0xde, 0xad, 0xbe, 0xef, 0, 0]);
        let mut config = [0u8; CONFIG_LEN];
        device.read_config(0, &mut config);
        assert_eq!(config, mac.0);
        assert_eq!(device.mac(), mac);
    }

    #[test]
    fn notify_before_activation_is_an_error_not_a_panic() {
        let mut device = device();
        assert!(!device.is_receiving());
        for queue in [RX_QUEUE, TX_QUEUE, 2, 7, u16::MAX] {
            assert!(device.notify(queue).is_err(), "queue {queue}");
        }
    }

    #[test]
    fn reset_on_an_inactive_device_is_harmless() {
        let mut device = device();
        device.reset();
        device.reset();
        assert!(!device.is_receiving());
        assert_eq!(device.stats().rx_frames(), 0);
    }

    #[test]
    fn wrong_queue_count_is_refused() {
        // Guards the transport contract: RX and TX, in that order.
        assert_eq!(NUM_QUEUES, 2);
        assert_eq!(QUEUE_MAX_SIZES.len(), NUM_QUEUES);
    }

    #[test]
    fn stats_start_at_zero_and_aggregate() {
        let stats = NetStats::default();
        assert_eq!(stats.rx_dropped(), 0);
        assert_eq!(stats.tx_dropped(), 0);
        NetStats::bump(&stats.rx_dropped_no_buffer);
        NetStats::bump(&stats.rx_dropped_no_space);
        NetStats::bump(&stats.rx_dropped_oversized);
        NetStats::bump(&stats.rx_dropped_bad_chain);
        NetStats::bump(&stats.tx_dropped_invalid);
        NetStats::bump(&stats.tx_dropped_backend);
        assert_eq!(stats.rx_dropped(), 4);
        assert_eq!(stats.tx_dropped(), 2);
    }
}
