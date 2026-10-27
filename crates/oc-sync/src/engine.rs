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
use crate::progress::RunProgress;
use crate::progress::RunTracker;
use crate::progress::TransferDirection;
use crate::scan::{self, LocalNode, RemoteNode, ServerNames};
use crate::transfer::{self, LocalGuard, TransferPaths, TransferResult};
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
    /// the job's `include`/`exclude` patterns and the root-files toggle.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        local_root: impl Into<PathBuf>,
        include: &[String],
        exclude: &[String],
        sync_root_files: bool,
        dav: WebDavClient<P>,
        db: StatusDb,
    ) -> Result<Self, SyncError> {
        Ok(Self {
            id: id.into(),
            local_root: local_root.into(),
            filter: Filter::new(include, exclude, sync_root_files)?,
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
    /// Pause between two folder PROPFINDs of the remote scan — the
    /// server-load throttle (`propfind_pause_ms`); zero disables it.
    pub propfind_pause: Duration,
    /// Delay before the first retry after a connectivity failure; the
    /// offline backoff doubles from here.
    pub offline_backoff_base: Duration,
    /// Upper bound of the offline backoff.
    pub offline_backoff_cap: Duration,
    /// Minimum time between two live progress events of a running pass
    /// (the throttle of [`EngineEvent::Progress`] while bodies stream).
    pub progress_interval: Duration,
    /// Global exclude patterns applied to every job on top of its own
    /// `exclude` list (the settings' `global_exclude`).
    pub global_exclude: Vec<String>,
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
            propfind_pause: Duration::ZERO,
            offline_backoff_base: Duration::from_secs(10),
            offline_backoff_cap: Duration::from_secs(60),
            progress_interval: Duration::from_millis(200),
            global_exclude: Vec::new(),
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
    /// Live progress of the running pass's transfers — sent when the
    /// plan is known, then throttled while transfers stream.
    Progress {
        /// The job's identifier.
        job_id: String,
        /// The aggregated transfer progress.
        progress: RunProgress,
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
    /// The server is unreachable; the job pauses its passes and retries
    /// with exponential backoff (Phase 8). Watcher and interval triggers
    /// stay queued and are applied once the connection is back.
    Offline {
        /// The job's identifier.
        job_id: String,
        /// How long the job waits before its next attempt.
        retry_in: Duration,
    },
    /// The server is reachable again after an offline phase; the job
    /// resumes its normal sync schedule.
    Online {
        /// The job's identifier.
        job_id: String,
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
            Self::Progress {
                job_id,
                progress:
                    RunProgress {
                        transfers_done,
                        transfers_total,
                        bytes_done,
                        bytes_total,
                        active,
                        ..
                    },
            } => write!(
                f,
                "[{job_id}] {transfers_done}/{transfers_total} transfers, \
                 {bytes_done}/{bytes_total} bytes, {} active",
                active.len()
            ),
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
            Self::Offline { job_id, retry_in } => write!(
                f,
                "[{job_id}] server unreachable; pausing (next attempt in {}s)",
                retry_in.as_secs()
            ),
            Self::Online { job_id } => {
                write!(f, "[{job_id}] connection restored; resuming sync")
            }
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
    /// periodic interval. A job whose watcher cannot be installed runs
    /// with the interval only (reported as [`EngineEvent::Failed`]).
    pub fn start<P: TokenProvider + 'static>(
        jobs: Vec<Job<P>>,
        options: EngineOptions,
    ) -> Result<Self, SyncError> {
        // The global excludes are merged into every job up front, so an
        // invalid pattern fails the start instead of a single job later.
        let mut jobs = jobs;
        for job in &mut jobs {
            job.filter.add_global_excludes(&options.global_exclude)?;
        }
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let mut commands = HashMap::new();
        let mut handles = Vec::new();
        for job in jobs {
            let (command_tx, command_rx) = mpsc::channel(16);
            let (trigger_tx, trigger_rx) = mpsc::unbounded_channel();
            // A failing watcher (e.g. inotify limit reached) must not keep
            // the other jobs from starting: the job falls back to the
            // periodic scan, and the failure is reported.
            let watcher = if options.watch {
                match LocalWatcher::start(job.local_root.clone(), options.debounce, trigger_tx) {
                    Ok(watcher) => Some(watcher),
                    Err(error) => {
                        let _ = event_tx.send(EngineEvent::Failed {
                            job_id: job.id.clone(),
                            path: String::new(),
                            error: format!(
                                "file watcher unavailable, falling back to the periodic scan: \
                                 {error}"
                            ),
                        });
                        None
                    }
                }
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
/// reacts to watcher triggers and the periodic interval, and rides out
/// offline phases with exponential backoff (Phase 8). While the server
/// is unreachable no passes run; triggers stay pending and are applied
/// once the connection is back (the queue survives). Explicit commands
/// (`SyncNow`, `Resume`) cancel a running backoff.
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
    // A `SyncNow` runs one pass even while paused; the pause stays.
    let mut forced = false;
    // Offline bookkeeping: `backoff` doubles per failed attempt, capped;
    // no pass starts before `next_attempt` while the server is down.
    let mut offline = false;
    let mut backoff = options.offline_backoff_base;
    let mut next_attempt = tokio::time::Instant::now();
    // The first tick of `interval` fires immediately; the startup pass
    // must not be followed by a second one.
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + options.sync_interval,
        options.sync_interval,
    );
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        if pending && (!paused || forced) && tokio::time::Instant::now() >= next_attempt {
            pending = false;
            forced = false;
            let _ = events.send(EngineEvent::RunStarted {
                job_id: job.id.clone(),
            });
            let (summary, aborted) = run_once(&job, &options, &events).await;
            let _ = events.send(EngineEvent::RunFinished {
                job_id: job.id.clone(),
                summary,
            });
            match aborted.filter(|error| error.should_back_off()) {
                Some(_) => {
                    // Unreachable server: pause the schedule, retry with
                    // a doubled backoff. Queued triggers stay pending.
                    let delay = if offline {
                        backoff.saturating_mul(2).min(options.offline_backoff_cap)
                    } else {
                        options.offline_backoff_base
                    };
                    offline = true;
                    backoff = delay;
                    next_attempt = tokio::time::Instant::now() + delay;
                    // The retry probe is the next run; without pending
                    // the loop would wait for the (possibly long)
                    // periodic interval instead.
                    pending = true;
                    let _ = events.send(EngineEvent::Offline {
                        job_id: job.id.clone(),
                        retry_in: delay,
                    });
                }
                None => {
                    if offline {
                        offline = false;
                        backoff = options.offline_backoff_base;
                        let _ = events.send(EngineEvent::Online {
                            job_id: job.id.clone(),
                        });
                    }
                    next_attempt = tokio::time::Instant::now();
                }
            }
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
            // Wake for the next offline retry attempt.
            _ = tokio::time::sleep_until(next_attempt), if pending && (!paused || forced) => {}
            command = commands.recv() => match command {
                Some(JobCommand::Pause) => paused = true,
                Some(command @ (JobCommand::Resume | JobCommand::SyncNow)) => {
                    // Resume catches up on what changed while the job was
                    // paused; SyncNow runs one pass and keeps a pause.
                    // Either cancels an offline backoff.
                    if command == JobCommand::Resume {
                        paused = false;
                    } else {
                        forced = true;
                    }
                    pending = true;
                    offline = false;
                    backoff = options.offline_backoff_base;
                    next_attempt = tokio::time::Instant::now();
                }
                Some(JobCommand::Stop) | None => break,
            },
        }
    }
    let _ = events.send(EngineEvent::Stopped {
        job_id: job.id.clone(),
    });
}

/// A queued transfer: whether it is an upload, its path, and the running
/// task.
type QueuedTransfer = (bool, String, JoinHandle<Result<TransferResult, SyncError>>);

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
        /// What the local file must still look like when the download
        /// lands (scan state), so later local edits are not overwritten.
        guard: LocalGuard,
    },
}

/// Per-path failure bookkeeping of one pass: counts errors, emits the
/// `Failed` events, and remembers the first throttling error (425/429).
/// A throttling server means the pass should stop and the job back off
/// — pushing through the remaining plan only prolongs the throttling
/// and leaves half-applied state behind.
struct PassFailures<'a> {
    /// The failing job (for the events).
    job_id: &'a str,
    /// Where the `Failed` events go.
    events: &'a UnboundedSender<EngineEvent>,
    /// How many paths failed so far (merged into the run summary).
    errors: usize,
    /// The first throttling error seen, if any.
    throttled: Option<SyncError>,
}

impl<'a> PassFailures<'a> {
    fn new(job_id: &'a str, events: &'a UnboundedSender<EngineEvent>) -> Self {
        Self {
            job_id,
            events,
            errors: 0,
            throttled: None,
        }
    }

    /// Records one failed path: counts it, emits the event, and notes
    /// the first throttling error. [`SyncError`] cannot be cloned (it
    /// wraps io errors), so a fresh representative of the throttling
    /// kind is kept; the per-path event carries the original.
    fn fail(&mut self, path: &str, error: &SyncError) {
        if self.throttled.is_none() {
            self.throttled = match error {
                SyncError::Api(ApiError::Status { code }) if *code == 425 => {
                    Some(SyncError::Api(ApiError::Status { code: *code }))
                }
                SyncError::Api(ApiError::RateLimited { retry_after }) => {
                    Some(SyncError::Api(ApiError::RateLimited {
                        retry_after: *retry_after,
                    }))
                }
                _ => None,
            };
        }
        self.errors += 1;
        let _ = self.events.send(EngineEvent::Failed {
            job_id: self.job_id.to_owned(),
            path: path.to_owned(),
            error: error.to_string(),
        });
    }
}

/// Reports the run-level failure and marks the pass aborted.
fn abort_pass(failures: &mut PassFailures<'_>, aborted: &mut Option<SyncError>, error: SyncError) {
    failures.fail("", &error);
    *aborted = Some(error);
}

/// Ends the pass: merges the counted per-path failures into the summary
/// and hands both to the job loop.
fn finish_pass(
    mut summary: RunSummary,
    failures: PassFailures<'_>,
    aborted: Option<SyncError>,
) -> (RunSummary, Option<SyncError>) {
    summary.errors += failures.errors;
    (summary, aborted)
}

/// One sync pass: recovery, scans, diff, plan execution, bookkeeping.
/// Failures of individual paths are reported as [`EngineEvent::Failed`]
/// and counted; only a failing scan or database aborts the pass early —
/// the aborting error is returned alongside the summary so the job loop
/// can classify it (a connectivity failure or server throttling switches
/// the job to offline backoff, Phase 8).
async fn run_once<P: TokenProvider + 'static>(
    job: &JobTask<P>,
    options: &EngineOptions,
    events: &UnboundedSender<EngineEvent>,
) -> (RunSummary, Option<SyncError>) {
    let mut summary = RunSummary::default();
    let mut aborted = None;
    let mut failures = PassFailures::new(&job.id, events);
    let now = unix_now();

    // Crash recovery: interrupted transfers keep their recorded baseline,
    // so the diff re-plans them; the stale state must not linger.
    if let Err(error) = db_lock(&job.db).reset_transfer_states() {
        abort_pass(&mut failures, &mut aborted, SyncError::from(error));
        return finish_pass(summary, failures, aborted);
    }

    // Stale .part files from a crash never become real files.
    if let Err(error) = scan::remove_part_files(&job.local_root) {
        abort_pass(&mut failures, &mut aborted, error);
        return finish_pass(summary, failures, aborted);
    }

    let (local, unreadable) = match scan::scan_local(&job.local_root, &job.filter) {
        Ok(scan) => (scan.entries, scan.unreadable),
        Err(error) => {
            abort_pass(&mut failures, &mut aborted, error);
            return finish_pass(summary, failures, aborted);
        }
    };

    // The recorded state, read once per pass. A changed filter
    // (include/exclude) first removes paths that no longer belong to the
    // synced set: their local copies are deleted (the cloud keeps them),
    // except for new or changed local content (see
    // `load_covered_entries`).
    let db_entries = match load_covered_entries(&job.db, &job.local_root, &job.filter) {
        Ok((entries, removed)) => {
            summary.deletes += removed;
            entries
        }
        Err(error) => {
            abort_pass(&mut failures, &mut aborted, error);
            return finish_pass(summary, failures, aborted);
        }
    };

    // Remote snapshot; the root ETag shortcuts the traversal when the
    // server reports no change since the last completed pass.
    let root = match job.dav.propfind("", Depth::Zero).await {
        Ok(root) => root,
        Err(error) => {
            let error = SyncError::from(error);
            abort_pass(&mut failures, &mut aborted, error);
            return finish_pass(summary, failures, aborted);
        }
    };
    let root_etag = root.first().and_then(|resource| resource.etag.clone());
    let stored_etag = db_lock(&job.db).meta("space_etag").ok().flatten();
    let skip_remote =
        options.root_etag_shortcut && stored_etag.is_some() && stored_etag == root_etag;

    let (remote, names, checksums, remote_unreadable) = if skip_remote {
        summary.remote_scan_skipped = true;
        let names = db_lock(&job.db).server_names().unwrap_or_default();
        (
            remote_from_db(&db_entries),
            names,
            HashMap::new(),
            Vec::new(),
        )
    } else {
        let listing = match job.dav.propfind("", Depth::One).await {
            Ok(listing) => listing,
            Err(error) => {
                let error = SyncError::from(error);
                abort_pass(&mut failures, &mut aborted, error);
                return finish_pass(summary, failures, aborted);
            }
        };
        match scan::scan_remote_from_root(
            job.dav.as_ref(),
            &job.filter,
            &listing,
            options.propfind_pause,
        )
        .await
        {
            Ok(snapshot) => {
                if let Err(error) = db_lock(&job.db).replace_server_names(&snapshot.server_names) {
                    failures.fail("", &SyncError::from(error));
                }
                (
                    snapshot.entries,
                    snapshot.server_names,
                    snapshot.checksums,
                    snapshot.unreadable,
                )
            }
            Err(error) => {
                abort_pass(&mut failures, &mut aborted, error);
                return finish_pass(summary, failures, aborted);
            }
        }
    };
    // Requests address paths by their server spelling.
    let names = ServerNames::new(names);

    // Open conflicts freeze their paths; nothing below them is touched.
    let mut frozen = HashSet::new();
    match db_lock(&job.db).pending_conflicts() {
        Ok(conflicts) => frozen.extend(conflicts.into_iter().map(|conflict| conflict.path)),
        Err(error) => {
            let error = SyncError::from(error);
            abort_pass(&mut failures, &mut aborted, error);
            return finish_pass(summary, failures, aborted);
        }
    }
    frozen.extend(
        db_entries
            .iter()
            .filter(|(_, entry)| entry.state == EntryState::Conflict)
            .map(|(path, _)| path.clone()),
    );
    // Locally unreadable paths are in an unknown state: freeze them for
    // this pass (a folder that cannot be listed must not look deleted).
    for (path, error) in unreadable {
        failures.fail(&path, &error);
        frozen.insert(path);
    }
    // The same applies to remote folders whose listing failed with a
    // server error (e.g. a 500 for a directory with more entries than
    // the server's internal message limit): their state is unknown, so
    // freeze the subtree and keep syncing the rest of the space.
    for (path, error) in remote_unreadable {
        failures.fail(&path, &error);
        frozen.insert(path);
    }
    // Server names that differ only in case cannot coexist on a
    // case-folding filesystem: freeze them (and their subtrees) instead
    // of letting one overwrite the other and the next pass delete it.
    if scan::folds_case(&job.local_root) {
        for group in scan::case_collisions(remote.keys()) {
            let error = SyncError::CaseCollision {
                paths: group.join("', '"),
            };
            failures.fail(&group[0], &error);
            frozen.extend(group);
        }
    }

    let plan = diff::build(&local, &remote, &db_entries, &frozen);
    let plan = verify_unrecorded_matches(plan, &checksums, &job.local_root).await;

    // What the run plans to move — the denominator of the live
    // progress events (uploads weigh their local size, downloads their
    // reported remote size; unknown sizes count as 0).
    let mut transfers_total = 0usize;
    let mut bytes_total = 0u64;
    for action in &plan {
        match action {
            SyncAction::Upload { path, .. } => {
                transfers_total += 1;
                bytes_total += planned_bytes(local.get(path).and_then(|node| node.size));
            }
            SyncAction::Download { remote, .. } => {
                transfers_total += 1;
                bytes_total += planned_bytes(remote.size);
            }
            _ => {}
        }
    }
    let tracker = RunTracker::new(
        &job.id,
        events.clone(),
        transfers_total,
        bytes_total,
        options.progress_interval,
    );
    if transfers_total > 0 {
        tracker.announce();
    }

    let context = TransferContext {
        dav: job.dav.clone(),
        root: job.local_root.clone(),
        chunk_size: options.download_chunk_size,
        semaphore: Arc::new(Semaphore::new(options.max_parallel_transfers.max(1))),
        tracker: tracker.clone(),
    };
    let mut transfers: Vec<QueuedTransfer> = Vec::new();
    // Plan steps that only touch the database are applied together in
    // one transaction after the loop (initial syncs record thousands).
    let mut db_writes: Vec<DbWrite> = Vec::new();

    for action in plan {
        // A throttling server ends the pass: no further requests are
        // issued, and the job loop backs off before the next one. The
        // remaining actions are re-planned from unchanged baselines.
        if failures.throttled.is_some() {
            break;
        }
        match action {
            SyncAction::MkcolRemote { path } => match job.dav.mkcol(&names.resolve(&path)).await {
                // 405: the folder already exists (created concurrently).
                Ok(()) | Err(ApiError::Status { code: 405 }) => {
                    summary.folders_created += 1;
                    match local_node_at(&job.local_root, &path) {
                        Ok(Some(node)) => {
                            if let Err(error) =
                                upsert_synced(&job.db, &path, true, node, None, None, now)
                            {
                                failures.fail(&path, &error);
                            }
                        }
                        Ok(None) => {}
                        Err(error) => failures.fail(&path, &error),
                    }
                }
                Err(error) => failures.fail(&path, &SyncError::from(error)),
            },
            SyncAction::MkcolLocal { path } => match scan::abs_local_path(&job.local_root, &path) {
                Ok(target) => {
                    if let Err(source) = std::fs::create_dir_all(&target) {
                        failures.fail(&path, &SyncError::io(&target, source));
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
                                    failures.fail(&path, &error);
                                }
                            }
                            Ok(None) => {}
                            Err(error) => failures.fail(&path, &error),
                        }
                    }
                }
                Err(error) => failures.fail(&path, &error),
            },
            SyncAction::DeleteRemote { path, is_dir } => {
                match job.dav.delete(&names.resolve(&path)).await {
                    // 404: already gone (e.g. the parent folder was deleted
                    // first) — the outcome is the same.
                    Ok(()) | Err(ApiError::NotFound) => {
                        let result = if is_dir {
                            db_lock(&job.db).delete_subtree(&path)
                        } else {
                            db_lock(&job.db).delete_entry(&path).map(|_| 0)
                        };
                        if let Err(error) = result {
                            failures.fail(&path, &SyncError::from(error));
                        } else {
                            summary.deletes += 1;
                        }
                    }
                    Err(error) => failures.fail(&path, &SyncError::from(error)),
                }
            }
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
                                failures.fail(&path, &SyncError::from(error));
                            } else {
                                summary.deletes += 1;
                            }
                        } else if let Err(error) = result {
                            failures.fail(&path, &error);
                        }
                    }
                    Err(error) => failures.fail(&path, &error),
                }
            }
            SyncAction::Upload {
                path,
                base_etag,
                create,
            } => {
                let _ = mark_state(&job.db, &path, EntryState::Uploading);
                let kind = PendingTransfer::Upload { base_etag, create };
                let planned = planned_bytes(local.get(&path).and_then(|node| node.size));
                let task =
                    spawn_transfer(&context, kind, path.clone(), names.resolve(&path), planned);
                transfers.push((true, path, task));
            }
            SyncAction::Download { path, remote } => {
                let _ = mark_state(&job.db, &path, EntryState::Downloading);
                let planned = planned_bytes(remote.size);
                let guard = LocalGuard::from_scan(local.get(&path).copied());
                let kind = PendingTransfer::Download { remote, guard };
                let task =
                    spawn_transfer(&context, kind, path.clone(), names.resolve(&path), planned);
                transfers.push((false, path, task));
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
                db_writes.push(DbWrite::Record(synced_entry(
                    &path, is_dir, node, etag, size, now,
                )));
            }
            SyncAction::DropEntry { path } => db_writes.push(DbWrite::Drop(path)),
        }
    }
    if !db_writes.is_empty() {
        let result = db_lock(&job.db).transaction(|db| {
            for write in &db_writes {
                match write {
                    DbWrite::Record(entry) => db.upsert_entry(entry)?,
                    DbWrite::Drop(path) => {
                        db.delete_entry(path)?;
                    }
                }
            }
            Ok(())
        });
        if let Err(error) = result {
            failures.fail("", &SyncError::from(error));
        }
    }

    // Transfers ran concurrently; apply their results to the database.
    let mut rescan_needed = false;
    for (is_upload, path, task) in transfers {
        let result = match task.await {
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
                        if is_upload {
                            summary.uploads += 1;
                        } else {
                            summary.downloads += 1;
                        }
                    }
                    Err(error) => failures.fail(&path, &error),
                }
            }
            Ok(TransferResult::PreconditionFailed) => {
                // The remote side changed while we uploaded: conflict,
                // with the server's current state as its remote half.
                // That lookup must succeed — a failed one (throttling,
                // network) must not read as "deleted remotely", so the
                // path is left to the next pass instead of freezing it
                // behind a conflict with a made-up remote side.
                let current_remote =
                    match current_remote(job.dav.as_ref(), &names.resolve(&path), &path).await {
                        Ok(remote) => remote,
                        Err(error) => {
                            failures.fail(&path, &error);
                            let _ = mark_state(&job.db, &path, EntryState::Synced);
                            continue;
                        }
                    };
                let local_node = local.get(&path).copied();
                if local_node.is_none() && current_remote.is_none() {
                    // The path vanished on both sides since the scan:
                    // not a conflict — whatever entry exists goes too.
                    if let Err(error) = db_lock(&job.db).delete_entry(&path) {
                        failures.fail(&path, &SyncError::from(error));
                    }
                } else {
                    let sighting = ConflictSighting {
                        path,
                        local: local_node,
                        remote: current_remote,
                        at: now,
                    };
                    handle_conflict(job, options, events, &mut summary, &sighting).await;
                }
            }
            Ok(TransferResult::LocalChanged) => {
                // The local file was edited while the download ran; the
                // edit is kept and the baseline stays as recorded, so the
                // next pass sees both sides changed and records a
                // conflict. Reset the transfer state for that pass.
                let _ = mark_state(&job.db, &path, EntryState::Synced);
                rescan_needed = true;
            }
            Err(error) => {
                let _ = mark_state(&job.db, &path, EntryState::Error);
                failures.fail(&path, &error);
            }
        }
    }
    if transfers_total > 0 {
        tracker.complete();
    }

    // The root ETag may only vouch for the recorded state when the pass
    // applied everything it planned. After a failed or skipped path the
    // database still holds the old remote state for it; storing the new
    // ETag would let the shortcut hide the change until something else
    // changes on the server, so the next pass must traverse again.
    // (Like before, a failing ETag write itself does not revoke the
    // pass's completeness — it only counts as an error.)
    let complete = failures.errors == 0 && summary.errors == 0 && !rescan_needed;
    let etag_result = match root_etag.filter(|_| complete) {
        Some(etag) => db_lock(&job.db).set_meta("space_etag", &etag),
        None => db_lock(&job.db).delete_meta("space_etag"),
    };
    if let Err(error) = etag_result {
        failures.fail("", &SyncError::from(error));
    }
    if let Err(error) = db_lock(&job.db).set_last_sync(now) {
        failures.fail("", &SyncError::from(error));
    }
    // The struct counted the per-path errors along the way; the summary
    // carries its own (conflict bookkeeping). A throttling server ends
    // the pass as its run-level error, so the job loop backs off.
    let aborted = aborted.or(failures.throttled.take());
    finish_pass(summary, failures, aborted)
}

