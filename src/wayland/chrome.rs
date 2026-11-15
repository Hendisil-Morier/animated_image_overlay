//! Wayland corner handle (`28x28` rounded pill) and edit-mode dashed outline subsurfaces.

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

// Chrome geometry/rasterizers, layout rects, subsurface/SHM helpers, and input state.
pub use super::chrome_raster::{
    ChromeGeometry, HANDLE_FADED_ALPHA, HANDLE_HOVER_ALPHA, HANDLE_IDLE_ALPHA,
    handle_opacity_alpha, needs_chrome_rebuild,
};
use super::{
    chrome_raster::{OUTLINE_ALPHA, OUTLINE_THICKNESS, fill_dashed_argb, fill_rounded_handle_argb},
    input::InputState,
    scene::create_desync_passive_subsurface,
    shm_ring::{create_argb_buffer, shm_pool_err},
};
use crate::{
    error::{OverlayError, WaylandError},
    layout::LogicalRect,
};

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

    // Pre-rendered faded, idle, and hover handle SHM buffers.
    handles: HandleBuffers,

    // Four dashed border edge subsurfaces `[top, bottom, left, right]` shown in edit mode.
    edges: [OutlineEdge; 4],
}

/// Pre-rendered rounded handle buffers spanning faded resting, idle, and active hover states.
struct HandleBuffers
{
    // Subtle resting handle kept visible after the 2-second fade timeout (~15% opacity).
    faded: Buffer,

    // Standard idle handle shown for 2 seconds after pointer exit or in edit mode (~25% opacity).
    idle: Buffer,

    // High-contrast handle shown while hovered or actively dragging (~85% opacity).
    hover: Buffer,
}

impl HandleBuffers
{
    /// Allocates faded, idle, and hover handle buffers at `geom` scale inside `pool`.
    fn create(pool: &mut SlotPool, geom: ChromeGeometry) -> Result<Self, OverlayError>
    {
        Ok(Self {
            faded: create_handle_buffer(pool, geom, HANDLE_FADED_ALPHA)?,
            idle: create_handle_buffer(pool, geom, HANDLE_IDLE_ALPHA)?,
            hover: create_handle_buffer(pool, geom, HANDLE_HOVER_ALPHA)?,
        })
    }

    /// Returns the handle buffer matching `input` interaction state.
    fn for_input(&self, input: &InputState) -> &Buffer
    {
        match handle_opacity_alpha(input)
        {
            HANDLE_HOVER_ALPHA => &self.hover,
            HANDLE_IDLE_ALPHA => &self.idle,
            _ => &self.faded,
        }
    }
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

        let handles = HandleBuffers::create(&mut pool, geom)?;

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
            handles,
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

        let new_handles = HandleBuffers::create(&mut new_pool, geom)?;
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

        self.handles = new_handles;
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

    /// Positions and attaches the faded, idle, or hover rounded corner handle buffer.
    pub(crate) fn sync_handle(
        &mut self,
        rect: LogicalRect,
        input: &InputState,
    ) -> Result<(), OverlayError>
    {
        self.handle_subsurface.set_position(rect.x, rect.y);
        let active_buf = self.handles.for_input(input);

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
