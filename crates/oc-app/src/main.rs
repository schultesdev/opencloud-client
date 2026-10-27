//! OpenCloud sync client application.
//!
//! Binary crate: GUI, tray icon, and `main()`. The sync engine is attached
//! via the event/command channels (see `oc-sync`) so that it can run without
//! the GUI in `--headless` mode.
//!
//! Without arguments the GUI starts (Phase 7): accounts, jobs, conflicts,
//! the new-job wizard, and the tray. `--headless` runs the engine over all
//! enabled jobs from `config.json`/`jobs.json` — one task per job, events
//! printed to stdout, Ctrl-C to exit.

mod assets;
mod gui;
mod logging;
mod updates;

use std::collections::HashMap;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use oc_auth::{AuthClient, DEFAULT_LOGIN_TIMEOUT, KeyringStore, TokenManager};
use oc_config::Account;
use oc_config::LogLevel;
use oc_db::{StatusDb, db_path};

/// OpenCloud sync client.
#[derive(Debug, Parser)]
#[command(name = "opencloud-sync", version, about = "Sync client for OpenCloud")]
#[command(group(
    clap::ArgGroup::new("mode")
        .args(["login", "logout", "list_spaces", "conflicts", "resolve", "status", "sync_once"])
        .multiple(false)
))]
struct Args {
    /// Run without GUI: start the sync engine only (CLI mode).
    #[arg(long)]
    headless: bool,

    /// Interactive login for an account from config.json: opens the
    /// browser (OIDC code flow + PKCE) and stores the refresh token in
    /// the OS keyring. Exits afterwards; combine with --headless to log
    /// in and start the engine in one call.
    #[arg(long, value_name = "ACCOUNT_ID")]
    login: Option<String>,

    /// Logs an account out: revokes its refresh token at the server (best
    /// effort) and removes it from the OS keyring.
    #[arg(long, value_name = "ACCOUNT_ID")]
    logout: Option<String>,

    /// Prints every job with its state: login, files and folders,
    /// pending conflicts, errors, and the last sync.
    #[arg(long)]
    status: bool,

    /// Runs one sync pass for every enabled job and exits; the exit code
    /// is non-zero when a pass reported errors (for cron/scripts).
    #[arg(long)]
    sync_once: bool,

    /// Lists the spaces of the account via the Graph API and prints id,
    /// type, and name of each space. Requires a prior `--login`.
    #[arg(long, value_name = "ACCOUNT_ID")]
    list_spaces: Option<String>,

    /// Lists the pending conflicts of all jobs with their ids and both
    /// sides' state (Phase 6; the ids feed `--resolve`).
    #[arg(long)]
    conflicts: bool,

    /// Resolves a pending conflict: `server` takes the server version,
    /// `local` keeps the local one. The target is a conflict id from
    /// `--conflicts`, optionally prefixed with its job
    /// (`<job-id>:<conflict-id>`) when the bare id is ambiguous.
    #[arg(long, value_names = ["CONFLICT_ID", "WINNER"], num_args = 2)]
    resolve: Option<Vec<String>>,