/// What every transfer task of one pass shares.
struct TransferContext<P: TokenProvider> {
    /// The job's WebDAV client.
    dav: Arc<WebDavClient<P>>,
    /// The job's local root.
    root: PathBuf,
    /// Chunk size of range downloads.
    chunk_size: u64,
    /// Caps the number of concurrent transfers (`max_parallel_transfers`).
    semaphore: Arc<Semaphore>,
    /// The pass's progress tracker.
    tracker: Arc<RunTracker>,
}

/// Spawns one transfer task; the semaphore caps the number of concurrent
/// transfers (`max_parallel_transfers`). The transfer registers with the
/// pass's progress tracker once it holds a permit — so the live view
/// lists running transfers only — and settles there on every outcome:
/// a completed transfer counts its `planned` bytes, any other result
/// (conflict, kept local edit, error) is abandoned.
fn spawn_transfer<P: TokenProvider + 'static>(
    context: &TransferContext<P>,
    kind: PendingTransfer,
    path: String,
    remote_path: String,
    planned: u64,
) -> JoinHandle<Result<TransferResult, SyncError>> {
    let tracker = context.tracker.clone();
    let dav = context.dav.clone();
    let root = context.root.clone();
    let chunk_size = context.chunk_size;
    let semaphore = context.semaphore.clone();
    tokio::spawn(async move {
        let _permit = semaphore
            .acquire_owned()
            .await
            .map_err(|_| SyncError::Api(ApiError::protocol("transfer semaphore closed")))?;
        let direction = match kind {
            PendingTransfer::Upload { .. } => TransferDirection::Upload,
            PendingTransfer::Download { .. } => TransferDirection::Download,
        };
        let handle = tracker.start(&path, direction, planned);
        let paths = TransferPaths {
            root: &root,
            path: &path,
            remote_path: &remote_path,
        };
        let result = match kind {
            PendingTransfer::Upload { base_etag, create } => {
                transfer::upload(dav.as_ref(), paths, base_etag, create, Some(&handle)).await
            }
            PendingTransfer::Download { remote, guard } => {
                transfer::download(
                    dav.as_ref(),
                    paths,
                    &remote,
                    chunk_size,
                    guard,
                    Some(&handle),
                )
                .await
            }
        };
        match &result {
            Ok(TransferResult::Done(_)) => handle.finish(planned),
            _ => handle.abandon(),
        }
        result
    })
}

