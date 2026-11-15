//! Overlay positioning, scale clamping, drag/scroll math, and input region geometry.
//!
//! Keeps the rendered widget strictly within the active monitor dimensions while
//! enforcing a minimum `84 px` (`3 * HANDLE_SIZE`) short edge and computing the
//! passive `28x28` corner handle vs full-overlay edit input rectangle.

// Persisted overlay coordinates and scale state.
use crate::config::OverlayState;

/// Corner drag/exit handle width and height in logical pixels.
pub const HANDLE_SIZE: u32 = 28;

/// Smallest permitted overlay dimension on the shorter image axis (`3 * HANDLE_SIZE`).
pub const MIN_OVERLAY_SIZE: u32 = 3 * HANDLE_SIZE;

/// Base exponential zoom multiplier per unit vertical scroll notch (`1.05 ** -dy`).
pub const SCROLL_ZOOM_BASE: f64 = 1.05;

/// Logical pixel step used by keyboard arrow/`hjkl` repositioning in edit mode.
pub const KEYBOARD_NUDGE_PX: f64 = 12.0;

/// Equivalent scroll notches applied per keyboard `+`/`-` scale adjustment in edit mode.
pub const KEYBOARD_ZOOM_STEP: f64 = 2.0;

/// Integer logical pixel rectangle aligned with Wayland surface-local input regions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogicalRect
{
    /// Left edge offset relative to monitor/layer surface origin.
    pub x: i32,

    /// Top edge offset relative to monitor/layer surface origin.
    pub y: i32,

    /// Width in logical pixels.
    pub width: u32,

    /// Height in logical pixels.
    pub height: u32,
}

/// Maintains clamped overlay geometry against frame and monitor bounds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayLayout
{
    // Source frame width at scale 1.0 (minimum 1 px).
    frame_width: u32,

    // Source frame height at scale 1.0 (minimum 1 px).
    frame_height: u32,

    // Active monitor width in logical pixels (minimum 1 px).
    screen_width: u32,

    // Active monitor height in logical pixels (minimum 1 px).
    screen_height: u32,

    // Current position and scale subject to clamping.
    state: OverlayState,
}

impl OverlayLayout
{
    /// Constructs a layout from base image dimensions, monitor dimensions, and initial state.
    pub fn new(
        frame_width: u32,
        frame_height: u32,
        screen_width: u32,
        screen_height: u32,
        state: OverlayState,
    ) -> Self
    {
        let mut layout = Self {
            frame_width: frame_width.max(1),
            frame_height: frame_height.max(1),
            screen_width: screen_width.max(1),
            screen_height: screen_height.max(1),
            state,
        };
        layout.clamp();
        layout
    }

    /// Updates monitor dimensions (e.g. after layer configure) and re-clamps state.
    pub fn set_screen_size(&mut self, width: u32, height: u32)
    {
        self.screen_width = width.max(1);
        self.screen_height = height.max(1);
        self.clamp();
    }

    /// Returns the stored base frame width and height in pixels.
    pub fn frame_size(&self) -> (u32, u32)
    {
        (self.frame_width, self.frame_height)
    }

    /// Returns the active monitor logical dimensions in pixels.
    pub fn screen_size(&self) -> (u32, u32)
    {
        (self.screen_width, self.screen_height)
    }

    /// Returns the current clamped overlay state for persistence or inspection.
    pub fn state(&self) -> OverlayState
    {
        self.state
    }

    /// Computes truncated logical display dimensions matching Python `int(fw * scale)`.
    pub fn overlay_size(&self) -> (u32, u32)
    {
        let width = ((f64::from(self.frame_width) * self.state.scale) as u32).max(1);
        let height = ((f64::from(self.frame_height) * self.state.scale) as u32).max(1);
        (width, height)
    }

    /// Clamps `scale` within `[min_scale, max_scale]` and `(x, y)` inside monitor bounds.
    pub fn clamp(&mut self)
    {
        let fw = f64::from(self.frame_width);
        let fh = f64::from(self.frame_height);
        let sw = f64::from(self.screen_width);
        let sh = f64::from(self.screen_height);

        let max_scale = (sw / fw).min(sh / fh);
        let min_scale = max_scale.min(f64::from(MIN_OVERLAY_SIZE) / fw.min(fh));

        if !self.state.scale.is_finite()
        {
            self.state.scale = OverlayState::default().scale;
        }
        self.state.scale = self.state.scale.clamp(min_scale, max_scale);

        let (w, h) = self.overlay_size();
        let max_x = (sw - f64::from(w)).max(0.0);
        let max_y = (sh - f64::from(h)).max(0.0);

        if !self.state.x.is_finite()
        {
            self.state.x = 0.0;
        }
        if !self.state.y.is_finite()
        {
            self.state.y = 0.0;
        }

        self.state.x = self.state.x.clamp(0.0, max_x);
        self.state.y = self.state.y.clamp(0.0, max_y);
    }

    /// Applies pointer drag translation relative to `(start_x, start_y)` and re-clamps.
    pub fn on_move(&mut self, start_x: f64, start_y: f64, dx: f64, dy: f64)
    {
        self.state.x = start_x + dx;
        self.state.y = start_y + dy;
        self.clamp();
    }

    /// Applies right-drag horizontal scaling anchored at the top-left corner and re-clamps.
    pub fn on_resize(&mut self, start_scale: f64, dx: f64)
    {
        let w0 = f64::from(self.frame_width) * start_scale;
        if w0 > 0.0 && w0.is_finite()
        {
            self.state.scale = start_scale * (w0 + dx) / w0;
        }
        self.clamp();
    }

    /// Applies vertical scroll wheel zooming (`scale *= 1.05 ** -dy`) and re-clamps.
    pub fn on_scroll(&mut self, dy: f64)
    {
        if dy.is_finite()
        {
            self.state.scale *= SCROLL_ZOOM_BASE.powf(-dy);
        }
        self.clamp();
    }

    /// Nudges overlay coordinates by `(dx, dy)` logical pixels (for keyboard shortcuts).
    pub fn nudge(&mut self, dx: f64, dy: f64)
    {
        let start_x = self.state.x;
        let start_y = self.state.y;
        self.on_move(start_x, start_y, dx, dy);
    }

    /// Returns the Wayland input-region rectangle for either edit mode or passive handle mode.
    pub fn input_rect(&self, editing: bool) -> LogicalRect
    {
        let (width, height) = self.overlay_size();
        let x = self.state.x as i32;
        let y = self.state.y as i32;

        if editing
        {
            return LogicalRect {
                x,
                y,
                width,
                height,
            };
        }

        let handle_w = HANDLE_SIZE.min(width);
        let handle_h = HANDLE_SIZE.min(height);
        LogicalRect {
            x: x + (width - handle_w) as i32,
            y: y + (height - handle_h) as i32,
            width: handle_w,
            height: handle_h,
        }
    }

    /// Returns the full overlay bounds rectangle for child subsurface placement and viewport sizing.
    pub fn overlay_rect(&self) -> LogicalRect
    {
        self.input_rect(true)
    }

    /// Checks whether a surface-local coordinate `(px, py)` hits the active bottom-right corner handle.
    pub fn handle_contains(&self, px: f64, py: f64) -> bool
    {
        let rect = self.input_rect(false);
        let rx = f64::from(rect.x);
        let ry = f64::from(rect.y);
        let rw = f64::from(rect.width);
        let rh = f64::from(rect.height);

        px >= rx && px < rx + rw && py >= ry && py < ry + rh
    }
}
