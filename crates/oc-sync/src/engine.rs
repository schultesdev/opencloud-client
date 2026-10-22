//! The sync engine: one background task per job, each running sync passes
//! (local scan → remote scan with root-ETag shortcut → three-way diff →
//! plan execution with a parallel transfer queue), triggered by the fs
//! watcher, a periodic interval, or explicit commands (see `PLAN.md`,
//! Phase 5).
//!
//! The engine is GUI-free by design: it reports through [`EngineEvent`]
//! and is controlled through [`JobCommand`], so it runs headless
//! (`--headless`) and is testable without any UI.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use oc_api::{ApiError, Depth, TokenProvider, WebDavClient};
use oc_config::ConflictPolicy;
use oc_db::StatusDb;
use oc_db::{ConflictResolution, EntryState, NewConflict, NewEntry};
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::conflict::{self, ResolveOutcome};
use crate::diff::{self, SyncAction};
use crate::error::SyncError;
use crate::filter::Filter;
use crate::scan::{self, LocalNode, RemoteNode};
use crate::transfer::{self, TransferResult};
use crate::watch::LocalWatcher;

/// Configuration of a sync job handed to [`Engine::start`].
#[derive(Debug)]
pub struct Job<P: TokenProvider> {
    /// Stable job identifier (also the database file name).
    pub id: String,
    /// Absolute local root directory mirroring the space.
    pub local_root: PathBuf,
    /// Selective-sync filter of the job.
    pub filter: Filter,
    /// WebDAV client of the space.
    pub dav: Arc<WebDavClient<P>>,
    /// Status database of the job.
    pub db: StatusDb,
}

impl<P: TokenProvider> Job<P> {
    /// Assembles a job from its config parts; the filter is built from
    /// the job's `include`/`exclude` patterns.
    pub fn new(
        id: impl Into<String>,
        local_root: impl Into<PathBuf>,
        include: &[String],
        exclude: &[String],
        dav: WebDavClient<P>,
        db: StatusDb,
    ) -> Result<Self, SyncError> {
        Ok(Self {
            id: id.into(),
            local_root: local_root.into(),
            filter: Filter::new(include, exclude)?,
            dav: Arc::new(dav),
            db,
        })
    }
}

/// Runtime options shared by all jobs.
#[derive(Debug, Clone)]
pub struct EngineOptions {
    /// Interval of the periodic fallback scan (`sync_interval_secs`).
    pub sync_interval: Duration,
    /// Maximum number of concurrent transfers (`max_parallel_transfers`).
    pub max_parallel_transfers: usize,
    /// Whether a filesystem watcher is installed per job.
    pub watch: bool,
    /// Debounce window of the watcher; a trigger fires once the tree has
    /// been quiet for this long.
    pub debounce: Duration,
    /// Chunk size of range downloads; files up to this size arrive in a
    /// single GET.
    pub download_chunk_size: u64,
    /// Whether a matching space-root ETag skips the remote scan (the
    /// recorded database state then serves as the remote snapshot).
    pub root_etag_shortcut: bool,
    /// How detected conflicts are treated: `ask` records them pending;
    /// the other policies resolve them right away (Phase 6).
    pub conflict_policy: ConflictPolicy,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            sync_interval: Duration::from_secs(30),
            max_parallel_transfers: 4,
            watch: true,
            debounce: Duration::from_millis(800),
            download_chunk_size: 8 * 1024 * 1024,
            root_etag_shortcut: true,
            conflict_policy: ConflictPolicy::default(),
        }
    }
}

/// Internal task state of one job: like [`Job`], but with the database
/// behind a mutex — rusqlite connections are `Send` but not `Sync`, and
/// the task future must be `Send` for `tokio::spawn`. The guard is never
/// held across an `await`.
struct JobTask<P: TokenProvider> {
    id: String,
    local_root: PathBuf,
    filter: Filter,
    dav: Arc<WebDavClient<P>>,
    db: std::sync::Mutex<StatusDb>,
}

impl<P: TokenProvider> From<Job<P>> for JobTask<P> {
    fn from(job: Job<P>) -> Self {
        Self {
            id: job.id,
            local_root: job.local_root,
            filter: job.filter,
            dav: job.dav,
            db: std::sync::Mutex::new(job.db),
        }
    }
}

