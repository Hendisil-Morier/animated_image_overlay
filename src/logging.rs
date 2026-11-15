//! Dual-output tracing diagnostics for console output and non-blocking log files.
//!
//! Console diagnostics stay silent unless `OVERLAY_DEBUG` enables verbose output,
//! while daily-rotated log files under `~/.cache/apng-overlay/logs/` capture
//! structured operational events with explicit flush on drop or panic.

// Standard library environment lookup, directories, I/O, panic hooks, and locking.
use std::{
    env, fs, io, panic,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

// Rolling file appender and background worker handle for disk logging.
use tracing_appender::{
    non_blocking::WorkerGuard,
    rolling::{RollingFileAppender, Rotation},
};
// Multi-layer subscriber composition and severity filtering.
use tracing_subscriber::{Layer, Registry, filter::LevelFilter, fmt, layer::SubscriberExt};

// Top-level domain error enum for mapping logging initialization failures.
use crate::error::OverlayError;

// Base application subdirectory under XDG cache storage.
const CACHE_APP_DIR: &str = "apng-overlay";

// Subdirectory holding persistent daily-rotated overlay log files.
const LOGS_SUBDIR: &str = "logs";

// Filename prefix assigned to rolling disk log files.
const LOG_FILE_PREFIX: &str = "apng-overlay";

// Filename suffix assigned to rolling disk log files.
const LOG_FILE_SUFFIX: &str = "log";

// Maximum number of rotated daily log files retained on disk.
const MAX_RETAINED_LOG_FILES: usize = 7;

/// Owns the non-blocking file appender guard and ensures synchronous flush on drop or panic.
#[derive(Clone, Debug)]
pub struct LoggingGuard
{
    // Shared optional guard slot taken once upon manual flush, panic, or drop.
    worker_guard: Arc<Mutex<Option<WorkerGuard>>>,
}

impl LoggingGuard
{
    /// Wraps a newly created background writer guard for shared lifecycle cleanup.
    fn new(worker_guard: WorkerGuard) -> Self
    {
        Self {
            worker_guard: Arc::new(Mutex::new(Some(worker_guard))),
        }
    }

    /// Flushes all queued log records synchronously by dropping the underlying worker handle.
    pub fn flush(&self)
    {
        flush_shared_guard(&self.worker_guard);
    }

    /// Returns a shared handle suitable for panic and signal shutdown hooks.
    fn shared_handle(&self) -> Arc<Mutex<Option<WorkerGuard>>>
    {
        Arc::clone(&self.worker_guard)
    }
}

impl Drop for LoggingGuard
{
    /// Flushes remaining buffered disk logs before the main guard goes out of scope.
    fn drop(&mut self)
    {
        self.flush();
    }
}

/// Drops the shared `WorkerGuard` once, poisoning-safe, to force buffered writes onto disk.
fn flush_shared_guard(shared: &Arc<Mutex<Option<WorkerGuard>>>)
{
    let maybe_guard = match shared.lock()
    {
        Ok(mut slot) => slot.take(),
        Err(poisoned) => poisoned.into_inner().take(),
    };
    drop(maybe_guard);
}

/// Interprets an optional `OVERLAY_DEBUG` value into a boolean verbosity flag.
///
/// Empty values, `"0"`, `"false"`, and `"no"` keep console logging disabled;
/// any other non-empty setting enables debug-level stdout diagnostics.
pub fn parse_debug_flag(value: Option<&str>) -> bool
{
    let Some(raw) = value
    else
    {
        return false;
    };

    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "0"
    {
        return false;
    }

    !trimmed.eq_ignore_ascii_case("false") && !trimmed.eq_ignore_ascii_case("no")
}

/// Reads `OVERLAY_DEBUG` from the process environment to determine console verbosity.
pub fn is_debug_env_enabled() -> bool
{
    let env_val = env::var("OVERLAY_DEBUG").ok();
    parse_debug_flag(env_val.as_deref())
}

/// Resolves the default log directory path (`$XDG_CACHE_HOME/apng-overlay/logs` or `~/.cache/...`).
pub fn default_logs_dir() -> Result<PathBuf, OverlayError>
{
    let cache_root = resolve_cache_root()?;
    Ok(cache_root.join(CACHE_APP_DIR).join(LOGS_SUBDIR))
}

/// Selects the base XDG cache directory from `XDG_CACHE_HOME` or `HOME`.
fn resolve_cache_root() -> Result<PathBuf, OverlayError>
{
    if let Some(xdg) = env::var_os("XDG_CACHE_HOME")
        && !xdg.is_empty()
    {
        return Ok(PathBuf::from(xdg));
    }

    let home = env::var_os("HOME")
        .filter(|path| !path.is_empty())
        .ok_or_else(|| {
            OverlayError::LoggingInit(String::from("HOME and XDG_CACHE_HOME are both unset"))
        })?;

    Ok(PathBuf::from(home).join(".cache"))
}

/// Constructs a dual-output `tracing` subscriber targeting stdout and rolling disk files.
///
/// Exposed publicly so integration tests can verify multi-configuration logging with
/// thread-local `tracing::subscriber::with_default` without mutating global state.
pub fn build_subscriber_in_dir(
    logs_dir: &Path,
    console_debug: bool,
) -> Result<(Box<dyn tracing::Subscriber + Send + Sync>, LoggingGuard), OverlayError>
{
    fs::create_dir_all(logs_dir).map_err(|err| {
        OverlayError::LoggingInit(format!(
            "Failed to create log directory {}: {err}",
            logs_dir.display()
        ))
    })?;

    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(LOG_FILE_PREFIX)
        .filename_suffix(LOG_FILE_SUFFIX)
        .max_log_files(MAX_RETAINED_LOG_FILES)
        .build(logs_dir)
        .map_err(|err| {
            OverlayError::LoggingInit(format!(
                "Failed to open rolling log appender in {}: {err}",
                logs_dir.display()
            ))
        })?;

    let (non_blocking, worker_guard) = tracing_appender::non_blocking(appender);
    let guard = LoggingGuard::new(worker_guard);

    let console_filter = if console_debug
    {
        LevelFilter::DEBUG
    }
    else
    {
        LevelFilter::OFF
    };

    let console_layer = fmt::layer()
        .with_writer(io::stdout)
        .with_target(false)
        .with_filter(console_filter);

    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_writer(non_blocking)
        .with_filter(LevelFilter::DEBUG);

    let subscriber = Registry::default().with(console_layer).with(file_layer);
    Ok((Box::new(subscriber), guard))
}

/// Initializes global dual-output logging inside `logs_dir` and registers a panic-flush hook.
pub fn init_logging_in_dir(
    logs_dir: &Path,
    console_debug: bool,
) -> Result<LoggingGuard, OverlayError>
{
    let (subscriber, guard) = build_subscriber_in_dir(logs_dir, console_debug)?;

    tracing::subscriber::set_global_default(subscriber).map_err(|err| {
        OverlayError::LoggingInit(format!("Failed to set global tracing subscriber: {err}"))
    })?;

    install_panic_flush_hook(guard.shared_handle());
    Ok(guard)
}

/// Initializes global dual-output logging using the default cache directory and `OVERLAY_DEBUG`.
pub fn init_logging() -> Result<LoggingGuard, OverlayError>
{
    let logs_dir = default_logs_dir()?;
    let console_debug = is_debug_env_enabled();
    init_logging_in_dir(&logs_dir, console_debug)
}

/// Chains a panic hook that logs the failure message and flushes buffered logs before aborting.
fn install_panic_flush_hook(shared_guard: Arc<Mutex<Option<WorkerGuard>>>)
{
    let previous_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        let location = panic_info
            .location()
            .map(|loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()))
            .unwrap_or_else(|| String::from("<unknown>"));

        let payload = panic_info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| {
                panic_info
                    .payload()
                    .downcast_ref::<String>()
                    .map(String::as_str)
            })
            .unwrap_or("non-string panic payload");

        tracing::error!(location = %location, panic = %payload, "Unhandled panic occurred");
        flush_shared_guard(&shared_guard);
        previous_hook(panic_info);
    }));
}
