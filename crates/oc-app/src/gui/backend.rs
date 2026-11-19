//! The tokio-side backend of the GUI (Phase 7): it owns the sync engine
//! and serves the UI's commands. The GUI never touches the engine
//! directly — everything flows through the command and event channels
//! (see `PLAN.md`, Phase 7: "Engine-Anbindung ausschließlich über den
//! Event-/Command-Kanal").
//!
//! The GUI thread sends [`UiCommand`]s through an unbounded tokio channel
//! (sending is synchronous and never blocks) and polls [`BackendEvent`]s
//! from a `std::sync::mpsc` receiver each frame. A dedicated forwarder
//! task moves engine events into that channel so the engine's receiver
//! never blocks the engine.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::Sender;
use std::time::Duration;

use oc_auth::DEFAULT_LOGIN_TIMEOUT;
use oc_config::{Account, Config, Jobs, SyncJob};
use oc_db::{Conflict, ConflictResolution, Stats, StatusDb, db_path};
use oc_storage::{RemoteEntry, RemoteStorage, StorageRoot};
use oc_sync::{Engine, EngineEvent, EngineOptions, Job, JobCommand};
use tokio::sync::mpsc;

use crate::provider::{Provider, Providers};

/// What the GUI asks the backend to do. Sent through an unbounded channel;
/// sending is synchronous and never blocks.
#[derive(Debug)]
pub enum UiCommand {
    /// A per-job control command (pause/resume/sync now/stop).
    JobCmd {
        /// The target job.
        job_id: String,
        /// The command.
        command: JobCommand,
    },
    /// Pause all jobs.
    PauseAll,
    /// Resume all jobs.
    ResumeAll,
    /// Resolve pending conflicts of one job: download the server version
    /// over the local file (`Server`) or upload the local version
    /// (`Local`). One answer per conflict ([`BackendEvent::ResolveFinished`]).
    ResolveConflicts {
        /// The job the conflicts belong to.
        job_id: String,
        /// The conflict row ids, resolved one after the other.
        conflict_ids: Vec<i64>,
        /// Which side wins.
        resolution: ConflictResolution,
    },
    /// Interactive login (opens the browser).
    Login {
        /// The account to log in.
        account_id: String,
    },
    /// Logout: revoke and clear the stored credential.
    Logout {
        /// The account to log out.
        account_id: String,
    },
    /// List the syncable roots (spaces) of an account for the wizard.
    ListSpaces {
        /// The account whose spaces to list.
        account_id: String,
    },
    /// List the direct subfolders of a remote folder for the wizard's
    /// folder tree.
    FetchChildren {
        /// The account (login).
        account_id: String,
        /// The space to traverse.
        space_id: String,
        /// The folder's server path inside the space (`""` = root), as
        /// reported by `RemoteEntry::server_path`.
        path: String,
        /// Correlation token; the answer arrives as
        /// [`BackendEvent::Children`] with the same token.
        token: u32,
    },
    /// Reload `config.json`/`jobs.json` and restart the engine — applied
    /// after the GUI saved changes (new job, filters, settings, login).
    ApplyJobs,
    /// Read statistics, pending conflicts, and running transfers of these
    /// jobs from their status databases; the answer arrives as
    /// [`BackendEvent::Snapshots`]. A request arriving while the previous
    /// one is still being read is dropped.
    RefreshSnapshots {
        /// The jobs to read.
        job_ids: Vec<String>,
    },
    /// Check the releases feed for a newer version (notification only);
    /// the answer arrives as [`BackendEvent::UpdateChecked`].
    CheckUpdates,
}

/// What the GUI shows of one job's status database.
#[derive(Debug, Default)]
pub struct JobSnapshot {
    /// The job.
    pub job_id: String,
    /// Aggregated numbers; `None` before the first sync (no database yet)
    /// or when it could not be read.
    pub stats: Option<Stats>,
    /// Pending conflicts.
    pub conflicts: Vec<Conflict>,
    /// Why the database could not be read, if it could not.
    pub error: Option<String>,
}

