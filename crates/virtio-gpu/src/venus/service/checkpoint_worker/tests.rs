use super::*;
use crate::venus::pump::{Batch, Consumed, STATUS_IDLE};
use crate::venus::ring::{RingCreateInfo, RingLayout};

fn pages() -> (Arc<RingPages>, RingLayout) {
    let layout = RingLayout::new(
        RingCreateInfo {
            flags: 0,
            resource_id: 1,
            offset: 0,
            size: 4096,
            idle_timeout_ns: 1000,
            head_offset: 0,
            tail_offset: 4,
            status_offset: 8,
            buffer_offset: 16,
            buffer_size: 64,
            extra_offset: 0,
            extra_size: 0,
        },
        4096,
    )
    .unwrap();
    (Arc::new(RingPages::new(4096).unwrap()), layout)
}

fn eventually(cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(
            Instant::now() < deadline,
            "worker did not reach the expected state"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

struct Counter {
    count: u64,
    supported: bool,
}
impl RingSink for Counter {
    fn consume(&mut self, batch: Batch<'_>) -> Consumed {
        self.count += batch.bytes().len() as u64;
        batch.all()
    }
    fn checkpoint_state(&self) -> Result<Vec<u8>, String> {
        if self.supported {
            Ok(self.count.to_le_bytes().to_vec())
        } else {
            Err("unsupported fixture sink".into())
        }
    }
}

fn worker<S: RingSink + Send + 'static>(
    sink: S,
    pages: &Arc<RingPages>,
    layout: RingLayout,
    gate: &Arc<Quiesce>,
    live: &LiveThreads,
) -> RingWorker {
    RingWorker::spawn(
        "checkpoint-worker-test".into(),
        RingService::new(
            RingPump::new(layout, pages.as_ref()).unwrap(),
            sink,
            Duration::ZERO,
        ),
        pages.clone(),
        gate.clone(),
        live,
    )
    .unwrap()
}

#[test]
fn a_parked_worker_checkpoints_without_writing_pages_and_restores_pending_work() {
    let (pages, layout) = pages();
    let gate = Quiesce::new();
    let live = LiveThreads::new();
    let worker = worker(
        Counter {
            count: 0,
            supported: true,
        },
        &pages,
        layout,
        &gate,
        &live,
    );
    assert_eq!(
        worker.checkpoint(Duration::from_secs(1)),
        Err(WorkerCheckpointError::NotQuiesced)
    );
    pages.write_bytes(16, b"done").unwrap();
    pages.guest_store_word(4, 4);
    worker.notify();
    eventually(|| {
        pages.guest_load_word(0) == Some(4) && pages.guest_load_word(8) == Some(STATUS_IDLE)
    });
    // Queue more bytes without ringing: capturing must not serve them or wake IDLE.
    pages.write_bytes(20, b"next").unwrap();
    pages.guest_store_word(4, 8);
    gate.pause();
    assert!(gate.wait_until_idle(Duration::from_secs(5)));
    let mut before = vec![0; 4096];
    pages.read_bytes(0, &mut before).unwrap();
    let saved = worker.checkpoint(Duration::from_secs(5)).unwrap();
    assert_eq!(saved.sink, 4_u64.to_le_bytes());
    let mut after = vec![0; 4096];
    pages.read_bytes(0, &mut after).unwrap();
    assert_eq!(before, after);
    worker.stop().unwrap();
    assert_eq!(live.count(), 0);
    let restored_pages = Arc::new(RingPages::new(4096).unwrap());
    restored_pages.write_bytes(0, &after).unwrap();
    let pump = RingPump::from_checkpoint(layout, restored_pages.as_ref(), &saved.pump).unwrap();
    let count = u64::from_le_bytes(saved.sink.try_into().unwrap());
    let service = RingService::from_checkpoint(
        pump,
        Counter {
            count,
            supported: true,
        },
        Duration::ZERO,
        restored_pages.as_ref(),
    );
    let next_gate = Quiesce::new();
    let restored = RingWorker::spawn(
        "restored-worker-test".into(),
        service,
        restored_pages.clone(),
        next_gate.clone(),
        &live,
    )
    .unwrap();
    eventually(|| restored_pages.guest_load_word(0) == Some(8));
    next_gate.pause();
    assert!(next_gate.wait_until_idle(Duration::from_secs(5)));
    assert_eq!(
        restored.checkpoint(Duration::from_secs(5)).unwrap().sink,
        8_u64.to_le_bytes()
    );
    restored.stop().unwrap();
    assert_eq!(live.count(), 0);
}

#[test]
fn a_failed_capture_leaves_the_parked_source_worker_usable() {
    let (pages, layout) = pages();
    let gate = Quiesce::new();
    let live = LiveThreads::new();
    let worker = worker(
        Counter {
            count: 0,
            supported: false,
        },
        &pages,
        layout,
        &gate,
        &live,
    );
    eventually(|| pages.guest_load_word(8) == Some(STATUS_IDLE));
    gate.pause();
    assert!(gate.wait_until_idle(Duration::from_secs(5)));
    assert!(matches!(
        worker.checkpoint(Duration::from_secs(5)),
        Err(WorkerCheckpointError::Sink(_))
    ));
    assert!(!worker.has_ended());
    pages.write_bytes(16, b"work").unwrap();
    pages.guest_store_word(4, 4);
    worker.notify();
    gate.resume();
    eventually(|| pages.guest_load_word(0) == Some(4));
    worker.stop().unwrap();
    assert_eq!(live.count(), 0);
}

#[test]
fn a_checkpoint_interrupt_cannot_lose_a_doorbell_already_consumed_at_the_pause_gate() {
    let (pages, layout) = pages();
    let gate = Quiesce::new();
    let live = LiveThreads::new();
    let worker = worker(
        Counter {
            count: 0,
            supported: true,
        },
        &pages,
        layout,
        &gate,
        &live,
    );
    eventually(|| pages.guest_load_word(8) == Some(STATUS_IDLE));
    gate.pause();
    assert!(gate.wait_until_idle(Duration::from_secs(5)));
    pages.write_bytes(16, b"work").unwrap();
    pages.guest_store_word(4, 4);
    worker.notify();
    eventually(|| !*relock(&worker.shared.doorbell));
    let saved = worker.checkpoint(Duration::from_secs(5)).unwrap();
    assert_eq!(saved.sink, 0_u64.to_le_bytes());
    assert_eq!(pages.guest_load_word(0), Some(0));
    assert_eq!(pages.guest_load_word(8), Some(STATUS_IDLE));
    gate.resume();
    // No second notify: the worker must retain the one it already consumed.
    eventually(|| pages.guest_load_word(0) == Some(4));
    worker.stop().unwrap();
}

#[test]
fn closing_the_gate_without_settling_an_active_sink_is_not_a_checkpoint() {
    use std::sync::mpsc;
    struct Blocking {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }
    impl RingSink for Blocking {
        fn consume(&mut self, batch: Batch<'_>) -> Consumed {
            self.entered.send(()).unwrap();
            self.release.recv_timeout(Duration::from_secs(5)).unwrap();
            batch.all()
        }
        fn checkpoint_state(&self) -> Result<Vec<u8>, String> {
            Ok(Vec::new())
        }
    }
    let (pages, layout) = pages();
    let gate = Quiesce::new();
    let live = LiveThreads::new();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let worker = worker(
        Blocking {
            entered: entered_tx,
            release: release_rx,
        },
        &pages,
        layout,
        &gate,
        &live,
    );
    pages.write_bytes(16, b"work").unwrap();
    pages.guest_store_word(4, 4);
    worker.notify();
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    gate.pause();
    assert_eq!(
        worker.checkpoint(Duration::ZERO),
        Err(WorkerCheckpointError::NotQuiesced)
    );
    release_tx.send(()).unwrap();
    assert!(gate.wait_until_idle(Duration::from_secs(5)));
    assert!(worker.checkpoint(Duration::from_secs(5)).is_ok());
    worker.stop().unwrap();
}

#[test]
fn a_fatal_worker_keeps_its_final_state_after_the_thread_exits() {
    let (pages, layout) = pages();
    let gate = Quiesce::new();
    let live = LiveThreads::new();
    pages.guest_store_word(4, 1000);
    let worker = worker(
        Counter {
            count: 0,
            supported: true,
        },
        &pages,
        layout,
        &gate,
        &live,
    );
    eventually(|| worker.has_ended());
    gate.pause();
    assert!(gate.wait_until_idle(Duration::from_secs(5)));
    let saved = worker.checkpoint(Duration::from_secs(5)).unwrap();
    let pump = RingPump::from_checkpoint(layout, pages.as_ref(), &saved.pump).unwrap();
    assert!(pump.is_fatal());
    assert_eq!(pump.cursor(), 0);
    assert_eq!(saved.sink, 0_u64.to_le_bytes());
    assert_eq!(worker.checkpoint(Duration::from_secs(5)).unwrap(), saved);
    worker.stop().unwrap();
}

#[test]
fn a_timed_out_reply_cannot_answer_a_later_request() {
    use std::sync::atomic::AtomicU64;
    use std::sync::mpsc;
    struct Slow {
        value: Arc<AtomicU64>,
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl RingSink for Slow {
        fn consume(&mut self, batch: Batch<'_>) -> Consumed {
            batch.all()
        }
        fn checkpoint_state(&self) -> Result<Vec<u8>, String> {
            let value = self.value.load(Ordering::Acquire);
            if self.calls.fetch_add(1, Ordering::AcqRel) == 0 {
                self.entered.send(()).unwrap();
                self.release.recv_timeout(Duration::from_secs(10)).unwrap();
            }
            Ok(value.to_le_bytes().to_vec())
        }
    }
    let (pages, layout) = pages();
    let gate = Quiesce::new();
    let live = LiveThreads::new();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let value = Arc::new(AtomicU64::new(1));
    let worker = worker(
        Slow {
            value: value.clone(),
            entered: entered_tx,
            release: release_rx,
            calls: Default::default(),
        },
        &pages,
        layout,
        &gate,
        &live,
    );
    eventually(|| pages.guest_load_word(8) == Some(STATUS_IDLE));
    gate.pause();
    assert!(gate.wait_until_idle(Duration::from_secs(5)));
    std::thread::scope(|scope| {
        let first = scope.spawn(|| worker.checkpoint(Duration::from_secs(2)));
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(first.join().unwrap(), Err(WorkerCheckpointError::Timeout));
        value.store(2, Ordering::Release);
        let second = scope.spawn(|| worker.checkpoint(Duration::from_secs(5)));
        eventually(|| relock(&worker.shared.capture).pending.is_some());
        release_tx.send(()).unwrap();
        assert_eq!(second.join().unwrap().unwrap().sink, 2_u64.to_le_bytes());
    });
    worker.stop().unwrap();
}
