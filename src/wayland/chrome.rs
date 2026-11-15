//! Corner handle (`28x28` rounded pill) and edit-mode dashed outline subsurfaces.

// Memory replacement when swapping HiDPI/resized SHM buffer pools cleanly.
use std::mem;

// SCTK compositing, SHM pool allocation, and subsurface builders.
use smithay_client_toolkit::{
    compositor::{Region, SurfaceData},
    shm::{
        Shm,
        slot::{Buffer, SlotPool},
    },
    subcompositor::{SubcompositorState, SubsurfaceData},
};
// Wayland surface, subsurface, and viewport cropping objects.
use wayland_client::{
    Dispatch, QueueHandle,
    protocol::{wl_subsurface::WlSubsurface, wl_surface::WlSurface},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

// Domain errors, pixel sizing, handle sizing, layout rects, subsurface/SHM helpers, and input state.
use super::{
    input::InputState,
    scene::create_desync_passive_subsurface,
    shm_ring::{create_argb_buffer, shm_pool_err},
};
use crate::{
    decode::BYTES_PER_PIXEL,
    error::{OverlayError, WaylandError},
    layout::{HANDLE_SIZE, LogicalRect},
};

// Corner radius of the rendered 28x28 corner handle matching CSS `border-radius: 8px`.
const HANDLE_CORNER_RADIUS: u32 = 8;

// Idle handle opacity byte (~0.25 * 255).
const HANDLE_IDLE_ALPHA: u8 = 64;

// Hovered handle opacity byte (~0.85 * 255).
const HANDLE_HOVER_ALPHA: u8 = 217;

// Thickness in logical pixels of each edit-mode dashed border subsurface.
const OUTLINE_THICKNESS: i32 = 1;

// Dash stroke length in logical pixels (6px solid, 6px transparent).
const OUTLINE_DASH_PERIOD: u32 = 12;

// Edit-mode dashed outline opacity byte (~0.55 * 255).
const OUTLINE_ALPHA: u8 = 140;

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

    /// Computes total SHM pool bytes needed for two handle buffers and four scaled border edges.
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
            .saturating_mul(2)
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

/// Corner handle subsurface and four dashed border subsurfaces drawn atop the overlay image.
pub(crate) struct OverlayChrome
{
    // Replaceable shared-memory pool owning the handle and outline border buffers.
    pool: SlotPool,

    // Active integer buffer scale advertised for HiDPI chrome rasterization.
    scale: i32,

    // Maximum logical horizontal span allocated in top/bottom outline strips.
    max_width: i32,

    // Maximum logical vertical span allocated in left/right outline strips.
    max_height: i32,

    // Subsurface positioning the 28x28 corner handle at the bottom-right corner.
    handle_subsurface: WlSubsurface,

    // Child surface displaying the 28x28 rounded corner handle.
    handle_surface: WlSurface,

    // Pre-rendered idle handle buffer (~25% opacity white rounded pill).
    handle_idle_buf: Buffer,

    // Pre-rendered hovered/active handle buffer (~85% opacity white rounded pill).
    handle_hover_buf: Buffer,

    // Four dashed border edge subsurfaces `[top, bottom, left, right]` shown in edit mode.
    edges: [OutlineEdge; 4],
}

/// One dashed border strip subsurface cropped dynamically via `wp_viewport`.
struct OutlineEdge
{
    // Subsurface positioning the border strip at overlay edges.
    subsurface: WlSubsurface,

    // Surface attaching the pre-rendered dashed pattern buffer.
    surface: WlSurface,

    // Viewport cropping `max_len` buffer down to live overlay width/height.
    viewport: WpViewport,

    // Pre-rendered full-span dashed buffer owned by `OverlayChrome::pool`.
    buffer: Buffer,

    // Maximum logical pixel span allocated in `buffer`.
    max_len: i32,

    // `true` for horizontal top/bottom strips, `false` for vertical left/right strips.
    horizontal: bool,
}

impl OverlayChrome
{
    /// Allocates handle and dashed border subsurfaces with an owned replaceable `SlotPool`.
    #[expect(clippy::too_many_arguments)]
    pub(crate) fn create<D>(
        qh: &QueueHandle<D>,
        subcompositor: &SubcompositorState,
        viewporter: &WpViewporter,
        shm: &Shm,
        parent_wl: &WlSurface,
        image_surface: &WlSurface,
        empty_region: &Region,
        screen_width: i32,
        screen_height: i32,
    ) -> Result<Self, OverlayError>
    where
        D: Dispatch<WlSurface, SurfaceData<()>>
            + Dispatch<WlSubsurface, SubsurfaceData>
            + Dispatch<WpViewport, ()>
            + 'static,
    {
        let geom = ChromeGeometry::for_scale(1);
        let max_width = screen_width.max(1);
        let max_height = screen_height.max(1);
        let mut pool = SlotPool::new(geom.pool_byte_budget(max_width, max_height), shm)
            .map_err(shm_pool_err)?;

        let (handle_subsurface, handle_surface) =
            create_desync_passive_subsurface(subcompositor, parent_wl, empty_region, qh);
        handle_subsurface.place_above(image_surface);
        handle_surface.set_buffer_scale(geom.scale);

        let handle_idle_buf = create_handle_buffer(&mut pool, geom, HANDLE_IDLE_ALPHA)?;
        let handle_hover_buf = create_handle_buffer(&mut pool, geom, HANDLE_HOVER_ALPHA)?;

        let edge_specs = [
            (max_width, true),
            (max_width, true),
            (max_height, false),
            (max_height, false),
        ];
        let mut built_edges = Vec::with_capacity(4);
        for (length, horizontal) in edge_specs
        {
            built_edges.push(create_outline_edge(
                qh,
                subcompositor,
                viewporter,
                parent_wl,
                &handle_surface,
                empty_region,
                &mut pool,
                geom,
                length,
                horizontal,
            )?);
        }
        let edges: [OutlineEdge; 4] = built_edges.try_into().map_err(|_| {
            WaylandError::ShmPool(String::from("Invalid outline edge array length"))
        })?;

        Ok(Self {
            pool,
            scale: geom.scale,
            max_width,
            max_height,
            handle_subsurface,
            handle_surface,
            handle_idle_buf,
            handle_hover_buf,
            edges,
        })
    }

    /// Rebuilds chrome buffers when `target_scale` changes or borders outgrow current bounds.
    pub(crate) fn ensure_scale_and_bounds(
        &mut self,
        shm: &Shm,
        target_scale: i32,
        required_width: i32,
        required_height: i32,
    ) -> Result<Option<SlotPool>, OverlayError>
    {
        let Some((next_scale, next_max_w, next_max_h)) = needs_chrome_rebuild(
            self.scale,
            self.max_width,
            self.max_height,
            target_scale,
            required_width,
            required_height,
        )
        else
        {
            return Ok(None);
        };

        let geom = ChromeGeometry::for_scale(next_scale);
        let mut new_pool = SlotPool::new(geom.pool_byte_budget(next_max_w, next_max_h), shm)
            .map_err(shm_pool_err)?;

        let new_idle = create_handle_buffer(&mut new_pool, geom, HANDLE_IDLE_ALPHA)?;
        let new_hover = create_handle_buffer(&mut new_pool, geom, HANDLE_HOVER_ALPHA)?;
        let mut new_edge_bufs = Vec::with_capacity(4);
        for edge in &self.edges
        {
            let span = if edge.horizontal
            {
                next_max_w
            }
            else
            {
                next_max_h
            };
            new_edge_bufs.push((
                create_edge_buffer(&mut new_pool, geom, span, edge.horizontal)?,
                span,
            ));
        }

        self.handle_idle_buf = new_idle;
        self.handle_hover_buf = new_hover;
        for (edge, (buf, span)) in self.edges.iter_mut().zip(new_edge_bufs)
        {
            edge.buffer = buf;
            edge.max_len = span;
        }
        self.handle_surface.set_buffer_scale(geom.scale);
        self.scale = geom.scale;
        self.max_width = next_max_w;
        self.max_height = next_max_h;

        let retired_pool = mem::replace(&mut self.pool, new_pool);
        Ok(Some(retired_pool))
    }

    /// Returns the active integer buffer scale used for chrome rasterization.
    pub(crate) fn scale(&self) -> i32
    {
        self.scale
    }

    /// Updates both handle placement/visibility and the four dashed outline strips.
    pub(crate) fn sync_all(
        &mut self,
        shm: &Shm,
        screen_size: (u32, u32),
        overlay_rect: LogicalRect,
        handle_rect: LogicalRect,
        input: &InputState,
    ) -> Result<(), OverlayError>
    {
        let need_w = (screen_size.0 as i32).max(overlay_rect.width as i32);
        let need_h = (screen_size.1 as i32).max(overlay_rect.height as i32);
        let _retired_pool = self.ensure_scale_and_bounds(shm, self.scale, need_w, need_h)?;

        self.sync_handle(handle_rect, input)?;
        self.sync_outline(overlay_rect, input.editing);
        Ok(())
    }

    /// Positions and attaches the appropriate rounded corner handle (or detaches when hidden).
    pub(crate) fn sync_handle(
        &mut self,
        rect: LogicalRect,
        input: &InputState,
    ) -> Result<(), OverlayError>
    {
        self.handle_subsurface.set_position(rect.x, rect.y);
        let visible = input.handle_visible || input.editing || input.hovered || input.is_dragging();

        if !visible
        {
            self.handle_surface.attach(None, 0, 0);
            self.handle_surface.commit();
            return Ok(());
        }

        let active_buf = if input.hovered || input.is_dragging()
        {
            &self.handle_hover_buf
        }
        else
        {
            &self.handle_idle_buf
        };

        let geom = ChromeGeometry::for_scale(self.scale);
        let px = geom.handle_px as i32;
        self.handle_surface.set_buffer_scale(geom.scale);
        // Ignore `AlreadyActive` when the active buffer stays unchanged across refreshes.
        let _ = active_buf.attach_to(&self.handle_surface);
        self.handle_surface.damage_buffer(0, 0, px, px);
        self.handle_surface.commit();
        Ok(())
    }

    /// Shows and crops the 4 dashed border subsurfaces around `rect` during edit mode.
    fn sync_outline(&mut self, rect: LogicalRect, editing: bool)
    {
        if !editing
        {
            for edge in &self.edges
            {
                edge.surface.attach(None, 0, 0);
                edge.surface.commit();
            }
            return;
        }

        let geom = ChromeGeometry::for_scale(self.scale);
        let w = (rect.width as i32).max(1);
        let h = (rect.height as i32).max(1);
        let placements = [
            (rect.x, rect.y, w),
            (rect.x, rect.y + h - OUTLINE_THICKNESS, w),
            (rect.x, rect.y, h),
            (rect.x + w - OUTLINE_THICKNESS, rect.y, h),
        ];

        for (edge, (ex, ey, span)) in self.edges.iter().zip(placements)
        {
            edge.apply_placement(geom, ex, ey, span);
        }
    }
}

impl OutlineEdge
{
    /// Positions, attaches, viewport-crops, and commits one outline border edge.
    fn apply_placement(&self, geom: ChromeGeometry, x: i32, y: i32, span: i32)
    {
        self.subsurface.set_position(x, y);
        let _ = self.buffer.attach_to(&self.surface);

        let (src_w, src_h, dst_w, dst_h) =
            geom.edge_viewport_crop(span, self.max_len, self.horizontal);

        self.viewport.set_source(0.0, 0.0, src_w, src_h);
        self.viewport.set_destination(dst_w, dst_h);
        self.surface.damage_buffer(0, 0, src_w as i32, src_h as i32);
        self.surface.commit();
    }
}

/// Allocates a scaled rounded-corner white handle buffer with premultiplied `alpha`.
fn create_handle_buffer(
    pool: &mut SlotPool,
    geom: ChromeGeometry,
    alpha: u8,
) -> Result<Buffer, OverlayError>
{
    let size = geom.handle_px as i32;
    let (buffer, slice) = create_argb_buffer(pool, size, size)?;

    fill_rounded_handle_argb(slice, geom.handle_px, geom.handle_radius_px, alpha);
    Ok(buffer)
}

/// Allocates one scaled dashed border strip buffer for `span` logical pixels.
fn create_edge_buffer(
    pool: &mut SlotPool,
    geom: ChromeGeometry,
    span: i32,
    horizontal: bool,
) -> Result<Buffer, OverlayError>
{
    let (bw, bh) = geom.edge_buffer_size(span, horizontal);
    let (buffer, slice) = create_argb_buffer(pool, bw, bh)?;

    fill_dashed_argb(
        slice,
        bw as u32,
        bh as u32,
        horizontal,
        geom.dash_period_px,
        OUTLINE_ALPHA,
    );
    Ok(buffer)
}

/// Allocates one viewport-cropped dashed outline edge subsurface placed below the corner handle.
#[expect(clippy::too_many_arguments)]
fn create_outline_edge<D>(
    qh: &QueueHandle<D>,
    subcompositor: &SubcompositorState,
    viewporter: &WpViewporter,
    parent: &WlSurface,
    handle_surface: &WlSurface,
    empty_region: &Region,
    pool: &mut SlotPool,
    geom: ChromeGeometry,
    length: i32,
    horizontal: bool,
) -> Result<OutlineEdge, OverlayError>
where
    D: Dispatch<WlSurface, SurfaceData<()>>
        + Dispatch<WlSubsurface, SubsurfaceData>
        + Dispatch<WpViewport, ()>
        + 'static,
{
    let len = length.max(1);
    let (subsurface, surface) =
        create_desync_passive_subsurface(subcompositor, parent, empty_region, qh);
    subsurface.place_below(handle_surface);

    let viewport = viewporter.get_viewport(&surface, qh, ());
    let buffer = create_edge_buffer(pool, geom, len, horizontal)?;

    Ok(OutlineEdge {
        subsurface,
        surface,
        viewport,
        buffer,
        max_len: len,
        horizontal,
    })
}

/// Rasterizes an anti-aliased premultiplied white rounded rectangle into `argb` (`size x size`).
fn fill_rounded_handle_argb(argb: &mut [u8], size: u32, radius: u32, alpha: u8)
{
    let r = f64::from(radius);
    let max_coord = f64::from(size);

    for py in 0..size
    {
        for px in 0..size
        {
            let dx = corner_axis_offset(f64::from(px) + 0.5, r, max_coord);
            let dy = corner_axis_offset(f64::from(py) + 0.5, r, max_coord);
            let dist = (dx * dx + dy * dy).sqrt();
            let coverage = (r + 0.5 - dist).clamp(0.0, 1.0);
            let a = (f64::from(alpha) * coverage).round() as u8;
            let idx = (py * size + px) as usize * BYTES_PER_PIXEL;
            argb[idx..idx + BYTES_PER_PIXEL].copy_from_slice(&[a, a, a, a]);
        }
    }
}

/// Returns signed distance outside the inner corner radius band along one axis.
fn corner_axis_offset(coord: f64, radius: f64, span: f64) -> f64
{
    if coord < radius
    {
        radius - coord
    }
    else if coord > span - radius
    {
        coord - (span - radius)
    }
    else
    {
        0.0
    }
}

/// Rasterizes a premultiplied white dashed pattern along `width x height` using `period` pixels.
fn fill_dashed_argb(
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
