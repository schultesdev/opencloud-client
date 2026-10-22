//! Live progress of one sync pass: what the UI shows while a run is in
//! flight — how many files are being moved, how many bytes, and (derived
//! from the event stream) the current speed and remaining time.
//!
//! The engine builds one [`RunTracker`] per pass over the planned
//! transfers; every transfer task reports through its [`TransferHandle`]
//! and the tracker publishes [`EngineEvent::Progress`] snapshots —
//! throttled to a fixed interval, so a fast upload does not flood the
//! event channel.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use tokio::sync::mpsc::UnboundedSender;

use crate::engine::EngineEvent;

/// Minimum time between two progress events of one job.
const EVENT_INTERVAL: Duration = Duration::from_millis(200);

/// Snapshot of a run's transfer progress.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunProgress {
    /// Transfers planned by the run (uploads and downloads).
    pub transfers_total: usize,
    /// Transfers completed by the run.
    pub transfers_done: usize,
    /// Total bytes the run plans to move.
    pub bytes_total: u64,
    /// Bytes moved so far: finished transfers plus in-flight progress.
    pub bytes_done: u64,
}

impl RunProgress {
    /// The completed fraction (`0.0`–`1.0`; `0.0` without a plan).
    pub fn fraction(&self) -> f32 {
        if self.bytes_total == 0 {
            return if self.transfers_total == 0 { 1.0 } else { 0.0 };
        }
        (self.bytes_done as f32 / self.bytes_total as f32).clamp(0.0, 1.0)
    }
}

/// Locks a mutex, recovering from poisoning like the engine's database
/// lock: a panic while locked loses at most one intermediate update.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// The shared progress state of one sync pass. Transfer tasks report into
/// it concurrently; it aggregates and publishes throttled snapshots.
pub(crate) struct RunTracker {
    /// The job the run belongs to.
    job_id: String,
    /// Where progress events go.
    events: UnboundedSender<EngineEvent>,
    /// Transfers planned by the run.
    transfers_total: usize,
    /// Bytes the run plans to move.
    bytes_total: u64,
    /// Bytes of completed transfers.
    bytes_finished: AtomicU64,
    /// Completed transfers.
    transfers_finished: AtomicUsize,
    /// Per in-flight transfer: its current byte count (of the current
    /// attempt; a retried attempt restarts at 0).
    inflight: Mutex<Vec<Arc<AtomicU64>>>,
    /// When the last progress event was sent.
    last_event: Mutex<Instant>,
}

impl RunTracker {
    /// Builds the tracker of one run.
    pub(crate) fn new(
        job_id: &str,
        events: UnboundedSender<EngineEvent>,
        transfers_total: usize,
        bytes_total: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            job_id: job_id.to_owned(),
            events,
            transfers_total,
            bytes_total,
            bytes_finished: AtomicU64::new(0),
            transfers_finished: AtomicUsize::new(0),
            inflight: Mutex::new(Vec::new()),
            last_event: Mutex::new(Instant::now() - EVENT_INTERVAL),
        })
    }

    /// Publishes the initial snapshot (`0/N`); called when the run
    /// actually plans to move files.
    pub(crate) fn announce(&self) {
        self.send_event();
    }

    /// Registers one in-flight transfer and returns its handle.
    pub(crate) fn handle(self: &Arc<Self>) -> TransferHandle {
        let counter = Arc::new(AtomicU64::new(0));
        lock(&self.inflight).push(counter.clone());
        TransferHandle {
            tracker: self.clone(),
            counter,
        }
    }

    /// Aggregated progress of the run so far.
    fn snapshot(&self) -> RunProgress {
        let inflight: u64 = lock(&self.inflight)
            .iter()
            .map(|counter| counter.load(Ordering::Relaxed))
            .sum();
        RunProgress {
            transfers_total: self.transfers_total,
            transfers_done: self.transfers_finished.load(Ordering::Relaxed),
            bytes_total: self.bytes_total,
            bytes_done: self.bytes_finished.load(Ordering::Relaxed) + inflight,
        }
    }

    /// Publishes a progress event if the throttle interval elapsed.
    fn maybe_event(&self) {
        let mut last_event = lock(&self.last_event);
        if last_event.elapsed() >= EVENT_INTERVAL {
            *last_event = Instant::now();
            self.send_event();
        }
    }

    /// Publishes a progress event unconditionally (initial and final
    /// snapshots bypass the throttle).
    fn send_event(&self) {
        let _ = self.events.send(EngineEvent::Progress {
            job_id: self.job_id.clone(),
            progress: self.snapshot(),
        });
    }
}