/// Locks the job's database. Poisoning (a panic while locked) is
/// recovered: SQLite survives it, at worst the aborted statement is
/// retried on the next run.
pub(crate) fn db_lock(db: &std::sync::Mutex<StatusDb>) -> std::sync::MutexGuard<'_, StatusDb> {
    db.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Control command for one job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobCommand {
    /// Pause the job: no sync passes until resumed.
    Pause,
    /// Resume a paused job (without forcing a run).
    Resume,
    /// Run a sync pass now — even while paused.
    SyncNow,
    /// Stop the job task.
    Stop,
}

/// What the engine reports to the UI (Phase 7) or a headless runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineEvent {
    /// A sync pass started.
    RunStarted {
        /// The job's identifier.
        job_id: String,
    },
    /// A sync pass finished.
    RunFinished {
        /// The job's identifier.
        job_id: String,
        /// What the pass did.
        summary: RunSummary,
    },
    /// A path could not be synced and stays in the error state.
    Failed {
        /// The job's identifier.
        job_id: String,
        /// The affected path; empty when the whole run failed.
        path: String,
        /// Human-readable error.
        error: String,
    },
    /// A new conflict was detected; the path is frozen until resolved
    /// (Phase 6).
    Conflict {
        /// The job's identifier.
        job_id: String,
        /// The conflicting path.
        path: String,
    },
    /// A conflict was resolved — by the conflict policy or a resolve
    /// command (Phase 6).
    ConflictResolved {
        /// The job's identifier.
        job_id: String,
        /// The resolved path.
        path: String,
        /// Which side won.
        resolution: ConflictResolution,
    },
    /// The job task stopped (Stop command or channel closed).
    Stopped {
        /// The job's identifier.
        job_id: String,
    },
}

impl fmt::Display for EngineEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RunStarted { job_id } => write!(f, "[{job_id}] sync run started"),
            Self::RunFinished { job_id, summary } => {
                write!(f, "[{job_id}] sync run finished: {summary}")
            }
            Self::Failed {
                job_id,
                path,
                error,
            } => {
                if path.is_empty() {
                    write!(f, "[{job_id}] sync failed: {error}")
                } else {
                    write!(f, "[{job_id}] '{path}' failed: {error}")
                }
            }
            Self::Conflict { job_id, path } => write!(f, "[{job_id}] conflict at '{path}'"),
            Self::ConflictResolved {
                job_id,
                path,
                resolution,
            } => write!(
                f,
                "[{job_id}] conflict at '{path}' resolved ({})",
                match resolution {
                    ConflictResolution::Server => "server wins",
                    ConflictResolution::Local => "local wins",
                }
            ),
            Self::Stopped { job_id } => write!(f, "[{job_id}] stopped"),
        }
    }
}

/// What one sync pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunSummary {
    /// Files uploaded to the server.
    pub uploads: usize,
    /// Files downloaded from the server.
    pub downloads: usize,
    /// Files/folders deleted on either side.
    pub deletes: usize,
    /// Folders created on either side.
    pub folders_created: usize,
    /// New conflicts recorded.
    pub conflicts: usize,
    /// Conflicts the policy resolved right away (not left pending).
    pub auto_resolved: usize,
    /// Paths that failed.
    pub errors: usize,
    /// Whether the remote scan was skipped (unchanged root ETag).
    pub remote_scan_skipped: bool,
}

impl fmt::Display for RunSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} up, {} down, {} deleted, {} folders, {} conflicts, {} errors",
            self.uploads,
            self.downloads,
            self.deletes,
            self.folders_created,
            self.conflicts,
            self.errors
        )?;
        if self.auto_resolved > 0 {
            write!(f, ", {} auto-resolved", self.auto_resolved)?;
        }
        Ok(())
    }
}

/// The running sync engine: one task per job, events for the UI.
#[derive(Debug)]
pub struct Engine {
    commands: HashMap<String, mpsc::Sender<JobCommand>>,
    handles: Vec<JoinHandle<()>>,
    events: UnboundedReceiver<EngineEvent>,
}

