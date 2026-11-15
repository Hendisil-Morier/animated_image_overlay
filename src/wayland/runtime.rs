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
    cache::FrameCache, config::ConfigStore, layout::OverlayLayout, playback::PlaybackClock,
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

    // Cursor image surface bound to `ThemedPointer`.
    pub(crate) cursor_surface: Option<WlSurface>,

    // System cursor theme pointer handle when pointer capability is advertised.
    pub(crate) themed_pointer: Option<ThemedPointer<()>>,

    // Active Wayland keyboard object while seat capability is present.
    pub(crate) keyboard: Option<WlKeyboard>,

    // Locked JSON state persistence store (`~/.config/apng-overlay.json`).
    pub(crate) config_store: ConfigStore,

    // Canonical absolute file key identifying this image in configuration storage.
    pub(crate) config_key: String,

    // LZ4 disk cache and 128 MiB RAM LRU store feeding rendered animation frames.
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

    // Active `500 ms` debounced config save timer registration.
    pub(crate) save_timer_token: Option<RegistrationToken>,

    // Active `2000 ms` handle idle auto-fade timer registration.
    pub(crate) fade_timer_token: Option<RegistrationToken>,

    // Termination flag triggered by signals, `q`, middle-click, or compositor layer close.
    pub(crate) exit_requested: bool,
}

impl OverlayRuntime
{
    /// Requests a Wayland frame callback on `image_surface` when animation or streaming needs ticks.
    pub(crate) fn ensure_frame_callback_if_needed(&mut self)
    {
        if !self.configured || self.frame_callback_pending || self.playback.is_paused()
        {
            return;
        }

        let (delays, decode_done) = self.cache.delays_snapshot();
        if delays.len() <= 1 && decode_done
        {
            return;
        }

        if let Some(scene) = &self.scene
        {
            self.frame_callback_pending = true;
            scene
                .image_surface
                .frame(&self.qh, FrameCallbackData(scene.image_surface.clone()));
            scene.image_surface.commit();
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

        if let Some(scene) = &mut self.scene
        {
            if effects.layout_changed || effects.edit_mode_changed.is_some()
            {
                if let Err(err) = scene.sync_layout(&self.compositor, &self.layout, &self.input)
                {
                    tracing::warn!(error = %err, "Failed syncing Wayland overlay layout");
                }
            }
            else if effects.handle_visual_changed
                && let Err(err) = scene.sync_handle_only(&self.layout, &self.input)
            {
                tracing::warn!(error = %err, "Failed syncing corner handle visuals");
            }
        }

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

        if effects.toggle_pause
        {
            let paused = self.playback.toggle_pause();
            tracing::debug!(paused, "Toggled playback pause");
            if !paused
            {
                self.ensure_frame_callback_if_needed();
            }
        }
    }

    /// Arms or resets the 2-second idle timer hiding the corner handle visual cue.
    pub(crate) fn schedule_fade_timer(&mut self)
    {
        self.cancel_fade_timer();
        let timer = Timer::from_duration(Duration::from_millis(HANDLE_FADE_MS));
        if let Ok(token) = self.loop_handle.insert_source(timer, |_, _, state| {
            state.fade_timer_token = None;
            let effects = state.input.on_fade_timeout();
            state.apply_input_effects(effects);
            TimeoutAction::Drop
        })
        {
            self.fade_timer_token = Some(token);
        }
    }

    /// Cancels an active corner-handle fade timer if armed.
    pub(crate) fn cancel_fade_timer(&mut self)
    {
        if let Some(token) = self.fade_timer_token.take()
        {
            self.loop_handle.remove(token);
        }
    }

    /// Arms or resets the 500 ms debounce timer before persisting overlay coordinates and scale.
    pub(crate) fn schedule_save_timer(&mut self)
    {
        if let Some(token) = self.save_timer_token.take()
        {
            self.loop_handle.remove(token);
        }

        let timer = Timer::from_duration(Duration::from_millis(SAVE_DEBOUNCE_MS));
        if let Ok(token) = self.loop_handle.insert_source(timer, |_, _, state| {
            state.save_timer_token = None;
            state.flush_pending_save();
            TimeoutAction::Drop
        })
        {
            self.save_timer_token = Some(token);
        }
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

    /// Updates cursor shape on the active pointer device to match drag/hover state.
    pub(crate) fn update_pointer_cursor(&self, pos: (f64, f64))
    {
        let Some(themed) = &self.themed_pointer
        else
        {
            return;
        };
        let icon = match self.input.desired_cursor(pos, &self.layout)
        {
            PointerCursor::Default => CursorIcon::Default,
            PointerCursor::Grab => CursorIcon::Grab,
            PointerCursor::Grabbing => CursorIcon::Grabbing,
            PointerCursor::ResizeBottomRight => CursorIcon::NwseResize,
        };
        let _ = themed.set_cursor(&self.conn, icon);
    }

    /// Flushes unsaved geometry, aborts background decoders cleanly, and removes active timers.
    pub(crate) fn shutdown_cleanup(&mut self)
    {
        self.cancel_fade_timer();
        if let Some(token) = self.save_timer_token.take()
        {
            self.loop_handle.remove(token);
        }
        self.flush_pending_save();
        self.cache.stop();
        tracing::info!("Wayland overlay shutdown cleanup complete");
    }
}
