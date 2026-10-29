//! Whole-call ring progress with the context/monitor metadata needed to start
//! new workers. Backing pages and blob identities belong to the global graph.

use std::collections::HashSet;
use std::time::Instant;

use super::*;
use crate::venus::executor::checkpoint::RecipeError;
use crate::venus::executor::checkpoint_pages::HostPageTable;
use crate::venus::service::{WorkerCheckpoint, MIN_MONITOR_PERIOD};
use crate::venus::wire::{Decoder, Encoder};

const VERSION: u32 = 1;
const MAX_BYTES: usize = 2 << 20;

#[derive(Debug)]
pub struct SavedRing {
    pub ring: u64,
    pub page: u32,
    pub info: RingCreateInfo,
    pub monitored: bool,
    pub seqno: u64,
    pub worker: WorkerCheckpoint,
}

#[derive(Debug)]
pub struct SavedContext {
    pub ctx_id: u32,
    pub capset_id: u32,
    pub poisoned: bool,
    /// Zero means no monitor; a retained monitor may have no watched rings.
    pub monitor_period_ns: u64,
    pub rings: Vec<SavedRing>,
}

#[derive(Debug)]
pub struct RingCheckpointSet {
    pub contexts: Vec<SavedContext>,
}

fn invalid(reason: &'static str) -> RecipeError {
    RecipeError::Invalid(reason)
}

fn boolean(decoder: &mut Decoder<'_>) -> Result<bool, RecipeError> {
    match decoder.u32()? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(invalid("invalid ring manifest boolean")),
    }
}

fn reserve<T>(vec: &mut Vec<T>, count: usize) -> Result<(), RecipeError> {
    vec.try_reserve(count).map_err(|_| RecipeError::Budget)
}

fn count(decoder: &mut Decoder<'_>, limit: usize) -> Result<usize, RecipeError> {
    let n = decoder.u32()? as usize;
    if n > limit {
        return Err(RecipeError::Budget);
    }
    Ok(n)
}