impl Engine {
    /// Starts one task per job and returns the engine handle. Each job
    /// runs a sync pass immediately, then on watcher triggers and the
    /// periodic interval.
    pub fn start<P: TokenProvider + 'static>(
        jobs: Vec<Job<P>>,
        options: EngineOptions,
    ) -> Result<Self, SyncError> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let mut commands = HashMap::new();
        let mut handles = Vec::new();
        for job in jobs {
            let (command_tx, command_rx) = mpsc::channel(16);
            let (trigger_tx, trigger_rx) = mpsc::unbounded_channel();
            let watcher = if options.watch {
                Some(LocalWatcher::start(
                    job.local_root.clone(),
                    options.debounce,
                    trigger_tx,
                )?)
            } else {
                None
            };
            commands.insert(job.id.clone(), command_tx);
            handles.push(tokio::spawn(job_loop(
                JobTask::from(job),
                options.clone(),
                command_rx,
                trigger_rx,
                watcher,
                event_tx.clone(),
            )));
        }
        Ok(Self {
            commands,
            handles,
            events: event_rx,
        })
    }

    /// The number of running job tasks.
    pub fn job_count(&self) -> usize {
        self.commands.len()
    }

    /// The engine's event stream (UI, headless runner, tests).
    pub fn events_mut(&mut self) -> &mut UnboundedReceiver<EngineEvent> {
        &mut self.events
    }

    /// Takes the engine's event stream out — for a consumer that forwards
    /// it from its own task (the UI bridge, Phase 7). Calling this again,
    /// or [`Engine::events_mut`] afterwards, yields an immediately closed
    /// stream; decide on one consumer per engine instance.
    pub fn take_events(&mut self) -> UnboundedReceiver<EngineEvent> {
        let (_, closed) = mpsc::unbounded_channel();
        std::mem::replace(&mut self.events, closed)
    }

    /// Sends a command to one job; errors when the job is unknown or its
    /// task has stopped.
    pub async fn send(
        &self,
        job_id: &str,
        command: JobCommand,
    ) -> Result<(), mpsc::error::SendError<JobCommand>> {
        match self.commands.get(job_id) {
            Some(sender) => sender.send(command).await,
            None => Err(mpsc::error::SendError(command)),
        }
    }

    /// Pauses every job.
    pub async fn pause_all(&self) {
        for sender in self.commands.values() {
            let _ = sender.send(JobCommand::Pause).await;
        }
    }

    /// Resumes every job.
    pub async fn resume_all(&self) {
        for sender in self.commands.values() {
            let _ = sender.send(JobCommand::Resume).await;
        }
    }

    /// Stops all job tasks and waits for them to finish.
    pub async fn stop(&mut self) {
        for sender in self.commands.values() {
            let _ = sender.send(JobCommand::Stop).await;
        }
        for handle in self.handles.drain(..) {
            let _ = handle.await;
        }
    }
}

/// The task body of one job: serializes sync passes, honors commands,
/// and reacts to watcher triggers and the periodic interval.
async fn job_loop<P: TokenProvider + 'static>(
    job: JobTask<P>,
    options: EngineOptions,
    mut commands: mpsc::Receiver<JobCommand>,
    mut triggers: mpsc::UnboundedReceiver<()>,
    _watcher: Option<LocalWatcher>,
    events: UnboundedSender<EngineEvent>,
) {
    let mut paused = false;
    let mut pending = true; // one pass right after startup
    // The first tick of `interval` fires immediately; the startup pass
    // must not be followed by a second one.
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + options.sync_interval,
        options.sync_interval,
    );
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        if pending && !paused {
            pending = false;
            let _ = events.send(EngineEvent::RunStarted {
                job_id: job.id.clone(),
            });
            let summary = run_once(&job, &options, &events).await;
            let _ = events.send(EngineEvent::RunFinished {
                job_id: job.id.clone(),
                summary,
            });
            continue;
        }
        tokio::select! {
            _ = ticker.tick() => {
                if !paused {
                    pending = true;
                }
            }
            trigger = triggers.recv() => {
                if trigger.is_some() && !paused {
                    pending = true;
                }
            }
            command = commands.recv() => match command {
                Some(JobCommand::Pause) => paused = true,
                Some(JobCommand::Resume) => {
                    // Catch up on what changed while the job was paused.
                    paused = false;
                    pending = true;
                }
                Some(JobCommand::SyncNow) => {
                    paused = false;
                    pending = true;
                }
                Some(JobCommand::Stop) | None => break,
            },
        }
    }
    let _ = events.send(EngineEvent::Stopped {
        job_id: job.id.clone(),
    });
}

/// A queued transfer: its kind, path, and the running task.
type QueuedTransfer = (
    PendingTransfer,
    String,
    JoinHandle<Result<TransferResult, SyncError>>,
);

/// A queued transfer of one path.
#[derive(Debug, Clone)]
enum PendingTransfer {
    /// Upload with an optimistic-write condition.
    Upload {
        /// Recorded ETag for `If-Match`; `None` when `create` is set.
        base_etag: Option<String>,
        /// Whether the file is new remotely (`If-None-Match: *`).
        create: bool,
    },
    /// Download of a known remote state.
    Download {
        /// The remote node (ETag/size) as seen by the scan.
        remote: RemoteNode,
    },
}