/// Checks the size-only matches of the initial sync against the server's
/// checksums. Without a recorded baseline, the diff treats a file that
/// exists on both sides with equal size as in sync; two different files
/// of equal size would then silently pair up, and the next change on
/// either side would overwrite the other. Where the server reports a
/// SHA1, the local file is hashed (on a blocking thread); a mismatch
/// turns the match into a conflict. Unreadable files keep the size match.
async fn verify_unrecorded_matches(
    plan: Vec<SyncAction>,
    checksums: &HashMap<String, String>,
    root: &std::path::Path,
) -> Vec<SyncAction> {
    let candidates: Vec<(usize, String, String)> = plan
        .iter()
        .enumerate()
        .filter_map(|(index, action)| match action {
            SyncAction::RecordSynced {
                path,
                is_dir: false,
                local: Some(_),
                remote: Some(_),
            } => checksums
                .get(path)
                .map(|sha1| (index, path.clone(), sha1.clone())),
            _ => None,
        })
        .collect();
    if candidates.is_empty() {
        return plan;
    }
    let root = root.to_path_buf();
    let mismatched: HashSet<usize> = tokio::task::spawn_blocking(move || {
        candidates
            .into_iter()
            .filter(|(_, path, expected)| {
                scan::local_sha1(&root, path).is_some_and(|actual| actual != *expected)
            })
            .map(|(index, ..)| index)
            .collect()
    })
    .await
    .unwrap_or_default();
    plan.into_iter()
        .enumerate()
        .map(|(index, action)| match action {
            SyncAction::RecordSynced {
                path,
                local,
                remote,
                ..
            } if mismatched.contains(&index) => SyncAction::Conflict {
                path,
                local,
                remote,
            },
            other => other,
        })
        .collect()
}

