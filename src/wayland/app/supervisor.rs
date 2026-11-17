//! Multi-overlay supervisor managing concurrent worker threads and signal routing.

// Process exit status, thread handles, and channel communication.
use std::{
    panic::{self, AssertUnwindSafe},
    process::ExitCode,
    sync::{
        Arc,
        mpsc::{self, Receiver, TryRecvError},
    },
    thread::{self, JoinHandle},
};

// `calloop` event loop, ping wakeups, and POSIX signal integration.
use calloop::{
    EventLoop,
    ping::{Ping, make_ping},
    signals::Signals,
};

// CLI arguments, error types, and single-overlay runner.
use super::runner::run_single_overlay;
use crate::{
    cli::CliArgs,
    error::{OverlayError, WaylandError},
};

/// State maintained by the coordinator across parallel overlay worker threads.
struct CoordinatorState
{
    // Count of worker threads that have not yet reported exit or terminated.
    remaining_workers: usize,

    // First error encountered by any failing worker overlay.
    first_error: Option<OverlayError>,
}

/// Connects to Wayland, validates compositor protocols, and runs one or more overlay loops until exit.
pub fn run_overlay(cli: &CliArgs, signals: Signals) -> Result<ExitCode, OverlayError>
{
    if cli.files.is_empty()
    {
        return Err(OverlayError::CliValidation(String::from(
            "At least one input file must be specified",
        )));
    }

    if cli.files.len() == 1
    {
        run_single_overlay(&cli.files[0], cli, Some(signals), None, None)?;
        return Ok(ExitCode::SUCCESS);
    }

    run_multi_overlay(cli, signals)
}

/// Manages multiple concurrent overlay worker threads, routing signals and joining on exit.
fn run_multi_overlay(cli: &CliArgs, signals: Signals) -> Result<ExitCode, OverlayError>
{
    let mut coordinator_loop: EventLoop<CoordinatorState> =
        EventLoop::try_new().map_err(|err| {
            WaylandError::EventLoop(format!("Failed creating coordinator event loop: {err}"))
        })?;
    let coord_handle = coordinator_loop.handle();

    let (done_tx, done_rx) = mpsc::channel::<Result<(), OverlayError>>();
    let (wake_ping, wake_source) = make_ping().map_err(|err| {
        WaylandError::EventLoop(format!("Failed creating coordinator wake ping: {err}"))
    })?;

    let mut exit_pings: Vec<Ping> = Vec::with_capacity(cli.files.len());
    let mut thread_handles: Vec<JoinHandle<()>> = Vec::with_capacity(cli.files.len());

    for file in &cli.files
    {
        let (exit_ping, exit_source) = make_ping().map_err(|err| {
            WaylandError::EventLoop(format!("Failed creating worker exit ping: {err}"))
        })?;
        exit_pings.push(exit_ping);

        let file_path = file.clone();
        let file_label = file.file_name().map(|n| n.to_string_lossy().into_owned());
        let cli_clone = cli.clone();
        let tx = done_tx.clone();
        let p = wake_ping.clone();

        let thread_name = format!(
            "aiw-{}",
            file.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("worker")
        );

        let handle = thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let res = match panic::catch_unwind(AssertUnwindSafe(|| {
                    run_single_overlay(&file_path, &cli_clone, None, Some(exit_source), file_label)
                }))
                {
                    Ok(outcome) => outcome,
                    Err(_) =>
                    {
                        tracing::error!(
                            path = %file_path.display(),
                            "Overlay worker thread panicked unexpectedly"
                        );
                        Err(OverlayError::Wayland(WaylandError::EventLoop(format!(
                            "Worker thread panicked while displaying {}",
                            file_path.display()
                        ))))
                    }
                };
                let _ = tx.send(res);
                p.ping();
            })
            .map_err(|err| {
                WaylandError::EventLoop(format!("Failed spawning worker thread: {err}"))
            })?;

        thread_handles.push(handle);
    }

    drop(done_tx);

    let shared_pings = Arc::new(exit_pings);
    let pings_for_signals = Arc::clone(&shared_pings);
    coord_handle
        .insert_source(signals, move |event, _, _| {
            tracing::info!(signal = ?event.signal(), "Termination signal received; stopping all overlays");
            for ping in pings_for_signals.iter()
            {
                ping.ping();
            }
        })
        .map_err(|err| WaylandError::EventLoop(err.to_string()))?;

    let pings_for_wake = Arc::clone(&shared_pings);
    coord_handle
        .insert_source(wake_source, move |_, _, state| {
            drain_worker_results(&done_rx, state, &pings_for_wake);
        })
        .map_err(|err| WaylandError::EventLoop(err.to_string()))?;

    let mut state = CoordinatorState {
        remaining_workers: cli.files.len(),
        first_error: None,
    };

    while state.remaining_workers > 0
    {
        coordinator_loop
            .dispatch(None, &mut state)
            .map_err(|err| WaylandError::EventLoop(err.to_string()))?;
    }

    for handle in thread_handles
    {
        let _ = handle.join();
    }

    if let Some(err) = state.first_error
    {
        return Err(err);
    }

    Ok(ExitCode::SUCCESS)
}

/// Drains available worker exit results from `done_rx`, recording errors and notifying siblings on failure.
fn drain_worker_results(
    done_rx: &Receiver<Result<(), OverlayError>>,
    state: &mut CoordinatorState,
    pings_for_wake: &[Ping],
)
{
    loop
    {
        match done_rx.try_recv()
        {
            Ok(res) =>
            {
                state.remaining_workers = state.remaining_workers.saturating_sub(1);
                if let Err(err) = res
                    && state.first_error.is_none()
                {
                    tracing::error!(error = %err, "Worker overlay encountered error; closing siblings");
                    state.first_error = Some(err);
                    for ping in pings_for_wake
                    {
                        ping.ping();
                    }
                }
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) =>
            {
                // All worker thread senders have dropped. Reset remaining workers to prevent hanging.
                state.remaining_workers = 0;
                break;
            }
        }
    }
}
