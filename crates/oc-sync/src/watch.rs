//! Local change detection: one filesystem watcher per sync job with
//! debouncing (see `PLAN.md`, Phase 5).
//!
//! Events are batched until the tree is quiet for the debounce duration,
//! then a single trigger is sent to the job loop, which runs a sync pass.
//! Only events on paths the job's [`Filter`] covers count: changes in
//! excluded or non-included folders (build output, `.git`, …) and on
//! `.part` transfer temp files (the engine's own downloads) are ignored,
//! as are pure reads (the engine's own directory scans). Bare modifies on
//! folders — what Windows reports for a watched folder whenever a child
//! inside it is added, renamed or removed — count neither.
//!
//! With inotify (Linux) the watcher does not watch the whole tree
//! recursively but each covered folder on its own, so excluded subtrees
//! cost neither watches nor events; the watch set follows folder
//! creations, renames and removals. Other platforms watch the root
//! recursively (one cheap OS-level stream) and only filter the events.
//!
//! A kernel queue overflow (`Rescan`) means events were lost: the watch
//! set is rebuilt and a sync pass triggered — at most once per
//! [`RESCAN_COOLDOWN`], since a large scan can itself overflow the queue
//! and must not keep the job busy. The periodic scan
//! ([`EngineOptions::sync_interval`](crate::EngineOptions)) remains the
//! fallback and covers remote changes.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notify::event::ModifyKind;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher, WatcherKind};
use oc_storage::normalize_nfc;
use tokio::sync::mpsc::UnboundedSender;

use crate::error::SyncError;
use crate::filter::Filter;
use crate::scan::PART_SUFFIX;

/// Minimum time between two sync passes triggered by a watcher queue
/// overflow.
pub const RESCAN_COOLDOWN: Duration = Duration::from_secs(60);

/// A debouncing fs watcher over one job's local root. Dropping it stops
/// the underlying notify watcher and the debounce thread.
#[derive(Debug)]
pub struct LocalWatcher {
    stop: Sender<Message>,
    handle: Option<JoinHandle<()>>,
}

/// What the debounce thread receives: notify results and the stop
/// signal (the watcher lives on that thread, so its channel never
/// disconnects on its own).
enum Message {
    Fs(notify::Result<Event>),
    Stop,
}

impl LocalWatcher {
    /// Watches the part of `root` (created when missing) that `filter`
    /// covers and sends one trigger per burst of relevant changes,
    /// after they have been quiet for `debounce`.
    pub fn start(
        root: impl Into<PathBuf>,
        filter: Filter,
        debounce: Duration,
        trigger: UnboundedSender<()>,
    ) -> Result<Self, SyncError> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(|source| SyncError::io(&root, source))?;
        let (tx, rx) = std::sync::mpsc::channel();
        let fs_tx = tx.clone();
        let watcher = notify::recommended_watcher(move |result| {
            let _ = fs_tx.send(Message::Fs(result));
        })
        .map_err(|source| SyncError::Watch(source.to_string()))?;
        let scope = Scope::new(root, filter);
        let mut watches = Watches::new(watcher, RecommendedWatcher::kind() == WatcherKind::Inotify);
        watches
            .sync(&scope)
            .map_err(|source| SyncError::Watch(source.to_string()))?;
        let handle = std::thread::Builder::new()
            .name("oc-sync-watch".to_owned())
            .spawn(move || watch_loop(rx, scope, watches, debounce, trigger))
            .map_err(|source| SyncError::Watch(source.to_string()))?;
        Ok(Self {
            stop: tx,
            handle: Some(handle),
        })
    }
}

