//! Runtime state coordinator managing timers, cursor updates, debounced saves, and cleanup.

// Monotonic timing and timeouts for debounce and handle fade timers.
use std::time::{Duration, Instant};

// `calloop` event loop handle and timer sources.
use calloop::{
    LoopHandle, RegistrationToken,
    timer::{TimeoutAction, Timer},
};
// SCTK registry, compositor, outputs, seat devices, SHM, and themed pointer cursor support.
use smithay_client_toolkit::{
    compositor::{CompositorState, FrameCallbackData},
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

// Frame cache, config store, layout math, playback clock, input state, and Wayland scene.
use super::{
    input::{HANDLE_FADE_MS, InputEffects, InputState, PointerCursor, SAVE_DEBOUNCE_MS},
    scene::OverlayScene,
};
use crate::{
    cache::FrameCache,
    config::ConfigStore,
    layout::OverlayLayout,
    playback::{PlaybackClock, TickOutcome},
};

// Recovery delay in milliseconds after a cache read or SHM frame upload error.
const FRAME_ERROR_RETRY_MS: u64 = 100;

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

    // Termination flag triggered by signals, `q`, middle-click, or compositor layer close.
    pub(crate) exit_requested: bool,
}

impl OverlayRuntime
{
    /// Requests a Wayland frame callback on `image_surface` and commits immediately.
    pub(crate) fn queue_image_frame_callback(&mut self)
    {
        if let Some(scene) = &self.scene
        {
            self.frame_callback_pending = true;
            scene
                .image_surface
                .frame(&self.qh, FrameCallbackData(scene.image_surface.clone()));
            scene.image_surface.commit();
        }
    }

    /// Starts or resumes timer-paced animation ticks when configured and unpaused.
    pub(crate) fn schedule_playback_if_needed(&mut self)
    {
        if !self.configured || self.frame_callback_pending || self.playback.is_paused()
        {
            return;
        }

        self.tick_playback();
    }

    /// Advances playback with borrowed cache delays and presents or arms the next wait timer.
    pub(crate) fn tick_playback(&mut self)
    {
        self.cancel_frame_timer();
        if !self.configured || self.frame_callback_pending
        {
            return;
        }

        let now_ms = self.start_instant.elapsed().as_secs_f64() * 1000.0;
        let (outcome, wait_ms) = self.cache.with_delays(|delays, decode_done| {
            let outcome = self.playback.advance(now_ms, delays, decode_done);
            let wait_ms = if matches!(outcome, TickOutcome::NoChange)
            {
                self.playback.ms_until_next_frame(now_ms)
            }
            else
            {
                None
            };
            (outcome, wait_ms)
        });

        match outcome
        {
            TickOutcome::StopCallbacks =>
            {}
            TickOutcome::FrameChanged(idx) => self.present_frame_at(idx),
            TickOutcome::NoChange =>
            {
                if let Some(delay_ms) = wait_ms
                {
                    self.schedule_frame_timer(delay_ms.max(1));
                }
            }
        }
    }

    /// Fetches frame `idx` from display cache or `FrameCache`, uploads it to SHM, and requests callback.
    fn present_frame_at(&mut self, idx: usize)
    {
        let Some(scene) = &mut self.scene
        else
        {
            return;
        };

        let staged = scene.stage_frame_cached(&self.shm, &self.layout, &self.cache, idx);

        match staged
        {
            Ok(_retired_ring) => self.queue_image_frame_callback(),
            Err(err) =>
            {
                tracing::warn!(error = %err, frame_index = idx, "Failed presenting frame");
                self.schedule_frame_timer(FRAME_ERROR_RETRY_MS);
            }
        }
    }

