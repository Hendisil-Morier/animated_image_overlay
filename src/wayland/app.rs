//! `calloop`-driven Wayland overlay initialization and event-loop runner.

// Process exit status, thread-safe wakeup handle, and monotonic startup time.
use std::{process::ExitCode, sync::Arc, time::Instant};

// `calloop` event loop, ping wakeups, and POSIX signal integration.
use calloop::{
    EventLoop,
    ping::make_ping,
    signals::{Signal, Signals},
};
// SCTK globals, compositor, subcompositor, outputs, layer-shell, seat, and SHM support.
use smithay_client_toolkit::{
    compositor::CompositorState, globals::GlobalData, output::OutputState,
    reexports::calloop_wayland_source::WaylandSource, registry::RegistryState, seat::SeatState,
    shell::wlr_layer::LayerShell, shm::Shm, subcompositor::SubcompositorState,
};
// Raw Wayland connection and initial registry enumeration.
use wayland_client::{Connection, globals::registry_queue_init, protocol::wl_output::WlOutput};
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

// Runtime coordinator, input state, monitor resolution, and overlay scene setup.
use super::{
    input::InputState,
    monitor::{MonitorDescriptor, resolve_monitor_index},
    runtime::OverlayRuntime,
    scene::OverlayScene,
};
use crate::{
    cache::FrameCache,
    cli::CliArgs,
    config::ConfigStore,
    error::{OverlayError, WaylandError},
    layout::OverlayLayout,
    playback::PlaybackClock,
};

/// Masks `SIGINT` and `SIGTERM` on the calling thread before worker threads spawn.
pub fn prepare_signals() -> Result<Signals, OverlayError>
{
    Signals::new(&[Signal::SIGINT, Signal::SIGTERM]).map_err(|err| {
        WaylandError::EventLoop(format!("Failed registering POSIX signals: {err}")).into()
    })
}

