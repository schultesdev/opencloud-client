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
    /// Resolve a pending conflict: download the server version over the
    /// local file (`Server`) or upload the local version (`Local`).
    ResolveConflict {
        /// The job the conflict belongs to.
        job_id: String,
        /// The conflict row id.
        conflict_id: i64,
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
        /// The folder path inside the space (`""` = root).
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
    let (mut engine, skipped) = start_engine(&config_dir, &config, &jobs)?;
    let (gui_tx, gui_rx) = std_mpsc::channel::<BackendEvent>();
    for (job_id, reason) in skipped {
        let _ = gui_tx.send(BackendEvent::Message {
            message: format!("Job '{job_id}' wurde übersprungen: {reason}"),
        });
    }
    spawn_forwarder(engine.take_events(), gui_tx.clone());

    let (command_tx, command_rx) = mpsc::unbounded_channel();
    tokio::spawn(backend_task(config_dir, gui_tx, command_rx, engine));
    Ok((
        BackendHandle {
            commands: command_tx,
        },
        gui_rx,
    ))
}

/// The backend task: serves UI commands until the GUI drops its handle,
/// then stops the engine cleanly.
async fn backend_task(
    config_dir: PathBuf,
    gui: Sender<BackendEvent>,
    mut commands: mpsc::UnboundedReceiver<UiCommand>,
    mut engine: Engine,
) {
    while let Some(command) = commands.recv().await {
        match command {
            UiCommand::JobCmd { job_id, command } => {
                if engine.send(&job_id, command).await.is_err() {
                    let _ = gui.send(BackendEvent::Message {
                        message: format!("Job '{job_id}' läuft nicht mehr (Engine neustarten?)"),
                    });
                }
            }
            UiCommand::PauseAll => engine.pause_all().await,
            UiCommand::ResumeAll => engine.resume_all().await,
            UiCommand::ApplyJobs => {
                engine = restart_engine(&config_dir, &gui, engine).await;
            }
            UiCommand::Login { account_id } => {
                let result = run_login(&config_dir, &account_id).await;
                let _ = gui.send(BackendEvent::LoginFinished {
                    account_id: account_id.clone(),
                    result: result.clone(),
                });
                if result.is_ok() {
                    engine = restart_engine(&config_dir, &gui, engine).await;
                }
            }
            UiCommand::Logout { account_id } => {
                let result = run_logout(&config_dir, &account_id).await;
                let _ = gui.send(BackendEvent::LogoutFinished {
                    account_id: account_id.clone(),
                    result: result.clone(),
                });
                if result.is_ok() {
                    engine = restart_engine(&config_dir, &gui, engine).await;
                }
            }
            UiCommand::ListSpaces { account_id } => {
                let result = run_list_spaces(&config_dir, &account_id).await;
                let _ = gui.send(BackendEvent::Spaces {
                    account_id,
                    result: result.map_err(|error| error.to_string()),
                });
            }
            UiCommand::FetchChildren {
                account_id,
                space_id,
                path,
                token,
            } => {
                let result = run_fetch_children(&config_dir, &account_id, &space_id, &path).await;
                let _ = gui.send(BackendEvent::Children {
                    token,
                    result: result.map_err(|error| error.to_string()),
                });
            }
            UiCommand::ResolveConflict {
                job_id,
                conflict_id,
                resolution,
            } => {
                let result = run_resolve(&config_dir, &job_id, conflict_id, resolution).await;
                let _ = gui.send(BackendEvent::ResolveFinished {
                    job_id,
                    conflict_id,
                    result,
                });
            }
        }
    }
    engine.stop().await;
}