/// A plan step that only writes the database (batched per pass).
enum DbWrite {
    /// Record a synced entry.
    Record(NewEntry),
    /// Drop the entry of a path that vanished on both sides.
    Drop(String),
}

/// Reads all recorded entries and removes the ones the job's filter no
/// longer covers (selective-sync changes) in one transaction. Their
/// local copies are deleted as well — the cloud keeps everything.
/// Only paths whose recorded baseline still matches the local state go
/// (the diff's own "unchanged" test), so new or changed local files
/// below a deselected folder stay, and so does every folder that is not
/// empty afterwards — the same safety rule as
/// `keep_folders_with_content` for deletions. Returns the remaining
/// entries by path and the number of removed local files/folders.
fn load_covered_entries(
    db: &std::sync::Mutex<StatusDb>,
    root: &std::path::Path,
    filter: &Filter,
) -> Result<(HashMap<String, NewEntry>, usize), SyncError> {
    let (mut entries, uncovered): (HashMap<String, NewEntry>, Vec<String>) = {
        let db = db_lock(db);
        let entries: HashMap<String, NewEntry> = db
            .all_entries()?
            .into_iter()
            .map(|entry| (entry.entry.path.clone(), entry.entry))
            .collect();
        let uncovered = entries
            .keys()
            .filter(|path| {
                let is_dir = entries.get(*path).is_some_and(|entry| entry.is_dir);
                !filter.covers(path, is_dir)
            })
            .cloned()
            .collect();
        (entries, uncovered)
    };
    if uncovered.is_empty() {
        return Ok((entries, 0));
    }
    let removed = remove_uncovered_local(root, &entries, &uncovered);
    db_lock(db).transaction(|db| {
        for path in &uncovered {
            db.delete_entry(path)?;
        }
        Ok(())
    })?;
    for path in &uncovered {
        entries.remove(path);
    }
    Ok((entries, removed))
}

