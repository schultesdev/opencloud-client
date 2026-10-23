//! Live progress of one sync pass: what the UI shows while a run is in
//! flight — how many files are being moved, how many bytes, which files
//! are transferring right now and how far each of them is, and (derived
//! from the event stream) the current speed and remaining time.
//!
//! The engine builds one [`RunTracker`] per pass over the planned
//! transfers; every running transfer reports through its
//! [`TransferHandle`] and the tracker publishes [`EngineEvent::Progress`]
//! snapshots — throttled to a fixed interval, so a fast upload does not
//! flood the event channel.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use tokio::sync::mpsc::UnboundedSender;

use crate::engine::EngineEvent;

/// Direction of a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    /// Local file goes to the server.
    Upload,
    /// Server file comes to the local tree.
    Download,
}

/// One transfer that is running right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferProgress {
    /// The normalized, space-relative path of the file.
    pub path: String,
    /// Upload or download.
    pub direction: TransferDirection,
    /// Bytes moved by the current attempt (a retried attempt restarts
    /// at 0).
    pub bytes_done: u64,
    /// The planned size of the file; 0 when unknown.
    pub bytes_total: u64,
}

/// Snapshot of a run's transfer progress.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunProgress {
    /// Transfers planned by the run (uploads and downloads).
    pub transfers_total: usize,
    /// Transfers completed by the run.
    pub transfers_done: usize,
    /// Transfers that ended without completing: failed, turned into a
    /// conflict, or discarded because the local file changed.
    pub transfers_failed: usize,
    /// Total bytes the run plans to move.
    pub bytes_total: u64,
    /// Bytes moved so far: finished transfers plus in-flight progress.
    pub bytes_done: u64,
    /// The transfers running right now, in the order they started (at
    /// most `max_parallel_transfers`).
    pub active: Vec<TransferProgress>,
}

impl RunProgress {
    /// The completed fraction (`0.0`–`1.0`; `0.0` without a plan).
    pub fn fraction(&self) -> f32 {
        if self.bytes_total == 0 {
            return if self.transfers_total == 0 { 1.0 } else { 0.0 };
        }
        (self.bytes_done as f32 / self.bytes_total as f32).clamp(0.0, 1.0)
    }

    /// Transfers that have not started yet.
    pub fn pending(&self) -> usize {
        self.transfers_total
            .saturating_sub(self.transfers_done)
            .saturating_sub(self.transfers_failed)
            .saturating_sub(self.active.len())
    }
}

/// Locks a mutex, recovering from poisoning like the engine's database
/// lock: a panic while locked loses at most one intermediate update.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// One running transfer as the tracker sees it.
struct Inflight {
    /// The file's path (diff key).
    path: String,
    /// Upload or download.
    direction: TransferDirection,
    /// Planned bytes of the file (0 when unknown).
    planned: u64,
    /// Bytes of the current attempt.
    bytes: AtomicU64,
}

/// The tracker's counters that only change together with the list of
/// running transfers — kept under the same lock so a snapshot never sees
/// a transfer half-finished (removed from the list but not yet counted).
struct Counters {
    /// The transfers running right now, in start order.
    inflight: Vec<Arc<Inflight>>,
    /// Bytes of completed transfers.
    bytes_finished: u64,
    /// Completed transfers.
    transfers_finished: usize,
    /// Abandoned transfers.
    transfers_failed: usize,
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
    /// Minimum time between two throttled events.
    interval: Duration,
    /// Running transfers and the completion counters.
    counters: Mutex<Counters>,
    /// When the last progress event was sent.
    last_event: Mutex<Instant>,
}

