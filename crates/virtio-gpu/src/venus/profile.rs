//! The Venus **frame profile**: debug-level counters of where a ring's time
//! goes, cheap enough to leave in and off unless asked for (ADR-0004, the
//! amendment on CSS pages in hardware WebRender).
//!
//! A guest frame on Zink over Venus is a burst of ring work: the recording,
//! the submit, a few synchronous answers the driver waits for, and a sync
//! file for the compositor. Whether a frame is slow because of the GPU, the
//! host's latency per round trip or a worker asleep is decided by numbers
//! only these threads can see, so each keeps a window of them and logs it
//! at `debug` on [`TARGET`] every [`PERIOD`]:
//!
//! * [`CommandProfile`], per ring sink: every command's count, the host time
//!   it took and how many of them the guest waited on for a reply, and the
//!   ones that took longest. `vkExecuteCommandStreamsMESA` is counted apart,
//!   because its time is the sum of the commands inside it.
//! * [`WorkerProfile`], per ring worker: the time spent pumping, polling by
//!   yielding, sleeping in the backoff, parked on the doorbell and blocked
//!   on the virtqueue, and how many times the ring parked. A parked ring
//!   needs a doorbell through the kernel before it runs the next command.
//!
//! Nothing is measured unless the target is enabled when the ring is
//! adopted ([`enabled`]), so a production run pays one branch per command.
//! `RUST_LOG=info,virtio_gpu::venus::profile=debug` turns it on.

use std::time::{Duration, Instant};

/// The tracing target the profile logs on.
pub const TARGET: &str = "virtio_gpu::venus::profile";

/// How often a ring's window is logged and started again.
pub const PERIOD: Duration = Duration::from_secs(2);

/// Commands named in a window's line, the longest-running first.
pub const TOP: usize = 20;

/// Opcodes with a slot of their own; anything above shares the last slot.
/// Venus command types stop in the low 300s.
const SLOTS: usize = 512;

/// Whether the profile is on for the rings adopted from now on.
#[must_use]
pub fn enabled() -> bool {
    tracing::enabled!(target: "virtio_gpu::venus::profile", tracing::Level::DEBUG)
}

#[derive(Debug, Clone, Copy, Default)]
struct Slot {
    name: &'static str,
    count: u64,
    replies: u64,
    total: Duration,
    max: Duration,
}

/// One ring sink's window of commands. See the module docs.
#[derive(Debug)]
pub struct CommandProfile {
    slots: Vec<Slot>,
    streams: u64,
    stream_bytes: u64,
    stream_time: Duration,
    /// Time spent decoding commands.
    decode: Duration,
    /// Of the commands' time, the part inside the driver's calls.
    host: Duration,
    started: Instant,
}

