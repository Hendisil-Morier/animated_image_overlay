//! Pointer drag/hover state machine transitioning `OverlayLayout` and scheduling UI side effects.

// Keyboard symbols and pointer button/axis constants from SCTK.
use smithay_client_toolkit::seat::{
    keyboard::Keysym,
    pointer::{AxisScroll, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT},
};

// Click tracking, keyboard shortcut lookup table, and scroll step helper.
use super::shortcuts::{
    ClickTracker, KeyShortcut, extract_scroll_steps, map_edit_keysym, within_click_distance,
};
// Layout geometry, dragging, zooming, and nudge step constants.
use crate::layout::{KEYBOARD_NUDGE_PX, KEYBOARD_ZOOM_STEP, OverlayLayout};

/// Debounce delay in milliseconds before flushing moved/resized overlay state to disk.
pub const SAVE_DEBOUNCE_MS: u64 = 500;

/// Idle duration in milliseconds before hiding the corner handle visual affordance.
pub const HANDLE_FADE_MS: u64 = 2000;

/// Active pointer drag operation anchored at initial pointer press coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum DragMode
{
    /// No active mouse drag.
    #[default]
    None,

    /// Moving the overlay via left-button drag from `(start_x, start_y)`.
    Move
    {
        /// Overlay X coordinate recorded on press.
        start_x: f64,

        /// Overlay Y coordinate recorded on press.
        start_y: f64,

        /// Pointer coordinates recorded on press.
        press_pos: (f64, f64),
    },

    /// Resizing the overlay from its top-left anchor via right-button drag.
    Resize
    {
        /// Scale multiplier recorded on press.
        start_scale: f64,

        /// Pointer X coordinate recorded on press.
        press_x: f64,
    },
}

/// Cursor icon hint requested based on current drag, hover, and edit state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerCursor
{
    /// Default pointer shape when idle or outside affordances.
    Default,

    /// Hand affordance inviting drag or click interactions.
    Grab,

    /// Closed hand while actively dragging overlay position.
    Grabbing,

    /// Diagonal resize cursor while right-dragging scale.
    ResizeBottomRight,
}

/// Side-effect flags returned by pointer or keyboard state transitions for the Wayland loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InputEffects
{
    /// Overlay position, scale, or input region changed and must be re-applied to surfaces.
    pub layout_changed: bool,

    /// Edit mode transitioned (`Some(true)` entered, `Some(false)` exited).
    pub edit_mode_changed: Option<bool>,

    /// Handle hover/visibility state changed and needs buffer/surface refresh.
    pub handle_visual_changed: bool,

    /// Handle fade timer should be reset (`!editing && !dragging && !hovered`) or cancelled.
    pub reset_fade_timer: bool,

    /// Cancel any active fade timer immediately because user is hovering/dragging/editing.
    pub cancel_fade_timer: bool,

    /// Persist overlay state after `SAVE_DEBOUNCE_MS`.
    pub schedule_save: bool,

    /// Pause or resume animation playback callbacks.
    pub toggle_pause: bool,

    /// Terminate the overlay cleanly.
    pub quit: bool,
}

/// Pure pointer + keyboard interaction state driving handle hover, drags, scroll, and editing.
#[derive(Debug, Clone, PartialEq)]
pub struct InputState
{
    /// Whether full-overlay edit mode and keyboard focus (`OnDemand`) are active.
    pub editing: bool,

    /// Whether cursor currently rests inside the overlay input region.
    pub hovered: bool,

    /// Whether the 28x28 corner handle is visibly displayed (initially visible before fade).
    pub handle_visible: bool,

    /// Current pointer drag gesture state.
    pub drag: DragMode,

    /// Triple-click detector toggling `editing`.
    pub click_tracker: ClickTracker,
}

impl Default for InputState
{
    /// Starts in click-through passive mode with handle visible until `HANDLE_FADE_MS` elapses.
    fn default() -> Self
    {
        Self {
            editing: false,
            hovered: false,
            handle_visible: true,
            drag: DragMode::None,
            click_tracker: ClickTracker::default(),
        }
    }
}

impl InputState
{
    /// Returns `true` while an active mouse move or resize drag is in progress.
    pub fn is_dragging(&self) -> bool
    {
        !matches!(self.drag, DragMode::None)
    }