    /// Configuration directory (default: the OS-specific config path).
    #[arg(long, value_name = "DIR")]
    config_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    // Process logging (Phase 8): level from config.json, daily-rotating
    // files under <config-dir>/logs. The guard flushes on drop; holding
    // it until process end.
    let log_dir = args
        .config_dir
        .clone()
        .or_else(|| oc_config::default_config_dir().ok());
    let log_level = log_dir
        .as_deref()
        .and_then(|dir| oc_config::load_config(dir).ok())
        .map(|config| config.settings.log_level)
        .unwrap_or(LogLevel::Info);
    let _log_guard = logging::init(log_dir.as_deref(), log_level);
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "opencloud-sync starting (headless: {})",
        args.headless
    );
    let cli_only = args.headless
        || args.login.is_some()
        || args.logout.is_some()
        || args.list_spaces.is_some()
        || args.conflicts
        || args.resolve.is_some()
        || args.status
        || args.sync_once;
    if !cli_only {
        // The GUI is the default; it blocks until the user quits.
        if let Err(error) = gui::run(args.config_dir.as_deref()) {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
        return;
    }
    let config_dir = args.config_dir.as_deref();
    // The modes exclude each other (clap group "mode"); --headless may
    // follow a --login.
    let result = if let Some(account_id) = args.login.as_deref() {
        match run_login(account_id, config_dir).await {
            Ok(()) if args.headless => run_headless(config_dir, false).await,
            other => other,
        }
    } else if let Some(account_id) = args.logout.as_deref() {
        run_logout(account_id, config_dir).await
    } else if let Some(account_id) = args.list_spaces.as_deref() {
        run_list_spaces(account_id, config_dir).await
    } else if let Some([target, winner, ..]) = args.resolve.as_deref() {
        run_resolve(config_dir, target, winner).await
    } else if args.conflicts {
        run_conflicts(config_dir)
    } else if args.status {
        run_status(config_dir)
    } else {
        run_headless(config_dir, args.sync_once).await
    };
    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

/// Loads the config and returns the account with the given id.
fn find_account(
    config_dir: Option<&std::path::Path>,
    account_id: &str,
) -> Result<Account, Box<dyn Error>> {
    let config_dir = match config_dir {
        Some(dir) => dir.to_path_buf(),
        None => oc_config::default_config_dir()?,
    };
    let config = oc_config::load_config(&config_dir)?;
    Ok(config
        .accounts
        .into_iter()
        .find(|account| account.id == account_id)
        .ok_or_else(|| format!("unknown account '{account_id}'"))?)
}

/// Interactive login for one account: opens the browser for the OIDC
/// flow and stores the refresh token in the OS keyring, so `--headless`
/// can pick the account up on later runs.
async fn run_login(
    account_id: &str,
    config_dir: Option<&std::path::Path>,
) -> Result<(), Box<dyn Error>> {
    let account = find_account(config_dir, account_id)?;

    let auth = AuthClient::new(&account.server_url)?;
    let manager = TokenManager::new(auth, KeyringStore::new(), &account.id);
    println!(
        "opening browser for login at {} (account '{}')",
        account.server_url, account.id
    );
    manager.login(true, DEFAULT_LOGIN_TIMEOUT).await?;
    println!(
        "login complete; refresh token stored for account '{}'",
        account.id
    );
    Ok(())
}

/// Logs an account out: revokes and removes its refresh token.
async fn run_logout(
    account_id: &str,
    config_dir: Option<&std::path::Path>,
) -> Result<(), Box<dyn Error>> {
    let account = find_account(config_dir, account_id)?;
    let auth = AuthClient::new(&account.server_url)?;
    let manager = TokenManager::new(auth, KeyringStore::new(), &account.id);
    manager.logout().await?;
    println!("account '{}' logged out", account.id);
    Ok(())
}

/// Prints every job's state from config, keyring, and status database.
fn run_status(config_dir: Option<&std::path::Path>) -> Result<(), Box<dyn Error>> {
    let (config_dir, config, jobs) = load_workspace(config_dir)?;
    if jobs.jobs.is_empty() {
        println!("no jobs configured");
        return Ok(());
    }
    let mut logins: HashMap<&str, bool> = HashMap::new();
    for account in &config.accounts {
        let logged_in = AuthClient::new(&account.server_url)
            .map(|auth| TokenManager::new(auth, KeyringStore::new(), &account.id))
            .is_ok_and(|manager| manager.has_stored_login());
        logins.insert(account.id.as_str(), logged_in);
    }
    for job in &jobs.jobs {
        let login = if logins
            .get(job.account_id.as_str())
            .copied()
            .unwrap_or(false)
        {
            "logged in"
        } else {
            "not logged in"
        };
        let state = if job.enabled { "enabled" } else { "disabled" };
        println!(
            "{} ({state}, account '{}' {login}) -> {}",
            job.id,
            job.account_id,
            job.local_path.display()
        );
        match open_job_db(&config_dir, &job.id)? {
            Some(db) => {
                let stats = db.stats()?;
                let last = stats
                    .last_sync
                    .map(|unix| format!("{unix} (unix)"))
                    .unwrap_or_else(|| "never".into());
                println!(
                    "    {} files, {} folders, {} conflicts, {} errors, last sync {last}",
                    stats.files, stats.dirs, stats.conflicts, stats.errors
                );
            }
            None => println!("    not synced yet"),
        }
    }
    Ok(())
}

/// Lists the spaces of the logged-in account via the Graph API
/// (`GET /graph/v1.0/me/drives`).
async fn run_list_spaces(
    account_id: &str,
    config_dir: Option<&std::path::Path>,
) -> Result<(), Box<dyn Error>> {
    let account = find_account(config_dir, account_id)?;

    let auth = AuthClient::new(&account.server_url)?;
    let manager = TokenManager::new(auth, KeyringStore::new(), &account.id);
    if !manager.has_stored_login() {
        return Err(
            format!("account '{account_id}' has no stored login (run --login first)").into(),
        );
    }
    let graph = oc_api::GraphClient::new(&account.server_url, manager)?;
    let spaces = graph.list_spaces().await?;
    if spaces.is_empty() {
        println!("no spaces found for account '{}'", account.id);
        return Ok(());
    }
    for space in &spaces {
        println!("{:<40} {:<10} {}", space.id, space.drive_type, space.name);
    }
    Ok(())
}

/// Resolves the configuration directory argument (default: the
/// OS-specific path).
fn config_dir_or_default(config_dir: Option<&std::path::Path>) -> Result<PathBuf, Box<dyn Error>> {
    Ok(match config_dir {
        Some(dir) => dir.to_path_buf(),
        None => oc_config::default_config_dir()?,
    })
}

/// Loads config and jobs (validating account references) and returns
/// them with the configuration directory they came from.
fn load_workspace(
    config_dir: Option<&std::path::Path>,
) -> Result<(PathBuf, oc_config::Config, oc_config::Jobs), Box<dyn Error>> {
    let config_dir = config_dir_or_default(config_dir)?;
    let config = oc_config::load_config(&config_dir)?;
    let jobs = oc_config::load_jobs(&config_dir)?;
    oc_config::validate_references(&config, &jobs)?;
    Ok((config_dir, config, jobs))
}

/// Opens the job's status database when its file exists, `None` when
/// the job has never synced.
fn open_job_db(
    config_dir: &std::path::Path,
    job_id: &str,
) -> Result<Option<StatusDb>, Box<dyn Error>> {
    let path = oc_db::db_path(config_dir, job_id)?;
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(StatusDb::open(path)?))
}