impl RingCheckpointSet {
    fn validate(&self) -> Result<(), RecipeError> {
        if self.contexts.len() > MAX_VENUS_CONTEXTS {
            return Err(RecipeError::Budget);
        }
        let mut contexts = HashSet::new();
        let mut rings = 0;
        for context in &self.contexts {
            if context.ctx_id == 0
                || context.capset_id != crate::CAPSET_VENUS
                || !contexts.insert(context.ctx_id)
                || (context.monitor_period_ns != 0
                    && (Duration::from_nanos(context.monitor_period_ns) < MIN_MONITOR_PERIOD
                        || Duration::from_nanos(context.monitor_period_ns)
                            > Duration::from_micros(u64::from(u32::MAX))))
            {
                return Err(invalid("invalid context identity or monitor period"));
            }
            rings += context.rings.len();
            if context.rings.len() > MAX_RINGS_PER_CONTEXT || rings > MAX_RINGS {
                return Err(RecipeError::Budget);
            }
            let mut ids = HashSet::new();
            for ring in &context.rings {
                if ring.page == 0
                    || ring.info.resource_id == 0
                    || !ids.insert(ring.ring)
                    || (ring.monitored && context.monitor_period_ns == 0)
                    || ring.worker.sink.len() > WorkerCheckpoint::MAX_SINK_BYTES
                {
                    return Err(invalid("invalid ring identity, monitor or sink metadata"));
                }
                RingLayout::new(ring.info, crate::blob::MAX_BLOB_BYTES)
                    .map_err(|_| invalid("invalid saved ring layout"))?;
            }
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, RecipeError> {
        self.validate()?;
        let mut encoder = Encoder::with_limit(MAX_BYTES);
        encoder.u32(VERSION)?;
        encoder.u32(self.contexts.len() as u32)?;
        for context in &self.contexts {
            encoder.u32(context.ctx_id)?;
            encoder.u32(context.capset_id)?;
            encoder.u32(u32::from(context.poisoned))?;
            encoder.u64(context.monitor_period_ns)?;
            encoder.u32(context.rings.len() as u32)?;
            for ring in &context.rings {
                encoder.u64(ring.ring)?;
                encoder.u32(ring.page)?;
                encoder.u32(u32::from(ring.monitored))?;
                encoder.u64(ring.seqno)?;
                encoder.u32(ring.info.flags)?;
                encoder.u32(ring.info.resource_id)?;
                for word in [
                    ring.info.offset,
                    ring.info.size,
                    ring.info.idle_timeout_ns,
                    ring.info.head_offset,
                    ring.info.tail_offset,
                    ring.info.status_offset,
                    ring.info.buffer_offset,
                    ring.info.buffer_size,
                    ring.info.extra_offset,
                    ring.info.extra_size,
                ] {
                    encoder.u64(word)?;
                }
                encoder.blob(&ring.worker.pump)?;
                encoder.u32(ring.worker.sink.len() as u32)?;
                encoder.blob(&ring.worker.sink)?;
            }
        }
        Ok(encoder.finish()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, RecipeError> {
        if bytes.len() > MAX_BYTES {
            return Err(RecipeError::Budget);
        }
        let mut decoder = Decoder::new(bytes);
        let version = decoder.u32()?;
        if version != VERSION {
            return Err(RecipeError::Version(version));
        }
        let n = count(&mut decoder, MAX_VENUS_CONTEXTS)?;
        let mut contexts = Vec::new();
        reserve(&mut contexts, n)?;
        let mut total = 0;
        for _ in 0..n {
            let ctx_id = decoder.u32()?;
            let capset_id = decoder.u32()?;
            let poisoned = boolean(&mut decoder)?;
            let monitor_period_ns = decoder.u64()?;
            let n = count(&mut decoder, MAX_RINGS_PER_CONTEXT)?;
            total += n;
            if total > MAX_RINGS {
                return Err(RecipeError::Budget);
            }
            let mut rings = Vec::new();
            reserve(&mut rings, n)?;
            for _ in 0..n {
                let ring = decoder.u64()?;
                let page = decoder.u32()?;
                let monitored = boolean(&mut decoder)?;
                let seqno = decoder.u64()?;
                let info = RingCreateInfo {
                    flags: decoder.u32()?,
                    resource_id: decoder.u32()?,
                    offset: decoder.u64()?,
                    size: decoder.u64()?,
                    idle_timeout_ns: decoder.u64()?,
                    head_offset: decoder.u64()?,
                    tail_offset: decoder.u64()?,
                    status_offset: decoder.u64()?,
                    buffer_offset: decoder.u64()?,
                    buffer_size: decoder.u64()?,
                    extra_offset: decoder.u64()?,
                    extra_size: decoder.u64()?,
                };
                let mut pump = [0; 24];
                pump.copy_from_slice(decoder.blob(24)?);
                let len = count(&mut decoder, WorkerCheckpoint::MAX_SINK_BYTES)?;
                let sink = decoder.blob(len)?.to_vec();
                rings.push(SavedRing {
                    ring,
                    page,
                    info,
                    monitored,
                    seqno,
                    worker: WorkerCheckpoint { pump, sink },
                });
            }
            contexts.push(SavedContext {
                ctx_id,
                capset_id,
                poisoned,
                monitor_period_ns,
                rings,
            });
        }
        if decoder.position() != bytes.len() {
            return Err(invalid("trailing ring manifest data"));
        }
        let saved = Self { contexts };
        saved.validate()?;
        Ok(saved)
    }
}

impl<F: SinkFactory> VenusRenderer<F> {
    /// Restore after factory objects and blob mappings, before opening the VM
    /// gate. Preflight every backing and pump before starting any worker. A
    /// failure discards this cold reconstruction; it never touches the source.
    pub fn restore_rings(
        &mut self,
        saved: &RingCheckpointSet,
        pages: &HostPageTable,
    ) -> Result<(), RecipeError> {
        if !self.quiesce.is_paused() || !self.contexts.is_empty() || self.live.count() != 0 {
            return Err(invalid("ring restore requires a fresh paused renderer"));
        }
        saved.validate()?;
        // No threads or shared writes until every ring passes its backing and
        // control-word validation. Guest ids do not establish page identity.
        for context in &saved.contexts {
            for ring in &context.rings {
                let page = pages.page(ring.page)?;
                let blob = self
                    .blobs
                    .get(&ring.info.resource_id)
                    .ok_or(invalid("saved ring has no restored blob"))?;
                if blob.kind != BlobKind::Shm
                    || (blob.ctx_id != 0 && blob.ctx_id != context.ctx_id)
                    || blob
                        .pages()
                        .is_none_or(|backing| !Arc::ptr_eq(page, backing))
                {
                    return Err(invalid(
                        "saved ring has a foreign or different page backing",
                    ));
                }
                let layout = RingLayout::new(ring.info, page.resource_len())
                    .map_err(|_| invalid("saved ring exceeds its restored pages"))?;
                super::super::pump::RingPump::from_checkpoint(
                    layout,
                    page.as_ref(),
                    &ring.worker.pump,
                )
                .map_err(|_| invalid("saved ring control words differ from its progress"))?;
            }
        }
        // Restore every sink before clearing IDLE or starting any monitor.
        let mut prepared = Vec::new();
        reserve(&mut prepared, saved.contexts.len())?;
        for context in &saved.contexts {
            self.sinks.context_created(context.ctx_id);
            let mut rings = Vec::new();
            reserve(&mut rings, context.rings.len())?;
            for ring in &context.rings {
                let page = pages.page(ring.page)?.clone();
                let layout = RingLayout::new(ring.info, page.resource_len())
                    .map_err(|_| invalid("saved ring layout changed"))?;
                let pump = super::super::pump::RingPump::from_checkpoint(
                    layout,
                    page.as_ref(),
                    &ring.worker.pump,
                )
                .map_err(|_| invalid("saved ring progress changed"))?;
                let seqno = Arc::new(VirtqueueSeqno::default());
                let mut sink = self
                    .sinks
                    .sink_for_ring(RingEnv {
                        ctx_id: context.ctx_id,
                        ring: ring.ring,
                        blobs: self.directory.for_context(context.ctx_id),
                        virtqueue_seqno: seqno.clone(),
                    })
                    .map_err(|error| RecipeError::Replay(error.to_string()))?;
                sink.restore_checkpoint_state(&ring.worker.sink, ring.seqno)
                    .map_err(RecipeError::Replay)?;
                if seqno.current() != 0 && seqno.current() != ring.seqno {
                    return Err(invalid("ring and sink roundtrip counts differ"));
                }
                seqno.submit(ring.seqno);
                rings.push((ring, page, layout, pump, sink, seqno));
            }
            prepared.push((context, rings));
        }
        let mut restored = HashMap::new();
        restored
            .try_reserve(saved.contexts.len())
            .map_err(|_| RecipeError::Budget)?;
        for (saved, rings) in prepared {
            let monitor = if saved.monitor_period_ns != 0 {
                Some(
                    RingMonitor::spawn(
                        format!("venus-{}-monitor", saved.ctx_id),
                        Duration::from_nanos(saved.monitor_period_ns),
                        self.quiesce.clone(),
                        &self.live,
                    )
                    .map_err(|error| RecipeError::Replay(error.to_string()))?,
                )
            } else {
                None
            };
            let mut context = Context {
                capset_id: saved.capset_id,
                poisoned: saved.poisoned,
                rings: HashMap::new(),
                monitor,
            };
            context
                .rings
                .try_reserve(rings.len())
                .map_err(|_| RecipeError::Budget)?;
            for (saved_ring, page, layout, pump, sink, seqno) in rings {
                let service =
                    RingService::from_checkpoint(pump, sink, Duration::ZERO, page.as_ref())
                        .with_spin(HOST_SPIN);
                let worker = RingWorker::spawn(
                    format!("venus-{}-{:x}", saved.ctx_id, saved_ring.ring),
                    service,
                    page.clone(),
                    self.quiesce.clone(),
                    &self.live,
                )
                .map_err(|error| RecipeError::Replay(error.to_string()))?;
                if saved_ring.monitored {
                    let monitor = context
                        .monitor
                        .as_ref()
                        .ok_or(invalid("restored ring monitor is absent"))?;
                    monitor.watch(
                        saved_ring.ring,
                        page.clone(),
                        layout.status(),
                        Duration::from_nanos(saved.monitor_period_ns),
                    );
                }
                context.rings.insert(
                    saved_ring.ring,
                    Ring {
                        pages: page,
                        resource_id: saved_ring.info.resource_id,
                        layout,
                        monitored: saved_ring.monitored,
                        virtqueue_seqno: seqno,
                        worker,
                    },
                );
            }
            restored.insert(saved.ctx_id, context);
        }
        self.contexts = restored;
        Ok(())
    }

    /// Capture all ring workers at complete-call boundaries without stopping
    /// them. The gate remains closed; the caller also drains GPU/presenter work
    /// and saves directory, blob mappings and object state at this same point.
    pub fn checkpoint_rings(
        &self,
        pages: &mut HostPageTable,
        timeout: Duration,
    ) -> Result<RingCheckpointSet, RecipeError> {
        let timeout = timeout.min(Duration::from_secs(30));
        let deadline = Instant::now() + timeout;
        if !self.quiesce.is_paused() || !self.quiesce.wait_until_idle(timeout) {
            return Err(invalid("renderer workers are not quiesced"));
        }
        let mut ids = self.contexts.keys().copied().collect::<Vec<_>>();
        ids.sort_unstable();
        let mut contexts = Vec::new();
        reserve(&mut contexts, ids.len())?;
        for ctx_id in ids {
            let context = &self.contexts[&ctx_id];
            let mut ids = context.rings.keys().copied().collect::<Vec<_>>();
            ids.sort_unstable();
            let mut rings = Vec::new();
            reserve(&mut rings, ids.len())?;
            for id in ids {
                let ring = &context.rings[&id];
                let worker = ring
                    .worker
                    .checkpoint(deadline.saturating_duration_since(Instant::now()))
                    .map_err(|error| {
                        RecipeError::Replay(format!("context {ctx_id} ring {id:#x}: {error}"))
                    })?;
                rings.push(SavedRing {
                    ring: id,
                    page: pages.register(&ring.pages)?,
                    info: ring.layout.create_info(),
                    monitored: ring.monitored,
                    seqno: ring.virtqueue_seqno.current(),
                    worker,
                });
            }
            let monitor_period_ns = context.monitor.as_ref().map_or(Ok(0), |monitor| {
                u64::try_from(monitor.period().as_nanos())
                    .map_err(|_| invalid("monitor period overflows"))
            })?;
            contexts.push(SavedContext {
                ctx_id,
                capset_id: context.capset_id,
                poisoned: context.poisoned,
                monitor_period_ns,
                rings,
            });
        }
        let saved = RingCheckpointSet { contexts };
        saved.validate()?;
        Ok(saved)
    }
}

#[cfg(test)]
mod tests;
