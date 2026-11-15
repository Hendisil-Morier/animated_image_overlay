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

/// Direction of a discrete 90-degree overlay rotation step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotateDirection
{
    /// Turn 90 degrees clockwise around the overlay center.
    Clockwise,

    /// Turn 90 degrees counter-clockwise around the overlay center.
    CounterClockwise,
}

impl RotateDirection
{
    /// Returns the opposite quarter-turn direction (`Clockwise` <-> `CounterClockwise`).
    pub const fn reversed(self) -> Self
    {
        match self
        {
            Self::Clockwise => Self::CounterClockwise,
            Self::CounterClockwise => Self::Clockwise,
        }
    }
}

/// Visual mirror axis applied across the displayed overlay bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlipAxis
{
    /// Mirror left-to-right across the vertical screen midline (`f`).
    Horizontal,

    /// Mirror top-to-bottom across the horizontal screen midline (`F`).
    Vertical,
}

/// Clockwise 90-degree orientation applied visually to the overlay surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rotation
{
    /// Default unrotated orientation (`0` degrees).
    #[default]
    Deg0,

    /// Rotated `90` degrees clockwise (swaps width and height).
    Deg90,

    /// Rotated `180` degrees (preserves original width and height).
    Deg180,

    /// Rotated `270` degrees clockwise (`90` counter-clockwise; swaps width and height).
    Deg270,
}

impl Rotation
{
    /// Advances orientation by one quarter turn in `direction`.
    pub const fn rotated(self, direction: RotateDirection) -> Self
    {
        match (self, direction)
        {
            (Self::Deg0, RotateDirection::Clockwise) => Self::Deg90,
            (Self::Deg90, RotateDirection::Clockwise) => Self::Deg180,
            (Self::Deg180, RotateDirection::Clockwise) => Self::Deg270,
            (Self::Deg270, RotateDirection::Clockwise) => Self::Deg0,
            (Self::Deg0, RotateDirection::CounterClockwise) => Self::Deg270,
            (Self::Deg270, RotateDirection::CounterClockwise) => Self::Deg180,
            (Self::Deg180, RotateDirection::CounterClockwise) => Self::Deg90,
            (Self::Deg90, RotateDirection::CounterClockwise) => Self::Deg0,
        }
    }

    /// Advances orientation by 180 degrees (`Deg0` <-> `Deg180`, `Deg90` <-> `Deg270`).
    pub const fn rotated_half_turn(self) -> Self
    {
        match self
        {
            Self::Deg0 => Self::Deg180,
            Self::Deg180 => Self::Deg0,
            Self::Deg90 => Self::Deg270,
            Self::Deg270 => Self::Deg90,
        }
    }

    /// Returns `(width, height)` oriented for this rotation (`Deg90` and `Deg270` swap axes).
    pub const fn orient_dimensions(self, width: u32, height: u32) -> (u32, u32)
    {
        match self
        {
            Self::Deg0 | Self::Deg180 => (width, height),
            Self::Deg90 | Self::Deg270 => (height, width),
        }
    }

    /// Returns the clockwise quarter-turn angle in degrees (`0`, `90`, `180`, or `270`).
    pub const fn degrees(self) -> u16
    {
        match self
        {
            Self::Deg0 => 0,
            Self::Deg90 => 90,
            Self::Deg180 => 180,
            Self::Deg270 => 270,
        }
    }

    /// Converts an exact clockwise quarter-turn degree value (`0`, `90`, `180`, or `270`).
    pub const fn from_degrees(degrees: u64) -> Option<Self>
    {
        match degrees
        {
            0 => Some(Self::Deg0),
            90 => Some(Self::Deg90),
            180 => Some(Self::Deg180),
            270 => Some(Self::Deg270),
            _ => None,
        }
    }
}

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

    // Current position, scale, and 90-degree orientation subject to clamping.
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

    /// Returns the stored unrotated base frame width and height in pixels.
    pub fn frame_size(&self) -> (u32, u32)
    {
        (self.frame_width, self.frame_height)
    }

    /// Returns frame width and height swapped when rotated 90 or 270 degrees.
    pub fn oriented_frame_size(&self) -> (u32, u32)
    {
        self.state
            .rotation
            .orient_dimensions(self.frame_width, self.frame_height)
    }

    /// Returns the active 90-degree visual rotation.
    pub fn rotation(&self) -> Rotation
    {
        self.state.rotation
    }

    /// Returns whether the overlay is horizontally mirrored after rotating by `rotation()`.
    pub fn flipped(&self) -> bool
    {
        self.state.flipped
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

    /// Computes truncated logical display dimensions matching oriented `frame * scale`.
    pub fn overlay_size(&self) -> (u32, u32)
    {
        let (fw, fh) = self.oriented_frame_size();
        let width = ((f64::from(fw) * self.state.scale) as u32).max(1);
        let height = ((f64::from(fh) * self.state.scale) as u32).max(1);
        (width, height)
    }

    /// Clamps `scale` within `[min_scale, max_scale]` and `(x, y)` inside monitor bounds.
    pub fn clamp(&mut self)
    {
        let (eff_w, eff_h) = self.oriented_frame_size();
        let fw = f64::from(eff_w);
        let fh = f64::from(eff_h);
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
        let (eff_w, _) = self.oriented_frame_size();
        let w0 = f64::from(eff_w) * start_scale;
        if w0 > 0.0 && w0.is_finite()
        {
            self.state.scale = start_scale * (w0 + dx) / w0;
        }
        self.clamp();
    }

    /// Rotates the overlay visually by 90 degrees while keeping its center stationary inside bounds.
    pub fn rotate(&mut self, direction: RotateDirection)
    {
        let (old_w, old_h) = self.overlay_size();
        let cx = self.state.x + f64::from(old_w) * 0.5;
        let cy = self.state.y + f64::from(old_h) * 0.5;

        // Rotating a horizontally reflected view flips chirality under dihedral composition.
        let effective_dir = if self.state.flipped
        {
            direction.reversed()
        }
        else
        {
            direction
        };
        self.state.rotation = self.state.rotation.rotated(effective_dir);
        self.clamp();

        let (new_w, new_h) = self.overlay_size();
        self.state.x = cx - f64::from(new_w) * 0.5;
        self.state.y = cy - f64::from(new_h) * 0.5;
        self.clamp();
    }

    /// Mirrors the visual overlay across `axis` while leaving bounding box coordinates unchanged.
    pub fn flip(&mut self, axis: FlipAxis)
    {
        self.state.flipped = !self.state.flipped;
        if axis == FlipAxis::Vertical
        {
            self.state.rotation = self.state.rotation.rotated_half_turn();
        }
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