impl CommandProfile {
    /// An empty window starting now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            slots: vec![Slot::default(); SLOTS],
            streams: 0,
            stream_bytes: 0,
            stream_time: Duration::ZERO,
            decode: Duration::ZERO,
            host: Duration::ZERO,
            started: Instant::now(),
        }
    }

    /// One command: its opcode, its name, whether the guest waits for its
    /// reply, and the host time it took.
    pub fn record(&mut self, opcode: u32, name: &'static str, reply: bool, took: Duration) {
        let index = usize::try_from(opcode).map_or(SLOTS - 1, |i| i.min(SLOTS - 1));
        if let Some(slot) = self.slots.get_mut(index) {
            slot.name = name;
            slot.count += 1;
            slot.replies += u64::from(reply);
            slot.total += took;
            slot.max = slot.max.max(took);
        }
    }

    /// One command's decode.
    pub fn record_decode(&mut self, took: Option<Duration>) {
        self.decode += took.unwrap_or_default();
    }

    /// The driver's share of one command's time.
    pub fn record_host(&mut self, took: Duration) {
        self.host += took;
    }

    /// One `vkExecuteCommandStreamsMESA` of `bytes`, whose commands are
    /// recorded one by one as well.
    pub fn record_streams(&mut self, bytes: u64, took: Duration) {
        self.streams += 1;
        self.stream_bytes += bytes;
        self.stream_time += took;
    }

    /// Whether the window is [`PERIOD`] old.
    #[must_use]
    pub fn due(&self) -> bool {
        self.started.elapsed() >= PERIOD
    }

    /// The window as one line: totals, then the [`TOP`] commands by host
    /// time as `name=count/total_ms/max_ms/replies`.
    #[must_use]
    pub fn summary(&self) -> CommandSummary {
        let mut used: Vec<&Slot> = self.slots.iter().filter(|s| s.count != 0).collect();
        used.sort_by_key(|s| std::cmp::Reverse(s.total));
        let mut top = String::new();
        for slot in used.iter().take(TOP) {
            if !top.is_empty() {
                top.push(' ');
            }
            top.push_str(&format!(
                "{}={}/{:.2}/{:.2}/{}",
                slot.name.strip_prefix("vk").unwrap_or(slot.name),
                slot.count,
                slot.total.as_secs_f64() * 1e3,
                slot.max.as_secs_f64() * 1e3,
                slot.replies
            ));
        }
        let (mut commands, mut replies, mut busy, mut reply_time) =
            (0, 0, Duration::ZERO, Duration::ZERO);
        for slot in &used {
            commands += slot.count;
            replies += slot.replies;
            busy += slot.total;
            if slot.replies != 0 {
                reply_time += slot.total;
            }
        }
        CommandSummary {
            window: self.started.elapsed(),
            commands,
            replies,
            busy,
            reply_time,
            streams: self.streams,
            stream_bytes: self.stream_bytes,
            stream_time: self.stream_time,
            decode: self.decode,
            host: self.host,
            top,
        }
    }

    /// Log the window if it is due and start the next one.
    pub fn log_if_due(&mut self, ctx_id: u32, ring: u64) {
        if !self.due() {
            return;
        }
        let s = self.summary();
        tracing::debug!(
            target: "virtio_gpu::venus::profile",
            ctx_id,
            ring = format_args!("{ring:#x}"),
            window_ms = s.window.as_millis(),
            commands = s.commands,
            replies = s.replies,
            busy_ms = format_args!("{:.2}", s.busy.as_secs_f64() * 1e3),
            reply_ms = format_args!("{:.2}", s.reply_time.as_secs_f64() * 1e3),
            streams = s.streams,
            stream_kib = s.stream_bytes / 1024,
            stream_ms = format_args!("{:.2}", s.stream_time.as_secs_f64() * 1e3),
            decode_ms = format_args!("{:.2}", s.decode.as_secs_f64() * 1e3),
            driver_ms = format_args!("{:.2}", s.host.as_secs_f64() * 1e3),
            top = %s.top,
            "venus ring commands"
        );
        *self = Self::new();
    }
}

impl Default for CommandProfile {
    fn default() -> Self {
        Self::new()
    }
}

/// What [`CommandProfile::summary`] makes of a window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSummary {
    /// How long the window has run.
    pub window: Duration,
    /// Commands executed, streams' contents included.
    pub commands: u64,
    /// Of those, the ones the guest waited on for a reply.
    pub replies: u64,
    /// Host time of every command.
    pub busy: Duration,
    /// Host time of the commands that had any reply.
    pub reply_time: Duration,
    /// `vkExecuteCommandStreamsMESA` calls.
    pub streams: u64,
    /// Bytes they named.
    pub stream_bytes: u64,
    /// Host time they took, their commands included.
    pub stream_time: Duration,
    /// Time spent decoding.
    pub decode: Duration,
    /// Of `busy`, the time inside the driver.
    pub host: Duration,
    /// The [`TOP`] commands by host time.
    pub top: String,
}

