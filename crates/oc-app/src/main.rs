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
use oc_auth::{AuthClient, KeyringStore, TokenManager};
use oc_config::Account;
use oc_db::{StatusDb, db_path};

/// OpenCloud sync client.
#[derive(Debug, Parser)]
#[command(name = "opencloud-sync", version, about = "Sync client for OpenCloud")]
struct Args {
    /// Run without GUI: start the sync engine only (CLI mode).
    #[arg(long)]
    headless: bool,

    /// Configuration directory (default: the OS-specific config path).
    #[arg(long, value_name = "DIR")]
    config_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    if !args.headless {
        // The GUI lands in Phase 7; until then point users at headless mode.
        println!("GUI not implemented yet (Phase 7); use --headless for engine-only mode");
        return;
    }
    if let Err(error) = run_headless(args.config_dir.as_deref()).await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
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
