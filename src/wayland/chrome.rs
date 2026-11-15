//! Corner handle (`28x28` rounded pill) and edit-mode dashed outline subsurfaces.

// SCTK compositing, SHM buffer slot allocation, and subsurface builders.
use smithay_client_toolkit::{
    compositor::{Region, SurfaceData},
    shm::slot::{Buffer, SlotPool},
    subcompositor::{SubcompositorState, SubsurfaceData},
};
// Wayland surface, subsurface, SHM format, and viewport cropping objects.
use wayland_client::{
    Dispatch, QueueHandle,
    protocol::{wl_shm, wl_subsurface::WlSubsurface, wl_surface::WlSurface},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

// Domain errors, handle sizing, layout rects, and pointer interaction state.
use crate::{
    error::{OverlayError, WaylandError},
    layout::{HANDLE_SIZE, LogicalRect},
    wayland::input::InputState,
};

// Corner radius of the rendered 28x28 corner handle matching CSS `border-radius: 8px`.
const HANDLE_CORNER_RADIUS: u32 = 8;

// Idle handle opacity byte (~0.25 * 255).
const HANDLE_IDLE_ALPHA: u8 = 64;

// Hovered handle opacity byte (~0.85 * 255).
const HANDLE_HOVER_ALPHA: u8 = 217;

// Thickness in pixels of each edit-mode dashed border subsurface.
const OUTLINE_THICKNESS: i32 = 1;

// Dash stroke length in pixels (6px solid, 6px transparent).
const OUTLINE_DASH_PERIOD: u32 = 12;

// Edit-mode dashed outline opacity byte (~0.55 * 255).
const OUTLINE_ALPHA: u8 = 140;

/// Corner handle subsurface and four dashed border subsurfaces drawn atop the overlay image.
pub(crate) struct OverlayChrome
{
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

/// One dashed 1 px border strip subsurface cropped dynamically via `wp_viewport`.
struct OutlineEdge
{
    // Subsurface positioning the border strip at overlay edges.
    subsurface: WlSubsurface,

    // Surface attaching the pre-rendered dashed pattern buffer.
    surface: WlSurface,

    // Viewport cropping `max_len` buffer down to live overlay width/height.
    viewport: WpViewport,

    // Pre-rendered full-span dashed buffer owned by `_chrome_pool`.
    buffer: Buffer,

    // Maximum pixel span allocated in `buffer`.
    max_len: i32,

    // `true` for horizontal top/bottom strips, `false` for vertical left/right strips.
    horizontal: bool,
}

impl OverlayChrome
{
    /// Allocates handle and dashed border subsurfaces from `chrome_pool`.
    #[expect(clippy::too_many_arguments)]
    pub(crate) fn create<D>(
        qh: &QueueHandle<D>,
        subcompositor: &SubcompositorState,
        viewporter: &WpViewporter,
        parent_wl: &WlSurface,
        image_surface: &WlSurface,
        empty_region: &Region,
        chrome_pool: &mut SlotPool,
        screen_width: i32,
        screen_height: i32,
    ) -> Result<Self, OverlayError>
    where
        D: Dispatch<WlSurface, SurfaceData<()>>
            + Dispatch<WlSubsurface, SubsurfaceData>
            + Dispatch<WpViewport, ()>
            + 'static,
    {
        let (handle_subsurface, handle_surface) =
            subcompositor.create_subsurface(parent_wl.clone(), qh);
        handle_subsurface.set_desync();
        handle_surface.set_input_region(Some(empty_region.wl_region()));
        handle_subsurface.place_above(image_surface);

        let handle_idle_buf = create_handle_buffer(chrome_pool, HANDLE_IDLE_ALPHA)?;
        let handle_hover_buf = create_handle_buffer(chrome_pool, HANDLE_HOVER_ALPHA)?;

        let edges = [
            create_outline_edge(
                qh,
                subcompositor,
                viewporter,
                parent_wl,
                &handle_surface,
                empty_region,
                chrome_pool,
                screen_width,
                true,
            )?,
            create_outline_edge(
                qh,
                subcompositor,
                viewporter,
                parent_wl,
                &handle_surface,
                empty_region,
                chrome_pool,
                screen_width,
                true,
            )?,
            create_outline_edge(
                qh,
                subcompositor,
                viewporter,
                parent_wl,
                &handle_surface,
                empty_region,
                chrome_pool,
                screen_height,
                false,
            )?,
            create_outline_edge(
                qh,
                subcompositor,
                viewporter,
                parent_wl,
                &handle_surface,
                empty_region,
                chrome_pool,
                screen_height,
                false,
            )?,
        ];

        Ok(Self {
            handle_subsurface,
            handle_surface,
            handle_idle_buf,
            handle_hover_buf,
            edges,
        })
    }

    /// Updates both handle placement/visibility and the four dashed outline strips.
    pub(crate) fn sync_all(
        &mut self,
        overlay_rect: LogicalRect,
        handle_rect: LogicalRect,
        input: &InputState,
    ) -> Result<(), OverlayError>
    {
        self.sync_handle(handle_rect, input)?;
        self.sync_outline(overlay_rect, input.editing);
        Ok(())
    }

    /// Positions and attaches the appropriate 28x28 rounded corner handle (or detaches when hidden).
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

        // Ignore `AlreadyActive` when same handle buffer remains displayed across state refreshes.
        let _ = active_buf.attach_to(&self.handle_surface);
        self.handle_surface
            .damage_buffer(0, 0, HANDLE_SIZE as i32, HANDLE_SIZE as i32);
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
            edge.apply_placement(ex, ey, span);
        }
    }
}