/// What the backend reports back to the GUI.
#[derive(Debug)]
pub enum BackendEvent {
    /// An event of the sync engine.
    Engine(EngineEvent),
    /// An interactive login finished.
    LoginFinished {
        /// The account.
        account_id: String,
        /// `Ok` when the credential is stored.
        result: Result<(), String>,
    },
    /// A logout finished.
    LogoutFinished {
        /// The account.
        account_id: String,
        /// `Ok` when the login was cleared.
        result: Result<(), String>,
    },
    /// The spaces of an account (wizard step 2).
    Spaces {
        /// The account.
        account_id: String,
        /// The spaces, or an error message.
        result: Result<Vec<StorageRoot>, String>,
    },
    /// The subfolders of a wizard tree node (correlated by token).
    Children {
        /// The correlation token of the request.
        token: u32,
        /// The folders, or an error message.
        result: Result<Vec<RemoteEntry>, String>,
    },
    /// A conflict resolution finished; `Ok(false)` means the server
    /// changed again and the conflict stayed pending.
    ResolveFinished {
        /// The job.
        job_id: String,
        /// The conflict row id.
        conflict_id: i64,
        /// See [`oc_sync::ResolveOutcome`]: `true` = resolved,
        /// `false` = still pending (remote changed again).
        result: Result<bool, String>,
    },
    /// A message for the GUI's status line (e.g. skipped jobs).
    Message {
        /// Human-readable text.
        message: String,
    },
    /// Database snapshots of the requested jobs.
    Snapshots(Vec<JobSnapshot>),
    /// The update check finished (see `crate::updates`).
    UpdateChecked {
        /// The outcome: up to date, a newer release, not configured, or
        /// an error message.
        result: Result<crate::updates::UpdateCheck, String>,
    },
}

/// The GUI end of the event channel: sends an event and wakes the GUI,
/// which otherwise only polls at its idle frame rate.
#[derive(Clone)]
struct GuiSink {
    events: Sender<BackendEvent>,
    repaint: Arc<std::sync::OnceLock<eframe::egui::Context>>,
}

impl GuiSink {
    /// Delivers an event; `false` once the GUI is gone.
    fn send(&self, event: BackendEvent) -> bool {
        let delivered = self.events.send(event).is_ok();
        if let Some(context) = self.repaint.get() {
            context.request_repaint();
        }
        delivered
    }
}

/// Handle for the GUI to talk to the backend; dropped when the GUI exits,
/// which ends the backend task and stops the engine.
#[derive(Debug, Clone)]
pub struct BackendHandle {
    commands: mpsc::UnboundedSender<UiCommand>,
    repaint: Arc<std::sync::OnceLock<eframe::egui::Context>>,
}

impl BackendHandle {
    /// Sends a command; failures (backend gone) are ignored — the GUI
    /// is on its way out anyway.
    pub fn send(&self, command: UiCommand) {
        let _ = self.commands.send(command);
    }

    /// Lets the backend wake the GUI whenever it sends an event (set
    /// once the egui context exists).
    pub fn set_repaint_context(&self, context: eframe::egui::Context) {
        let _ = self.repaint.set(context);
    }
}

/// Starts the backend: loads config and jobs, starts the sync engine,
/// and spawns the forwarder and command tasks. Returns the GUI's handle
/// and the event receiver to poll each frame.
pub fn spawn(config_dir: PathBuf) -> Result<(BackendHandle, Receiver<BackendEvent>), String> {
    let (config, jobs) = load_workspace(&config_dir)?;
    prune_orphan_dbs(&config_dir, &jobs);
    let providers = Providers::default();
    let (mut engine, notices) = start_engine(&config_dir, &providers, &config, &jobs)?;
    let (gui_tx, gui_rx) = std_mpsc::channel::<BackendEvent>();
    let repaint = Arc::new(std::sync::OnceLock::new());
    let gui = GuiSink {
        events: gui_tx,
        repaint: repaint.clone(),
    };
    for message in notices {
        gui.send(BackendEvent::Message { message });
    }
    spawn_forwarder(engine.take_events(), gui.clone());

    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let backend = Backend {
        config_dir,
        providers,
        gui,
        // Weak: the backend must not keep its own command channel open,
        // or it would never notice the GUI quitting.
        internal: command_tx.downgrade(),
        snapshot_dbs: Arc::default(),
        snapshot_busy: Arc::default(),
    };
    tokio::spawn(backend_task(backend, command_rx, engine));
    Ok((
        BackendHandle {
            commands: command_tx,
            repaint,
        },
        gui_rx,
    ))
}

