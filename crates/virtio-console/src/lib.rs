//! virtio-console with one named port: the channel a guest agent talks to
//! the host over (ADR-0008, the host–guest clipboard).
//!
//! The device is multiport (`VIRTIO_CONSOLE_F_MULTIPORT`) with two ports:
//! port 0, the console every virtio-console has, which this one leaves
//! silent, and port 1, named [`ConsoleDevice::new`]'s name. A Windows guest
//! with virtio-win's `vioserial` sees port 1 as `\\.\Global\<name>`, a Linux
//! guest as `/dev/virtio-ports/<name>`. Six queues, in the spec's order:
//!
//! | Queue | |
//! |---|---|
//! | 0, 1 | port 0 receive / transmit |
//! | 2, 3 | control receive (device → driver) / transmit (driver → device) |
//! | 4, 5 | port 1 receive / transmit |
//!
//! The host side is a [`PortHandle`]: bytes the guest writes to the port
//! arrive through [`PortHandle::receive`], bytes the host sends are queued
//! and delivered into the guest's receive buffers as they come. The port is
//! a byte stream; framing is the agent protocol's business.
//!
//! Everything the guest supplies — chains, lengths, control messages — is
//! bounded and read through `vm-memory`; a malformed chain is returned
//! unused, never a panic.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use virtio_core::chain::{self, Segment};
use virtio_core::{
    DeviceError, DeviceResources, DeviceType, GuestMem, Interrupt, Quiesce, VirtioDevice,
    VIRTIO_F_VERSION_1,
};
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress};

/// `VIRTIO_CONSOLE_F_MULTIPORT`.
pub const VIRTIO_CONSOLE_F_MULTIPORT: u64 = 1 << 1;
/// The two ports.
const PORTS: u32 = 2;
/// The agent's port.
const AGENT_PORT: u32 = 1;
const QUEUE_SIZE: u16 = 128;
const NUM_QUEUES: usize = 6;
const CONTROL_RX: u16 = 2;
const CONTROL_TX: u16 = 3;
const AGENT_RX: u16 = 4;
const AGENT_TX: u16 = 5;
/// Port 0's transmit queue: whatever arrives is dropped.
const CONSOLE_TX: u16 = 1;

/// Control events (virtio 1.2 §5.3.6.2).
mod event {
    pub const DEVICE_READY: u16 = 0;
    pub const DEVICE_ADD: u16 = 1;
    pub const PORT_READY: u16 = 3;
    pub const PORT_OPEN: u16 = 6;
    pub const PORT_NAME: u16 = 7;
}
/// Size of `struct virtio_console_control`.
const CONTROL_LEN: usize = 8;

/// Most bytes the host keeps for a guest that is not reading: 1 MiB.
const MAX_PENDING: usize = 1 << 20;
/// Most bytes taken from one guest transmit chain.
const MAX_CHAIN_BYTES: u32 = 1 << 20;
/// Chains served per notification, so a flood cannot hold the vCPU.
const CHAINS_PER_DRAIN: usize = 256;

/// What the host receives from the guest.
pub type Receiver = Arc<dyn Fn(&[u8]) + Send + Sync>;

struct Active {
    mem: Arc<GuestMem>,
    interrupt: Arc<dyn Interrupt>,
    quiesce: Arc<Quiesce>,
    queues: Vec<Queue>,
}

#[derive(Default)]
struct State {
    active: Option<Active>,
    /// Control messages waiting for a control receive buffer.
    control_out: VecDeque<Vec<u8>>,
    /// Host bytes waiting for an agent receive buffer.
    pending: VecDeque<u8>,
    /// Whether the guest has the port open.
    guest_open: bool,
    receiver: Option<Receiver>,
}

struct Shared {
    name: String,
    state: Mutex<State>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The device.
pub struct ConsoleDevice {
    shared: Arc<Shared>,
    acked_features: u64,
}

/// The host's end of the named port.
#[derive(Clone)]
pub struct PortHandle {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for PortHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortHandle")
            .field("name", &self.shared.name)
            .finish_non_exhaustive()
    }
}