/// Where a ring worker's time went. See the module docs.
#[derive(Debug)]
pub struct WorkerProfile {
    pumping: Duration,
    yielding: Duration,
    sleeping: Duration,
    parked: Duration,
    blocked: Duration,
    parks: u64,
    passes: u64,
    started: Instant,
}

/// What a ring worker was doing for one stretch of time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    /// A pass of the pump.
    Pumping,
    /// A poll that only yielded.
    Yielding,
    /// A backoff sleep.
    Sleeping,
    /// Parked on the doorbell, `IDLE` published.
    Parked,
    /// Waiting for the virtqueue (`vkWaitVirtqueueSeqnoMESA`).
    Blocked,
}

impl WorkerProfile {
    /// An empty window starting now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pumping: Duration::ZERO,
            yielding: Duration::ZERO,
            sleeping: Duration::ZERO,
            parked: Duration::ZERO,
            blocked: Duration::ZERO,
            parks: 0,
            passes: 0,
            started: Instant::now(),
        }
    }

    /// `took` spent in `state`.
    pub fn add(&mut self, state: WorkerState, took: Duration) {
        match state {
            WorkerState::Pumping => {
                self.pumping += took;
                self.passes += 1;
            }
            WorkerState::Yielding => self.yielding += took,
            WorkerState::Sleeping => self.sleeping += took,
            WorkerState::Parked => {
                self.parked += took;
                self.parks += 1;
            }
            WorkerState::Blocked => self.blocked += took,
        }
    }

    /// How many times the ring parked in this window.
    #[must_use]
    pub fn parks(&self) -> u64 {
        self.parks
    }

    /// Log the window if it is [`PERIOD`] old and start the next one.
    pub fn log_if_due(&mut self, name: &str) {
        let window = self.started.elapsed();
        if window < PERIOD {
            return;
        }
        let ms = |d: Duration| format_args!("{:.1}", d.as_secs_f64() * 1e3).to_string();
        tracing::debug!(
            target: "virtio_gpu::venus::profile",
            worker = name,
            window_ms = window.as_millis(),
            passes = self.passes,
            pumping_ms = %ms(self.pumping),
            yielding_ms = %ms(self.yielding),
            sleeping_ms = %ms(self.sleeping),
            parked_ms = %ms(self.parked),
            blocked_ms = %ms(self.blocked),
            parks = self.parks,
            "venus ring worker"
        );
        *self = Self::new();
    }
}

impl Default for WorkerProfile {
    fn default() -> Self {
        Self::new()
    }
}

/// What the device's queue worker did for one context in a window: the
/// transport commands of its `SUBMIT_3D`s and its blob calls. The worker is
/// one thread for every context, so time spent here for one context is time
/// every other context's commands wait behind it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextCalls {
    /// `SUBMIT_3D`s.
    pub submits: u64,
    /// Host time of all of them, the waits below included.
    pub submit_time: Duration,
    /// `vkNotifyRingMESA` doorbells.
    pub doorbells: u64,
    /// `vkWaitRingSeqnoMESA` on the device worker, their total and longest.
    pub ring_waits: u64,
    /// Summed.
    pub ring_wait_time: Duration,
    /// Longest.
    pub ring_wait_max: Duration,
    /// `vkSubmitVirtqueueSeqnoMESA`.
    pub virtqueue_seqnos: u64,
    /// `RESOURCE_CREATE_BLOB`s.
    pub blob_creates: u64,
    /// `RESOURCE_MAP_BLOB`s.
    pub blob_maps: u64,
    /// Their host time.
    pub blob_map_time: Duration,
    /// `RESOURCE_UNMAP_BLOB`s.
    pub blob_unmaps: u64,
    /// Ring fences asked for.
    pub ring_fences: u64,
}

