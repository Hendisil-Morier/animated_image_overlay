//! Pointer input event translation and side-effect synchronization for `OverlayRuntime`.

// SCTK pointer event descriptors and transition effect flags.
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind};

// Parent runtime coordinator and input side-effect mask.
use super::state::OverlayRuntime;
use crate::{config::AudioPrefs, wayland::input::InputEffects};

// Volume change per `.` / `,` key press (5% of full scale).
const AUDIO_VOLUME_STEP: f64 = 0.05;

impl OverlayRuntime
{
    /// Translates one pointer event into input state transitions and side-effect flags.
    pub(in crate::wayland) fn handle_pointer_event(&mut self, event: &PointerEvent)
    -> InputEffects
    {
        match event.kind
        {
            PointerEventKind::Enter { .. } =>
            {
                tracing::debug!(
                    x = event.position.0,
                    y = event.position.1,
                    editing = self.input.editing,
                    overlay = ?self.layout.overlay_rect(),
                    input = ?self.layout.input_rect(self.input.editing),
                    "Pointer entered overlay surface"
                );
                self.input.on_pointer_enter()
            }
            PointerEventKind::Leave { .. } =>
            {
                self.last_cursor = None;
                tracing::debug!(
                    x = event.position.0,
                    y = event.position.1,
                    "Pointer left overlay surface"
                );
                self.input.on_pointer_leave()
            }
            PointerEventKind::Motion { .. } => self
                .input
                .on_pointer_motion(event.position, &mut self.layout),
            PointerEventKind::Press { time, button, .. } =>
            {
                tracing::debug!(
                    button,
                    x = event.position.0,
                    y = event.position.1,
                    editing = self.input.editing,
                    overlay = ?self.layout.overlay_rect(),
                    "Pointer button pressed"
                );
                self.input
                    .on_button_press(button, time, event.position, &self.layout)
            }
            PointerEventKind::Release { button, .. } =>
            {
                tracing::debug!(
                    button,
                    x = event.position.0,
                    y = event.position.1,
                    overlay = ?self.layout.overlay_rect(),
                    "Pointer button released"
                );
                self.input.on_button_release(button)
            }
            PointerEventKind::Axis { vertical, .. } =>
            {
                self.input.on_scroll(vertical, &mut self.layout)
            }
        }
    }

    /// Applies non-scene effects (`quit`, edit logging, timers, pause) and returns whether shutdown started.
    pub(crate) fn apply_immediate_effects(&mut self, effects: InputEffects) -> bool
    {
        if effects.quit
        {
            self.exit_requested = true;
            return true;
        }

        if let Some(editing) = effects.edit_mode_changed
        {
            tracing::debug!(editing, "Toggled edit mode");
        }

        self.sync_timers_for_effects(effects);
        self.apply_audio_effects(effects);

        if effects.toggle_pause
        {
            self.apply_pause_toggle();
        }

        false
    }

    /// Applies mute and volume key effects to the audio engine and the persisted preferences.
    fn apply_audio_effects(&mut self, effects: InputEffects)
    {
        let Some(audio) = &self.audio
        else
        {
            return;
        };

        if effects.toggle_mute
        {
            self.audio_prefs.muted = !self.audio_prefs.muted;
            audio.set_muted(self.audio_prefs.muted);
        }
        if effects.volume_steps != 0
        {
            let stepped =
                self.audio_prefs.volume + f64::from(effects.volume_steps) * AUDIO_VOLUME_STEP;
            self.audio_prefs.volume = AudioPrefs::clamped_volume(stepped);
            audio.set_volume(self.audio_prefs.volume);
        }
    }

    /// Applies immediate side effects and synchronizes surfaces, buffers, and corner handle.
    pub(crate) fn apply_input_effects(&mut self, effects: InputEffects)
    {
        if self.apply_immediate_effects(effects)
        {
            return;
        }

        self.sync_scene_for_effects(effects);
    }

    /// Synchronizes surface layout, resampled frame buffers, or corner handle opacity after input.
    pub(crate) fn sync_scene_for_effects(&mut self, effects: InputEffects)
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
        if let Some(audio) = &self.audio
        {
            audio.set_paused(paused);
        }
        if paused
        {
            self.cancel_frame_timer();
        }
        else
        {
            self.schedule_playback_if_needed();
        }
    }
}
