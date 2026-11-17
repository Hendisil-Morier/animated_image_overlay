//! Wayland overlay lifecycle runner for a single display surface instance.

// Process exit status, thread-safe wakeup handle, and monotonic startup time.
use std::{path::Path, sync::Arc, time::Instant};

// `calloop` event loop, ping wakeups, and POSIX signal integration.
use calloop::{
    EventLoop, LoopHandle,
    ping::{PingSource, make_ping},
    signals::{Signal, Signals},
};
// SCTK globals, compositor, subcompositor, outputs, layer-shell, seat, and SHM support.
use smithay_client_toolkit::{
    compositor::CompositorState, globals::GlobalData, output::OutputState,
    reexports::calloop_wayland_source::WaylandSource, registry::RegistryState, seat::SeatState,
    shell::wlr_layer::LayerShell, shm::Shm, subcompositor::SubcompositorState,
};
// Raw Wayland connection and initial registry enumeration.
use wayland_client::{
    Connection, EventQueue,
    globals::{GlobalList, registry_queue_init},
    protocol::wl_output::WlOutput,
};
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

// Runtime coordinator, input state, monitor resolution, overlay scene, and stats setup.
use crate::{
    audio::{AVSyncMode, AudioEngine, AudioProbe},
    cache::{FrameCache, WakeupCallback},
    cli::CliArgs,
    config::{AudioPrefs, ConfigStore, OverlayState},
    decode::video::has_video_signature,
    error::{OverlayError, WaylandError},
    layout::OverlayLayout,
    playback::PlaybackClock,
    wayland::{
        input::InputState,
        monitor::{MonitorDescriptor, resolve_monitor_index},
        runtime::OverlayRuntime,
        scene::OverlayScene,
        stats::OverlayStats,
    },
};

/// Bound Wayland globals required before initializing overlay surfaces and subsurfaces.
struct WaylandBindings
{
    // Compositor global creating surfaces and click regions.
    compositor: CompositorState,

    // Subcompositor global managing child image, handle, and border layers.
    subcompositor: SubcompositorState,

    // `wlr-layer-shell` global anchoring the overlay surface above desktop windows.
    layer_shell: LayerShell,

    // `wp_viewporter` global scaling and cropping buffers without software rescaling.
    viewporter: WpViewporter,

    // Shared-memory pool allocator backing overlay frame and chrome slots.
    shm: Shm,
}

/// Startup configuration, canonical key, initial placement state, and opened frame store.
struct StartupResources
{
    // Locked configuration JSON store (`apng-overlay.json`).
    config_store: ConfigStore,

    // Canonical path key identifying this image inside `config_store`.
    config_key: String,

    // Initial placement, scale, rotation, and flip state with CLI `--scale` applied.
    initial_state: OverlayState,

    // Open frame store holding decoded frame 0 and disk/RAM chunks.
    cache: FrameCache,

    // Optional audio output for an embedded video track or `--audio` companion file.
    audio: Option<AudioEngine>,

    // Volume and mute preferences resolved from saved config plus `--volume` / `--mute`.
    audio_prefs: AudioPrefs,

    // Configured `DisplayFrameCache` byte limit (`-c display=<MB>`).
    display_budget_bytes: usize,
}

/// Masks `SIGINT` and `SIGTERM` on the calling thread before worker threads spawn.
pub fn prepare_signals() -> Result<Signals, OverlayError>
{
    Signals::new(&[Signal::SIGINT, Signal::SIGTERM]).map_err(|err| {
        WaylandError::EventLoop(format!("Failed registering POSIX signals: {err}")).into()
    })
}