impl PendingTransfer {
    /// Whether this is an upload.
    fn is_upload(&self) -> bool {
        matches!(self, Self::Upload { .. })
    }
}

/// One sync pass: recovery, scans, diff, plan execution, bookkeeping.
/// Failures of individual paths are reported as [`EngineEvent::Failed`]
/// and counted; only a failing scan or database aborts the pass early.
async fn run_once<P: TokenProvider + 'static>(
    job: &JobTask<P>,
    options: &EngineOptions,
    events: &UnboundedSender<EngineEvent>,
) -> RunSummary {
    let mut summary = RunSummary::default();
    let now = unix_now();

    let fail = |summary: &mut RunSummary, path: &str, error: &SyncError| {
        summary.errors += 1;
        let _ = events.send(EngineEvent::Failed {
            job_id: job.id.clone(),
            path: path.to_owned(),
            error: error.to_string(),
        });
    };

    // Crash recovery: interrupted transfers keep their recorded baseline,
    // so the diff re-plans them; the stale state must not linger.
    if let Err(error) = recover_interrupted(&job.db) {
        fail(&mut summary, "", &error);
        return summary;
    }

    // A changed filter (include/exclude) removes paths from the database
    // that no longer belong to the synced set; local files stay (the GUI
    // asks before cleaning them up, see PLAN.md Phase 5/7).
    if let Err(error) = drop_uncovered(&job.db, &job.filter) {
        fail(&mut summary, "", &error);
        return summary;
    }

    // Stale .part files from a crash never become real files.
    if let Err(error) = scan::remove_part_files(&job.local_root) {
        fail(&mut summary, "", &error);
        return summary;
    }

    let local = match scan::scan_local(&job.local_root, &job.filter) {
        Ok(local) => local,
        Err(error) => {
            fail(&mut summary, "", &error);
            return summary;
        }
    };

    let db_entries = match db_lock(&job.db).all_entries() {
        Ok(entries) => entries
            .into_iter()
            .map(|entry| (entry.entry.path.clone(), entry.entry))
            .collect::<HashMap<_, _>>(),
        Err(error) => {
            fail(&mut summary, "", &SyncError::from(error));
            return summary;
        }
    };

    // Remote snapshot; the root ETag shortcuts the traversal when the
    // server reports no change since the last completed pass.
    let root = match job.dav.propfind("", Depth::Zero).await {
        Ok(root) => root,
        Err(error) => {
            fail(&mut summary, "", &SyncError::from(error));
            return summary;
        }
    };
    let root_etag = root.first().and_then(|resource| resource.etag.clone());
    let stored_etag = db_lock(&job.db).meta("space_etag").ok().flatten();
    let skip_remote =
        options.root_etag_shortcut && stored_etag.is_some() && stored_etag == root_etag;

    let remote = if skip_remote {
        summary.remote_scan_skipped = true;
        remote_from_db(&db_entries)
    } else {
        let listing = match job.dav.propfind("", Depth::One).await {
            Ok(listing) => listing,
            Err(error) => {
                fail(&mut summary, "", &SyncError::from(error));
                return summary;
            }
        };
        match scan::scan_remote_from_root(job.dav.as_ref(), &job.filter, &listing).await {
            Ok(snapshot) => snapshot.entries,
            Err(error) => {
                fail(&mut summary, "", &error);
                return summary;
            }
        }
    };

    // Open conflicts freeze their paths; nothing below them is touched.
    let mut frozen = HashSet::new();
    match db_lock(&job.db).pending_conflicts() {
        Ok(conflicts) => frozen.extend(conflicts.into_iter().map(|conflict| conflict.path)),
        Err(error) => {
            fail(&mut summary, "", &SyncError::from(error));
            return summary;
        }
    }
    frozen.extend(
        db_entries
            .iter()
            .filter(|(_, entry)| entry.state == EntryState::Conflict)
            .map(|(path, _)| path.clone()),
    );

    let plan = diff::build(&local, &remote, &db_entries, &frozen);

    let semaphore = Arc::new(Semaphore::new(options.max_parallel_transfers.max(1)));
    let mut transfers: Vec<QueuedTransfer> = Vec::new();

    for action in plan {
        match action {
            SyncAction::MkcolRemote { path } => match job.dav.mkcol(&path).await {
                // 405: the folder already exists (created concurrently).
                Ok(()) | Err(ApiError::Status { code: 405 }) => {
                    summary.folders_created += 1;
                    match local_node_at(&job.local_root, &path) {
                        Ok(Some(node)) => {
                            if let Err(error) =
                                upsert_synced(&job.db, &path, true, node, None, None, now)
                            {
                                fail(&mut summary, &path, &error);
                            }
                        }
                        Ok(None) => {}
                        Err(error) => fail(&mut summary, &path, &error),
                    }
                }
                Err(error) => fail(&mut summary, &path, &SyncError::from(error)),
            },
            SyncAction::MkcolLocal { path } => match scan::abs_local_path(&job.local_root, &path) {
                Ok(target) => {
                    if let Err(source) = std::fs::create_dir_all(&target) {
                        fail(&mut summary, &path, &SyncError::io(&target, source));
                    } else {
                        summary.folders_created += 1;
                        let remote_node = remote.get(&path);
                        match local_node_at(&job.local_root, &path) {
                            Ok(Some(node)) => {
                                if let Err(error) = upsert_synced(
                                    &job.db,
                                    &path,
                                    true,
                                    node,
                                    remote_node.and_then(|node| node.etag.clone()),
                                    remote_node.and_then(|node| node.size),
                                    now,
                                ) {
                                    fail(&mut summary, &path, &error);
                                }
                            }
                            Ok(None) => {}
                            Err(error) => fail(&mut summary, &path, &error),
                        }
                    }
                }
                Err(error) => fail(&mut summary, &path, &error),
            },
            SyncAction::DeleteRemote { path, is_dir } => match job.dav.delete(&path).await {
                // 404: already gone (e.g. the parent folder was deleted
                // first) — the outcome is the same.
                Ok(()) | Err(ApiError::NotFound) => {
                    let result = if is_dir {
                        db_lock(&job.db).delete_subtree(&path)
                    } else {
                        db_lock(&job.db).delete_entry(&path).map(|_| 0)
                    };
                    if let Err(error) = result {
                        fail(&mut summary, &path, &SyncError::from(error));
                    } else {
                        summary.deletes += 1;
                    }
                }
                Err(error) => fail(&mut summary, &path, &SyncError::from(error)),
            },
            SyncAction::DeleteLocal { path, is_dir } => {
                match scan::abs_local_path(&job.local_root, &path) {
                    Ok(target) => {
                        let result = if is_dir {
                            std::fs::remove_dir_all(&target)
                                .map_err(|source| SyncError::io(&target, source))
                        } else {
                            transfer::make_writable(&target).and_then(|()| {
                                std::fs::remove_file(&target)
                                    .map_err(|source| SyncError::io(&target, source))
                            })
                        };
                        // NotFound: already gone (parent removed first).
                        let gone = matches!(
                            &result,
                            Err(SyncError::Io { source, .. }) if source.kind() == ErrorKind::NotFound
                        );
                        if gone || result.is_ok() {
                            let result = if is_dir {
                                db_lock(&job.db).delete_subtree(&path)
                            } else {
                                db_lock(&job.db).delete_entry(&path).map(|_| 0)
                            };
                            if let Err(error) = result {
                                fail(&mut summary, &path, &SyncError::from(error));
                            } else {
                                summary.deletes += 1;
                            }
                        } else if let Err(error) = result {
                            fail(&mut summary, &path, &error);
                        }
                    }
                    Err(error) => fail(&mut summary, &path, &error),
                }
            }
            SyncAction::Upload {
                path,
                base_etag,
                create,
            } => {
                let _ = mark_state(&job.db, &path, EntryState::Uploading);
                let kind = PendingTransfer::Upload { base_etag, create };
                transfers.push((
                    kind.clone(),
                    path.clone(),
                    spawn_transfer(
                        job.dav.clone(),
                        job.local_root.clone(),
                        options.download_chunk_size,
                        semaphore.clone(),
                        kind,
                        path,
                    ),
                ));
            }
            SyncAction::Download { path, remote } => {
                let _ = mark_state(&job.db, &path, EntryState::Downloading);
                let kind = PendingTransfer::Download { remote };
                transfers.push((
                    kind.clone(),
                    path.clone(),
                    spawn_transfer(
                        job.dav.clone(),
                        job.local_root.clone(),
                        options.download_chunk_size,
                        semaphore.clone(),
                        kind,
                        path,
                    ),
                ));
            }
            SyncAction::Conflict {
                path,
                local,
                remote,
            } => {
                let sighting = ConflictSighting {
                    path,
                    local,
                    remote,
                    at: now,
                };
                handle_conflict(job, options, events, &mut summary, &sighting).await;
            }
            SyncAction::RecordSynced {
                path,
                is_dir,
                local,
                remote,
            } => {
                let (etag, size) = match remote {
                    Some(remote) => (remote.etag, remote.size),
                    None => (None, None),
                };
                let node = local.unwrap_or(LocalNode {
                    is_dir,
                    mtime: None,
                    size: None,
                });
                if let Err(error) = upsert_synced(&job.db, &path, is_dir, node, etag, size, now) {
                    fail(&mut summary, &path, &error);
                }
            }
            SyncAction::DropEntry { path } => {
                if let Err(error) = db_lock(&job.db).delete_entry(&path) {
                    fail(&mut summary, &path, &SyncError::from(error));
                }
            }
        }
    }

    // Transfers ran concurrently; apply their results to the database.
    for (kind, path, handle) in transfers {
        let result = match handle.await {
            Ok(result) => result,
            Err(source) => Err(SyncError::Api(ApiError::protocol(format!(
                "transfer task for '{path}' failed: {source}"
            )))),
        };
        match result {
            Ok(TransferResult::Done(meta)) => {
                match upsert_synced(
                    &job.db,
                    &path,
                    false,
                    meta.local,
                    meta.remote_etag,
                    meta.remote_size,
                    now,
                ) {
                    Ok(()) => {
                        if kind.is_upload() {
                            summary.uploads += 1;
                        } else {
                            summary.downloads += 1;
                        }
                    }
                    Err(error) => fail(&mut summary, &path, &error),
                }
            }
            Ok(TransferResult::PreconditionFailed) => {
                // The remote side changed while we uploaded: conflict.
                let current_remote = job
                    .dav
                    .propfind(&path, Depth::Zero)
                    .await
                    .ok()
                    .and_then(|resources| resources.first().map(RemoteNode::from_resource));
                let local_node = local.get(&path).copied();
                let sighting = ConflictSighting {
                    path,
                    local: local_node,
                    remote: current_remote,
                    at: now,
                };
                handle_conflict(job, options, events, &mut summary, &sighting).await;
            }
            Err(error) => {
                let _ = mark_state(&job.db, &path, EntryState::Error);
                fail(&mut summary, &path, &error);
            }
        }
    }

    if let Some(etag) = root_etag {
        if let Err(error) = db_lock(&job.db).set_meta("space_etag", &etag) {
            fail(&mut summary, "", &SyncError::from(error));
        }
    }
    if let Err(error) = db_lock(&job.db).set_last_sync(now) {
        fail(&mut summary, "", &SyncError::from(error));
    }
    summary
}