impl Drop for LocalWatcher {
    fn drop(&mut self) {
        // The thread owns the notify watcher and drops it on exit.
        let _ = self.stop.send(Message::Stop);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The job's local root and filter: decides which absolute paths belong
/// to the synced set.
struct Scope {
    root: PathBuf,
    /// The canonical spelling of `root`, when it differs: some backends
    /// (FSEvents) report resolved paths, e.g. `/private/var/…`.
    canonical: Option<PathBuf>,
    filter: Filter,
}

impl Scope {
    fn new(root: PathBuf, filter: Filter) -> Self {
        let canonical = std::fs::canonicalize(&root)
            .ok()
            .filter(|canonical| *canonical != root);
        Self {
            root,
            canonical,
            filter,
        }
    }

    /// `path` relative to the root, `/`-separated and NFC like the scan's
    /// keys; `None` when it lies outside the root.
    fn relative(&self, path: &Path) -> Option<String> {
        let rest = path.strip_prefix(&self.root).ok().or_else(|| {
            self.canonical
                .as_deref()
                .and_then(|canonical| path.strip_prefix(canonical).ok())
        })?;
        let mut segments = Vec::new();
        for component in rest.components() {
            match component {
                Component::Normal(segment) => {
                    segments.push(normalize_nfc(&segment.to_string_lossy()));
                }
                Component::CurDir => {}
                _ => return None,
            }
        }
        Some(segments.join("/"))
    }

    /// Whether a change at `path` can matter to a sync pass. Paths
    /// outside the root count (fail open); a path that no longer exists
    /// counts when the filter covers it as file or as folder.
    fn covers(&self, path: &Path) -> bool {
        if path.to_string_lossy().ends_with(PART_SUFFIX) {
            return false;
        }
        let Some(rel) = self.relative(path) else {
            return true;
        };
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => self.filter.covers(&rel, metadata.is_dir()),
            Err(_) => self.filter.covers(&rel, true) || self.filter.covers(&rel, false),
        }
    }

    /// Whether an event of `kind` at `path` can matter to a sync pass.
    /// Windows reports the parent folder of a created, renamed or removed
    /// child as a bare modify (`ReadDirectoryChangesW` cannot say what
    /// changed); that must not count even when the folder is covered —
    /// the child's own event does the counting.
    fn counts(&self, kind: &EventKind, path: &Path) -> bool {
        if matches!(
            kind,
            EventKind::Modify(ModifyKind::Any | ModifyKind::Metadata(_))
        ) && std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
        {
            return false;
        }
        self.covers(path)
    }

    /// Whether `path` is an existing, covered folder (not a symlink).
    fn covers_dir(&self, path: &Path) -> bool {
        let is_dir = std::fs::symlink_metadata(path)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false);
        is_dir
            && self
                .relative(path)
                .is_some_and(|rel| self.filter.covers(&rel, true))
    }

    /// The root and every covered folder below it, without following
    /// symlinks or descending into folders the filter rules out.
    fn covered_dirs(&self) -> HashSet<PathBuf> {
        let mut dirs = HashSet::new();
        let mut pending = vec![(self.root.clone(), String::new())];
        while let Some((dir, rel_dir)) = pending.pop() {
            if let Ok(read) = std::fs::read_dir(&dir) {
                for entry in read.flatten() {
                    if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                        continue;
                    }
                    let name = normalize_nfc(&entry.file_name().to_string_lossy());
                    let rel = if rel_dir.is_empty() {
                        name
                    } else {
                        format!("{rel_dir}/{name}")
                    };
                    if self.filter.covers(&rel, true) {
                        pending.push((entry.path(), rel));
                    }
                }
            }
            dirs.insert(dir);
        }
        dirs
    }
}

/// The notify watcher plus the set of paths it watches.
struct Watches {
    watcher: RecommendedWatcher,
    /// One non-recursive watch per covered folder (inotify) instead of
    /// one recursive watch on the root.
    per_dir: bool,
    watched: HashSet<PathBuf>,
}

impl Watches {
    fn new(watcher: RecommendedWatcher, per_dir: bool) -> Self {
        Self {
            watcher,
            per_dir,
            watched: HashSet::new(),
        }
    }