/// Connects to Wayland, validates compositor protocols, and runs the overlay loop until exit.
pub fn run_overlay(cli: &CliArgs, signals: Signals) -> Result<ExitCode, OverlayError>
{
    let conn =
        Connection::connect_to_env().map_err(|err| WaylandError::Connection(err.to_string()))?;

    let (globals, mut event_queue) = registry_queue_init::<OverlayRuntime>(&conn)
        .map_err(|err| WaylandError::EventLoop(err.to_string()))?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|err| missing_protocol("wl_compositor", err))?;

    let subcompositor = SubcompositorState::bind(compositor.wl_compositor().clone(), &globals, &qh)
        .map_err(|err| missing_protocol("wl_subcompositor", err))?;

    let layer_shell = LayerShell::bind(&globals, &qh)
        .map_err(|err| missing_protocol("zwlr_layer_shell_v1", err))?;

    let viewporter: WpViewporter = globals
        .bind(&qh, 1..=1, GlobalData)
        .map_err(|err| missing_protocol("wp_viewporter", err))?;

    let shm = Shm::bind(&globals, &qh).map_err(|err| missing_protocol("wl_shm", err))?;

    let config_store = ConfigStore::open_default()?;
    let config_key = ConfigStore::state_key(&cli.file)?;
    let mut initial_state = config_store.load_state_entry(&config_key);
    if let Some(scale) = cli.scale
    {
        initial_state.scale = scale;
    }

    let (wakeup_ping, ping_source) = make_ping().map_err(|err| {
        WaylandError::EventLoop(format!("Failed creating calloop wakeup ping: {err}"))
    })?;
    let wakeup_cb = Arc::new(move || {
        wakeup_ping.ping();
    });

    let ram_budget_bytes = cli.ram_cache_bytes()?;
    let display_budget_bytes = cli.display_cache_bytes()?;
    let cache = FrameCache::open_with_ram_budget(
        &cli.file,
        cli.base_width,
        cli.max_frames,
        ram_budget_bytes,
        Some(wakeup_cb),
    )?;
    let (frame_w, frame_h) = cache.frame_dimensions();

    let mut event_loop: EventLoop<OverlayRuntime> = EventLoop::try_new().map_err(|err| {
        WaylandError::EventLoop(format!("Failed creating calloop event loop: {err}"))
    })?;
    let loop_handle = event_loop.handle();

    // Create the cursor surface before `roundtrip` so `Capability::Pointer` binds immediately.
    let cursor_surface = compositor.create_surface(&qh);

    let mut runtime = OverlayRuntime {
        conn: conn.clone(),
        qh: qh.clone(),
        loop_handle: loop_handle.clone(),
        registry_state: RegistryState::new(&globals),
        compositor,
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        shm,
        cursor_surface,
        themed_pointer: None,
        last_cursor: None,
        keyboard: None,
        config_store,
        config_key,
        cache,
        scene: None,
        layout: OverlayLayout::new(frame_w, frame_h, 1920, 1080, initial_state),
        input: InputState::with_editing(cli.edit),
        playback: PlaybackClock::new(),
        start_instant: Instant::now(),
        configured: false,
        frame_callback_pending: false,
        dirty_save: false,
        frame_timer_token: None,
        save_timer_token: None,
        fade_timer_token: None,
        exit_requested: false,
    };

    // Perform two roundtrips so `wl_output` + `xdg_output` connector names and geometry resolve.
    event_queue
        .roundtrip(&mut runtime)
        .map_err(|err| WaylandError::EventLoop(err.to_string()))?;
    event_queue
        .roundtrip(&mut runtime)
        .map_err(|err| WaylandError::EventLoop(err.to_string()))?;

    let outputs: Vec<WlOutput> = runtime.output_state.outputs().collect();
    let descriptors: Vec<MonitorDescriptor> = outputs
        .iter()
        .enumerate()
        .filter_map(|(idx, output)| {
            runtime
                .output_state
                .info(output)
                .map(|info| MonitorDescriptor::from_output_info(idx, &info))
        })
        .collect();

    let target_idx = resolve_monitor_index(&descriptors, &cli.monitor)?;
    let target_output = outputs
        .get(target_idx)
        .ok_or_else(|| WaylandError::MonitorNotFound {
            selector: cli.monitor.clone(),
        })?;
    let target_desc = &descriptors[target_idx];

    runtime
        .layout
        .set_screen_size(target_desc.width as u32, target_desc.height as u32);

    let scene = OverlayScene::create(
        &qh,
        &runtime.compositor,
        &subcompositor,
        &layer_shell,
        &viewporter,
        &runtime.shm,
        target_output,
        target_desc,
        frame_w,
        frame_h,
        display_budget_bytes,
    )?;
    runtime.scene = Some(scene);

    WaylandSource::new(conn, event_queue)
        .insert(loop_handle.clone())
        .map_err(|err| WaylandError::EventLoop(err.to_string()))?;

    loop_handle
        .insert_source(signals, |event, _, state| {
            tracing::info!(signal = ?event.signal(), "Received termination signal");
            state.exit_requested = true;
        })
        .map_err(|err| WaylandError::EventLoop(err.to_string()))?;

    loop_handle
        .insert_source(ping_source, |_, _, state| {
            state.schedule_playback_if_needed();
        })
        .map_err(|err| WaylandError::EventLoop(err.to_string()))?;

    tracing::info!(
        monitor = target_desc.name.as_deref().unwrap_or("unknown"),
        screen_w = target_desc.width,
        screen_h = target_desc.height,
        frame_w,
        frame_h,
        "Entering Wayland overlay event loop"
    );

    while !runtime.exit_requested
    {
        event_loop
            .dispatch(None, &mut runtime)
            .map_err(|err| WaylandError::EventLoop(err.to_string()))?;
    }

    runtime.shutdown_cleanup();
    Ok(ExitCode::SUCCESS)
}

/// Builds a uniform `MissingProtocol` diagnostic error when a required Wayland global fails to bind.
fn missing_protocol(interface: &'static str, detail: impl ToString) -> WaylandError
{
    WaylandError::MissingProtocol {
        interface,
        detail: detail.to_string(),
    }
}