/// Rebuilds the engine from the current `config.json`/`jobs.json` and
/// wires a fresh event forwarder. Used after every change that affects
/// the running jobs (new job, filters, settings, login/logout).
async fn restart_engine(
    config_dir: &std::path::Path,
    gui: &Sender<BackendEvent>,
    mut engine: Engine,
) -> Engine {
    engine.stop().await;
    let (config, jobs) = match load_workspace(config_dir) {
        Ok(pair) => pair,
        Err(error) => {
            let _ = gui.send(BackendEvent::Message {
                message: format!("Konfiguration unlesbar, Engine gestoppt: {error}"),
            });
            return engine;
        }
    };
    match start_engine(config_dir, &config, &jobs) {
        Ok((mut new_engine, skipped)) => {
            for (job_id, reason) in skipped {
                let _ = gui.send(BackendEvent::Message {
                    message: format!("Job '{job_id}' wurde übersprungen: {reason}"),
                });
            }
            spawn_forwarder(new_engine.take_events(), gui.clone());
            new_engine
        }
        Err(error) => {
            let _ = gui.send(BackendEvent::Message {
                message: format!("Sync-Engine konnte nicht gestartet werden: {error}"),
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

/// Builds the token manager of an account; fails on unknown accounts.
fn manager(config_dir: &std::path::Path, account_id: &str) -> Result<Manager, String> {
    let config = oc_config::load_config(config_dir).map_err(|error| error.to_string())?;
    let account = config
        .accounts
        .iter()
        .find(|account| account.id == account_id)
        .ok_or_else(|| format!("unbekanntes Konto '{account_id}'"))?;
    let auth = AuthClient::new(&account.server_url).map_err(|error| error.to_string())?;
    Ok(TokenManager::new(auth, KeyringStore::new(), &account.id))
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
async fn run_login(config_dir: &std::path::Path, account_id: &str) -> Result<(), String> {
    let manager = manager(config_dir, account_id)?;
    manager
        .login(true, DEFAULT_LOGIN_TIMEOUT)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// Logout of one account: revokes the refresh token and clears the store.
async fn run_logout(config_dir: &std::path::Path, account_id: &str) -> Result<(), String> {
    let manager = manager(config_dir, account_id)?;
    manager.logout().await.map_err(|error| error.to_string())?;
    Ok(())
}

/// Lists the spaces of an account via the Graph API.
async fn run_list_spaces(
    config_dir: &std::path::Path,
    account_id: &str,
) -> Result<Vec<Space>, String> {
    let manager = manager(config_dir, account_id)?;
    require_login(&manager, account_id)?;
    let server = server_of(config_dir, account_id)?;
    let graph = GraphClient::new(&server, manager).map_err(|error| error.to_string())?;
    graph.list_spaces().await.map_err(|error| error.to_string())
}

/// Lists the direct subfolders of a remote folder (wizard folder tree).
async fn run_fetch_children(
    config_dir: &std::path::Path,
    account_id: &str,
    space_id: &str,
    path: &str,
) -> Result<Vec<DavResource>, String> {
    let manager = manager(config_dir, account_id)?;
    require_login(&manager, account_id)?;
    let server = server_of(config_dir, account_id)?;
    let dav = WebDavClient::new(&server, space_id, manager).map_err(|error| error.to_string())?;
    let listing = dav
        .propfind(path, Depth::One)
        .await
        .map_err(|error| error.to_string())?;
    let mut folders: Vec<DavResource> = listing
        .into_iter()
        .filter(|resource| resource.is_dir && resource.path != path)
        .collect();
    folders.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(folders)
}

/// Resolves one pending conflict (see `PLAN.md`, Phase 6).
async fn run_resolve(
    config_dir: &std::path::Path,
    job_id: &str,
    conflict_id: i64,
    resolution: ConflictResolution,
) -> Result<bool, String> {
    let (job, server) = job_server(config_dir, job_id)?;
    let manager = manager(config_dir, &job.account_id)?;
    require_login(&manager, &job.account_id)?;
    let dav =
        WebDavClient::new(&server, &job.space_id, manager).map_err(|error| error.to_string())?;
    let db_path = db_path(config_dir, &job.id).map_err(|error| error.to_string())?;
    let db = StatusDb::open(db_path).map_err(|error| error.to_string())?;
    let db = std::sync::Mutex::new(db);
    let chunk = EngineOptions::default().download_chunk_size;
    let outcome = oc_sync::resolve(&dav, &db, &job.local_path, conflict_id, resolution, chunk)
        .await
        .map_err(|error| error.to_string())?;
    Ok(matches!(outcome, oc_sync::ResolveOutcome::Resolved))
}

/// Builds the engine jobs from config + jobs, skipping accounts without
/// a stored login; returns the engine and the skipped jobs with reasons.
fn start_engine(
    config_dir: &std::path::Path,
    config: &Config,
    jobs: &Jobs,
) -> Result<(Engine, Vec<(String, String)>), String> {
    let servers: HashMap<&str, &str> = config
        .accounts
        .iter()
        .map(|account| (account.id.as_str(), account.server_url.as_str()))
        .collect();
    let mut engine_jobs = Vec::new();
    let mut skipped = Vec::new();
    for job in jobs.jobs.iter().filter(|job| job.enabled) {
        let Some(server) = servers.get(job.account_id.as_str()) else {
            skipped.push((
                job.id.clone(),
                format!("unbekanntes Konto '{}'", job.account_id),
            ));
            continue;
        };
        let manager = match AuthClient::new(server) {
            Ok(auth) => TokenManager::new(auth, KeyringStore::new(), &job.account_id),
            Err(error) => {
                skipped.push((job.id.clone(), format!("Auth-Client: {error}")));
                continue;
            }
        };
        if !manager.has_stored_login() {
            skipped.push((
                job.id.clone(),
                format!("Konto '{}' ist nicht angemeldet", job.account_id),
            ));
            continue;
        }
        let dav = match WebDavClient::new(server, &job.space_id, manager) {
            Ok(dav) => dav,
            Err(error) => {
                skipped.push((job.id.clone(), format!("WebDAV-Client: {error}")));
                continue;
            }
        };
        let db_path = match db_path(config_dir, &job.id) {
            Ok(path) => path,
            Err(error) => {
                skipped.push((job.id.clone(), format!("Status-Datenbank-Pfad: {error}")));
                continue;
            }
        };
        let db = match StatusDb::open(db_path) {
            Ok(db) => db,
            Err(error) => {
                skipped.push((job.id.clone(), format!("Status-Datenbank: {error}")));
                continue;
            }
        };
        match Job::new(
            &job.id,
            job.local_path.clone(),
            &job.include,
            &job.exclude,
            dav,
            db,
        ) {
            Ok(engine_job) => engine_jobs.push(engine_job),
            Err(error) => skipped.push((job.id.clone(), error.to_string())),
        }
    }
    let options = engine_options(config);
    let engine = Engine::start(engine_jobs, options).map_err(|error| error.to_string())?;
    Ok((engine, skipped))
}
