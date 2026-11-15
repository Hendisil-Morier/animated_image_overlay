//! Triple-click detector, edit-mode keyboard shortcut lookup table, and scroll normalization.

// Wayland keyboard symbol constants and scroll axis metrics from SCTK.
use smithay_client_toolkit::seat::{keyboard::Keysym, pointer::AxisScroll};

/// Maximum inter-click interval in milliseconds for consecutive triple-click taps.
pub const TRIPLE_CLICK_WINDOW_MS: u32 = 400;

/// Maximum Euclidean pointer travel in logical pixels across triple-click taps.
pub const TRIPLE_CLICK_DISTANCE_PX: f64 = 4.0;

/// Logical shortcut action decoded during edit mode (`KeyboardInteractivity::OnDemand`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyShortcut
{
    /// Leave edit mode (`Esc`), restoring click-through playback.
    ExitEditMode,

    /// Exit the overlay application cleanly (`q` / `Q`).
    Quit,

    /// Nudge overlay position along `(dir_x, dir_y)` by `KEYBOARD_NUDGE_PX` (`Arrows` / `hjkl`).
    Move
    {
        /// Horizontal direction step (`-1`, `0`, or `1`).
        dir_x: i8,

        /// Vertical direction step (`-1`, `0`, or `1`).
        dir_y: i8,
    },

    /// Adjust scale by signed step (`+`/`=` up, `-`/`_` down).
    Scale
    {
        /// Signed zoom step count (`1` enlarges, `-1` shrinks).
        steps: i8,
    },

    /// Toggle animation playback pause/resume (`Space`).
    TogglePause,
}

/// Recognizes consecutive left mouse presses within `400 ms` and `4 px` as triple-click.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ClickTracker
{
    // Current count of consecutive left presses (`0..3`).
    count: u8,

    // Wayland timestamp (ms) of the most recent valid click in the sequence.
    last_time_ms: Option<u32>,

    // Anchor coordinate of the first tap so pointer drift stays within threshold.
    anchor: (f64, f64),
}

impl ClickTracker
{
    /// Registers a left mouse press at `(time_ms, pos)` and returns `true` on the 3rd click.
    pub fn register_left_press(&mut self, time_ms: u32, pos: (f64, f64)) -> bool
    {
        let within_sequence = self.last_time_ms.is_some_and(|prev| {
            let elapsed = time_ms.wrapping_sub(prev);
            let dx = pos.0 - self.anchor.0;
            let dy = pos.1 - self.anchor.1;
            let dist_sq = dx * dx + dy * dy;
            elapsed <= TRIPLE_CLICK_WINDOW_MS
                && dist_sq <= TRIPLE_CLICK_DISTANCE_PX * TRIPLE_CLICK_DISTANCE_PX
        });

        if within_sequence
        {
            self.count += 1;
        }
        else
        {
            self.count = 1;
            self.anchor = pos;
        }

        if self.count >= 3
        {
            self.reset();
            return true;
        }

        self.last_time_ms = Some(time_ms);
        false
    }

    /// Clears partial click progress when pointer moves significantly, drags, or leaves.
    pub fn reset(&mut self)
    {
        self.count = 0;
        self.last_time_ms = None;
    }

    /// Returns the number of accumulated clicks toward triple-click (`0..=2`).
    pub fn pending_count(&self) -> u8
    {
        self.count
    }
}

/// Maps XKB `Keysym` values into supported edit-mode shortcut commands.
pub fn map_edit_keysym(keysym: Keysym) -> Option<KeyShortcut>
{
    match keysym
    {
        Keysym::Escape => Some(KeyShortcut::ExitEditMode),
        Keysym::q | Keysym::Q => Some(KeyShortcut::Quit),
        Keysym::Left | Keysym::h | Keysym::H => Some(KeyShortcut::Move {
            dir_x: -1,
            dir_y: 0,
        }),
        Keysym::Right | Keysym::l | Keysym::L => Some(KeyShortcut::Move { dir_x: 1, dir_y: 0 }),
        Keysym::Up | Keysym::k | Keysym::K => Some(KeyShortcut::Move {
            dir_x: 0,
            dir_y: -1,
        }),
        Keysym::Down | Keysym::j | Keysym::J => Some(KeyShortcut::Move { dir_x: 0, dir_y: 1 }),
        Keysym::plus | Keysym::equal | Keysym::KP_Add => Some(KeyShortcut::Scale { steps: 1 }),
        Keysym::minus | Keysym::underscore | Keysym::KP_Subtract =>
        {
            Some(KeyShortcut::Scale { steps: -1 })
        }
        Keysym::space => Some(KeyShortcut::TogglePause),
        _ => None,
    }
}

/// Normalizes Wayland wheel/touchpad `AxisScroll` metrics into GTK-compatible discrete steps.
pub(crate) fn extract_scroll_steps(scroll: AxisScroll) -> f64
{
    if scroll.discrete != 0
    {
        return f64::from(scroll.discrete);
    }
    if scroll.value120 != 0
    {
        return f64::from(scroll.value120) / 120.0;
    }
    scroll.absolute / 15.0
}