    /// Brings the watch set in line with the covered folders on disk.
    /// Folders that vanish meanwhile are skipped; other failures (e.g.
    /// the inotify watch limit) abort with the error.
    fn sync(&mut self, scope: &Scope) -> notify::Result<()> {
        if !self.per_dir {
            if self.watched.is_empty() {
                self.watcher.watch(&scope.root, RecursiveMode::Recursive)?;
                self.watched.insert(scope.root.clone());
            }
            return Ok(());
        }
        let desired = scope.covered_dirs();
        // Unwatch first: after a rename the old path's watch still sits
        // on the moved folder and would otherwise be removed after the
        // new path re-registered it.
        let stale: Vec<PathBuf> = self.watched.difference(&desired).cloned().collect();
        for path in stale {
            let _ = self.watcher.unwatch(&path);
            self.watched.remove(&path);
        }
        let mut added: Vec<PathBuf> = desired.difference(&self.watched).cloned().collect();
        // Parents first, so a failure leaves a connected watch set.
        added.sort_by_key(|path| path.components().count());
        for path in added {
            match self.watcher.watch(&path, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    self.watched.insert(path);
                }
                // Gone again before it could be watched.
                Err(_) if std::fs::symlink_metadata(&path).is_err() => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Whether `event` changes the folder structure the watch set has to
    /// follow: a covered folder appeared, or a watched one was renamed
    /// or removed.
    fn restructures(&self, scope: &Scope, event: &Event) -> bool {
        if !self.per_dir {
            return false;
        }
        let created = matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_))
        );
        let removed = matches!(
            event.kind,
            EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
        );
        event.paths.iter().any(|path| {
            (created && scope.covers_dir(path)) || (removed && self.watched.contains(path))
        })
    }
}

/// What a burst of events asks for.
#[derive(Default)]
struct Burst {
    /// A covered path changed: run a sync pass.
    relevant: bool,
    /// The watch set has to be rebuilt.
    restructure: bool,
    /// Events were lost (queue overflow).
    rescan: bool,
}

impl Burst {
    /// Folds one notify result in; returns whether it counted (and thus
    /// extends the quiet period).
    fn add(&mut self, scope: &Scope, watches: &Watches, result: notify::Result<Event>) -> bool {
        let Ok(event) = result else {
            return false;
        };
        if event.need_rescan() {
            self.rescan = true;
            self.restructure |= watches.per_dir;
            return true;
        }
        if matches!(event.kind, EventKind::Access(_)) {
            return false;
        }
        let restructure = watches.restructures(scope, &event);
        let relevant = event
            .paths
            .iter()
            .any(|path| scope.counts(&event.kind, path));
        self.restructure |= restructure;
        self.relevant |= relevant;
        relevant || restructure
    }

    fn is_empty(&self) -> bool {
        !(self.relevant || self.restructure || self.rescan)
    }
}

