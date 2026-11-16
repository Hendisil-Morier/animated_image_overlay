//! Runtime struct, `calloop` timer scheduling, stats snapshotting, pointer cursor, and cleanup.

// Monotonic timing and timeouts for debounce and handle fade timers.
use std::time::{Duration, Instant};

// `calloop` event loop handle and timer sources.
use calloop::{
    LoopHandle, RegistrationToken,
    timer::{TimeoutAction, Timer},
};
// SCTK compositor, outputs, registry, seat, SHM, and themed pointer cursor support.
use smithay_client_toolkit::{
    compositor::CompositorState,
    output::OutputState,
    registry::RegistryState,
    seat::{
        SeatState,
        pointer::{CursorIcon, ThemedPointer},
    },
    shm::Shm,
};
// Raw Wayland connection, queue handle, and device protocol handles.
use wayland_client::{
    Connection, QueueHandle,
    protocol::{wl_keyboard::WlKeyboard, wl_surface::WlSurface},
};

// Core subsystems, input state, cursor types, Wayland scene, and `--stats` telemetry types.
use crate::{
    cache::FrameCache,
    config::ConfigStore,
    layout::OverlayLayout,
    playback::PlaybackClock,
    wayland::{
        input::{HANDLE_FADE_MS, InputState, PointerCursor, SAVE_DEBOUNCE_MS},
        scene::OverlayScene,
        stats::{OverlayStats, STATS_INTERVAL_MS, StatsSnapshot},
    },
};

/// Active Wayland protocol state, timers, and overlay lifecycle coordinator.
pub(crate) struct OverlayRuntime
{
    // Active Wayland server connection used for cursor updates and protocol roundtrips.
    pub(crate) conn: Connection,

    // Event queue handle used when creating surfaces and registering frame callbacks.
    pub(crate) qh: QueueHandle<Self>,

    // `calloop` dispatcher handle used to arm/remove fade and save debounce timers.
    pub(crate) loop_handle: LoopHandle<'static, Self>,

    // Wayland global registry tracker required by SCTK `ProvidesRegistryState`.
    pub(crate) registry_state: RegistryState,

    // Wayland compositor state providing surfaces and click regions.
    pub(crate) compositor: CompositorState,

    // Wayland output state exposing monitor connector names and dimensions.
    pub(crate) output_state: OutputState,

    // Wayland seat tracker managing pointer and keyboard capabilities.
    pub(crate) seat_state: SeatState,

    // Wayland shared-memory global for frame rings and cursor themes.
    pub(crate) shm: Shm,

    // Cursor image surface bound to `ThemedPointer`, created eagerly before seat roundtrips.
    pub(crate) cursor_surface: WlSurface,

    // System cursor theme pointer handle when pointer capability is advertised.
    pub(crate) themed_pointer: Option<ThemedPointer<()>>,

    // Last cursor shape sent to the active pointer serial, cleared on pointer leave.
    pub(crate) last_cursor: Option<PointerCursor>,

    // Active Wayland keyboard object while seat capability is present.
    pub(crate) keyboard: Option<WlKeyboard>,

    // Locked JSON state persistence store (`~/.config/apng-overlay.json`).
    pub(crate) config_store: ConfigStore,

    // Canonical absolute file key identifying this image in configuration storage.
    pub(crate) config_key: String,

    // LZ4 disk cache and configurable RAM LRU store feeding rendered animation frames.
    pub(crate) cache: FrameCache,

    // Fullscreen layer surface, frame ring, and corner-handle/border subsurfaces.
    pub(crate) scene: Option<OverlayScene>,

    // Monitor-clamped overlay geometry, handle coordinates, and input region math.
    pub(crate) layout: OverlayLayout,

    // Pure pointer and edit-mode keyboard interaction state machine.
    pub(crate) input: InputState,

    // Animation frame deadline scheduler and stall recovery clock.
    pub(crate) playback: PlaybackClock,

    // Monotonic startup instant providing millisecond timestamps to `PlaybackClock`.
    pub(crate) start_instant: Instant,

    // Optional `--stats` accumulator emitting periodic 1-second playback and cache metrics.
    pub(crate) stats: Option<OverlayStats>,