impl RunTracker {
    /// Builds the tracker of one run; `interval` throttles the events
    /// reported while bodies stream (`Duration::ZERO` publishes every
    /// report, for tests).
    pub(crate) fn new(
        job_id: &str,
        events: UnboundedSender<EngineEvent>,
        transfers_total: usize,
        bytes_total: u64,
        interval: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            job_id: job_id.to_owned(),
            events,
            transfers_total,
            bytes_total,
            interval,
            counters: Mutex::new(Counters {
                inflight: Vec::new(),
                bytes_finished: 0,
                transfers_finished: 0,
                transfers_failed: 0,
            }),
            last_event: Mutex::new(Instant::now() - interval),
        })
    }

    /// Publishes the initial snapshot (`0/N`); called when the run
    /// actually plans to move files.
    pub(crate) fn announce(&self) {
        self.send_event();
    }

    /// Publishes the final snapshot unconditionally; called once all
    /// transfer tasks have settled, so the UI sees the end state even
    /// when the last reports fell into the throttle window.
    pub(crate) fn complete(&self) {
        self.send_event();
    }

    /// Registers a transfer that starts now (it holds a transfer permit)
    /// and returns its reporting handle.
    pub(crate) fn start(
        self: &Arc<Self>,
        path: &str,
        direction: TransferDirection,
        planned: u64,
    ) -> TransferHandle {
        let entry = Arc::new(Inflight {
            path: path.to_owned(),
            direction,
            planned,
            bytes: AtomicU64::new(0),
        });
        lock(&self.counters).inflight.push(entry.clone());
        self.maybe_event();
        TransferHandle {
            tracker: self.clone(),
            entry,
        }
    }

    /// Aggregated progress of the run so far.
    fn snapshot(&self) -> RunProgress {
        let counters = lock(&self.counters);
        let mut bytes_done = counters.bytes_finished;
        let active = counters
            .inflight
            .iter()
            .map(|entry| {
                let bytes = entry.bytes.load(Ordering::Relaxed);
                bytes_done += bytes;
                TransferProgress {
                    path: entry.path.clone(),
                    direction: entry.direction,
                    bytes_done: bytes,
                    bytes_total: entry.planned,
                }
            })
            .collect();
        RunProgress {
            transfers_total: self.transfers_total,
            transfers_done: counters.transfers_finished,
            transfers_failed: counters.transfers_failed,
            bytes_total: self.bytes_total,
            bytes_done,
            active,
        }
    }

    /// Publishes a progress event if the throttle interval elapsed.
    fn maybe_event(&self) {
        let mut last_event = lock(&self.last_event);
        if last_event.elapsed() >= self.interval {
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

    /// Removes a running transfer from the list and applies `settle` to
    /// the counters in the same critical section.
    fn settle(&self, entry: &Arc<Inflight>, settle: impl FnOnce(&mut Counters)) {
        let mut counters = lock(&self.counters);
        counters
            .inflight
            .retain(|running| !Arc::ptr_eq(running, entry));
        settle(&mut counters);
        drop(counters);
        self.maybe_event();
    }
}

/// One running transfer's handle for progress reporting; cheap to clone
/// (the upload body reports from its own closure).
#[derive(Clone)]
pub struct TransferHandle {
    tracker: Arc<RunTracker>,
    entry: Arc<Inflight>,
}

impl TransferHandle {
    /// Reports the cumulative byte count of the current attempt —
    /// called continuously while the body streams.
    pub fn report(&self, bytes: u64) {
        self.entry.bytes.store(bytes, Ordering::Relaxed);
        self.tracker.maybe_event();
    }

    /// Marks the transfer completed: its planned bytes count as moved.
    /// (A completed transfer whose database record then fails to write
    /// still counts here; the run summary reports the error.)
    pub(crate) fn finish(self, planned_bytes: u64) {
        self.tracker.settle(&self.entry, |counters| {
            counters.bytes_finished += planned_bytes;
            counters.transfers_finished += 1;
        });
    }

    /// Removes the transfer without counting its bytes — it failed or
    /// turned into a conflict, so they never arrive.
    pub(crate) fn abandon(self) {
        self.tracker.settle(&self.entry, |counters| {
            counters.transfers_failed += 1;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    /// Collects all progress snapshots sent so far.
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
    fn aggregates_counts_and_lists_active_transfers() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let tracker = RunTracker::new("job", tx, 2, 1000, Duration::ZERO);
        tracker.announce();
        assert_eq!(
            drain(&mut rx),
            vec![RunProgress {
                transfers_total: 2,
                bytes_total: 1000,
                ..RunProgress::default()
            }],
            "the announcement is one event"
        );

        let first = tracker.start("a.bin", TransferDirection::Upload, 400);
        let second = tracker.start("b.bin", TransferDirection::Download, 600);
        first.report(300);
        second.report(500);
        let events = drain(&mut rx);
        assert_eq!(events.len(), 4, "start and every report publish");
        let last = events.last().expect("events");
        assert_eq!(last.bytes_done, 800);
        assert_eq!(last.pending(), 0);
        assert_eq!(
            last.active,
            vec![
                TransferProgress {
                    path: "a.bin".into(),
                    direction: TransferDirection::Upload,
                    bytes_done: 300,
                    bytes_total: 400,
                },
                TransferProgress {
                    path: "b.bin".into(),
                    direction: TransferDirection::Download,
                    bytes_done: 500,
                    bytes_total: 600,
                },
            ]
        );

        first.finish(400);
        let events = drain(&mut rx);
        assert_eq!(
            events.last(),
            Some(&RunProgress {
                transfers_total: 2,
                transfers_done: 1,
                transfers_failed: 0,
                bytes_total: 1000,
                bytes_done: 900,
                active: vec![TransferProgress {
                    path: "b.bin".into(),
                    direction: TransferDirection::Download,
                    bytes_done: 500,
                    bytes_total: 600,
                }],
            })
        );

        second.abandon();
        let events = drain(&mut rx);
        let last = events.last().expect("abandon publishes");
        assert_eq!(last.transfers_failed, 1);
        assert_eq!(last.bytes_done, 400, "abandoned bytes do not count");
        assert!(last.active.is_empty());
        assert_eq!(last.pending(), 0);

        // The retry of the abandoned transfer starts over and counts its
        // planned bytes when it finishes.
        let third = tracker.start("b.bin", TransferDirection::Download, 600);
        third.report(700);
        third.finish(600);
        tracker.complete();
        let events = drain(&mut rx);
        assert_eq!(
            events.last(),
            Some(&RunProgress {
                transfers_total: 2,
                transfers_done: 2,
                transfers_failed: 1,
                bytes_total: 1000,
                bytes_done: 1000,
                active: vec![],
            })
        );
    }

    #[test]
    fn throttle_holds_reports_but_not_announce_and_complete() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let tracker = RunTracker::new("job", tx, 1, 100, Duration::from_secs(3600));
        tracker.announce();
        assert_eq!(drain(&mut rx).len(), 1);
        // The first throttled event after construction passes (the
        // start); the reports and the finish within the window are held.
        let handle = tracker.start("a.bin", TransferDirection::Upload, 100);
        let events = drain(&mut rx);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].active.len(), 1);
        handle.report(50);
        handle.finish(100);
        assert!(drain(&mut rx).is_empty(), "report and finish are throttled");
        tracker.complete();
        let events = drain(&mut rx);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].transfers_done, 1);
        assert_eq!(events[0].bytes_done, 100);
    }

    #[test]
    fn fraction_and_pending_handle_empty_plan() {
        assert_eq!(RunProgress::default().fraction(), 1.0);
        assert_eq!(RunProgress::default().pending(), 0);
        let progress = RunProgress {
            transfers_total: 4,
            transfers_done: 1,
            transfers_failed: 1,
            bytes_total: 100,
            bytes_done: 25,
            active: vec![TransferProgress {
                path: "a".into(),
                direction: TransferDirection::Upload,
                bytes_done: 0,
                bytes_total: 10,
            }],
        };
        assert_eq!(progress.fraction(), 0.25);
        assert_eq!(progress.pending(), 1);
    }
}