/// Spawns one transfer task; the semaphore caps the number of concurrent
/// transfers (`max_parallel_transfers`).
fn spawn_transfer<P: TokenProvider + 'static>(
    dav: Arc<WebDavClient<P>>,
    root: PathBuf,
    chunk_size: u64,
    semaphore: Arc<Semaphore>,
    kind: PendingTransfer,
    path: String,
) -> JoinHandle<Result<TransferResult, SyncError>> {
    tokio::spawn(async move {
        let _permit = semaphore
            .acquire_owned()
            .await
            .map_err(|_| SyncError::Api(ApiError::protocol("transfer semaphore closed")))?;
        match kind {
            PendingTransfer::Upload { base_etag, create } => {
                transfer::upload(dav.as_ref(), &root, &path, base_etag, create).await
            }
            PendingTransfer::Download { remote } => {
                transfer::download(dav.as_ref(), &root, &path, &remote, chunk_size).await
            }
        }
    })
}

/// Resets entries stuck in a transfer state after a crash: the recorded
/// baseline is intact, so the diff re-plans the transfer.
fn recover_interrupted(db: &std::sync::Mutex<StatusDb>) -> Result<(), SyncError> {
    let db = db_lock(db);
    for entry in db.all_entries()? {
        if matches!(
            entry.entry.state,
            EntryState::Uploading | EntryState::Downloading
        ) {
            let mut updated = entry.entry;
            updated.state = EntryState::Synced;
            db.upsert_entry(&updated)?;
        }
    }
    Ok(())
}

