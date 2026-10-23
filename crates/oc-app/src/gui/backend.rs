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

use oc_api::{DavResource, Depth, GraphClient, Space, WebDavClient};
use oc_auth::{AuthClient, DEFAULT_LOGIN_TIMEOUT, KeyringStore, TokenManager};
use oc_config::{Config, Jobs, SyncJob};
use oc_db::{ConflictResolution, StatusDb, db_path};
use oc_sync::{Engine, EngineEvent, EngineOptions, Job, JobCommand};
use tokio::sync::mpsc;

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
    /// Interactive OIDC login (opens the browser).
    Login {
        /// The account to log in.
        account_id: String,
    },
    /// Logout: revoke and clear the stored refresh token.
    Logout {
        /// The account to log out.
        account_id: String,
    },
    /// List the spaces of an account (Graph API) for the wizard.
    ListSpaces {
        /// The account whose spaces to list.
        account_id: String,
    },
    /// List the direct subfolders of a remote folder (PROPFIND Depth 1)
    /// for the wizard's folder tree.
    FetchChildren {
        /// The account (login).
        account_id: String,
        /// The space to traverse.
        space_id: String,
        /// The folder's server path inside the space (`""` = root), as
        /// reported by `DavResource::server_path`.
        path: String,
        /// Correlation token; the answer arrives as
        /// [`BackendEvent::Children`] with the same token.
        token: u32,
    },
    /// Reload `config.json`/`jobs.json` and restart the engine — applied
    /// after the GUI saved changes (new job, filters, settings, login).
    ApplyJobs,
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
        /// `Ok` when the refresh token is stored.
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
        result: Result<Vec<Space>, String>,
    },
    /// The subfolders of a wizard tree node (correlated by token).
    Children {
        /// The correlation token of the request.
        token: u32,
        /// The folders, or an error message.
        result: Result<Vec<DavResource>, String>,
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
}

/// Handle for the GUI to talk to the backend; dropped when the GUI exits,
/// which ends the backend task and stops the engine.
#[derive(Debug, Clone)]
pub struct BackendHandle {
    commands: mpsc::UnboundedSender<UiCommand>,
}

impl BackendHandle {
    /// Sends a command; failures (backend gone) are ignored — the GUI
    /// is on its way out anyway.
    pub fn send(&self, command: UiCommand) {
        let _ = self.commands.send(command);
    }
}

/// Starts the backend: loads config and jobs, starts the sync engine,
/// and spawns the forwarder and command tasks. Returns the GUI's handle
/// and the event receiver to poll each frame.
pub fn spawn(config_dir: PathBuf) -> Result<(BackendHandle, Receiver<BackendEvent>), String> {
    let (config, jobs) = load_workspace(&config_dir)?;
    prune_orphan_dbs(&config_dir, &jobs);
    let managers = Managers::default();
    let (mut engine, notices) = start_engine(&config_dir, &managers, &config, &jobs)?;
    let (gui_tx, gui_rx) = std_mpsc::channel::<BackendEvent>();
    for message in notices {
        let _ = gui_tx.send(BackendEvent::Message { message });
    }
    spawn_forwarder(engine.take_events(), gui_tx.clone());

    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let backend = Backend {
        config_dir,
        managers,
        gui: gui_tx,
        // Weak: the backend must not keep its own command channel open,
        // or it would never notice the GUI quitting.
        internal: command_tx.downgrade(),
    };
    tokio::spawn(backend_task(backend, command_rx, engine));
    Ok((
        BackendHandle {
            commands: command_tx,
        },
        gui_rx,
    ))
}

/// What the backend's tasks share.
#[derive(Clone)]
struct Backend {
    /// The configuration directory.
    config_dir: PathBuf,
    /// Token managers per account.
    managers: Managers,
    /// The GUI's event channel.
    gui: Sender<BackendEvent>,
    /// The backend's own command channel (e.g. to restart the engine after
    /// a login finished in its own task).
    internal: mpsc::WeakUnboundedSender<UiCommand>,
}