/// Deletes the local copies of paths the filter no longer covers. Files
/// go first, folders afterwards deepest-first and only once they are
/// empty; returns how many paths were removed.
fn remove_uncovered_local(
    root: &std::path::Path,
    entries: &HashMap<String, NewEntry>,
    uncovered: &[String],
) -> usize {
    let mut removed = 0usize;
    let mut dirs: Vec<&str> = Vec::new();
    for path in uncovered {
        match entries.get(path) {
            Some(entry) if entry.is_dir => dirs.push(path),
            Some(entry)
                if local_baseline_matches(root, path, entry) && remove_local_file(root, path) =>
            {
                removed += 1;
            }
            _ => {}
        }
    }
    // Children sort before their parents, so a folder only disappears
    // once everything below it is gone.
    dirs.sort_unstable_by(|a, b| b.cmp(a));
    for dir in dirs {
        if remove_empty_local_dir(root, dir) {
            removed += 1;
        }
    }
    removed
}

/// Whether the local file still looks exactly as recorded at the last
/// sync — the same test the diff uses to detect local changes. A path
/// that cannot be read counts as changed: it is never deleted blindly.
fn local_baseline_matches(root: &std::path::Path, rel: &str, entry: &NewEntry) -> bool {
    match local_node_at(root, rel) {
        Ok(Some(node)) => {
            diff::mtime_matches(node.mtime, entry.local_mtime) && node.size == entry.local_size
        }
        _ => false,
    }
}