/// Debounce loop: wait for a counting event, then keep collecting until
/// no counting event arrived for `debounce`; adjust the watch set and
/// send one trigger. Exits on [`Message::Stop`] or when the receiver of
/// the triggers is gone.
fn watch_loop(
    rx: Receiver<Message>,
    scope: Scope,
    mut watches: Watches,
    debounce: Duration,
    trigger: UnboundedSender<()>,
) {
    let mut last_rescan: Option<Instant> = None;
    while let Ok(Message::Fs(result)) = rx.recv() {
        let mut burst = Burst::default();
        burst.add(&scope, &watches, result);
        if burst.is_empty() {
            continue;
        }
        let mut quiet_at = Instant::now() + debounce;
        loop {
            let wait = quiet_at.saturating_duration_since(Instant::now());
            match rx.recv_timeout(wait) {
                Ok(Message::Fs(result)) => {
                    if burst.add(&scope, &watches, result) {
                        quiet_at = Instant::now() + debounce;
                    }
                }
                Ok(Message::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => break,
            }
        }
        if burst.restructure {
            // A failure (e.g. the watch limit) leaves part of the tree to
            // the periodic scan; the next restructure retries.
            let _ = watches.sync(&scope);
        }
        let mut fire = burst.relevant;
        if burst.rescan && last_rescan.is_none_or(|at| at.elapsed() >= RESCAN_COOLDOWN) {
            last_rescan = Some(Instant::now());
            fire = true;
        }
        if fire && trigger.send(()).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEBOUNCE: Duration = Duration::from_millis(100);
    const WAIT: Duration = Duration::from_millis(1500);

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    async fn triggered(rx: &mut tokio::sync::mpsc::UnboundedReceiver<()>) -> bool {
        let fired = tokio::time::timeout(WAIT, rx.recv()).await.is_ok();
        while rx.try_recv().is_ok() {}
        fired
    }

    #[tokio::test]
    async fn reads_do_not_trigger_but_writes_do() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/file.txt"), b"x").unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let _watcher = LocalWatcher::start(dir.path(), Filter::everything(), DEBOUNCE, tx).unwrap();

        // What a scan does: list directories, stat and read files.
        for _ in 0..5 {
            for entry in std::fs::read_dir(dir.path().join("sub")).unwrap() {
                let path = entry.unwrap().path();
                std::fs::metadata(&path).unwrap();
                std::fs::read(&path).unwrap();
            }
            std::fs::read_dir(dir.path()).unwrap().for_each(drop);
        }
        assert!(
            !triggered(&mut rx).await,
            "pure reads must not trigger a sync pass"
        );

        std::fs::write(dir.path().join("sub/new.txt"), b"y").unwrap();
        assert!(triggered(&mut rx).await, "a write must trigger a sync pass");
    }

    #[tokio::test]
    async fn changes_outside_the_filter_do_not_trigger() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("work/target/debug")).unwrap();
        std::fs::create_dir_all(root.join("other")).unwrap();
        let filter = Filter::new(
            &strings(&["work"]),
            &strings(&["work/target", "*.tmp"]),
            false,
        )
        .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let _watcher = LocalWatcher::start(root, filter, DEBOUNCE, tx).unwrap();

        std::fs::write(root.join("work/target/debug/out.o"), b"x").unwrap();
        std::fs::create_dir(root.join("work/target/debug/deps")).unwrap();
        std::fs::write(root.join("other/file.txt"), b"x").unwrap();
        std::fs::write(root.join("root-file.txt"), b"x").unwrap();
        std::fs::write(root.join("work/scratch.tmp"), b"x").unwrap();
        std::fs::write(root.join("work/file.txt.part"), b"x").unwrap();
        assert!(!triggered(&mut rx).await, "excluded paths must not trigger");

        std::fs::write(root.join("work/file.txt"), b"x").unwrap();
        assert!(triggered(&mut rx).await, "a covered path must trigger");
    }

    #[tokio::test]
    async fn new_and_moved_in_folders_are_watched() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("c")).unwrap();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        let filter = Filter::new(&strings(&["c"]), &[], false).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let _watcher = LocalWatcher::start(root, filter, DEBOUNCE, tx).unwrap();

        std::fs::create_dir_all(root.join("c/new/deep")).unwrap();
        assert!(triggered(&mut rx).await, "a new folder must trigger");
        std::fs::write(root.join("c/new/deep/file.txt"), b"x").unwrap();
        assert!(
            triggered(&mut rx).await,
            "a write in a new folder must trigger"
        );

        // `a` lies outside the filter and is not watched; moved into
        // `c` its subtree has to be picked up.
        std::fs::rename(root.join("a"), root.join("c/moved")).unwrap();
        assert!(triggered(&mut rx).await, "a folder moved in must trigger");
        std::fs::write(root.join("c/moved/b/file.txt"), b"x").unwrap();
        assert!(
            triggered(&mut rx).await,
            "a write in a moved-in folder must trigger"
        );

        std::fs::rename(root.join("c/moved"), root.join("c/renamed")).unwrap();
        assert!(triggered(&mut rx).await, "a folder rename must trigger");
        std::fs::write(root.join("c/renamed/b/other.txt"), b"x").unwrap();
        assert!(
            triggered(&mut rx).await,
            "a write in a renamed folder must trigger"
        );
    }
}
