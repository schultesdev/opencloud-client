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

use std::collections::HashMap;
use std::error::Error;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use oc_auth::{AuthClient, DEFAULT_LOGIN_TIMEOUT, KeyringStore, TokenManager};
use oc_config::Account;
use oc_db::{StatusDb, db_path};

/// OpenCloud sync client.
#[derive(Debug, Parser)]
#[command(name = "opencloud-sync", version, about = "Sync client for OpenCloud")]
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
    let cli_only = args.headless
        || args.login.is_some()
        || args.list_spaces.is_some()
        || args.conflicts
        || args.resolve.is_some();
    if !cli_only {
        // The GUI is the default; it blocks until the user quits.
        if let Err(error) = gui::run(args.config_dir.as_deref()) {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
        return;
    }
    let config_dir = args.config_dir.as_deref();
    let result = match (args.login.as_deref(), args.list_spaces.as_deref()) {
        (Some(account_id), _) => run_login(account_id, config_dir).await,
        (None, Some(account_id)) => run_list_spaces(account_id, config_dir).await,
        (None, None) => {
            if let Some([target, winner, ..]) = args.resolve.as_deref() {
                run_resolve(config_dir, target, winner).await
            } else if args.conflicts {
                run_conflicts(config_dir)
            } else {
                run_headless(config_dir).await
            }
        }
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
            let matches: Vec<&oc_config::SyncJob> = jobs
                .jobs
                .iter()
                .filter(|job| {
                    open_job_db(&config_dir, &job.id)
                        .ok()
                        .flatten()
                        .and_then(|db| db.get_conflict(conflict_id).ok().flatten())
                        .is_some_and(|conflict| conflict.status == ConflictStatus::Pending)
                })
                .collect();
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
/// prints its events until Ctrl-C.
async fn run_headless(config_dir: Option<&std::path::Path>) -> Result<(), Box<dyn Error>> {
    let (config_dir, config, jobs) = load_workspace(config_dir)?;

    let accounts: HashMap<&str, &Account> = config
        .accounts
        .iter()
        .map(|account| (account.id.as_str(), account))
        .collect();

    let mut engine_jobs = Vec::new();
    for job in jobs.jobs.iter().filter(|job| job.enabled) {
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
        let auth = AuthClient::new(&account.server_url)?;
        let manager = TokenManager::new(auth, KeyringStore::new(), &account.id);
        if !manager.has_stored_login() {
            eprintln!(
                "skipping job '{}': account '{}' has no stored login (log in first)",
                job.id, account.id
            );
            continue;
        }
        let dav = oc_api::WebDavClient::new(&account.server_url, &job.space_id, manager)?;
        let db = StatusDb::open(db_path(&config_dir, &job.id)?)?;
        engine_jobs.push(oc_sync::Job::new(
            &job.id,
            job.local_path.clone(),
            &job.include,
            &job.exclude,
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
        conflict_policy: config.settings.conflict_policy,
        ..oc_sync::EngineOptions::default()
    };
    let mut engine = oc_sync::Engine::start(engine_jobs, options)?;
    println!(
        "sync engine started ({} job(s)); press Ctrl-C to exit",
        engine.job_count()
    );

    loop {
        tokio::select! {
            event = engine.events_mut().recv() => match event {
                Some(event) => println!("{event}"),
                None => break,
            },
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    engine.stop().await;
    println!("sync engine stopped");
    Ok(())
}