    // Tracks whether the compositor has delivered the initial `wl_layer_surface::configure`.
    pub(crate) configured: bool,

    // Prevents queuing overlapping `wl_surface::frame` callbacks before compositor fires.
    pub(crate) frame_callback_pending: bool,

    // Tracks unpersisted coordinate or scale edits queued for debounced disk write.
    pub(crate) dirty_save: bool,

    // Active `calloop` animation timer waiting for the next frame deadline or recovery retry.
    pub(crate) frame_timer_token: Option<RegistrationToken>,

    // Active `500 ms` debounced config save timer registration.
    pub(crate) save_timer_token: Option<RegistrationToken>,

    // Active `2000 ms` handle idle auto-fade timer registration.
    pub(crate) fade_timer_token: Option<RegistrationToken>,

    // Active 1-second periodic `--stats` stdout telemetry timer registration.
    pub(crate) stats_timer_token: Option<RegistrationToken>,

    // Termination flag triggered by signals, `q`, middle-click, or compositor layer close.
    pub(crate) exit_requested: bool,
}

impl OverlayRuntime
{
    /// Arms or replaces the one-shot animation frame timer for `delay`.
    pub(crate) fn schedule_frame_timer(&mut self, delay: Duration)
    {
        arm_timer(
            &self.loop_handle,
            &mut self.frame_timer_token,
            delay,
            Self::on_frame_timer,
        );
    }

    /// Cancels an active animation wait or recovery timer if armed.
    pub(crate) fn cancel_frame_timer(&mut self)
    {
        clear_timer(&self.loop_handle, &mut self.frame_timer_token);
    }

    /// Arms or resets the 2-second idle timer hiding the corner handle visual cue.
    pub(crate) fn schedule_fade_timer(&mut self)
    {
        arm_timer(
            &self.loop_handle,
            &mut self.fade_timer_token,
            Duration::from_millis(HANDLE_FADE_MS),
            Self::on_fade_timer,
        );
    }

    /// Cancels an active corner-handle fade timer if armed.
    pub(crate) fn cancel_fade_timer(&mut self)
    {
        clear_timer(&self.loop_handle, &mut self.fade_timer_token);
    }

    /// Cancels an active state persistence debounce timer if armed.
    pub(crate) fn cancel_save_timer(&mut self)
    {
        clear_timer(&self.loop_handle, &mut self.save_timer_token);
    }

    /// Arms or resets the 500 ms debounce timer before persisting overlay coordinates and scale.
    pub(crate) fn schedule_save_timer(&mut self)
    {
        arm_timer(
            &self.loop_handle,
            &mut self.save_timer_token,
            Duration::from_millis(SAVE_DEBOUNCE_MS),
            Self::on_save_timer,
        );
    }

    /// Arms or re-arms the 1-second periodic `--stats` reporter when telemetry is enabled.
    pub(crate) fn schedule_stats_timer_if_enabled(&mut self)
    {
        if self.stats.is_none()
        {
            return;
        }
        arm_timer(
            &self.loop_handle,
            &mut self.stats_timer_token,
            Duration::from_millis(STATS_INTERVAL_MS),
            Self::on_stats_timer,
        );
    }

    /// Cancels an active `--stats` reporting timer if armed.
    pub(crate) fn cancel_stats_timer(&mut self)
    {
        clear_timer(&self.loop_handle, &mut self.stats_timer_token);
    }

    /// Emits a 1-second `--stats` summary line to stdout and reschedules the next sample.
    fn on_stats_timer(&mut self)
    {
        self.stats_timer_token = None;
        let now = Instant::now();
        let snap = self.collect_stats_snapshot();
        if let Some(stats) = &mut self.stats
        {
            if let Some(msg) =
                stats.check_decode_completion(now, snap.frame_count, snap.decode_done)
            {
                println!("{msg}");
            }
            let report = stats.take_interval_report(now, snap);
            println!("{report}");
        }
        self.schedule_stats_timer_if_enabled();
    }