/// Runs a single overlay instance for `file` until exit or external termination.
pub(crate) fn run_single_overlay(
    file: &Path,
    cli: &CliArgs,
    signals: Option<Signals>,
    exit_source: Option<PingSource>,
    file_label: Option<String>,
) -> Result<(), OverlayError>
{
    let conn =
        Connection::connect_to_env().map_err(|err| WaylandError::Connection(err.to_string()))?;
    let (globals, mut event_queue, bindings) = bind_wayland_globals(&conn)?;
    let qh = event_queue.handle();

    let (wakeup_ping, ping_source) = make_ping().map_err(|err| {
        WaylandError::EventLoop(format!("Failed creating calloop wakeup ping: {err}"))
    })?;
    let wakeup_cb: WakeupCallback = Arc::new(move || wakeup_ping.ping());
    let startup = load_startup_resources(file, cli, Some(wakeup_cb))?;
    let (frame_w, frame_h) = startup.cache.frame_dimensions();

    let event_loop: EventLoop<OverlayRuntime> = EventLoop::try_new().map_err(|err| {
        WaylandError::EventLoop(format!("Failed creating calloop event loop: {err}"))
    })?;
    let loop_handle = event_loop.handle();
    // Create the cursor surface before `roundtrip` so `Capability::Pointer` binds immediately.
    let cursor_surface = bindings.compositor.create_surface(&qh);

    let initial_decode_done = startup.cache.is_decode_done();
    let now = Instant::now();

    let mut runtime = OverlayRuntime {
        conn: conn.clone(),
        qh: qh.clone(),
        loop_handle: loop_handle.clone(),
        registry_state: RegistryState::new(&globals),
        compositor: bindings.compositor,
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        shm: bindings.shm,
        cursor_surface,
        themed_pointer: None,
        last_cursor: None,
        keyboard: None,
        config_store: startup.config_store,
        config_key: startup.config_key,
        cache: startup.cache,
        audio: startup.audio,
        audio_prefs: startup.audio_prefs,
        audio_started: false,
        scene: None,
        layout: OverlayLayout::new(frame_w, frame_h, 1920, 1080, startup.initial_state),
        input: InputState::with_editing(cli.edit),
        playback: PlaybackClock::with_max_fps(cli.max_fps),
        start_instant: now,
        stats: cli.stats.then(|| {
            OverlayStats::with_cpu(now, initial_decode_done, OverlayStats::sample_cpu_time())
        }),
        configured: false,
        frame_callback_pending: false,
        dirty_save: false,
        frame_timer_token: None,
        save_timer_token: None,
        fade_timer_token: None,
        stats_timer_token: None,
        file_label,
        exit_requested: false,
    };

    let (target_output, target_desc) =
        select_target_output(&mut event_queue, &mut runtime, &cli.monitor)?;
    runtime
        .layout
        .set_screen_size(target_desc.width as u32, target_desc.height as u32);

    let scene = OverlayScene::create(
        &qh,
        &runtime.compositor,
        &bindings.subcompositor,
        &bindings.layer_shell,
        &bindings.viewporter,
        &runtime.shm,
        &target_output,
        &target_desc,
        &runtime.layout,
        runtime.cache.cache_profile(),
        startup.display_budget_bytes,
    )?;
    runtime.scene = Some(scene);

    register_loop_sources(
        &loop_handle,
        conn,
        event_queue,
        signals,
        exit_source,
        ping_source,
    )?;
    runtime.schedule_stats_timer_if_enabled();
    tracing::info!(
        file = %file.display(),
        monitor = target_desc.name.as_deref().unwrap_or("unknown"),
        screen_w = target_desc.width,
        screen_h = target_desc.height,
        frame_w,
        frame_h,
        "Entering Wayland overlay event loop"
    );

    run_dispatch_loop(event_loop, &mut runtime)?;
    runtime.shutdown_cleanup();
    Ok(())
}

/// Binds required Wayland globals and returns the initialized queue alongside bound protocol state.
fn bind_wayland_globals(
    conn: &Connection,
) -> Result<(GlobalList, EventQueue<OverlayRuntime>, WaylandBindings), OverlayError>
{
    let (globals, event_queue) = registry_queue_init::<OverlayRuntime>(conn)
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

    Ok((
        globals,
        event_queue,
        WaylandBindings {
            compositor,
            subcompositor,
            layer_shell,
            viewporter,
            shm,
        },
    ))
}

/// Loads persisted per-image config, applies CLI scale override, and opens the frame cache store.
fn load_startup_resources(
    file: &Path,
    cli: &CliArgs,
    wakeup_cb: Option<WakeupCallback>,
) -> Result<StartupResources, OverlayError>
{
    let config_store = ConfigStore::open_default()?;
    let config_key = ConfigStore::state_key(file)?;
    let mut initial_state = config_store.load_state_entry(&config_key);
    if let Some(scale) = cli.scale
    {
        initial_state.scale = scale;
    }

    let ram_budget_bytes = cli.ram_cache_bytes()?;
    let display_budget_bytes = cli.display_cache_bytes()?;
    let video_cache_bytes = cli.video_cache_bytes()?;
    let cache = FrameCache::open_with_options(
        file,
        cli.base_width,
        cli.max_frames,
        ram_budget_bytes,
        video_cache_bytes,
        Some(cli.gpu.as_str()),
        wakeup_cb,
    )?;

    let mut audio_prefs = config_store.load_audio_prefs(&config_key);
    if let Some(volume) = cli.volume
    {
        audio_prefs.volume = AudioPrefs::clamped_volume(volume);
    }
    audio_prefs.muted |= cli.mute;
    let audio = open_audio(file, cli, audio_prefs)?;

    Ok(StartupResources {
        config_store,
        config_key,
        initial_state,
        cache,
        audio,
        audio_prefs,
        display_budget_bytes,
    })
}