/// What the backend's tasks share.
#[derive(Clone)]
struct Backend {
    /// The configuration directory.
    config_dir: PathBuf,
    /// Storage providers per account.
    providers: Providers,
    /// The GUI's event channel.
    gui: GuiSink,
    /// The backend's own command channel (e.g. to restart the engine after
    /// a login finished in its own task).
    internal: mpsc::WeakUnboundedSender<UiCommand>,
    /// Read connections to the job databases for GUI snapshots (opened
    /// once; dropped before an engine restart so removed jobs' files can
    /// be deleted).
    snapshot_dbs: Arc<std::sync::Mutex<HashMap<String, StatusDb>>>,
    /// A snapshot read is in progress.
    snapshot_busy: Arc<std::sync::atomic::AtomicBool>,
}

impl Backend {
    /// Sends an event to the GUI; a closed GUI is ignored.
    fn report(&self, event: BackendEvent) {
        self.gui.send(event);
    }

    /// Asks the backend task for an engine restart.
    fn request_restart(&self) {
        if let Some(commands) = self.internal.upgrade() {
            let _ = commands.send(UiCommand::ApplyJobs);
        }
    }
}

/// The backend task: serves UI commands until the GUI drops its handle,
/// then stops the engine cleanly. Engine commands are applied in order;
/// requests that talk to the server (login, spaces, folder tree,
/// conflicts) run as their own tasks, so a pending browser login does
/// not hold up pause/resume or the wizard.
async fn backend_task(
    backend: Backend,
    mut commands: mpsc::UnboundedReceiver<UiCommand>,
    mut engine: Engine,
) {
    while let Some(command) = commands.recv().await {
        match command {
            UiCommand::JobCmd { job_id, command } => {
                if engine.send(&job_id, command).await.is_err() {
                    backend.report(BackendEvent::Message {
                        message: format!("Job '{job_id}' läuft nicht mehr (Engine neustarten?)"),
                    });
                }
            }
            UiCommand::PauseAll => engine.pause_all().await,
            UiCommand::ResumeAll => engine.resume_all().await,
            UiCommand::ApplyJobs => {
                backend
                    .snapshot_dbs
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .clear();
                engine = restart_engine(
                    &backend.config_dir,
                    &backend.providers,
                    &backend.gui,
                    engine,
                )
                .await;
            }
            UiCommand::RefreshSnapshots { job_ids } => {
                use std::sync::atomic::Ordering;
                if backend.snapshot_busy.swap(true, Ordering::AcqRel) {
                    continue;
                }
                let backend = backend.clone();
                tokio::spawn(async move {
                    let reader = backend.clone();
                    let snapshots =
                        tokio::task::spawn_blocking(move || read_snapshots(&reader, &job_ids))
                            .await
                            .unwrap_or_default();
                    backend.snapshot_busy.store(false, Ordering::Release);
                    backend.report(BackendEvent::Snapshots(snapshots));
                });
            }
            UiCommand::Login { account_id } => {
                let backend = backend.clone();
                tokio::spawn(async move {
                    let result =
                        run_login(&backend.config_dir, &backend.providers, &account_id).await;
                    let ok = result.is_ok();
                    backend.report(BackendEvent::LoginFinished { account_id, result });
                    if ok {
                        backend.request_restart();
                    }
                });
            }
            UiCommand::Logout { account_id } => {
                let backend = backend.clone();
                tokio::spawn(async move {
                    let result =
                        run_logout(&backend.config_dir, &backend.providers, &account_id).await;
                    let ok = result.is_ok();
                    backend.report(BackendEvent::LogoutFinished { account_id, result });
                    if ok {
                        backend.request_restart();
                    }
                });
            }
            UiCommand::ListSpaces { account_id } => {
                let backend = backend.clone();
                tokio::spawn(async move {
                    let result =
                        run_list_spaces(&backend.config_dir, &backend.providers, &account_id).await;
                    backend.report(BackendEvent::Spaces {
                        account_id,
                        result: result.map_err(|error| error.to_string()),
                    });
                });
            }
            UiCommand::FetchChildren {
                account_id,
                space_id,
                path,
                token,
            } => {
                let backend = backend.clone();
                tokio::spawn(async move {
                    let result = run_fetch_children(
                        &backend.config_dir,
                        &backend.providers,
                        &account_id,
                        &space_id,
                        &path,
                    )
                    .await;
                    backend.report(BackendEvent::Children {
                        token,
                        result: result.map_err(|error| error.to_string()),
                    });
                });
            }
            UiCommand::ResolveConflicts {
                job_id,
                conflict_ids,
                resolution,
            } => {
                let backend = backend.clone();
                tokio::spawn(async move {
                    run_resolve(&backend, &job_id, &conflict_ids, resolution).await;
                });
            }
            UiCommand::CheckUpdates => {
                let backend = backend.clone();
                tokio::spawn(async move {
                    let result = crate::updates::check_latest().await;
                    backend.report(BackendEvent::UpdateChecked { result });
                });
            }
        }
    }
    engine.stop().await;
}