/// Removes database entries that the job's filter no longer covers
/// (selective-sync changes); local files are left in place.
fn drop_uncovered(db: &std::sync::Mutex<StatusDb>, filter: &Filter) -> Result<(), SyncError> {
    let db = db_lock(db);
    for entry in db.all_entries()? {
        if !filter.covers(&entry.entry.path) {
            if entry.entry.is_dir {
                db.delete_subtree(&entry.entry.path)?;
            } else {
                db.delete_entry(&entry.entry.path)?;
            }
        }
    }
    Ok(())
}

/// Builds a remote snapshot from the recorded database state — used when
/// the root-ETag shortcut says the server has not changed.
fn remote_from_db(db_entries: &HashMap<String, NewEntry>) -> HashMap<String, RemoteNode> {
    db_entries
        .iter()
        .map(|(path, entry)| {
            (
                path.clone(),
                RemoteNode {
                    is_dir: entry.is_dir,
                    etag: entry.remote_etag.clone(),
                    size: entry.remote_size,
                },
            )
        })
        .collect()
}

/// The local state of a path, read from the filesystem after an
/// operation (scan maps are pre-operation snapshots); `None` when the
/// path does not exist.
pub(crate) fn local_node_at(
    root: &std::path::Path,
    rel: &str,
) -> Result<Option<LocalNode>, SyncError> {
    let target = scan::abs_local_path(root, rel)?;
    match std::fs::metadata(&target) {
        Ok(metadata) => Ok(Some(LocalNode {
            is_dir: metadata.is_dir(),
            mtime: scan::unix_mtime(&metadata),
            size: if metadata.is_dir() {
                None
            } else {
                Some(metadata.len() as i64)
            },
        })),
        Err(source) if source.kind() == ErrorKind::NotFound => Ok(None),
        Err(source) => Err(SyncError::io(&target, source)),
    }
}