impl Backend {
    /// Sends an event to the GUI; a closed GUI is ignored.
    fn report(&self, event: BackendEvent) {
        let _ = self.gui.send(event);
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
                engine =
                    restart_engine(&backend.config_dir, &backend.managers, &backend.gui, engine)
                        .await;
            }
            UiCommand::Login { account_id } => {
                let backend = backend.clone();
                tokio::spawn(async move {
                    let result =
                        run_login(&backend.config_dir, &backend.managers, &account_id).await;
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
                        run_logout(&backend.config_dir, &backend.managers, &account_id).await;
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
                        run_list_spaces(&backend.config_dir, &backend.managers, &account_id).await;
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
                        &backend.managers,
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
    managers: &Managers,
    gui: &Sender<BackendEvent>,
    mut engine: Engine,
) -> Engine {
    let (config, jobs) = match load_workspace(config_dir) {
        Ok(pair) => pair,
        Err(error) => {
            let _ = gui.send(BackendEvent::Message {
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
    match start_engine(config_dir, managers, &config, &jobs) {
        Ok((mut new_engine, notices)) => {
            for message in notices {
                let _ = gui.send(BackendEvent::Message { message });
            }
            spawn_forwarder(new_engine.take_events(), gui.clone());
            new_engine
        }
        Err(error) => {
            let _ = gui.send(BackendEvent::Message {
                message: format!(
                    "Sync-Engine konnte nicht gestartet werden, alle Jobs stehen: {error}"
                ),
            });
            engine
        }
    }
}

/// Forwards engine events into the GUI's event channel until the engine
/// stops (channel closed). Every event also goes to the process log.
fn spawn_forwarder(mut events: mpsc::UnboundedReceiver<EngineEvent>, gui: Sender<BackendEvent>) {
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            tracing::info!("{event}");
            if gui.send(BackendEvent::Engine(event)).is_err() {
                break;
            }
        }
    });
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
        conflict_policy: config.settings.conflict_policy,
        ..EngineOptions::default()
    }
}

/// The concrete token manager type used throughout the backend.
type Manager = TokenManager<KeyringStore>;

/// One token manager per account, shared by the engine's jobs and the
/// GUI's one-off requests (login, spaces, folder tree, conflicts). With a
/// manager per client, two of them would refresh with the same refresh
/// token; the server rotates it on the first use and rejects the second,
/// which used to clear the account's login.
#[derive(Clone, Default)]
struct Managers(Arc<std::sync::Mutex<ManagerMap>>);

/// Account id → (server URL, shared manager).
type ManagerMap = HashMap<String, (String, Arc<Manager>)>;

impl Managers {
    /// The shared manager of `account_id` at `server_url`; a changed
    /// server URL replaces the cached manager.
    fn get(&self, account_id: &str, server_url: &str) -> Result<Arc<Manager>, String> {
        let mut cache = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Some((server, manager)) = cache.get(account_id) {
            if server == server_url {
                return Ok(manager.clone());
            }
        }
        let auth = AuthClient::new(server_url).map_err(|error| error.to_string())?;
        let manager = Arc::new(TokenManager::new(auth, KeyringStore::new(), account_id));
        cache.insert(
            account_id.to_owned(),
            (server_url.to_owned(), manager.clone()),
        );
        Ok(manager)
    }
}

/// The shared token manager of an account; fails on unknown accounts.
fn manager(
    config_dir: &std::path::Path,
    managers: &Managers,
    account_id: &str,
) -> Result<Arc<Manager>, String> {
    let server = server_of(config_dir, account_id)?;
    managers.get(account_id, &server)
}

/// Looks up a job and its account's server URL.
fn job_server(config_dir: &std::path::Path, job_id: &str) -> Result<(SyncJob, String), String> {
    let (config, jobs) = load_workspace(config_dir)?;
    let job = jobs
        .jobs
        .iter()
        .find(|job| job.id == job_id)
        .ok_or_else(|| format!("unbekannter Job '{job_id}'"))?
        .clone();
    let account = config
        .accounts
        .iter()
        .find(|account| account.id == job.account_id)
        .ok_or_else(|| format!("Job '{job_id}' verweist auf unbekanntes Konto"))?;
    Ok((job, account.server_url.clone()))
}

/// Requires a stored login for the account.
fn require_login(manager: &Manager, account_id: &str) -> Result<(), String> {
    if !manager.has_stored_login() {
        return Err(format!(
            "Konto '{account_id}' ist nicht angemeldet (zuerst einloggen)"
        ));
    }
    Ok(())
}

/// The server URL of an account.
fn server_of(config_dir: &std::path::Path, account_id: &str) -> Result<String, String> {
    let config = oc_config::load_config(config_dir).map_err(|error| error.to_string())?;
    config
        .accounts
        .iter()
        .find(|account| account.id == account_id)
        .map(|account| account.server_url.clone())
        .ok_or_else(|| format!("unbekanntes Konto '{account_id}'"))
}

/// Interactive browser login for one account.
async fn run_login(
    config_dir: &std::path::Path,
    managers: &Managers,
    account_id: &str,
) -> Result<(), String> {
    let manager = manager(config_dir, managers, account_id)?;
    manager
        .login(true, DEFAULT_LOGIN_TIMEOUT)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// Logout of one account: revokes the refresh token and clears the store.
async fn run_logout(
    config_dir: &std::path::Path,
    managers: &Managers,
    account_id: &str,
) -> Result<(), String> {
    let manager = manager(config_dir, managers, account_id)?;
    manager.logout().await.map_err(|error| error.to_string())?;
    Ok(())
}

/// Lists the spaces of an account via the Graph API.
async fn run_list_spaces(
    config_dir: &std::path::Path,
    managers: &Managers,
    account_id: &str,
) -> Result<Vec<Space>, String> {
    let manager = manager(config_dir, managers, account_id)?;
    require_login(&manager, account_id)?;
    let server = server_of(config_dir, account_id)?;
    let graph = GraphClient::new(&server, manager).map_err(|error| error.to_string())?;
    graph.list_spaces().await.map_err(|error| error.to_string())
}

/// Lists the direct subfolders of a remote folder (wizard folder tree).
async fn run_fetch_children(
    config_dir: &std::path::Path,
    managers: &Managers,
    account_id: &str,
    space_id: &str,
    path: &str,
) -> Result<Vec<DavResource>, String> {
    let manager = manager(config_dir, managers, account_id)?;
    require_login(&manager, account_id)?;
    let server = server_of(config_dir, account_id)?;
    let dav = WebDavClient::new(&server, space_id, manager).map_err(|error| error.to_string())?;
    let listing = dav
        .propfind(path, Depth::One)
        .await
        .map_err(|error| error.to_string())?;
    let mut folders: Vec<DavResource> = listing
        .into_iter()
        .filter(|resource| resource.is_dir && resource.server_path != path)
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
    let setup = resolve_setup(&backend.config_dir, &backend.managers, job_id);
    let (dav, db, local_path) = match setup {
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
        let result = oc_sync::resolve(&dav, &db, &local_path, conflict_id, resolution, chunk)
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

/// Client, database, and local root for resolving a job's conflicts.
type ResolveSetup = (
    WebDavClient<Arc<Manager>>,
    std::sync::Mutex<StatusDb>,
    PathBuf,
);

/// Builds what [`run_resolve`] needs for one job.
fn resolve_setup(
    config_dir: &std::path::Path,
    managers: &Managers,
    job_id: &str,
) -> Result<ResolveSetup, String> {
    let (job, server) = job_server(config_dir, job_id)?;
    let manager = managers.get(&job.account_id, &server)?;
    require_login(&manager, &job.account_id)?;
    let dav =
        WebDavClient::new(&server, &job.space_id, manager).map_err(|error| error.to_string())?;
    let db_path = db_path(config_dir, &job.id).map_err(|error| error.to_string())?;
    let db = StatusDb::open(db_path).map_err(|error| error.to_string())?;
    Ok((dav, std::sync::Mutex::new(db), job.local_path))
}

/// Builds the engine jobs from config + jobs, skipping accounts without
/// a stored login and jobs whose local folders overlap; returns the
/// engine and human-readable notices (skipped jobs, reset databases).
fn start_engine(
    config_dir: &std::path::Path,
    managers: &Managers,
    config: &Config,
    jobs: &Jobs,
) -> Result<(Engine, Vec<String>), String> {
    let servers: HashMap<&str, &str> = config
        .accounts
        .iter()
        .map(|account| (account.id.as_str(), account.server_url.as_str()))
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
        let Some(server) = servers.get(job.account_id.as_str()) else {
            skip(&job.id, format!("unbekanntes Konto '{}'", job.account_id));
            continue;
        };
        let manager = match managers.get(&job.account_id, server) {
            Ok(manager) => manager,
            Err(error) => {
                skip(&job.id, format!("Auth-Client: {error}"));
                continue;
            }
        };
        if !manager.has_stored_login() {
            skip(
                &job.id,
                format!("Konto '{}' ist nicht angemeldet", job.account_id),
            );
            continue;
        }
        let dav = match WebDavClient::new(server, &job.space_id, manager) {
            Ok(dav) => dav,
            Err(error) => {
                skip(&job.id, format!("WebDAV-Client: {error}"));
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
            dav,
            db,
        ) {
            Ok(engine_job) => engine_jobs.push(engine_job),
            Err(error) => skip(&job.id, error.to_string()),
        }
    }
    for job_id in resets {
        notices.push(format!(
            "Status-Datenbank von Job '{job_id}' gehörte zu einem anderen Space oder Ordner \
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