impl OutlineEdge
{
    /// Positions, attaches, viewport-crops, and commits one outline border edge.
    fn apply_placement(&self, x: i32, y: i32, span: i32)
    {
        let clamped = span.clamp(1, self.max_len);
        self.subsurface.set_position(x, y);
        let _ = self.buffer.attach_to(&self.surface);

        let (cw, ch) = if self.horizontal
        {
            (clamped, OUTLINE_THICKNESS)
        }
        else
        {
            (OUTLINE_THICKNESS, clamped)
        };

        self.viewport
            .set_source(0.0, 0.0, f64::from(cw), f64::from(ch));
        self.viewport.set_destination(cw, ch);
        self.surface.damage_buffer(0, 0, cw, ch);
        self.surface.commit();
    }
}

/// Allocates a 28x28 rounded-corner white handle buffer with premultiplied `alpha`.
fn create_handle_buffer(pool: &mut SlotPool, alpha: u8) -> Result<Buffer, OverlayError>
{
    let size = HANDLE_SIZE as i32;
    let (buffer, slice) = pool
        .create_buffer(size, size, size * 4, wl_shm::Format::Argb8888)
        .map_err(|err| WaylandError::ShmPool(err.to_string()))?;

    fill_rounded_handle_argb(slice, HANDLE_SIZE, HANDLE_CORNER_RADIUS, alpha);
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
    let (subsurface, surface) = subcompositor.create_subsurface(parent.clone(), qh);
    subsurface.set_desync();
    surface.set_input_region(Some(empty_region.wl_region()));
    subsurface.place_below(handle_surface);

    let viewport = viewporter.get_viewport(&surface, qh, ());
    let (bw, bh) = if horizontal
    {
        (len, OUTLINE_THICKNESS)
    }
    else
    {
        (OUTLINE_THICKNESS, len)
    };

    let (buffer, slice) = pool
        .create_buffer(bw, bh, bw * 4, wl_shm::Format::Argb8888)
        .map_err(|err| WaylandError::ShmPool(err.to_string()))?;

    fill_dashed_argb(slice, bw as u32, bh as u32, horizontal, OUTLINE_ALPHA);

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
            let idx = ((py * size + px) * 4) as usize;
            argb[idx..idx + 4].copy_from_slice(&[a, a, a, a]);
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

/// Rasterizes a premultiplied white dashed pattern (6px on / 6px off) along `width x height`.
fn fill_dashed_argb(argb: &mut [u8], width: u32, height: u32, horizontal: bool, alpha: u8)
{
    let half_period = OUTLINE_DASH_PERIOD / 2;
    for py in 0..height
    {
        for px in 0..width
        {
            let axis_pos = if horizontal { px } else { py };
            let val = if (axis_pos % OUTLINE_DASH_PERIOD) < half_period
            {
                alpha
            }
            else
            {
                0
            };
            let idx = ((py * width + px) * 4) as usize;
            argb[idx..idx + 4].copy_from_slice(&[val, val, val, val]);
        }
    }
}