/// Removes one deselected local file (read-only attributes are cleared
/// first). Errors keep the file: it only stays behind as an untracked
/// leftover, like a path the filter never covered.
fn remove_local_file(root: &std::path::Path, rel: &str) -> bool {
    let Ok(target) = scan::abs_local_path(root, rel) else {
        return false;
    };
    match transfer::make_writable(&target) {
        Ok(()) => {}
        Err(SyncError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
            return true;
        }
        Err(_) => return false,
    }
    std::fs::remove_file(&target).is_ok()
}

/// Removes a deselected folder, but never recursively and only once it
/// is empty — any content the sync cannot account for keeps the folder
/// alive.
fn remove_empty_local_dir(root: &std::path::Path, rel: &str) -> bool {
    let Ok(target) = scan::abs_local_path(root, rel) else {
        return false;
    };
    let Ok(children) = std::fs::read_dir(&target) else {
        return false;
    };
    let empty = children.filter_map(Result::ok).next().is_none();
    empty && std::fs::remove_dir(&target).is_ok()
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

/// The current remote state of a path, as needed after a rejected
/// optimistic write: `Ok(None)` — the path is gone on the server; `Err`
/// — the state could not be determined (the PROPFIND failed or returned
/// nothing). `None` must never be guessed from a failed lookup: a
/// conflict row would read it as "deleted remotely", and resolutions
/// would delete or discard local data on that guess.
async fn current_remote<P: TokenProvider>(
    dav: &WebDavClient<P>,
    remote_path: &str,
    path: &str,
) -> Result<Option<RemoteNode>, SyncError> {
    match dav.propfind(remote_path, Depth::Zero).await {
        Ok(resources) => resources
            .first()
            .map(RemoteNode::from_resource)
            .map(Some)
            .ok_or_else(|| {
                SyncError::Api(ApiError::protocol(format!(
                    "'{path}' rejected the write and reported no current state"
                )))
            }),
        Err(ApiError::NotFound) => Ok(None),
        Err(source) => Err(SyncError::Api(source)),
    }
}

/// Marks an existing entry with a new lifecycle state (transfer/error);
/// new paths get their first entry only once a transfer succeeds.
fn mark_state(
    db: &std::sync::Mutex<StatusDb>,
    path: &str,
    state: EntryState,
) -> Result<(), SyncError> {
    db_lock(db).set_state(path, state)?;
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
    db_lock(db).upsert_entry(&synced_entry(
        path,
        is_dir,
        local,
        remote_etag,
        remote_size,
        now,
    ))?;
    Ok(())
}

/// The database entry of a path in sync with the given local and remote
/// state.
fn synced_entry(
    path: &str,
    is_dir: bool,
    local: LocalNode,
    remote_etag: Option<String>,
    remote_size: Option<i64>,
    now: i64,
) -> NewEntry {
    NewEntry {
        path: path.to_owned(),
        is_dir,
        local_mtime: local.mtime,
        local_size: local.size,
        remote_etag,
        remote_size,
        checksum: None,
        last_synced_at: Some(now),
        state: EntryState::Synced,
    }
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
    // Conflict row and frozen entry are written together: a crash in
    // between must not leave a conflict whose path is not frozen.
    db_lock(db)
        .transaction(|db| {
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
        })
        .map_err(SyncError::from)
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

/// A transfer's planned byte count; unknown sizes count as 0.
fn planned_bytes(size: Option<i64>) -> u64 {
    size.map(|size| size.max(0) as u64).unwrap_or(0)
}