/// Marks an existing entry with a new lifecycle state (transfer/error);
/// new paths get their first entry only once a transfer succeeds.
fn mark_state(
    db: &std::sync::Mutex<StatusDb>,
    path: &str,
    state: EntryState,
) -> Result<(), SyncError> {
    let db = db_lock(db);
    if let Some(entry) = db.get_entry(path)? {
        let mut updated = entry.entry;
        updated.state = state;
        db.upsert_entry(&updated)?;
    }
    Ok(())
}

/// Upserts a synced entry with the current local and remote state.
fn upsert_synced(
    db: &std::sync::Mutex<StatusDb>,
    path: &str,
    is_dir: bool,
    local: LocalNode,
    remote_etag: Option<String>,
    remote_size: Option<i64>,
    now: i64,
) -> Result<(), SyncError> {
    let db = db_lock(db);
    db.upsert_entry(&NewEntry {
        path: path.to_owned(),
        is_dir,
        local_mtime: local.mtime,
        local_size: local.size,
        remote_etag,
        remote_size,
        checksum: None,
        last_synced_at: Some(now),
        state: EntryState::Synced,
    })?;
    Ok(())
}

/// What a scan saw when it detected a conflict: the path, both sides'
/// states, and the time — the input of [`record_conflict`].
struct ConflictSighting {
    /// The conflicting path.
    path: String,
    /// Local state at detection (`None` = deleted locally).
    local: Option<LocalNode>,
    /// Remote state at detection (`None` = deleted remotely).
    remote: Option<RemoteNode>,
    /// Detection time (unix seconds).
    at: i64,
}

/// Records a conflict in the database, freezes the path, and returns
/// the conflict row id (Phase 6 resolutions address it).
fn record_conflict(
    db: &std::sync::Mutex<StatusDb>,
    sighting: &ConflictSighting,
) -> Result<i64, SyncError> {
    let ConflictSighting {
        path,
        local,
        remote,
        at,
    } = sighting;
    // Both node types are cheap copies/clones; work with values below.
    let local = *local;
    let remote = remote.clone();
    let db = db_lock(db);
    let conflict_id = db.record_conflict(&NewConflict {
        path: path.to_owned(),
        detected_at: *at,
        local_mtime: local.and_then(|node| node.mtime),
        local_size: local.and_then(|node| node.size),
        remote_etag: remote.as_ref().and_then(|node| node.etag.clone()),
        remote_size: remote.as_ref().and_then(|node| node.size),
        remote_mtime: None,
    })?;
    let is_dir = local
        .map(|node| node.is_dir)
        .or_else(|| remote.as_ref().map(|node| node.is_dir))
        .unwrap_or(false);
    let mut entry = match db.get_entry(path)? {
        Some(entry) => entry.entry,
        None => NewEntry::new(path, is_dir, EntryState::Conflict),
    };
    entry.state = EntryState::Conflict;
    if let Some(local) = local {
        entry.local_mtime = local.mtime;
        entry.local_size = local.size;
    }
    if let Some(remote) = remote {
        entry.remote_etag = remote.etag;
        entry.remote_size = remote.size;
    }
    db.upsert_entry(&entry)?;
    Ok(conflict_id)
}

