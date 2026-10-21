//! Local change detection: one filesystem watcher per sync job with
//! debouncing (see `PLAN.md`, Phase 5).
//!
//! Events are batched until the tree is quiet for the debounce duration,
//! then a single trigger is sent to the job loop, which runs a sync pass.
//! Events on `.part` transfer temp files are ignored — they are caused by
//! the engine's own downloads. The periodic scan
//! ([`EngineOptions::sync_interval`](crate::EngineOptions)) remains the
//! fallback and covers remote changes.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::RecvTimeoutError;
use std::thread::JoinHandle;
use std::time::Duration;

use notify::Event;
use notify::RecommendedWatcher;
use notify::RecursiveMode;
use notify::Watcher;
use tokio::sync::mpsc::UnboundedSender;

use crate::error::SyncError;
use crate::scan::PART_SUFFIX;

/// A debouncing fs watcher over one job's local root. Dropping it stops
/// the underlying notify watcher and the debounce thread.
#[derive(Debug)]
pub struct LocalWatcher {
    watcher: Option<RecommendedWatcher>,
    handle: Option<JoinHandle<()>>,
}

impl LocalWatcher {
    /// Watches `root` recursively (created when missing) and sends one
    /// trigger per burst of changes, after the tree has been quiet for
    /// `debounce`.
    pub fn start(
        root: impl Into<PathBuf>,
        debounce: Duration,
        trigger: UnboundedSender<()>,
    ) -> Result<Self, SyncError> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(|source| SyncError::io(&root, source))?;
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = notify::recommended_watcher(tx)
            .map_err(|source| SyncError::Watch(source.to_string()))?;
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(|source| SyncError::Watch(source.to_string()))?;
        let handle = std::thread::Builder::new()
            .name("oc-sync-watch".to_owned())
            .spawn(move || watch_loop(rx, debounce, trigger))
            .map_err(|source| SyncError::Watch(source.to_string()))?;
        Ok(Self {
            watcher: Some(watcher),
            handle: Some(handle),
        })
    }
}

impl Drop for LocalWatcher {
    fn drop(&mut self) {
        // Dropping the watcher disconnects the channel; the thread then
        // exits and can be joined.
        self.watcher = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Debounce loop: wait for an event, then keep draining until the tree
/// has been quiet for `debounce`; send one trigger. Exits when the
/// watcher (and thus the channel) is gone or the receiver is dropped.
fn watch_loop(
    rx: Receiver<Result<Event, notify::Error>>,
    debounce: Duration,
    trigger: UnboundedSender<()>,
) {
    while let Ok(Ok(event)) = rx.recv() {
        if !relevant(&event) {
            continue;
        }
        loop {
            match rx.recv_timeout(debounce) {
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        if trigger.send(()).is_err() {
            return;
        }
    }
}

/// Whether an event is worth a sync run: anything that touches a path
/// other than a `.part` temp file.
fn relevant(event: &Event) -> bool {
    event
        .paths
        .iter()
        .any(|path| !path.to_string_lossy().ends_with(PART_SUFFIX))
}