    /// Determines desired pointer cursor shape at `pos` for UX clarity.
    pub fn desired_cursor(&self, pos: (f64, f64), layout: &OverlayLayout) -> PointerCursor
    {
        match self.drag
        {
            DragMode::Move { .. } => PointerCursor::Grabbing,
            DragMode::Resize { .. } => PointerCursor::ResizeBottomRight,
            DragMode::None =>
            {
                if self.editing || layout.handle_contains(pos.0, pos.1)
                {
                    PointerCursor::Grab
                }
                else
                {
                    PointerCursor::Default
                }
            }
        }
    }

    /// Handles pointer entering the overlay surface input region.
    pub fn on_pointer_enter(&mut self) -> InputEffects
    {
        self.hovered = true;
        self.show_handle_effects()
    }

    /// Handles pointer leaving the surface, clearing hover while preserving drag if active.
    pub fn on_pointer_leave(&mut self) -> InputEffects
    {
        self.hovered = false;
        self.click_tracker.reset();
        self.sync_fade_timer(false)
    }

    /// Clears hover, active drag, and click state when `Capability::Pointer` is detached.
    pub fn on_pointer_detach(&mut self) -> InputEffects
    {
        let was_dragging = self.is_dragging();
        self.hovered = false;
        self.drag = DragMode::None;
        self.click_tracker.reset();
        let mut effects = self.sync_fade_timer(false);
        effects.schedule_save = was_dragging;
        effects
    }

    /// Handles pointer motion: drives active move/resize drags and refreshes handle visibility.
    pub fn on_pointer_motion(&mut self, pos: (f64, f64), layout: &mut OverlayLayout)
    -> InputEffects
    {
        let mut effects = self.show_handle_effects();

        match self.drag
        {
            DragMode::Move {
                start_x,
                start_y,
                press_pos,
            } =>
            {
                let dx = pos.0 - press_pos.0;
                let dy = pos.1 - press_pos.1;
                if !within_click_distance(pos, press_pos)
                {
                    self.click_tracker.reset();
                }
                layout.on_move(start_x, start_y, dx, dy);
                effects.layout_changed = true;
            }
            DragMode::Resize {
                start_scale,
                press_x,
            } =>
            {
                let dx = pos.0 - press_x;
                layout.on_resize(start_scale, dx);
                effects.layout_changed = true;
            }
            DragMode::None =>
            {}
        }

        effects
    }

    /// Handles pointer button presses (triple-click edit toggle, move, resize, or middle-click quit).
    pub fn on_button_press(
        &mut self,
        button: u32,
        time_ms: u32,
        pos: (f64, f64),
        layout: &OverlayLayout,
    ) -> InputEffects
    {
        let mut effects = self.show_handle_effects();
        if self.is_dragging() && matches!(button, BTN_LEFT | BTN_RIGHT)
        {
            self.click_tracker.reset();
            return self.merge_timer_effects(effects);
        }

        let state = layout.state();

        match button
        {
            BTN_LEFT =>
            {
                if self.click_tracker.register_left_press(time_ms, pos)
                {
                    self.drag = DragMode::None;
                    self.editing = !self.editing;
                    effects.edit_mode_changed = Some(self.editing);
                    effects.layout_changed = true;
                    return self.merge_timer_effects(effects);
                }

                self.drag = DragMode::Move {
                    start_x: state.x,
                    start_y: state.y,
                    press_pos: pos,
                };
            }
            BTN_RIGHT =>
            {
                self.click_tracker.reset();
                self.drag = DragMode::Resize {
                    start_scale: state.scale,
                    press_x: pos.0,
                };
            }
            BTN_MIDDLE =>
            {
                self.click_tracker.reset();
                if !self.editing || layout.handle_contains(pos.0, pos.1)
                {
                    effects.quit = true;
                }
            }
            _ =>
            {
                self.click_tracker.reset();
            }
        }

        self.merge_timer_effects(effects)
    }

    /// Ends active pointer drags and schedules debounced state persistence.
    pub fn on_button_release(&mut self, button: u32) -> InputEffects
    {
        let was_dragging = matches!(
            (button, self.drag),
            (BTN_LEFT, DragMode::Move { .. }) | (BTN_RIGHT, DragMode::Resize { .. })
        );

        if !was_dragging
        {
            return InputEffects::default();
        }

        self.drag = DragMode::None;
        let mut effects = self.show_handle_effects();
        effects.schedule_save = true;
        effects
    }