/// Records a conflict and applies the job's conflict policy (Phase 6):
/// `ask` leaves it pending for the user; `keep_server`, `keep_local`,
/// and `conflict_copy` resolve it right away. Whatever needs the user
/// after all (remote changed again mid-resolution, unresolvable kind,
/// failed transfer) surfaces as a conflict event.
async fn handle_conflict<P: TokenProvider>(
    job: &JobTask<P>,
    options: &EngineOptions,
    events: &UnboundedSender<EngineEvent>,
    summary: &mut RunSummary,
    sighting: &ConflictSighting,
) {
    let conflict_id = match record_conflict(&job.db, sighting) {
        Ok(conflict_id) => conflict_id,
        Err(error) => {
            summary.errors += 1;
            let _ = events.send(EngineEvent::Failed {
                job_id: job.id.clone(),
                path: sighting.path.clone(),
                error: error.to_string(),
            });
            return;
        }
    };
    match apply_policy(job, options, conflict_id, &sighting.path, sighting.local).await {
        PolicyOutcome::Resolved { resolution } => {
            summary.auto_resolved += 1;
            let _ = events.send(EngineEvent::ConflictResolved {
                job_id: job.id.clone(),
                path: sighting.path.clone(),
                resolution,
            });
        }
        PolicyOutcome::Recorded => {
            summary.conflicts += 1;
            let _ = events.send(EngineEvent::Conflict {
                job_id: job.id.clone(),
                path: sighting.path.clone(),
            });
        }
        PolicyOutcome::NeedsReview { error } => {
            summary.conflicts += 1;
            let _ = events.send(EngineEvent::Conflict {
                job_id: job.id.clone(),
                path: sighting.path.clone(),
            });
            if let Some(error) = error {
                summary.errors += 1;
                let _ = events.send(EngineEvent::Failed {
                    job_id: job.id.clone(),
                    path: sighting.path.clone(),
                    error: error.to_string(),
                });
            }
        }
    }
}

/// What the conflict policy decided for one conflict.
enum PolicyOutcome {
    /// Policy `ask`: recorded pending, waiting for the user.
    Recorded,
    /// The policy resolved the conflict immediately.
    Resolved {
        /// Which side won.
        resolution: ConflictResolution,
    },
    /// The policy tried to resolve but the conflict needs the user.
    NeedsReview {
        /// Why the automatic resolution failed, if it failed.
        error: Option<SyncError>,
    },
}

/// Applies the configured conflict policy to a freshly recorded
/// conflict. Keeps the engine's bookkeeping in one place; the actual
/// resolution logic lives in [`crate::conflict`].
async fn apply_policy<P: TokenProvider>(
    job: &JobTask<P>,
    options: &EngineOptions,
    conflict_id: i64,
    path: &str,
    local: Option<LocalNode>,
) -> PolicyOutcome {
    let resolution = match options.conflict_policy {
        ConflictPolicy::Ask => return PolicyOutcome::Recorded,
        ConflictPolicy::KeepServer => ConflictResolution::Server,
        ConflictPolicy::KeepLocal => ConflictResolution::Local,
        ConflictPolicy::ConflictCopy => {
            // Keep the local version as a dated copy, then let the
            // server version win. The copy syncs as a new file.
            if local.is_some_and(|node| !node.is_dir) {
                if let Err(error) = conflict::keep_conflict_copy(&job.local_root, path) {
                    return PolicyOutcome::NeedsReview { error: Some(error) };
                }
            }
            ConflictResolution::Server
        }
    };
    match conflict::resolve(
        job.dav.as_ref(),
        &job.db,
        &job.local_root,
        conflict_id,
        resolution,
        options.download_chunk_size,
    )
    .await
    {
        Ok(ResolveOutcome::Resolved) => PolicyOutcome::Resolved { resolution },
        // The remote changed again; the pending row carries the new
        // remote state and the user decides with fresh information.
        Ok(ResolveOutcome::RemoteChanged) => PolicyOutcome::NeedsReview { error: None },
        Err(SyncError::UnresolvableConflict { .. }) => PolicyOutcome::NeedsReview { error: None },
        Err(error) => PolicyOutcome::NeedsReview { error: Some(error) },
    }
}

/// Unix seconds of "now".
pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}