/// One in-flight transfer's handle for progress reporting; cheap to
/// clone (reporting from the transfer task, finishing from the engine).
#[derive(Clone)]
pub struct TransferHandle {
    tracker: Arc<RunTracker>,
    counter: Arc<AtomicU64>,
}

impl TransferHandle {
    /// Reports the cumulative byte count of the current attempt —
    /// called continuously while the body streams.
    pub fn report(&self, bytes: u64) {
        self.counter.store(bytes, Ordering::Relaxed);
        self.tracker.maybe_event();
    }

    /// Marks the transfer completed: its planned bytes count as moved.
    pub(crate) fn finish(self, planned_bytes: u64) {
        self.counter.store(0, Ordering::Relaxed);
        let mut inflight = lock(&self.tracker.inflight);
        inflight.retain(|counter| !Arc::ptr_eq(counter, &self.counter));
        self.tracker
            .bytes_finished
            .fetch_add(planned_bytes, Ordering::Relaxed);
        self.tracker
            .transfers_finished
            .fetch_add(1, Ordering::Relaxed);
        drop(inflight);
        self.tracker.send_event();
    }

    /// Removes the transfer without counting it — it failed or turned
    /// into a conflict, so its bytes never arrive.
    pub(crate) fn abandon(self) {
        self.counter.store(0, Ordering::Relaxed);
        lock(&self.tracker.inflight).retain(|counter| !Arc::ptr_eq(counter, &self.counter));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    /// Collects all events sent so far.
    fn drain(rx: &mut mpsc::UnboundedReceiver<EngineEvent>) -> Vec<RunProgress> {
        let mut progress = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let EngineEvent::Progress { progress: p, .. } = event {
                progress.push(p);
            }
        }
        progress
    }

    #[test]
    fn aggregates_and_counts_transfers() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let tracker = RunTracker::new("job", tx, 2, 1000);
        tracker.announce();
        assert_eq!(drain(&mut rx).len(), 1, "the announcement is one event");

        let first = tracker.handle();
        let second = tracker.handle();
        first.report(300);
        second.report(500);
        // At least one intermediate update came through (the exact
        // count depends on timing — the throttle is 200 ms).
        let events = drain(&mut rx);
        assert!(!events.is_empty(), "expected updates: {events:?}");
        assert!(
            events.iter().any(|progress| progress.bytes_done >= 300),
            "expected an update beyond 300 bytes: {events:?}"
        );

        first.finish(400);
        let events = drain(&mut rx);
        assert_eq!(
            events.last(),
            Some(&RunProgress {
                transfers_total: 2,
                transfers_done: 1,
                bytes_total: 1000,
                bytes_done: 900,
            })
        );

        second.abandon();
        assert!(drain(&mut rx).is_empty(), "abandon does not count");

        // A fresh handle can report again after a reset (retry); its
        // planned bytes count when it finishes.
        let third = tracker.handle();
        third.report(700);
        third.finish(600);
        let events = drain(&mut rx);
        assert_eq!(
            events.last(),
            Some(&RunProgress {
                transfers_total: 2,
                transfers_done: 2,
                bytes_total: 1000,
                bytes_done: 1000,
            })
        );
    }

    #[test]
    fn fraction_handles_empty_plan() {
        assert_eq!(RunProgress::default().fraction(), 1.0);
        assert_eq!(
            RunProgress {
                transfers_total: 1,
                bytes_total: 100,
                bytes_done: 25,
                transfers_done: 0,
            }
            .fraction(),
            0.25
        );
    }
}