impl ConsoleDevice {
    /// A console whose port 1 is called `name`.
    pub fn new(name: &str) -> Self {
        Self {
            shared: Arc::new(Shared {
                name: name.to_owned(),
                state: Mutex::new(State::default()),
            }),
            acked_features: 0,
        }
    }

    pub fn handle(&self) -> PortHandle {
        PortHandle {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl PortHandle {
    /// Where the guest's bytes go. Called on the vCPU (or kick worker)
    /// thread that served the guest's transmit; it must not block.
    pub fn receive(&self, receiver: Receiver) {
        self.shared.lock().receiver = Some(receiver);
    }

    /// Whether a guest program has the port open.
    pub fn is_open(&self) -> bool {
        self.shared.lock().guest_open
    }

    /// Queues `bytes` for the guest and delivers what its buffers can take.
    /// Answers `false` when the host buffer is full and nothing was queued.
    pub fn send(&self, bytes: &[u8]) -> bool {
        let mut state = self.shared.lock();
        if state.pending.len() + bytes.len() > MAX_PENDING {
            return false;
        }
        state.pending.extend(bytes);
        // A host thread: only past the pause gate (ADR-0005). Paused, the
        // bytes wait for the guest's next kick or the next send.
        let pass = state.active.as_ref().map(|a| Arc::clone(&a.quiesce));
        if let Some(quiesce) = pass {
            if let Some(_pass) = quiesce.try_enter() {
                if let Err(error) = drain_agent_rx(&mut state) {
                    tracing::warn!(%error, "virtio-console delivery failed");
                }
            }
        }
        true
    }
}

fn walk(mem: &GuestMem, queue: &Queue, head: u16) -> Result<Vec<Segment>, chain::ChainError> {
    chain::walk(mem, queue.desc_table(), queue.size(), head)
}

/// Pops a chain from `queue`, hands its segments to `f`, and returns it with
/// the byte count `f` reports. Answers how many chains were used.
fn serve(
    active: &mut Active,
    index: u16,
    max_chains: usize,
    mut f: impl FnMut(&GuestMem, &[Segment], &[Segment]) -> Option<u32>,
) -> Result<usize, DeviceError> {
    let mem = Arc::clone(&active.mem);
    let mut served = 0;
    while served < max_chains.min(CHAINS_PER_DRAIN) {
        let queue = &mut active.queues[usize::from(index)];
        let Some(head) = queue
            .pop_descriptor_chain(Arc::clone(&mem))
            .map(|c| c.head_index())
        else {
            break;
        };
        let written = match walk(&mem, queue, head).ok().and_then(|segments| {
            let (readable, writable) = chain::split_rw(&segments).ok()?;
            f(&mem, readable, writable)
        }) {
            Some(n) => n,
            None => {
                tracing::warn!(queue = index, head, "unusable virtio-console buffer");
                0
            }
        };
        queue
            .add_used(mem.as_ref(), head, written)
            .map_err(|e| DeviceError::Queue(e.to_string()))?;
        served += 1;
    }
    if served > 0
        && active.queues[usize::from(index)]
            .needs_notification(mem.as_ref())
            .map_err(|e| DeviceError::Queue(e.to_string()))?
    {
        active.interrupt.signal_used_queue(index)?;
    }
    Ok(served)
}

/// Copies `bytes` into the writable segments; answers how many fitted.
fn fill(mem: &GuestMem, writable: &[Segment], bytes: &[u8]) -> Option<u32> {
    let mut done = 0usize;
    for seg in writable {
        if done == bytes.len() {
            break;
        }
        let take = (seg.len as usize).min(bytes.len() - done);
        mem.write_slice(&bytes[done..done + take], GuestAddress(seg.addr))
            .ok()?;
        done += take;
    }
    u32::try_from(done).ok()
}

/// Reads the readable segments, up to `max` bytes.
fn gather(mem: &GuestMem, readable: &[Segment], max: u32) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for seg in readable {
        let take = seg.len.min(max.saturating_sub(out.len() as u32));
        if take == 0 {
            break;
        }
        let at = out.len();
        out.resize(at + take as usize, 0);
        mem.read_slice(&mut out[at..], GuestAddress(seg.addr))
            .ok()?;
    }
    Some(out)
}

fn drain_agent_rx(state: &mut State) -> Result<(), DeviceError> {
    let State {
        active, pending, ..
    } = state;
    let Some(active) = active.as_mut() else {
        return Ok(());
    };
    if pending.is_empty() {
        return Ok(());
    }
    // One buffer at a time, and only while there is something to put in
    // it: a buffer taken with nothing to write is a buffer the guest loses.
    for _ in 0..CHAINS_PER_DRAIN {
        if pending.is_empty() {
            break;
        }
        let served = serve(active, AGENT_RX, 1, |mem, readable, writable| {
            if !readable.is_empty() {
                return Some(0);
            }
            let (a, b) = pending.as_slices();
            let mut chunk = a.to_vec();
            chunk.extend_from_slice(b);
            let n = fill(mem, writable, &chunk)?;
            pending.drain(..n as usize);
            Some(n)
        })?;
        if served == 0 {
            break;
        }
    }
    Ok(())
}

fn drain_control_rx(state: &mut State) -> Result<(), DeviceError> {
    let State {
        active,
        control_out,
        ..
    } = state;
    let Some(active) = active.as_mut() else {
        return Ok(());
    };
    if control_out.is_empty() {
        return Ok(());
    }
    for _ in 0..CHAINS_PER_DRAIN {
        let Some(message) = control_out.front().cloned() else {
            break;
        };
        let served = serve(active, CONTROL_RX, 1, |mem, readable, writable| {
            let room: u32 = writable.iter().map(|s| s.len).sum();
            if !readable.is_empty() || (room as usize) < message.len() {
                // Unusable for this message: returned unused, and the
                // message waits for the next buffer.
                return Some(0);
            }
            let n = fill(mem, writable, &message)?;
            control_out.pop_front();
            Some(n)
        })?;
        if served == 0 {
            break;
        }
    }
    Ok(())
}

fn control(id: u32, event: u16, value: u16) -> Vec<u8> {
    let mut m = Vec::with_capacity(CONTROL_LEN);
    m.extend_from_slice(&id.to_le_bytes());
    m.extend_from_slice(&event.to_le_bytes());
    m.extend_from_slice(&value.to_le_bytes());
    m
}

/// Acts on one control message from the driver.
fn on_control(state: &mut State, name: &str, message: &[u8]) {
    if message.len() < CONTROL_LEN {
        return;
    }
    let id = u32::from_le_bytes([message[0], message[1], message[2], message[3]]);
    let event = u16::from_le_bytes([message[4], message[5]]);
    let value = u16::from_le_bytes([message[6], message[7]]);
    tracing::debug!(id, event, value, "virtio-console control from the driver");
    match event {
        event::DEVICE_READY if value == 1 => {
            for port in 0..PORTS {
                state
                    .control_out
                    .push_back(control(port, event::DEVICE_ADD, 0));
            }
        }
        event::PORT_READY if value == 1 && id == AGENT_PORT => {
            let mut named = control(id, event::PORT_NAME, 0);
            named.extend_from_slice(name.as_bytes());
            state.control_out.push_back(named);
            // The host end is always open.
            state
                .control_out
                .push_back(control(id, event::PORT_OPEN, 1));
        }
        event::PORT_OPEN if id == AGENT_PORT => {
            state.guest_open = value == 1;
            tracing::info!(port = name, open = state.guest_open, "virtio-console port");
        }
        _ => {}
    }
}

impl VirtioDevice for ConsoleDevice {
    fn device_type(&self) -> DeviceType {
        DeviceType::Console
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &[QUEUE_SIZE; NUM_QUEUES]
    }

    fn device_features(&self) -> u64 {
        VIRTIO_F_VERSION_1 | VIRTIO_CONSOLE_F_MULTIPORT
    }

    fn ack_features(&mut self, negotiated: u64) -> bool {
        self.acked_features = negotiated;
        true
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // cols, rows (u16 each), max_nr_ports (u32), emerg_wr (u32).
        let mut config = [0u8; 12];
        config[4..8].copy_from_slice(&PORTS.to_le_bytes());
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = usize::try_from(offset)
                .ok()
                .and_then(|o| config.get(o + i))
                .copied()
                .unwrap_or(0);
        }
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {
        // emerg_wr: an emergency console write nobody reads.
    }

    fn activate(&mut self, resources: DeviceResources) -> Result<(), DeviceError> {
        if resources.queues.len() != NUM_QUEUES {
            return Err(DeviceError::QueueCount {
                expected: NUM_QUEUES,
                actual: resources.queues.len(),
            });
        }
        let mut state = self.shared.lock();
        state.control_out.clear();
        state.guest_open = false;
        state.active = Some(Active {
            mem: resources.mem,
            interrupt: resources.interrupt,
            quiesce: resources.quiesce,
            queues: resources.queues,
        });
        tracing::info!(port = %self.shared.name, "virtio-console ready");
        Ok(())
    }

    fn notify(&mut self, queue_index: u16) -> Result<(), DeviceError> {
        let mut state = self.shared.lock();
        if state.active.is_none() {
            return Err(DeviceError::NotActivated);
        }
        match queue_index {
            CONTROL_TX => {
                let mut messages = Vec::new();
                if let Some(active) = state.active.as_mut() {
                    serve(active, CONTROL_TX, CHAINS_PER_DRAIN, |mem, readable, _| {
                        messages.push(gather(mem, readable, 4096)?);
                        Some(0)
                    })?;
                }
                let name = self.shared.name.clone();
                for message in messages {
                    on_control(&mut state, &name, &message);
                }
                drain_control_rx(&mut state)
            }
            CONTROL_RX => drain_control_rx(&mut state),
            AGENT_RX => drain_agent_rx(&mut state),
            AGENT_TX => {
                let mut received = Vec::new();
                if let Some(active) = state.active.as_mut() {
                    serve(active, AGENT_TX, CHAINS_PER_DRAIN, |mem, readable, _| {
                        received.extend(gather(mem, readable, MAX_CHAIN_BYTES)?);
                        Some(0)
                    })?;
                }
                let receiver = state.receiver.clone();
                drop(state);
                if let (Some(receiver), false) = (receiver, received.is_empty()) {
                    receiver(&received);
                }
                Ok(())
            }
            CONSOLE_TX => {
                if let Some(active) = state.active.as_mut() {
                    serve(active, CONSOLE_TX, CHAINS_PER_DRAIN, |_, _, _| Some(0))?;
                }
                Ok(())
            }
            0 => Ok(()),
            other => Err(DeviceError::UnknownQueue(other)),
        }
    }

    fn queue_positions(&self) -> Vec<virtio_core::QueuePosition> {
        let state = self.shared.lock();
        state
            .active
            .as_ref()
            .map(|active| {
                active
                    .queues
                    .iter()
                    .map(|q| virtio_core::QueuePosition {
                        next_avail: q.next_avail(),
                        next_used: q.next_used(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn reset(&mut self) {
        let mut state = self.shared.lock();
        state.active = None;
        state.control_out.clear();
        state.pending.clear();
        state.guest_open = false;
        self.acked_features = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use virtio_core::testing::{guest_memory, SplitRing, TestInterrupt};

    const WRITE: u16 = 2;

    struct Driver {
        mem: Arc<GuestMem>,
        rings: Vec<SplitRing>,
        device: ConsoleDevice,
    }

    impl Driver {
        fn new() -> Self {
            let mem = Arc::new(guest_memory(0x10_0000));
            let rings: Vec<SplitRing> = (0..NUM_QUEUES as u64)
                .map(|i| SplitRing::layout(0x1000 + i * 0x2000, 16))
                .collect();
            let queues = rings
                .iter()
                .map(|r| r.queue_config(QUEUE_SIZE).build(&mem).unwrap())
                .collect();
            let mut device = ConsoleDevice::new("org.entangled.agent.0");
            device
                .activate(DeviceResources {
                    mem: Arc::clone(&mem),
                    queues,
                    interrupt: Arc::new(TestInterrupt::default()),
                    quiesce: Quiesce::new(),
                })
                .unwrap();
            Self { mem, rings, device }
        }

        /// Posts one buffer of `len` at `addr` on queue `q`.
        fn post(&mut self, q: usize, desc: u16, addr: u64, len: u32, flags: u16) {
            self.rings[q].write_desc(&self.mem, desc, addr, len, flags, 0);
            self.rings[q].publish(&self.mem, desc);
        }

        fn used(&self, q: usize) -> u16 {
            self.rings[q].used_idx(&self.mem)
        }
    }

    fn message(id: u32, ev: u16, value: u16) -> Vec<u8> {
        control(id, ev, value)
    }

    #[test]
    fn the_handshake_names_and_opens_the_agent_port() {
        let mut d = Driver::new();
        for i in 0..4u16 {
            d.post(2, i, 0x40000 + u64::from(i) * 0x100, 0x100, WRITE);
        }
        d.mem
            .write_slice(&message(0, event::DEVICE_READY, 1), GuestAddress(0x50000))
            .unwrap();
        d.post(3, 0, 0x50000, 8, 0);
        d.device.notify(CONTROL_TX).unwrap();
        assert_eq!(d.used(2), 2, "two DEVICE_ADDs");
        d.mem
            .write_slice(&message(1, event::PORT_READY, 1), GuestAddress(0x50100))
            .unwrap();
        d.post(3, 1, 0x50100, 8, 0);
        d.device.notify(CONTROL_TX).unwrap();
        assert_eq!(d.used(2), 4, "PORT_NAME and PORT_OPEN");
        let mut named = [0u8; 8 + 21];
        d.mem.read_slice(&mut named, GuestAddress(0x40200)).unwrap();
        assert_eq!(u16::from_le_bytes([named[4], named[5]]), event::PORT_NAME);
        assert_eq!(&named[8..], b"org.entangled.agent.0");
        // The guest opens the port.
        d.mem
            .write_slice(&message(1, event::PORT_OPEN, 1), GuestAddress(0x50200))
            .unwrap();
        d.post(3, 2, 0x50200, 8, 0);
        d.device.notify(CONTROL_TX).unwrap();
        assert!(d.device.handle().is_open());
    }

    #[test]
    fn bytes_flow_both_ways() {
        let mut d = Driver::new();
        let got = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&got);
        let handle = d.device.handle();
        handle.receive(Arc::new(move |b: &[u8]| {
            sink.lock().unwrap().extend_from_slice(b)
        }));
        d.mem.write_slice(b"hello", GuestAddress(0x60000)).unwrap();
        d.post(5, 0, 0x60000, 5, 0);
        d.device.notify(AGENT_TX).unwrap();
        assert_eq!(&*got.lock().unwrap(), b"hello");
        // Host to guest: queued, then delivered into a posted buffer.
        assert!(handle.send(b"from host"));
        d.post(4, 0, 0x61000, 4, WRITE);
        d.post(4, 1, 0x61100, 64, WRITE);
        d.device.notify(AGENT_RX).unwrap();
        let mut a = [0u8; 4];
        let mut b = [0u8; 5];
        d.mem.read_slice(&mut a, GuestAddress(0x61000)).unwrap();
        d.mem.read_slice(&mut b, GuestAddress(0x61100)).unwrap();
        assert_eq!((&a, &b), (b"from", b" host"));
    }

    #[test]
    fn hostile_chains_are_returned_unused() {
        let mut d = Driver::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&calls);
        d.device.handle().receive(Arc::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        }));
        // A transmit buffer outside guest memory, and a looping chain.
        d.post(5, 0, 0xffff_ffff_0000, 64, 0);
        d.rings[5].write_desc(&d.mem, 1, 0x60000, 8, 1, 1);
        d.rings[5].publish(&d.mem, 1);
        d.device.notify(AGENT_TX).unwrap();
        assert_eq!(d.used(5), 2, "both returned");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(d.device.notify(9).is_err());
    }

    #[test]
    fn the_host_buffer_is_bounded() {
        let d = ConsoleDevice::new("x");
        let handle = d.handle();
        assert!(handle.send(&vec![0; MAX_PENDING]));
        assert!(!handle.send(b"one more"));
    }
}