/// Rebuilds the engine from the current `config.json`/`jobs.json` and
/// wires a fresh event forwarder. Used after every change that affects
/// the running jobs (new job, filters, settings, login/logout).
///
/// The configuration is loaded and validated before the running engine is
/// stopped: an unreadable file keeps the current engine running.
async fn restart_engine(
    config_dir: &std::path::Path,
    providers: &Providers,
    gui: &GuiSink,
    mut engine: Engine,
) -> Engine {
    let (config, jobs) = match load_workspace(config_dir) {
        Ok(pair) => pair,
        Err(error) => {
            gui.send(BackendEvent::Message {
                message: format!(
                    "Konfiguration unlesbar, die Engine läuft mit dem bisherigen Stand weiter: \
                     {error}"
                ),
            });
            return engine;
        }
    };
    engine.stop().await;
    prune_orphan_dbs(config_dir, &jobs);
    match start_engine(config_dir, providers, &config, &jobs) {
        Ok((mut new_engine, notices)) => {
            for message in notices {
                gui.send(BackendEvent::Message { message });
            }
            spawn_forwarder(new_engine.take_events(), gui.clone());
            new_engine
        }
        Err(error) => {
            gui.send(BackendEvent::Message {
                message: format!(
                    "Sync-Engine konnte nicht gestartet werden, alle Jobs stehen: {error}"
                ),
            });
            engine
        }
    }
}

/// Forwards engine events into the GUI's event channel until the engine
/// stops (channel closed). Every event also goes to the process log —
/// the frequent progress snapshots at debug level only.
fn spawn_forwarder(mut events: mpsc::UnboundedReceiver<EngineEvent>, gui: GuiSink) {
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if matches!(event, EngineEvent::Progress { .. }) {
                tracing::debug!("{event}");
            } else {
                tracing::info!("{event}");
            }
            if !gui.send(BackendEvent::Engine(event)) {
                break;
            }
        }
    });
}

