//! Headless corner handle and dashed edit-border geometry math and `ARGB8888` software rasterizers.

// Corner handle visibility state machine and 4-byte pixel stride constant.
use super::input::InputState;
use crate::{decode::BYTES_PER_PIXEL, layout::HANDLE_SIZE};

// Corner radius of the rendered 28x28 corner handle matching CSS `border-radius: 8px`.
const HANDLE_CORNER_RADIUS: u32 = 8;

/// Faded resting handle opacity byte (`~0.15 * 255`) kept visible after the 2-second fade timeout.
pub const HANDLE_FADED_ALPHA: u8 = 38;

/// Idle handle opacity byte (`~0.25 * 255`) shown before fade and in edit mode.
pub const HANDLE_IDLE_ALPHA: u8 = 64;

/// Hovered handle opacity byte (`~0.85 * 255`) shown under pointer hover or drag.
pub const HANDLE_HOVER_ALPHA: u8 = 217;

/// Selects the corner handle opacity byte matching `input` visibility, edit, hover, and drag states.
pub fn handle_opacity_alpha(input: &InputState) -> u8
{
    if input.hovered || input.is_dragging()
    {
        HANDLE_HOVER_ALPHA
    }
    else if input.handle_visible || input.editing
    {
        HANDLE_IDLE_ALPHA
    }
    else
    {
        HANDLE_FADED_ALPHA
    }
}

/// Thickness in logical pixels of each edit-mode dashed border subsurface.
pub(crate) const OUTLINE_THICKNESS: i32 = 1;

// Dash stroke length in logical pixels (6px solid, 6px transparent).
const OUTLINE_DASH_PERIOD: u32 = 12;

/// Edit-mode dashed outline opacity byte (~0.55 * 255).
pub(crate) const OUTLINE_ALPHA: u8 = 140;

// Extra headroom bytes reserved alongside handle and edge buffers inside `OverlayChrome`'s pool.
const CHROME_POOL_HEADROOM_BYTES: usize = 4096;

/// Physical buffer geometry derived from an integer compositor scale factor (`>= 1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChromeGeometry
{
    /// Effective integer scale factor (`>= 1`).
    pub scale: i32,

    /// Corner handle side length in buffer pixels (`28 * scale`).
    pub handle_px: u32,

    /// Corner handle pill radius in buffer pixels (`8 * scale`).
    pub handle_radius_px: u32,

    /// Outline border strip thickness in buffer pixels (`1 * scale`).
    pub outline_thickness_px: i32,

    /// Dash pattern full cycle length in buffer pixels (`12 * scale`).
    pub dash_period_px: u32,
}

impl ChromeGeometry
{
    /// Computes physical chrome dimensions for compositor `scale` (clamped to `>= 1`).
    pub fn for_scale(scale: i32) -> Self
    {
        let clamped = scale.max(1);
        let factor = clamped as u32;
        Self {
            scale: clamped,
            handle_px: HANDLE_SIZE * factor,
            handle_radius_px: HANDLE_CORNER_RADIUS * factor,
            outline_thickness_px: OUTLINE_THICKNESS * clamped,
            dash_period_px: OUTLINE_DASH_PERIOD * factor,
        }
    }

    /// Returns byte size of one `ARGB8888` corner handle buffer at this scale.
    pub fn handle_buffer_bytes(self) -> usize
    {
        (self.handle_px as usize)
            .saturating_mul(self.handle_px as usize)
            .saturating_mul(BYTES_PER_PIXEL)
    }

    /// Computes physical `(width, height)` in buffer pixels for a dashed outline edge.
    pub fn edge_buffer_size(self, logical_span: i32, horizontal: bool) -> (i32, i32)
    {
        let scaled_span = logical_span.max(1).saturating_mul(self.scale);
        if horizontal
        {
            (scaled_span, self.outline_thickness_px)
        }
        else
        {
            (self.outline_thickness_px, scaled_span)
        }
    }

    /// Computes `(src_w, src_h, dst_w, dst_h)` for `wp_viewport` cropping at scale `1`.
    pub fn edge_viewport_crop(
        self,
        logical_span: i32,
        max_len: i32,
        horizontal: bool,
    ) -> (f64, f64, i32, i32)
    {
        let clamped = logical_span.clamp(1, max_len.max(1));
        let (dst_w, dst_h) = if horizontal
        {
            (clamped, OUTLINE_THICKNESS)
        }
        else
        {
            (OUTLINE_THICKNESS, clamped)
        };
        let (src_w, src_h) = self.edge_buffer_size(clamped, horizontal);
        (f64::from(src_w), f64::from(src_h), dst_w, dst_h)
    }

    /// Computes total SHM pool bytes needed for three handle buffers and four scaled border edges.
    pub fn pool_byte_budget(self, max_width: i32, max_height: i32) -> usize
    {
        let (hw, hh) = self.edge_buffer_size(max_width, true);
        let (vw, vh) = self.edge_buffer_size(max_height, false);
        let horiz_bytes = (hw as usize)
            .saturating_mul(hh as usize)
            .saturating_mul(BYTES_PER_PIXEL);
        let vert_bytes = (vw as usize)
            .saturating_mul(vh as usize)
            .saturating_mul(BYTES_PER_PIXEL);

        self.handle_buffer_bytes()
            .saturating_mul(3)
            .saturating_add(horiz_bytes.saturating_mul(2))
            .saturating_add(vert_bytes.saturating_mul(2))
            .saturating_add(CHROME_POOL_HEADROOM_BYTES)
    }
}