    /// Resamples and atomically commits the current playback frame for updated orientation or scale.
    pub(crate) fn refresh_active_oriented_frame(&mut self)
    {
        let Some(scene) = &mut self.scene
        else
        {
            return;
        };

        let frame_idx = self.playback.current_index();
        let refreshed = scene.refresh_frame_cached(
            &self.compositor,
            &self.shm,
            &self.layout,
            &self.input,
            &self.cache,
            frame_idx,
        );
        if let Err(err) = refreshed
        {
            tracing::warn!(
                error = %err,
                frame_index = frame_idx,
                "Failed refreshing rotated Wayland frame"
            );
        }
    }

    /// Cancels or schedules the `500 ms` debounced config save and `2000 ms` handle fade timer.
    pub(crate) fn apply_input_effects(&mut self, effects: InputEffects)
    {
        if effects.quit
        {
            self.exit_requested = true;
            return;
        }

        if let Some(editing) = effects.edit_mode_changed
        {
            tracing::debug!(editing, "Toggled edit mode");
        }

        self.sync_scene_for_effects(effects);
        self.sync_timers_for_effects(effects);

        if effects.toggle_pause
        {
            self.apply_pause_toggle();
        }
    }

    /// Synchronizes surface layout, resampled frame buffers, or corner handle opacity after input.
    fn sync_scene_for_effects(&mut self, effects: InputEffects)
    {
        let needs_redraw = effects.frame_redraw_needed
            || (effects.layout_changed
                && !self.input.is_resizing()
                && self
                    .scene
                    .as_ref()
                    .is_some_and(|scene| scene.needs_buffer_resync(&self.layout, &self.cache)));
        if needs_redraw
        {
            self.refresh_active_oriented_frame();
            return;
        }

        let Some(scene) = &mut self.scene
        else
        {
            return;
        };

        if effects.layout_changed || effects.edit_mode_changed.is_some()
        {
            if let Err(err) =
                scene.sync_layout(&self.compositor, &self.shm, &self.layout, &self.input)
            {
                tracing::warn!(error = %err, "Failed syncing Wayland overlay layout");
            }
            return;
        }

        if effects.handle_visual_changed
            && let Err(err) = scene.sync_handle_only(&self.layout, &self.input)
        {
            tracing::warn!(error = %err, "Failed syncing corner handle visuals");
        }
    }

    /// Arms or cancels the handle auto-fade timer and debounced config write timer.
    fn sync_timers_for_effects(&mut self, effects: InputEffects)
    {
        if effects.cancel_fade_timer
        {
            self.cancel_fade_timer();
        }
        else if effects.reset_fade_timer
        {
            self.schedule_fade_timer();
        }

        if effects.schedule_save
        {
            self.dirty_save = true;
            self.schedule_save_timer();
        }
    }

    /// Toggles animation pause state and arms or cancels frame wakeup timers accordingly.
    fn apply_pause_toggle(&mut self)
    {
        let paused = self.playback.toggle_pause();
        tracing::debug!(paused, "Toggled playback pause");
        if paused
        {
            self.cancel_frame_timer();
        }
        else
        {
            self.schedule_playback_if_needed();
        }
    }

    /// Arms or replaces the one-shot animation frame timer for `delay_ms` milliseconds.
    pub(crate) fn schedule_frame_timer(&mut self, delay_ms: u64)
    {
        arm_timer(
            &self.loop_handle,
            &mut self.frame_timer_token,
            delay_ms,
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
            HANDLE_FADE_MS,
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
            SAVE_DEBOUNCE_MS,
            Self::on_save_timer,
        );
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
        self.flush_pending_save();
        self.cache.stop();
        tracing::info!("Wayland overlay shutdown cleanup complete");
    }
}

/// Replaces any timer registered under `slot` with a one-shot timeout calling `on_fire`.
fn arm_timer(
    loop_handle: &LoopHandle<'static, OverlayRuntime>,
    slot: &mut Option<RegistrationToken>,
    delay_ms: u64,
    on_fire: fn(&mut OverlayRuntime),
)
{
    clear_timer(loop_handle, slot);
    let timer = Timer::from_duration(Duration::from_millis(delay_ms));
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