/// Opens audio for an embedded video track or the `--audio` file, or continues silently on failure.
///
/// An explicit `--audio` file loops independently of the visuals, while a video's own track is
/// realigned to frame 0 at every visual loop wrap.
fn open_audio(
    file: &Path,
    cli: &CliArgs,
    prefs: AudioPrefs,
) -> Result<Option<AudioEngine>, OverlayError>
{
    let is_explicit = cli.audio.is_some();
    let (path, mode) = match &cli.audio
    {
        Some(audio) => (audio.clone(), AVSyncMode::IndependentAudioLoop),
        None if has_video_signature(file).unwrap_or(false) =>
        {
            (file.to_path_buf(), AVSyncMode::CoupledLoop)
        }
        None => return Ok(None),
    };

    let probe = match AudioProbe::probe_optional(&path)
    {
        Ok(Some(probe)) => probe,
        Ok(None) if is_explicit =>
        {
            return Err(OverlayError::Audio(crate::error::AudioError::ProbeFailed {
                path,
                reason: String::from("Explicit --audio file contains no valid audio streams"),
            }));
        }
        Ok(None) =>
        {
            tracing::info!(path = %path.display(), "Media file has no audio stream");
            return Ok(None);
        }
        Err(err) if is_explicit => return Err(OverlayError::Audio(err)),
        Err(err) =>
        {
            tracing::warn!(error = %err, "Audio probe failed; continuing silently");
            return Ok(None);
        }
    };

    match AudioEngine::open(
        &path,
        probe,
        mode,
        None,
        Instant::now(),
        prefs.volume,
        prefs.muted,
        None,
    )
    {
        Ok(engine) => Ok(Some(engine)),
        Err(err) if is_explicit => Err(OverlayError::Audio(err)),
        Err(err) =>
        {
            tracing::warn!(error = %err, "Audio engine failed to start; continuing silently");
            Ok(None)
        }
    }
}

/// Performs two compositor roundtrips so `wl_output` / `xdg_output` metadata resolves `--monitor`.
fn select_target_output(
    event_queue: &mut EventQueue<OverlayRuntime>,
    runtime: &mut OverlayRuntime,
    selector: &str,
) -> Result<(WlOutput, MonitorDescriptor), OverlayError>
{
    event_queue
        .roundtrip(runtime)
        .map_err(|err| WaylandError::EventLoop(err.to_string()))?;
    event_queue
        .roundtrip(runtime)
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

    let target_idx = resolve_monitor_index(&descriptors, selector)?;
    let target_output =
        outputs
            .get(target_idx)
            .cloned()
            .ok_or_else(|| WaylandError::MonitorNotFound {
                selector: selector.to_string(),
            })?;

    Ok((target_output, descriptors[target_idx].clone()))
}

/// Attaches the Wayland socket, termination sources, and decode wakeup ping to the `calloop` dispatcher.
fn register_loop_sources(
    loop_handle: &LoopHandle<'static, OverlayRuntime>,
    conn: Connection,
    event_queue: EventQueue<OverlayRuntime>,
    signals: Option<Signals>,
    exit_source: Option<PingSource>,
    ping_source: PingSource,
) -> Result<(), OverlayError>
{
    WaylandSource::new(conn, event_queue)
        .insert(loop_handle.clone())
        .map_err(|err| WaylandError::EventLoop(err.to_string()))?;

    if let Some(signals) = signals
    {
        loop_handle
            .insert_source(signals, |event, _, state| {
                tracing::info!(signal = ?event.signal(), "Received termination signal");
                state.exit_requested = true;
            })
            .map_err(|err| WaylandError::EventLoop(err.to_string()))?;
    }

    if let Some(exit_source) = exit_source
    {
        loop_handle
            .insert_source(exit_source, |_, _, state| {
                tracing::info!("Received external exit ping");
                state.exit_requested = true;
            })
            .map_err(|err| WaylandError::EventLoop(err.to_string()))?;
    }

    loop_handle
        .insert_source(ping_source, |_, _, state| {
            state.schedule_playback_if_needed();
        })
        .map_err(|err| WaylandError::EventLoop(err.to_string()))?;

    Ok(())
}

/// Drives `event_loop` dispatch iterations until signals, keyboard quit, or layer close request exit.
fn run_dispatch_loop(
    mut event_loop: EventLoop<OverlayRuntime>,
    runtime: &mut OverlayRuntime,
) -> Result<(), OverlayError>
{
    while !runtime.exit_requested
    {
        event_loop
            .dispatch(None, runtime)
            .map_err(|err| WaylandError::EventLoop(err.to_string()))?;
    }
    Ok(())
}

/// Builds a uniform `MissingProtocol` diagnostic error when a required Wayland global fails to bind.
fn missing_protocol(interface: &'static str, detail: impl ToString) -> WaylandError
{
    WaylandError::MissingProtocol {
        interface,
        detail: detail.to_string(),
    }
}
