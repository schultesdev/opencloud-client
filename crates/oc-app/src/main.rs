//! OpenCloud sync client application.
//!
//! Binary crate: GUI, tray icon, and `main()`. The sync engine is attached
//! via the event/command channels (see `oc-sync`) so that it can run without
//! the GUI in `--headless` mode.
//!
//! Phase 5: headless mode runs the engine over all enabled jobs from
//! `config.json`/`jobs.json` — one task per job, events printed to stdout,
//! Ctrl-C to exit. The GUI lands in Phase 7.

use std::collections::HashMap;
use std::error::Error;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use oc_auth::{AuthClient, KeyringStore, TokenManager, DEFAULT_LOGIN_TIMEOUT};
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

    /// Configuration directory (default: the OS-specific config path).
    #[arg(long, value_name = "DIR")]
    config_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    if !args.headless && args.login.is_none() && args.list_spaces.is_none() {
        // The GUI lands in Phase 7; until then point users at headless mode.
        println!("GUI not implemented yet (Phase 7); use --headless for engine-only mode");
        return;
    }
    let result = match (args.login.as_deref(), args.list_spaces.as_deref()) {
        (Some(account_id), _) => run_login(account_id, args.config_dir.as_deref()).await,
        (None, Some(account_id)) => run_list_spaces(account_id, args.config_dir.as_deref()).await,
        (None, None) => run_headless(args.config_dir.as_deref()).await,
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
    println!("login complete; refresh token stored for account '{}'", account.id);
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
        return Err(format!(
            "account '{account_id}' has no stored login (run --login first)"
        )
        .into());
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

/// Loads config and jobs, starts the engine for every enabled job, and
/// prints its events until Ctrl-C.
async fn run_headless(config_dir: Option<&std::path::Path>) -> Result<(), Box<dyn Error>> {
    let config_dir = match config_dir {
        Some(dir) => dir.to_path_buf(),
        None => oc_config::default_config_dir()?,
    };
    let config = oc_config::load_config(&config_dir)?;
    let jobs = oc_config::load_jobs(&config_dir)?;
    oc_config::validate_references(&config, &jobs)?;

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