    /// Handles vertical scroll wheel zoom (`1.05 ** -dy`) and schedules debounced save.
    pub fn on_scroll(&mut self, vertical: AxisScroll, layout: &mut OverlayLayout) -> InputEffects
    {
        let dy = extract_scroll_steps(vertical);
        if dy.abs() <= f64::EPSILON
        {
            return InputEffects::default();
        }

        layout.on_scroll(dy);
        let mut effects = self.show_handle_effects();
        effects.layout_changed = true;
        effects.schedule_save = true;
        effects
    }

    /// Applies fade timer expiration when passive, unhovered, and not dragging.
    pub fn on_fade_timeout(&mut self) -> InputEffects
    {
        if !self.editing && !self.is_dragging() && !self.hovered && self.handle_visible
        {
            self.handle_visible = false;
            return InputEffects {
                handle_visual_changed: true,
                ..InputEffects::default()
            };
        }
        InputEffects::default()
    }

    /// Executes an edit-mode keyboard shortcut against `layout` when `editing` is active.
    pub fn on_key_press(&mut self, keysym: Keysym, layout: &mut OverlayLayout) -> InputEffects
    {
        self.apply_key_event(keysym, false, layout)
    }

    /// Executes repeatable edit-mode navigation/zoom shortcuts on Wayland key-repeat events.
    pub fn on_key_repeat(&mut self, keysym: Keysym, layout: &mut OverlayLayout) -> InputEffects
    {
        self.apply_key_event(keysym, true, layout)
    }

    /// Dispatches one mapped edit-mode shortcut, filtering non-repeatable actions when `is_repeat`.
    fn apply_key_event(
        &mut self,
        keysym: Keysym,
        is_repeat: bool,
        layout: &mut OverlayLayout,
    ) -> InputEffects
    {
        if !self.editing
        {
            return InputEffects::default();
        }

        let Some(shortcut) = map_edit_keysym(keysym)
        else
        {
            return InputEffects::default();
        };

        if is_repeat && !shortcut.is_repeatable()
        {
            return InputEffects::default();
        }

        let mut effects = InputEffects::default();
        match shortcut
        {
            KeyShortcut::ExitEditMode =>
            {
                self.editing = false;
                self.drag = DragMode::None;
                effects.edit_mode_changed = Some(false);
                effects.layout_changed = true;
                return self.merge_timer_effects(effects);
            }
            KeyShortcut::Quit =>
            {
                effects.quit = true;
            }
            KeyShortcut::Move { dir_x, dir_y } =>
            {
                layout.nudge(
                    f64::from(dir_x) * KEYBOARD_NUDGE_PX,
                    f64::from(dir_y) * KEYBOARD_NUDGE_PX,
                );
                effects.layout_changed = true;
                effects.schedule_save = true;
            }
            KeyShortcut::Scale { steps } =>
            {
                let dy = -f64::from(steps) * KEYBOARD_ZOOM_STEP;
                layout.on_scroll(dy);
                effects.layout_changed = true;
                effects.schedule_save = true;
            }
            KeyShortcut::TogglePause =>
            {
                effects.toggle_pause = true;
            }
        }

        effects
    }

    /// Forces handle visibility and sets appropriate fade-timer scheduling flags.
    fn show_handle_effects(&mut self) -> InputEffects
    {
        let mut effects = InputEffects::default();
        if !self.handle_visible
        {
            self.handle_visible = true;
            effects.handle_visual_changed = true;
        }
        self.merge_timer_effects(effects)
    }

    /// Updates fade timer expectations based on `editing`, `drag`, and `hovered`.
    fn sync_fade_timer(&mut self, visual_changed: bool) -> InputEffects
    {
        let effects = InputEffects {
            handle_visual_changed: visual_changed,
            ..InputEffects::default()
        };
        self.merge_timer_effects(effects)
    }

    /// Populates `reset_fade_timer` vs `cancel_fade_timer` from current interaction state.
    fn merge_timer_effects(&self, mut effects: InputEffects) -> InputEffects
    {
        if !self.editing && !self.is_dragging() && !self.hovered
        {
            effects.reset_fade_timer = true;
            effects.cancel_fade_timer = false;
        }
        else
        {
            effects.cancel_fade_timer = true;
            effects.reset_fade_timer = false;
        }
        effects
    }
}
