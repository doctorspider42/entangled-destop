//! On-demand capture on the owning worker, without stopping it or putting a
//! mutex around every ring pass. The hot path only checks atomic flags.
//! Queries require a closed, settled VM gate and never resume guest execution.

use super::*;

const MAX_SINK_BYTES: usize = 4096;
const MAX_WAIT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerCheckpoint {
    pub pump: [u8; 24],
    pub sink: Vec<u8>,
}

impl WorkerCheckpoint {
    pub const MAX_SINK_BYTES: usize = MAX_SINK_BYTES;
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WorkerCheckpointError {
    #[error("checkpoint requires a paused VM with settled workers")]
    NotQuiesced,
    #[error("another ring checkpoint request is still pending")]
    Busy,
    #[error("ring checkpoint request timed out")]
    Timeout,
    #[error("ring stopped during teardown, without a checkpointable boundary")]
    Stopped,
    #[error("ring checkpoint request ids exhausted")]
    Exhausted,
    #[error("ring sink cannot be checkpointed: {0}")]
    Sink(String),
}

type Reply = Result<WorkerCheckpoint, WorkerCheckpointError>;

#[derive(Debug, Default)]
pub(super) struct CaptureState {
    serial: u64,
    pending: Option<u64>,
    reply: Option<(u64, Reply)>,
    final_reply: Option<Reply>,
}

fn snapshot<S: RingSink>(service: &RingService<S>) -> Reply {
    let sink = service
        .sink()
        .checkpoint_state()
        .map_err(WorkerCheckpointError::Sink)?;
    if sink.len() > MAX_SINK_BYTES {
        return Err(WorkerCheckpointError::Sink(
            "sink metadata exceeds the record limit".into(),
        ));
    }
    Ok(WorkerCheckpoint {
        pump: service.pump().checkpoint(),
        sink,
    })
}

impl RingWorker {
    /// Capture between whole sink calls. Keep the VM gate closed and all
    /// guest/GPU writers stopped until all other manifest state is captured.
    /// Failure leaves this worker usable; it never writes shared control words.
    pub fn checkpoint(&self, timeout: Duration) -> Reply {
        let timeout = timeout.min(MAX_WAIT);
        let deadline = Instant::now() + timeout;
        if !self.quiesce.is_paused() || !self.quiesce.wait_until_idle(timeout) {
            return Err(WorkerCheckpointError::NotQuiesced);
        }
        let mut state = relock(&self.shared.capture);
        if let Some(reply) = &state.final_reply {
            return reply.clone();
        }
        if state.pending.is_some() {
            return Err(WorkerCheckpointError::Busy);
        }
        state.serial = state
            .serial
            .checked_add(1)
            .ok_or(WorkerCheckpointError::Exhausted)?;
        let serial = state.serial;
        state.pending = Some(serial);
        state.reply = None;
        self.shared.capture_requested.store(true, Ordering::Release);
        drop(state);
        // Wake a park/backoff without setting or consuming a real doorbell.
        {
            let _guard = relock(&self.shared.doorbell);
            self.shared.changed.notify_all();
        }
        self.quiesce.wake();
        let mut state = relock(&self.shared.capture);
        loop {
            if state.reply.as_ref().is_some_and(|(id, _)| *id == serial) {
                let (_, reply) = state.reply.take().expect("matching reply was checked");
                state.pending = None;
                return reply;
            }
            if let Some(reply) = state.final_reply.clone() {
                state.pending = None;
                return reply;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                state.pending = None;
                self.shared
                    .capture_requested
                    .store(false, Ordering::Release);
                return Err(WorkerCheckpointError::Timeout);
            }
            state = self
                .shared
                .capture_done
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

pub(super) fn capture_pending<S: RingSink>(
    service: &RingService<S>,
    quiesce: &Quiesce,
    shared: &WorkerShared,
) {
    if !shared.capture_requested.load(Ordering::Acquire) {
        return;
    }
    if !shared.capture_requested.swap(false, Ordering::AcqRel) {
        return;
    }
    let Some(serial) = relock(&shared.capture).pending else {
        return;
    };
    let reply = if quiesce.is_paused() {
        snapshot(service)
    } else {
        Err(WorkerCheckpointError::NotQuiesced)
    };
    let mut state = relock(&shared.capture);
    // A timeout may have cancelled this query and a new query may be waiting.
    // Never deliver the old result to the next request.
    if state.pending == Some(serial) {
        state.reply = Some((serial, reply));
        shared.capture_done.notify_all();
    }
}

pub(super) fn capture_final<S: RingSink>(service: &RingService<S>, shared: &WorkerShared) {
    let reply = if shared.stopping() {
        Err(WorkerCheckpointError::Stopped)
    } else {
        snapshot(service)
    };
    let mut state = relock(&shared.capture);
    state.final_reply = Some(reply);
    shared.capture_done.notify_all();
}

#[cfg(test)]
mod tests;