/// Returns updated `(scale, max_w, max_h)` when scale changes or layout grows past current bounds.
pub fn needs_chrome_rebuild(
    current_scale: i32,
    current_max_w: i32,
    current_max_h: i32,
    target_scale: i32,
    required_w: i32,
    required_h: i32,
) -> Option<(i32, i32, i32)>
{
    let next_scale = target_scale.max(1);
    let clamped_req_w = required_w.max(1);
    let clamped_req_h = required_h.max(1);

    if next_scale == current_scale
        && clamped_req_w <= current_max_w
        && clamped_req_h <= current_max_h
    {
        return None;
    }

    Some((
        next_scale,
        current_max_w.max(clamped_req_w),
        current_max_h.max(clamped_req_h),
    ))
}

/// Rasterizes an anti-aliased premultiplied white handle pill with a dark drop shadow into `argb`.
///
/// Composites a translucent white rounded pill over a dark drop shadow so the handle remains
/// clearly visible against both white, dark, and transparent backgrounds without blending away.
pub fn fill_rounded_handle_argb(argb: &mut [u8], size: u32, radius: u32, alpha: u8)
{
    let size_f = f64::from(size.max(1));
    let scale_f = size_f / f64::from(HANDLE_SIZE);

    // Inset the white pill slightly within the buffer to reserve margins for the drop shadow.
    let pad = 3.0 * scale_f;
    let pill_w = (size_f - 2.0 * pad).max(1.0);
    let half_w = pill_w * 0.5;
    let half_h = half_w;
    let pill_cx = size_f * 0.5;
    let pill_cy = (size_f * 0.5) - 0.5 * scale_f;
    let r_pill = (f64::from(radius) * 0.75).clamp(2.0 * scale_f, half_w);

    // Directional drop shadow offset and blur extents.
    let offset_x = 0.5 * scale_f;
    let offset_y = 1.0 * scale_f;
    let blur_dir = 2.5 * scale_f;
    let blur_ambient = 1.5 * scale_f;

    // Peak shadow alpha scales smoothly with handle interaction state (faded, idle, hover).
    let shadow_peak = (f64::from(alpha) * 0.45 + 50.0).min(160.0);

    for py in 0..size
    {
        let y = f64::from(py) + 0.5;
        for px in 0..size
        {
            let x = f64::from(px) + 0.5;

            // 1. Signed distance and anti-aliased coverage for the white pill.
            let dist_pill = rounded_box_sdf(x, y, pill_cx, pill_cy, half_w, half_h, r_pill);
            let pill_cov = (0.5 - dist_pill).clamp(0.0, 1.0);
            let a_pill = f64::from(alpha) * pill_cov;

            // 2. Directional drop shadow falloff.
            let dist_dir = rounded_box_sdf(
                x - offset_x,
                y - offset_y,
                pill_cx,
                pill_cy,
                half_w,
                half_h,
                r_pill,
            );
            let dir_cov = if dist_dir <= 0.0
            {
                1.0
            }
            else if dist_dir < blur_dir
            {
                let t = dist_dir / blur_dir;
                1.0 - t * t * (3.0 - 2.0 * t)
            }
            else
            {
                0.0
            };

            // 3. Ambient contact shadow falloff around all edges.
            let ambient_cov = if dist_pill <= 0.0
            {
                1.0
            }
            else if dist_pill < blur_ambient
            {
                let t = dist_pill / blur_ambient;
                1.0 - t * t * (3.0 - 2.0 * t)
            }
            else
            {
                0.0
            };

            let shadow_cov = dir_cov.max(ambient_cov * 0.7);
            let a_shadow = shadow_peak * shadow_cov;

            // 4. Alpha compositing: white pill over black drop shadow.
            // In premultiplied ARGB8888, white pill contributes `a_pill` to RGB; black shadow contributes 0.
            let out_c = a_pill;
            let out_a = a_pill + a_shadow * (1.0 - a_pill / 255.0);

            let c_byte = out_c.round().clamp(0.0, 255.0) as u8;
            let a_byte = out_a.round().clamp(0.0, 255.0) as u8;

            let idx = (py * size + px) as usize * BYTES_PER_PIXEL;
            argb[idx..idx + BYTES_PER_PIXEL].copy_from_slice(&[c_byte, c_byte, c_byte, a_byte]);
        }
    }
}

/// Signed distance to a 2D rounded box centered at `(cx, cy)` with half-extents `(half_w, half_h)` and radius `r`.
///
/// Returns negative values inside the rounded box, zero on its perimeter, and Euclidean distance outside.
fn rounded_box_sdf(x: f64, y: f64, cx: f64, cy: f64, half_w: f64, half_h: f64, r: f64) -> f64
{
    let px = (x - cx).abs();
    let py = (y - cy).abs();
    let qx = px - (half_w - r);
    let qy = py - (half_h - r);
    let dist_corner = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    let dist_straight = qx.max(qy).min(0.0);
    dist_corner + dist_straight - r
}

/// Rasterizes a premultiplied white dashed pattern along `width x height` using `period` pixels.
pub(crate) fn fill_dashed_argb(
    argb: &mut [u8],
    width: u32,
    height: u32,
    horizontal: bool,
    period: u32,
    alpha: u8,
)
{
    let effective_period = period.max(2);
    let half_period = effective_period / 2;
    for py in 0..height
    {
        for px in 0..width
        {
            let axis_pos = if horizontal { px } else { py };
            let val = if (axis_pos % effective_period) < half_period
            {
                alpha
            }
            else
            {
                0
            };
            let idx = (py * width + px) as usize * BYTES_PER_PIXEL;
            argb[idx..idx + BYTES_PER_PIXEL].copy_from_slice(&[val, val, val, val]);
        }
    }
}