/// Reads the GUI snapshots of `job_ids` (blocking; runs on a blocking
/// thread). Connections are opened once and kept; those of jobs no longer
/// asked for are closed.
fn read_snapshots(backend: &Backend, job_ids: &[String]) -> Vec<JobSnapshot> {
    let mut dbs = backend
        .snapshot_dbs
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    dbs.retain(|job_id, _| job_ids.contains(job_id));
    let mut snapshots = Vec::with_capacity(job_ids.len());
    for job_id in job_ids {
        let mut snapshot = JobSnapshot {
            job_id: job_id.clone(),
            ..JobSnapshot::default()
        };
        if !dbs.contains_key(job_id) {
            let path = match db_path(&backend.config_dir, job_id) {
                Ok(path) => path,
                Err(error) => {
                    snapshot.error = Some(error.to_string());
                    snapshots.push(snapshot);
                    continue;
                }
            };
            // No database yet: the job has not synced.
            if !path.exists() {
                snapshots.push(snapshot);
                continue;
            }
            match StatusDb::open(&path) {
                Ok(db) => {
                    dbs.insert(job_id.clone(), db);
                }
                Err(error) => {
                    snapshot.error = Some(error.to_string());
                    snapshots.push(snapshot);
                    continue;
                }
            }
        }
        let Some(db) = dbs.get(job_id) else {
            snapshots.push(snapshot);
            continue;
        };
        let read = (|| -> Result<(), oc_db::DbError> {
            snapshot.stats = Some(db.stats()?);
            snapshot.conflicts = db.pending_conflicts()?;
            Ok(())
        })();
        if let Err(error) = read {
            snapshot.error = Some(error.to_string());
        }
        snapshots.push(snapshot);
    }
    snapshots
}

/// Loads config and jobs, validating cross-references.
fn load_workspace(config_dir: &std::path::Path) -> Result<(Config, Jobs), String> {
    let config = oc_config::load_config(config_dir).map_err(|error| error.to_string())?;
    let jobs = oc_config::load_jobs(config_dir).map_err(|error| error.to_string())?;
    oc_config::validate_references(&config, &jobs).map_err(|error| error.to_string())?;
    Ok((config, jobs))
}

/// Derives the engine options from the global settings.
fn engine_options(config: &Config) -> EngineOptions {
    EngineOptions {
        sync_interval: Duration::from_secs(config.settings.sync_interval_secs),
        max_parallel_transfers: config.settings.max_parallel_transfers as usize,
        propfind_pause: Duration::from_millis(config.settings.propfind_pause_ms),
        remote_scan_concurrency: config.settings.max_parallel_listings as usize,
        conflict_policy: config.settings.conflict_policy,
        global_exclude: config.settings.global_exclude.clone(),
        ..EngineOptions::default()
    }
}

/// Looks up an account in `config.json`.
fn account_of(config_dir: &std::path::Path, account_id: &str) -> Result<Account, String> {
    let config = oc_config::load_config(config_dir).map_err(|error| error.to_string())?;
    config
        .accounts
        .into_iter()
        .find(|account| account.id == account_id)
        .ok_or_else(|| format!("unbekanntes Konto '{account_id}'"))
}

/// The shared provider of an account; fails on unknown accounts.
fn provider_of(
    config_dir: &std::path::Path,
    providers: &Providers,
    account_id: &str,
) -> Result<Provider, String> {
    let account = account_of(config_dir, account_id)?;
    providers.get(&account).map_err(|error| error.to_string())
}

/// Looks up a job and its account.
fn job_account(config_dir: &std::path::Path, job_id: &str) -> Result<(SyncJob, Account), String> {
    let (config, jobs) = load_workspace(config_dir)?;
    let job = jobs
        .jobs
        .iter()
        .find(|job| job.id == job_id)
        .ok_or_else(|| format!("unbekannter Job '{job_id}'"))?
        .clone();
    let account = config
        .accounts
        .into_iter()
        .find(|account| account.id == job.account_id)
        .ok_or_else(|| format!("Job '{job_id}' verweist auf unbekanntes Konto"))?;
    Ok((job, account))
}

/// Requires a stored login for the account.
fn require_login(provider: &Provider, account_id: &str) -> Result<(), String> {
    if !provider.has_stored_login() {
        return Err(format!(
            "Konto '{account_id}' ist nicht angemeldet (zuerst einloggen)"
        ));
    }
    Ok(())
}

/// Interactive browser login for one account.
async fn run_login(
    config_dir: &std::path::Path,
    providers: &Providers,
    account_id: &str,
) -> Result<(), String> {
    let provider = provider_of(config_dir, providers, account_id)?;
    provider
        .login(true, DEFAULT_LOGIN_TIMEOUT)
        .await
        .map_err(|error| error.to_string())
}

