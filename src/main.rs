//! Binary entry point for the Wayland animated image overlay.

#![deny(clippy::unwrap_used, clippy::expect_used)]

// Deterministic POSIX exit status for CLI, validation, and runtime failures.
use std::process::ExitCode;

// Application CLI parsing, tracing diagnostics, and Wayland runtime runner.
use animated_image_widget::{
    cli::{CliArgs, CliOutcome},
    logging, wayland,
};

/// Application entry point returning a deterministic POSIX exit status.
fn main() -> ExitCode
{
    let cli = match CliArgs::parse_env()
    {
        Ok(CliOutcome::Run(args)) => args,
        Ok(CliOutcome::EarlyExit(message)) =>
        {
            print!("{message}");
            return ExitCode::SUCCESS;
        }
        Err(err) =>
        {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    // Block SIGINT/SIGTERM before spawning logging or decoder worker threads.
    let signals = match wayland::prepare_signals()
    {
        Ok(signals) => signals,
        Err(err) =>
        {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    let log_guard = match logging::init_logging_with_debug(cli.debug)
    {
        Ok(guard) => guard,
        Err(err) =>
        {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    let file_names: Vec<String> = cli.files.iter().map(|p| p.display().to_string()).collect();
    tracing::info!(
        files = ?file_names,
        max_frames = cli.max_frames,
        base_width = cli.base_width,
        monitor = %cli.monitor,
        scale = ?cli.scale,
        max_fps = cli.max_fps,
        ram_cache_mb = cli.ram_cache_mb,
        stats = cli.stats,
        debug = cli.debug,
        "Starting Wayland animated image overlay"
    );

    let exit_code = match wayland::run_overlay(&cli, signals)
    {
        Ok(code) => code,
        Err(err) =>
        {
            tracing::error!(error = %err, "Overlay exited with error");
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    };

    log_guard.flush();
    exit_code
}