/// The device side of the profile: [`ContextCalls`] per context.
#[derive(Debug)]
pub struct DeviceProfile {
    contexts: std::collections::BTreeMap<u32, ContextCalls>,
    /// Scanout flushes that reached the renderer: the frames the guest's
    /// compositor presented.
    flips: u64,
    started: Instant,
}

impl DeviceProfile {
    /// An empty window starting now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            contexts: std::collections::BTreeMap::new(),
            flips: 0,
            started: Instant::now(),
        }
    }

    /// One scanout flush.
    pub fn flip(&mut self) {
        self.flips += 1;
        self.log_if_due();
    }

    /// The counts of `ctx_id`, to add to.
    pub fn context(&mut self, ctx_id: u32) -> &mut ContextCalls {
        self.contexts.entry(ctx_id).or_default()
    }

    /// Log every context's window if it is [`PERIOD`] old, and start again.
    pub fn log_if_due(&mut self) {
        let window = self.started.elapsed();
        if window < PERIOD {
            return;
        }
        let ms = |d: Duration| format!("{:.2}", d.as_secs_f64() * 1e3);
        tracing::debug!(
            target: "virtio_gpu::venus::profile",
            window_ms = window.as_millis(),
            flips = self.flips,
            flip_fps = format_args!("{:.1}", self.flips as f64 / window.as_secs_f64()),
            "venus scanout flips"
        );
        for (ctx_id, c) in &self.contexts {
            tracing::debug!(
                target: "virtio_gpu::venus::profile",
                ctx_id,
                window_ms = window.as_millis(),
                submits = c.submits,
                submit_ms = %ms(c.submit_time),
                doorbells = c.doorbells,
                ring_waits = c.ring_waits,
                ring_wait_ms = %ms(c.ring_wait_time),
                ring_wait_max_ms = %ms(c.ring_wait_max),
                virtqueue_seqnos = c.virtqueue_seqnos,
                ring_fences = c.ring_fences,
                blob_creates = c.blob_creates,
                blob_maps = c.blob_maps,
                blob_map_ms = %ms(c.blob_map_time),
                blob_unmaps = c.blob_unmaps,
                "venus device calls"
            );
        }
        *self = Self::new();
    }
}

impl Default for DeviceProfile {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_sums_its_commands_and_names_the_longest_first() {
        let mut p = CommandProfile::new();
        p.record(10, "vkQueueSubmit", false, Duration::from_micros(300));
        p.record(10, "vkQueueSubmit", false, Duration::from_micros(500));
        p.record(20, "vkWaitForFences", true, Duration::from_millis(3));
        p.record(9999, "vkSomethingNew", true, Duration::from_micros(1));
        p.record_streams(4096, Duration::from_micros(900));
        let s = p.summary();
        assert_eq!(s.commands, 4);
        assert_eq!(s.replies, 2);
        assert_eq!(s.busy, Duration::from_micros(3801));
        assert_eq!(s.reply_time, Duration::from_micros(3001));
        assert_eq!((s.streams, s.stream_bytes), (1, 4096));
        assert!(
            s.top
                .starts_with("WaitForFences=1/3.00/3.00/1 QueueSubmit=2/0.80/0.50/0"),
            "{}",
            s.top
        );
        assert!(s.top.contains("SomethingNew=1/"), "{}", s.top);
    }

    #[test]
    fn a_worker_window_counts_its_parks_and_passes() {
        let mut w = WorkerProfile::new();
        w.add(WorkerState::Pumping, Duration::from_micros(10));
        w.add(WorkerState::Pumping, Duration::from_micros(10));
        w.add(WorkerState::Parked, Duration::from_millis(5));
        w.add(WorkerState::Yielding, Duration::from_micros(3));
        assert_eq!(w.parks(), 1);
        assert_eq!(w.passes, 2);
        assert_eq!(w.pumping, Duration::from_micros(20));
    }

    #[test]
    fn nothing_is_due_before_the_period() {
        let p = CommandProfile::new();
        assert!(!p.due());
    }
}
