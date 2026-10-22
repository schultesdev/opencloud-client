//! Process logging (Phase 8): `tracing` with a daily-rotating log file
//! in the configuration directory, leveled from `config.json`
//! (`settings.log_level`). The non-blocking writer's guard must stay
//! alive for the process duration — `main` holds it.
//!
//! The `RUST_LOG` environment variable (via `tracing-subscriber`'s env
//! filter) takes precedence over the configured level for debugging.

use std::path::Path;

use oc_config::LogLevel;
use tracing::level_filters::LevelFilter;
use tracing_appender::non_blocking;
use tracing_appender::rolling;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;

/// The name of the log file inside `<config-dir>/logs/`.
const LOG_FILE: &str = "opencloud-sync.log";

/// Maps a configured [`LogLevel`] to a tracing level filter.
fn level_filter(level: LogLevel) -> LevelFilter {
    match level {
        LogLevel::Error => LevelFilter::ERROR,
        LogLevel::Warn => LevelFilter::WARN,
        LogLevel::Info => LevelFilter::INFO,
        LogLevel::Debug => LevelFilter::DEBUG,
        LogLevel::Trace => LevelFilter::TRACE,
    }
}

/// Initializes the global tracing subscriber. Log lines land in
/// `<config-dir>/logs/opencloud-sync.log.<date>` (one file per day,
/// kept until the user removes them). Returns the writer guard; dropping
/// it discards pending log lines, so keep it alive until shutdown.
///
/// When the config directory cannot be resolved, logging falls back to
/// stderr — better a loud client than a silent one.
pub fn init(config_dir: Option<&Path>, level: LogLevel) -> non_blocking::WorkerGuard {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(level_filter(level).to_string()));
    match config_dir.map(|dir| dir.join("logs")) {
        Some(log_dir) => {
            let file = rolling::daily(&log_dir, LOG_FILE);
            let (writer, guard) = non_blocking(file);
            let _ = fmt()
                .with_env_filter(filter)
                .with_writer(writer)
                .with_ansi(false)
                .try_init();
            guard
        }
        None => {
            let _ = fmt().with_env_filter(filter).with_ansi(false).try_init();
            // A dummy guard keeps the caller's signature uniform; stderr
            // needs no background worker.
            non_blocking(std::io::sink()).1
        }
    }
}