/// Lists the pending conflicts of all jobs: id, path, and both sides'
/// state. The ids feed `--resolve`.
fn run_conflicts(config_dir: Option<&std::path::Path>) -> Result<(), Box<dyn Error>> {
    let (config_dir, _config, jobs) = load_workspace(config_dir)?;
    let mut printed = 0;
    for job in &jobs.jobs {
        let Some(db) = open_job_db(&config_dir, &job.id)? else {
            continue;
        };
        for conflict in db.pending_conflicts()? {
            println!(
                "{:<20} #{:<4} {:<40} local {} B, remote {} B (etag {})",
                job.id,
                conflict.id,
                conflict.path,
                conflict
                    .local_size
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "?".into()),
                conflict
                    .remote_size
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "?".into()),
                conflict.remote_etag.as_deref().unwrap_or("-"),
            );
            printed += 1;
        }
    }
    if printed == 0 {
        println!("no pending conflicts");
    }
    Ok(())
}

/// Resolves a pending conflict (Phase 6): `server` downloads the server
/// version over the local file, `local` uploads the local version
/// (guarded by the conflict's ETag — when the server changed again, the
/// conflict stays pending with the fresh remote state).
async fn run_resolve(
    config_dir: Option<&std::path::Path>,
    target: &str,
    winner: &str,
) -> Result<(), Box<dyn Error>> {
    use oc_db::{ConflictResolution, ConflictStatus};

    let resolution = match winner.to_ascii_lowercase().as_str() {
        "server" => ConflictResolution::Server,
        "local" => ConflictResolution::Local,
        other => return Err(format!("unknown winner '{other}' (use 'server' or 'local')").into()),
    };
    let (config_dir, config, jobs) = load_workspace(config_dir)?;

    // The target is `<job-id>:<conflict-id>` or a bare conflict id.
    let (job, conflict_id) = match target.split_once(':') {
        Some((job_id, id)) => {
            let conflict_id: i64 = id
                .parse()
                .map_err(|_| format!("invalid conflict id '{id}'"))?;
            let job = jobs
                .jobs
                .iter()
                .find(|job| job.id == job_id)
                .ok_or_else(|| format!("unknown job '{job_id}'"))?;
            (job, conflict_id)
        }
        None => {
            let conflict_id: i64 = target
                .parse()
                .map_err(|_| format!("invalid conflict id '{target}'"))?;
            let mut matches: Vec<&oc_config::SyncJob> = Vec::new();
            for job in &jobs.jobs {
                let Some(db) = open_job_db(&config_dir, &job.id)? else {
                    continue;
                };
                let pending = db
                    .get_conflict(conflict_id)?
                    .is_some_and(|conflict| conflict.status == ConflictStatus::Pending);
                if pending {
                    matches.push(job);
                }
            }
            match matches.as_slice() {
                [job] => (*job, conflict_id),
                [] => return Err(format!("no pending conflict with id {conflict_id}").into()),
                many => {
                    let ids: Vec<&str> = many.iter().map(|job| job.id.as_str()).collect();
                    return Err(format!(
                        "conflict id {conflict_id} is pending in several jobs ({ids:?}); \
                         use --resolve <job-id>:{conflict_id} <winner>"
                    )
                    .into());
                }
            }
        }
    };

    let db = open_job_db(&config_dir, &job.id)?
        .ok_or_else(|| format!("job '{}' has no status database yet", job.id))?;
    let conflict = db
        .get_conflict(conflict_id)?
        .filter(|conflict| conflict.status == ConflictStatus::Pending)
        .ok_or_else(|| {
            format!(
                "job '{}' has no pending conflict with id {conflict_id}",
                job.id
            )
        })?;
    let path = conflict.path;

    let account = config
        .accounts
        .iter()
        .find(|account| account.id == job.account_id)
        .ok_or_else(|| {
            format!(
                "job '{}' references unknown account '{}'",
                job.id, job.account_id
            )
        })?;
    let auth = AuthClient::new(&account.server_url)?;
    let manager = TokenManager::new(auth, KeyringStore::new(), &account.id);
    if !manager.has_stored_login() {
        return Err(format!(
            "account '{}' has no stored login (log in first with --login)",
            account.id
        )
        .into());
    }
    let dav = oc_api::WebDavClient::new(&account.server_url, &job.space_id, manager)?;

    let chunk_size = oc_sync::EngineOptions::default().download_chunk_size;
    let outcome = oc_sync::resolve(
        &dav,
        &std::sync::Mutex::new(db),
        &job.local_path,
        conflict_id,
        resolution,
        chunk_size,
    )
    .await?;
    match outcome {
        oc_sync::ResolveOutcome::Resolved => println!(
            "conflict #{conflict_id} at '{path}' resolved ({})",
            match resolution {
                ConflictResolution::Server => "server version taken",
                ConflictResolution::Local => "local version kept",
            }
        ),
        oc_sync::ResolveOutcome::RemoteChanged => println!(
            "conflict #{conflict_id} stays pending: the server changed again in the meantime; \
             its current state was recorded — list it with --conflicts and resolve anew"
        ),
    }
    Ok(())
}