/// Logout of one account: revokes the stored credential and clears it.
async fn run_logout(
    config_dir: &std::path::Path,
    providers: &Providers,
    account_id: &str,
) -> Result<(), String> {
    let provider = provider_of(config_dir, providers, account_id)?;
    provider.logout().await.map_err(|error| error.to_string())
}

/// Lists the syncable roots (OpenCloud spaces) of an account.
async fn run_list_spaces(
    config_dir: &std::path::Path,
    providers: &Providers,
    account_id: &str,
) -> Result<Vec<StorageRoot>, String> {
    let provider = provider_of(config_dir, providers, account_id)?;
    require_login(&provider, account_id)?;
    provider.roots().await.map_err(|error| error.to_string())
}

/// Lists the direct subfolders of a remote folder (wizard folder tree).
async fn run_fetch_children(
    config_dir: &std::path::Path,
    providers: &Providers,
    account_id: &str,
    space_id: &str,
    path: &str,
) -> Result<Vec<RemoteEntry>, String> {
    let provider = provider_of(config_dir, providers, account_id)?;
    require_login(&provider, account_id)?;
    let storage = provider
        .storage(space_id)
        .map_err(|error| error.to_string())?;
    let listing = storage
        .list(path)
        .await
        .map_err(|error| error.to_string())?;
    let mut folders: Vec<RemoteEntry> = listing
        .children
        .into_iter()
        .filter(|entry| entry.is_dir)
        .collect();
    folders.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(folders)
}

/// Resolves pending conflicts of one job (see `PLAN.md`, Phase 6) with
/// one client and one database connection, reporting each outcome.
async fn run_resolve(
    backend: &Backend,
    job_id: &str,
    conflict_ids: &[i64],
    resolution: ConflictResolution,
) {
    let setup = resolve_setup(&backend.config_dir, &backend.providers, job_id);
    let (storage, db, local_path) = match setup {
        Ok(setup) => setup,
        Err(error) => {
            for &conflict_id in conflict_ids {
                backend.report(BackendEvent::ResolveFinished {
                    job_id: job_id.to_owned(),
                    conflict_id,
                    result: Err(error.clone()),
                });
            }
            return;
        }
    };
    let chunk = EngineOptions::default().download_chunk_size;
    for &conflict_id in conflict_ids {
        let result = oc_sync::resolve(
            storage.as_ref(),
            &db,
            &local_path,
            conflict_id,
            resolution,
            chunk,
        )
        .await
        .map(|outcome| matches!(outcome, oc_sync::ResolveOutcome::Resolved))
        .map_err(|error| error.to_string());
        backend.report(BackendEvent::ResolveFinished {
            job_id: job_id.to_owned(),
            conflict_id,
            result,
        });
    }
}

/// Storage, database, and local root for resolving a job's conflicts.
type ResolveSetup = (Arc<dyn RemoteStorage>, std::sync::Mutex<StatusDb>, PathBuf);

/// Builds what [`run_resolve`] needs for one job.
fn resolve_setup(
    config_dir: &std::path::Path,
    providers: &Providers,
    job_id: &str,
) -> Result<ResolveSetup, String> {
    let (job, account) = job_account(config_dir, job_id)?;
    let provider = providers.get(&account).map_err(|error| error.to_string())?;
    require_login(&provider, &job.account_id)?;
    let storage = provider
        .storage(&job.space_id)
        .map_err(|error| error.to_string())?;
    let db_path = db_path(config_dir, &job.id).map_err(|error| error.to_string())?;
    let db = StatusDb::open(db_path).map_err(|error| error.to_string())?;
    Ok((storage, std::sync::Mutex::new(db), job.local_path))
}