    /// Captures live CPU, overall RAM, decode, and cache byte counts without blocking decoding.
    fn collect_stats_snapshot(&self) -> StatsSnapshot
    {
        let (frame_count, decode_done, fits_source_ram) = self.cache.cache_profile();
        let (display_cache_active, display_cache_bytes) = self
            .scene
            .as_ref()
            .map_or((false, 0), OverlayScene::display_cache_stats);

        StatsSnapshot {
            frame_count,
            decode_done,
            paused: self.playback.is_paused(),
            fits_source_ram,
            ram_used_bytes: self.cache.ram_used_bytes(),
            display_cache_active,
            display_cache_bytes,
            cpu_time: OverlayStats::sample_cpu_time(),
            logical_cpus: OverlayStats::logical_cpu_count(),
            process_ram_bytes: OverlayStats::sample_process_ram_bytes(),
        }
    }

    /// Advances animation playback once the scheduled wait or error-recovery timer fires.
    fn on_frame_timer(&mut self)
    {
        self.frame_timer_token = None;
        self.tick_playback();
    }

    /// Hides the corner handle when its 2-second idle timer expires while unhovered.
    fn on_fade_timer(&mut self)
    {
        self.fade_timer_token = None;
        let effects = self.input.on_fade_timeout();
        self.apply_input_effects(effects);
    }

    /// Flushes dirty geometry edits once the 500 ms debounce timer expires.
    fn on_save_timer(&mut self)
    {
        self.save_timer_token = None;
        self.flush_pending_save();
    }

    /// Writes pending state edits to `~/.config/apng-overlay.json` under exclusive `flock`.
    pub(crate) fn flush_pending_save(&mut self)
    {
        if !self.dirty_save
        {
            return;
        }
        self.dirty_save = false;

        let current_state = self.layout.state();
        if let Err(err) = self
            .config_store
            .save_state_entry(&self.config_key, current_state)
        {
            tracing::warn!(error = %err, "Failed saving overlay state");
        }
    }

    /// Updates cursor shape on the active pointer device when it changes or upon re-entering.
    pub(crate) fn update_pointer_cursor(&mut self, pos: (f64, f64))
    {
        let desired = self.input.desired_cursor(pos, &self.layout);
        if self.last_cursor == Some(desired)
        {
            return;
        }
        let Some(themed) = &self.themed_pointer
        else
        {
            return;
        };
        let icon = match desired
        {
            PointerCursor::Default => CursorIcon::Default,
            PointerCursor::Grab => CursorIcon::Grab,
            PointerCursor::Grabbing => CursorIcon::Grabbing,
            PointerCursor::ResizeBottomRight => CursorIcon::NwseResize,
        };
        match themed.set_cursor(&self.conn, icon)
        {
            Ok(()) => self.last_cursor = Some(desired),
            Err(err) => tracing::warn!(error = %err, ?icon, "Failed updating pointer cursor"),
        }
    }

    /// Flushes unsaved geometry, aborts background decoders cleanly, and removes active timers.
    pub(crate) fn shutdown_cleanup(&mut self)
    {
        self.cancel_frame_timer();
        self.cancel_fade_timer();
        self.cancel_save_timer();
        self.cancel_stats_timer();
        self.flush_pending_save();
        self.cache.stop();
        tracing::info!("Wayland overlay shutdown cleanup complete");
    }
}

/// Replaces any timer registered under `slot` with a one-shot timeout calling `on_fire`.
fn arm_timer(
    loop_handle: &LoopHandle<'static, OverlayRuntime>,
    slot: &mut Option<RegistrationToken>,
    delay: Duration,
    on_fire: fn(&mut OverlayRuntime),
)
{
    clear_timer(loop_handle, slot);
    let timer = Timer::from_duration(delay);
    if let Ok(token) = loop_handle.insert_source(timer, move |_, _, state| {
        on_fire(state);
        TimeoutAction::Drop
    })
    {
        *slot = Some(token);
    }
}

/// Removes an optional scheduled `calloop` timer token cleanly from the event loop.
fn clear_timer(
    loop_handle: &LoopHandle<'static, OverlayRuntime>,
    slot: &mut Option<RegistrationToken>,
)
{
    if let Some(token) = slot.take()
    {
        loop_handle.remove(token);
    }
}