/// Loads config and jobs, starts the engine for every enabled job, and
/// prints its events until Ctrl-C — or, with `once`, until every job
/// finished one pass (an error then fails the call).
async fn run_headless(
    config_dir: Option<&std::path::Path>,
    once: bool,
) -> Result<(), Box<dyn Error>> {
    let (config_dir, config, jobs) = load_workspace(config_dir)?;

    let accounts: HashMap<&str, &Account> = config
        .accounts
        .iter()
        .map(|account| (account.id.as_str(), account))
        .collect();

    let enabled = oc_config::Jobs {
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
    // One token manager per account, shared by its jobs: separate
    // managers would refresh with the same (rotating) refresh token.
    let mut managers: HashMap<String, Arc<TokenManager<KeyringStore>>> = HashMap::new();
    let mut engine_jobs = Vec::new();
    for job in &enabled.jobs {
        if let Some(other) = overlapping.get(&job.id) {
            eprintln!(
                "skipping job '{}': its local folder overlaps with job '{other}'",
                job.id
            );
            continue;
        }
        let account = match accounts.get(job.account_id.as_str()) {
            Some(account) => *account,
            None => {
                eprintln!(
                    "skipping job '{}': unknown account '{}'",
                    job.id, job.account_id
                );
                continue;
            }
        };
        let manager = match managers.get(&account.id) {
            Some(manager) => manager.clone(),
            None => {
                let auth = AuthClient::new(&account.server_url)?;
                let manager = Arc::new(TokenManager::new(auth, KeyringStore::new(), &account.id));
                managers.insert(account.id.clone(), manager.clone());
                manager
            }
        };
        if !manager.has_stored_login() {
            eprintln!(
                "skipping job '{}': account '{}' has no stored login (log in first)",
                job.id, account.id
            );
            continue;
        }
        let dav = oc_api::WebDavClient::new(&account.server_url, &job.space_id, manager)?;
        let db = StatusDb::open(db_path(&config_dir, &job.id)?)?;
        if db.ensure_identity(&job.identity())? {
            eprintln!(
                "job '{}': its status database belonged to another space or folder and was reset",
                job.id
            );
        }
        engine_jobs.push(oc_sync::Job::new(
            &job.id,
            job.local_path.clone(),
            &job.include,
            &job.exclude,
            job.sync_root_files,
            dav,
            db,
        )?);
    }

    if engine_jobs.is_empty() {
        println!("no runnable jobs (none enabled, or no account logged in)");
        return Ok(());
    }

    let options = oc_sync::EngineOptions {
        sync_interval: Duration::from_secs(config.settings.sync_interval_secs),
        max_parallel_transfers: config.settings.max_parallel_transfers as usize,
        propfind_pause: Duration::from_millis(config.settings.propfind_pause_ms),
        conflict_policy: config.settings.conflict_policy,
        global_exclude: config.settings.global_exclude.clone(),
        ..oc_sync::EngineOptions::default()
    };
    let mut engine = oc_sync::Engine::start(engine_jobs, options)?;
    if once {
        println!("running one pass for {} job(s)", engine.job_count());
    } else {
        println!(
            "sync engine started ({} job(s)); press Ctrl-C to exit",
            engine.job_count()
        );
    }

    // `--sync-once`: the first finished pass of every job ends the run.
    let mut finished: HashMap<String, oc_sync::RunSummary> = HashMap::new();
    loop {
        tokio::select! {
            event = engine.events_mut().recv() => match event {
                Some(event) => {
                    tracing::info!("{event}");
                    println!("{event}");
                    if let oc_sync::EngineEvent::RunFinished { job_id, summary } = event {
                        finished.entry(job_id).or_insert(summary);
                        if once && finished.len() >= engine.job_count() {
                            break;
                        }
                    }
                }
                None => break,
            },
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    engine.stop().await;
    println!("sync engine stopped");
    if once {
        let failed: Vec<&String> = finished
            .iter()
            .filter(|(_, summary)| summary.errors > 0)
            .map(|(job_id, _)| job_id)
            .collect();
        if !failed.is_empty() {
            return Err(format!("pass reported errors in job(s) {failed:?}").into());
        }
    }
    Ok(())
}