/// Builds the engine jobs from config + jobs, skipping accounts without
/// a stored login and jobs whose local folders overlap; returns the
/// engine and human-readable notices (skipped jobs, reset databases).
fn start_engine(
    config_dir: &std::path::Path,
    providers: &Providers,
    config: &Config,
    jobs: &Jobs,
) -> Result<(Engine, Vec<String>), String> {
    let accounts: HashMap<&str, &Account> = config
        .accounts
        .iter()
        .map(|account| (account.id.as_str(), account))
        .collect();
    let enabled = Jobs {
        version: jobs.version,
        jobs: jobs
            .jobs
            .iter()
            .filter(|job| job.enabled)
            .cloned()
            .collect(),
    };
    let mut overlapping: HashMap<String, String> = HashMap::new();
    for (a, b) in enabled.overlapping() {
        overlapping.insert(a.id.clone(), b.id.clone());
        overlapping.insert(b.id.clone(), a.id.clone());
    }
    let mut engine_jobs = Vec::new();
    let mut notices = Vec::new();
    let mut skip = |job_id: &str, reason: String| {
        notices.push(format!("Job '{job_id}' wurde übersprungen: {reason}"));
    };
    let mut resets = Vec::new();
    for job in &enabled.jobs {
        if let Some(other) = overlapping.get(&job.id) {
            skip(
                &job.id,
                format!("lokaler Ordner überschneidet sich mit Job '{other}'"),
            );
            continue;
        }
        let Some(account) = accounts.get(job.account_id.as_str()) else {
            skip(&job.id, format!("unbekanntes Konto '{}'", job.account_id));
            continue;
        };
        let provider = match providers.get(account) {
            Ok(provider) => provider,
            Err(error) => {
                skip(&job.id, format!("Anbieter: {error}"));
                continue;
            }
        };
        if !provider.has_stored_login() {
            skip(
                &job.id,
                format!("Konto '{}' ist nicht angemeldet", job.account_id),
            );
            continue;
        }
        let storage = match provider.storage(&job.space_id) {
            Ok(storage) => storage,
            Err(error) => {
                skip(&job.id, format!("Speicher-Client: {error}"));
                continue;
            }
        };
        let db_path = match db_path(config_dir, &job.id) {
            Ok(path) => path,
            Err(error) => {
                skip(&job.id, format!("Status-Datenbank-Pfad: {error}"));
                continue;
            }
        };
        let db = match StatusDb::open(db_path) {
            Ok(db) => db,
            Err(error) => {
                skip(&job.id, format!("Status-Datenbank: {error}"));
                continue;
            }
        };
        match db.ensure_identity(&job.identity()) {
            Ok(true) => resets.push(job.id.clone()),
            Ok(false) => {}
            Err(error) => {
                skip(&job.id, format!("Status-Datenbank: {error}"));
                continue;
            }
        }
        match Job::new(
            &job.id,
            job.local_path.clone(),
            &job.include,
            &job.exclude,
            job.sync_root_files,
            storage,
            db,
        ) {
            Ok(engine_job) => engine_jobs.push(engine_job),
            Err(error) => skip(&job.id, error.to_string()),
        }
    }
    for job_id in resets {
        notices.push(format!(
            "Status-Datenbank von Job '{job_id}' gehörte zu einem anderen Speicherort oder Ordner \
             und wurde zurückgesetzt"
        ));
    }
    let options = engine_options(config);
    let engine = Engine::start(engine_jobs, options).map_err(|error| error.to_string())?;
    Ok((engine, notices))
}

/// Deletes status databases (with their WAL/SHM side files) of jobs that
/// no longer exist. Best effort: a file still held open (Windows) is
/// left for the next attempt — the identity check guards it meanwhile.
fn prune_orphan_dbs(config_dir: &std::path::Path, jobs: &Jobs) {
    let Ok(entries) = std::fs::read_dir(config_dir.join(oc_db::DB_DIR)) else {
        return;
    };
    let known: std::collections::HashSet<&str> =
        jobs.jobs.iter().map(|job| job.id.as_str()).collect();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some(oc_db::DB_EXTENSION) {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if known.contains(stem) {
            continue;
        }
        tracing::info!("removing status database of deleted job '{stem}'");
        if std::fs::remove_file(&path).is_ok() {
            for suffix in ["-wal", "-shm"] {
                let mut side = path.clone().into_os_string();
                side.push(suffix);
                let _ = std::fs::remove_file(side);
            }
        }
    }
}
