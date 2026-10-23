//! Process logging (Phase 8): `tracing` with a daily-rotating log file
//! in the configuration directory, leveled from `config.json`
//! (`settings.log_level`). The non-blocking writer's guard must stay
//! alive for the process duration — `main` holds it.
//!
//! The `RUST_LOG` environment variable (via `tracing-subscriber`'s env
//! filter) takes precedence over the configured level for debugging.
//! A changed `log_level` applies at runtime ([`set_level`]).

use std::path::Path;

use oc_config::LogLevel;
use tracing::level_filters::LevelFilter;
use tracing_appender::non_blocking;
use tracing_appender::rolling;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Registry;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::reload;
use tracing_subscriber::util::SubscriberInitExt;

/// The name of the log file inside `<config-dir>/logs/`.
const LOG_FILE: &str = "opencloud-sync.log";

/// Swaps the active filter at runtime (set once by [`init`]).
static FILTER: std::sync::OnceLock<reload::Handle<EnvFilter, Registry>> =
    std::sync::OnceLock::new();

/// Applies a changed `settings.log_level` without a restart. A `RUST_LOG`
/// override stays in force.
pub fn set_level(level: LogLevel) {
    if std::env::var_os("RUST_LOG").is_some() {
        return;
    }
    if let Some(handle) = FILTER.get() {
        let _ = handle.reload(EnvFilter::new(level_filter(level).to_string()));
    }
}

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
    let (filter, handle) = reload::Layer::new(filter);
    let registry = tracing_subscriber::registry().with(filter);
    let guard = match config_dir.map(|dir| dir.join("logs")) {
        Some(log_dir) => {
            let file = rolling::daily(&log_dir, LOG_FILE);
            let (writer, guard) = non_blocking(file);
            let _ = registry
                .with(fmt::layer().with_writer(writer).with_ansi(false))
                .try_init();
            guard
        }
        None => {
            let _ = registry.with(fmt::layer().with_ansi(false)).try_init();
            // A dummy guard keeps the caller's signature uniform; stderr
            // needs no background worker.
            non_blocking(std::io::sink()).1
        }
    };
    let _ = FILTER.set(handle);
    guard
}
