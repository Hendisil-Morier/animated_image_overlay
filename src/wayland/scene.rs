//! Wayland layer-shell parent, image subsurface, viewport scaling, and input-region updates.

// Lanczos3 resizer reused across oversized quarter-turn downscales.
use fast_image_resize::Resizer;
// SCTK compositing, layer-shell, SHM pool, and subsurface abstractions.
use smithay_client_toolkit::{
    compositor::{CompositorState, Region, SurfaceData},
    reexports::protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerSurface, LayerSurfaceData,
        },
    },
    shm::{
        Shm,
        slot::{Buffer, SlotPool},
    },
    subcompositor::{SubcompositorState, SubsurfaceData},
};
// Raw Wayland protocol surface, subsurface, output, and viewport objects.
use wayland_client::{
    Dispatch, Proxy, QueueHandle,
    protocol::{
        wl_output::{Transform, WlOutput},
        wl_subsurface::WlSubsurface,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

// Chrome subsurfaces, display cache, SHM frame ring, monitor metadata, layout, and input state.
use super::{
    chrome::OverlayChrome,
    display_cache::{DisplayCacheKey, DisplayFrameCache, angle_key},
    input::InputState,
    monitor::MonitorDescriptor,
    shm_ring::{ImageBufferRing, create_argb_buffer, shm_pool_err},
};
use crate::{
    cache::FrameCache,
    decode::{checked_argb_byte_len, resize_premultiplied_argb8888, rotate_argb8888_into},
    error::{OverlayError, WaylandError},
    layout::{OverlayLayout, Rotation},
};

// Dedicated 4 KiB pool capacity backing the 1x1 transparent parent layer buffer.
const PARENT_POOL_BYTES: usize = 4096;

/// Maps visual overlay `(rotation, flipped)` onto Wayland `wl_surface::set_buffer_transform`.
///
/// Wayland compositors display the inverse of the surface buffer transform (`wl_output::transform`),
/// where flipped variants flip horizontally around the vertical axis before counter-clockwise
/// rotation (`inverse(R_ccw * F) = F * R_cw`).
pub fn wl_transform_for_orientation(rotation: Rotation, flipped: bool) -> Transform
{
    match (rotation, flipped)
    {
        (Rotation::Deg0, false) => Transform::Normal,
        (Rotation::Deg90, false) => Transform::_90,
        (Rotation::Deg180, false) => Transform::_180,
        (Rotation::Deg270, false) => Transform::_270,
        (Rotation::Deg0, true) => Transform::Flipped,
        (Rotation::Deg90, true) => Transform::Flipped90,
        (Rotation::Deg180, true) => Transform::Flipped180,
        (Rotation::Deg270, true) => Transform::Flipped270,
    }
}

/// Creates a desynchronized child subsurface with empty click-through input region.
pub(crate) fn create_desync_passive_subsurface<D>(
    subcompositor: &SubcompositorState,
    parent: &WlSurface,
    empty_region: &Region,
    qh: &QueueHandle<D>,
) -> (WlSubsurface, WlSurface)
where
    D: Dispatch<WlSurface, SurfaceData<()>> + Dispatch<WlSubsurface, SubsurfaceData> + 'static,
{
    let (subsurface, surface) = subcompositor.create_subsurface(parent.clone(), qh);
    subsurface.set_desync();
    surface.set_input_region(Some(empty_region.wl_region()));
    (subsurface, surface)
}

/// Coordinates parent transparent layer surface, child animation subsurface, and chrome overlays.
pub(crate) struct OverlayScene
{
    /// Fullscreen click-region-controlled parent layer shell surface.
    pub(crate) parent_layer: LayerSurface,

    /// Child image subsurface hosting decoded video/animation frames.
    pub(crate) image_surface: WlSurface,

    // Subsurface controller positioning `image_surface` relative to parent.
    image_subsurface: WlSubsurface,

    // Hardware scaler mapping `frame_width x frame_height` buffer to `(w, h)` logical space.
    image_viewport: WpViewport,

    // Viewport stretching the 1x1 transparent parent buffer to cover the monitor.
    parent_viewport: WpViewport,

    // Reusable 1x1 transparent SHM buffer mapped onto `parent_layer`.
    parent_transparent_buf: Buffer,

    // Dedicated shared memory pool holding `parent_transparent_buf`.
    _parent_pool: SlotPool,

    // Reusable frame ring backing `image_surface` at source or scaled rotated bounds.
    frame_ring: ImageBufferRing,

    // Budgeted RAM cache of display-resolution frames avoiding repeated LZ4 and rotation work.
    display_cache: DisplayFrameCache,

    // Reusable Lanczos3 resizer downscaling oversized quarter-turn frames into `display_cache`.
    resizer: Resizer,

    // Corner handle and edit-mode dashed border subsurfaces.
    chrome: OverlayChrome,

    // Current known monitor width for lazy parent viewport updates.
    screen_width: u32,

    // Current known monitor height for lazy parent viewport updates.
    screen_height: u32,

    // Cached last committed edit mode to avoid redundant `set_keyboard_interactivity` commits.
    last_editing: bool,

    // Cached last committed Wayland buffer transform applied to `image_surface`.
    last_buffer_transform: Transform,

    // Cached last committed `wp_viewport` destination size on `image_surface`.
    last_image_dest: (i32, i32),
}

impl OverlayScene
{
    /// Creates the fullscreen `LayerSurface`, child image subsurface, handle, and edit borders.
    #[expect(clippy::too_many_arguments)]
    pub(crate) fn create<D>(
        qh: &QueueHandle<D>,
        compositor: &CompositorState,
        subcompositor: &SubcompositorState,
        layer_shell: &LayerShell,
        viewporter: &WpViewporter,
        shm: &Shm,
        output: &WlOutput,
        monitor: &MonitorDescriptor,
        frame_width: u32,
        frame_height: u32,
        display_cache_budget_bytes: usize,
    ) -> Result<Self, OverlayError>
    where
        D: Dispatch<ZwlrLayerSurfaceV1, LayerSurfaceData>
            + Dispatch<WlSurface, SurfaceData<()>>
            + Dispatch<WlSubsurface, SubsurfaceData>
            + Dispatch<WpViewport, ()>
            + 'static,
    {
        let parent_surface = compositor.create_surface(qh);
        let parent_layer = layer_shell.create_layer_surface(
            qh,
            parent_surface,
            Layer::Overlay,
            Some("apng-overlay"),
            Some(output),
        );
        parent_layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        parent_layer.set_exclusive_zone(-1);
        parent_layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        parent_layer.wl_surface().commit();

        let empty_region = Region::new(compositor)
            .map_err(|err| WaylandError::ShmPool(format!("Failed creating empty region: {err}")))?;

        let parent_wl = parent_layer.wl_surface().clone();
        let (image_subsurface, image_surface) =
            create_desync_passive_subsurface(subcompositor, &parent_wl, &empty_region, qh);

        let parent_viewport = viewporter.get_viewport(&parent_wl, qh, ());
        let image_viewport = viewporter.get_viewport(&image_surface, qh, ());

        let sw = (monitor.width.max(1)) as u32;
        let sh = (monitor.height.max(1)) as u32;
        let mut parent_pool = SlotPool::new(PARENT_POOL_BYTES, shm).map_err(shm_pool_err)?;

        let (parent_transparent_buf, parent_slice) = create_argb_buffer(&mut parent_pool, 1, 1)?;
        parent_slice.fill(0);

        let chrome = OverlayChrome::create(
            qh,
            subcompositor,
            viewporter,
            shm,
            &parent_wl,
            &image_surface,
            &empty_region,
            sw as i32,
            sh as i32,
        )?;

        let frame_ring = ImageBufferRing::new(shm, frame_width, frame_height)?;

        Ok(Self {
            parent_layer,
            image_surface,
            image_subsurface,
            image_viewport,
            parent_viewport,
            parent_transparent_buf,
            _parent_pool: parent_pool,
            frame_ring,
            display_cache: DisplayFrameCache::new(display_cache_budget_bytes),
            resizer: Resizer::new(),
            chrome,
            screen_width: sw,
            screen_height: sh,
            last_editing: false,
            last_buffer_transform: Transform::Normal,
            last_image_dest: (0, 0),
        })
    }

    /// Uploads frame 0 and maps the initial transparent fullscreen layer surface + subsurfaces.
    pub(crate) fn initialize_first_frame(
        &mut self,
        compositor: &CompositorState,
        shm: &Shm,
        layout: &OverlayLayout,
        input: &InputState,
        frame0_argb: &[u8],
    ) -> Result<(), OverlayError>
    {
        let (sw, sh) = layout.screen_size();
        self.screen_width = sw;
        self.screen_height = sh;

        self.parent_transparent_buf
            .attach_to(self.parent_layer.wl_surface())
            .map_err(shm_pool_err)?;
        self.parent_layer.wl_surface().damage_buffer(0, 0, 1, 1);
        self.parent_viewport.set_destination(sw as i32, sh as i32);

        let _retired_ring = self.stage_oriented_buffer(shm, layout, frame0_argb)?;
        self.sync_layout_inner(compositor, shm, layout, input, true)?;
        Ok(())
    }

    /// Rebuilds handle and dashed outline buffers when compositor `preferred_buffer_scale` changes.
    pub(crate) fn update_chrome_scale(
        &mut self,
        shm: &Shm,
        target_scale: i32,
        layout: &OverlayLayout,
        input: &InputState,
        configured: bool,
    ) -> Result<(), OverlayError>
    {
        let clamped_scale = target_scale.max(1);
        if clamped_scale == self.chrome.scale()
        {
            return Ok(());
        }

        let (sw, sh) = layout.screen_size();
        let rect = layout.overlay_rect();
        let need_w = (sw as i32).max(rect.width as i32);
        let need_h = (sh as i32).max(rect.height as i32);
        let _retired_pool =
            self.chrome
                .ensure_scale_and_bounds(shm, clamped_scale, need_w, need_h)?;

        if configured && _retired_pool.is_some()
        {
            self.chrome
                .sync_all(shm, (sw, sh), rect, layout.input_rect(false), input)?;
            self.parent_layer.wl_surface().commit();
        }
        Ok(())
    }

    /// Uploads and attaches animation frame `frame_idx`, reusing `display_cache` when within budget.
    ///
    /// Returns any retired `ImageBufferRing` so callers keep the old SHM pool alive until after
    /// committing `image_surface`.
    pub(crate) fn stage_frame_cached(
        &mut self,
        shm: &Shm,
        layout: &OverlayLayout,
        cache: &FrameCache,
        frame_idx: usize,
    ) -> Result<Option<ImageBufferRing>, OverlayError>
    {
        let spec = self.target_spec(layout, cache);
        if let Some(key) = spec.cache_key
        {
            let (frame_count, decode_done, _) = cache.cache_profile();
            if self.display_cache.sync(key, frame_count, decode_done)
            {
                if let Some(hit) = self.display_cache.get(frame_idx)
                {
                    return with_target_ring(
                        &mut self.frame_ring,
                        shm,
                        spec.width,
                        spec.height,
                        |ring| ring.upload_and_attach(&self.image_surface, hit),
                    );
                }

                let (frame_w, frame_h) = layout.frame_size();
                let resizer = &mut self.resizer;
                let buf = cache.with_frame_argb(
                    frame_idx,
                    |argb| -> Result<Vec<u8>, OverlayError> {
                        if layout.quarter_turn().is_some()
                        {
                            return Ok(resize_premultiplied_argb8888(
                                argb,
                                frame_w,
                                frame_h,
                                spec.width,
                                spec.height,
                                resizer,
                            )?);
                        }

                        let mut dst = vec![0_u8; checked_argb_byte_len(spec.width, spec.height)?];
                        rotate_argb8888_into(
                            argb,
                            frame_w,
                            frame_h,
                            layout.rotation(),
                            layout.flipped(),
                            spec.width,
                            spec.height,
                            &mut dst,
                        )?;
                        Ok(dst)
                    },
                )??;
                let retired =
                    with_target_ring(&mut self.frame_ring, shm, spec.width, spec.height, |ring| {
                        ring.upload_and_attach(&self.image_surface, &buf)
                    })?;
                self.display_cache.store(frame_idx, buf);
                return Ok(retired);
            }
        }

        self.display_cache.clear();
        cache
            .with_frame_argb(frame_idx, |argb| {
                with_target_ring(&mut self.frame_ring, shm, spec.width, spec.height, |ring| {
                    fill_ring_slot(ring, &self.image_surface, layout, argb)
                })
            })
            .and_then(|inner| inner)
    }

    /// Computes desired SHM dimensions and optional `display_cache` key for `layout` and `cache`.
    fn target_spec(&self, layout: &OverlayLayout, cache: &FrameCache) -> BufferTargetSpec
    {
        let buf_scale = self.chrome.scale().max(1) as u32;
        let (ov_w, ov_h) = layout.overlay_size();
        if let Some(quarter) = layout.quarter_turn()
        {
            let (frame_w, frame_h) = layout.frame_size();
            let (scaled_w, scaled_h) = clamped_physical_size(
                quarter.orient_dimensions(ov_w, ov_h),
                buf_scale,
                (frame_w, frame_h),
            );
            let (frame_count, decode_done, fits_source_ram) = cache.cache_profile();
            if !fits_source_ram
                && (scaled_w, scaled_h) != (frame_w, frame_h)
                && self
                    .display_cache
                    .would_fit(scaled_w, scaled_h, frame_count, decode_done)
            {
                // Canonical 0 deg unflipped key lets Wayland buffer_transform handle quarter turns
                // and horizontal flips without rebuilding cached pixels.
                return BufferTargetSpec {
                    width: scaled_w,
                    height: scaled_h,
                    cache_key: Some(DisplayCacheKey {
                        width: scaled_w,
                        height: scaled_h,
                        angle_bits: angle_key(0.0),
                        flipped: false,
                    }),
                };
            }

            return BufferTargetSpec {
                width: frame_w,
                height: frame_h,
                cache_key: None,
            };
        }

        let (target_w, target_h) =
            clamped_physical_size((ov_w, ov_h), buf_scale, layout.oriented_frame_size());
        BufferTargetSpec {
            width: target_w,
            height: target_h,
            cache_key: Some(DisplayCacheKey {
                width: target_w,
                height: target_h,
                angle_bits: angle_key(layout.rotation()),
                flipped: layout.flipped(),
            }),
        }
    }

    /// Returns initial SHM dimensions for frame 0 before `display_cache` or full decode completes.
    pub(crate) fn oriented_target_size(&self, layout: &OverlayLayout) -> (u32, u32)
    {
        if layout.quarter_turn().is_some()
        {
            return layout.frame_size();
        }

        let buf_scale = self.chrome.scale().max(1) as u32;
        clamped_physical_size(
            layout.overlay_size(),
            buf_scale,
            layout.oriented_frame_size(),
        )
    }

    /// Reports when the active SHM ring should resize for sharpness, shrinking, or cache mode changes.
    pub(crate) fn needs_buffer_resync(&self, layout: &OverlayLayout, cache: &FrameCache) -> bool
    {
        let spec = self.target_spec(layout, cache);
        let (cur_w, cur_h) = self.frame_ring.dimensions();
        if spec.width > cur_w || spec.height > cur_h
        {
            return true;
        }

        let want_pixels = u64::from(spec.width) * u64::from(spec.height);
        let cur_pixels = u64::from(cur_w) * u64::from(cur_h);
        want_pixels * 3 <= cur_pixels * 2
    }

    /// Re-uploads `frame_idx` for `layout` orientation and atomically commits buffer + layout changes.
    pub(crate) fn refresh_frame_cached(
        &mut self,
        compositor: &CompositorState,
        shm: &Shm,
        layout: &OverlayLayout,
        input: &InputState,
        cache: &FrameCache,
        frame_idx: usize,
    ) -> Result<(), OverlayError>
    {
        let _retired_ring = self.stage_frame_cached(shm, layout, cache, frame_idx)?;
        self.sync_layout_inner(compositor, shm, layout, input, true)?;
        Ok(())
    }

    /// Ensures the SHM frame ring matches `oriented_target_size(layout)` and renders or copies the
    /// current frame straight into the active SHM slot attached to `self.image_surface`.
    fn stage_oriented_buffer(
        &mut self,
        shm: &Shm,
        layout: &OverlayLayout,
        frame_argb: &[u8],
    ) -> Result<Option<ImageBufferRing>, OverlayError>
    {
        let (target_w, target_h) = self.oriented_target_size(layout);
        with_target_ring(&mut self.frame_ring, shm, target_w, target_h, |ring| {
            fill_ring_slot(ring, &self.image_surface, layout, frame_argb)
        })
    }

    /// Updates image subsurface position/scale, handle visibility/hover, dashed border, and input region.
    pub(crate) fn sync_layout(
        &mut self,
        compositor: &CompositorState,
        shm: &Shm,
        layout: &OverlayLayout,
        input: &InputState,
    ) -> Result<(), OverlayError>
    {
        self.sync_layout_inner(compositor, shm, layout, input, false)
    }

    /// Applies layout updates onto parent and image subsurfaces, forcing `image_surface.commit()`
    /// whenever a new buffer was staged immediately beforehand.
    fn sync_layout_inner(
        &mut self,
        compositor: &CompositorState,
        shm: &Shm,
        layout: &OverlayLayout,
        input: &InputState,
        force_image_commit: bool,
    ) -> Result<(), OverlayError>
    {
        let (sw, sh) = layout.screen_size();
        if sw != self.screen_width || sh != self.screen_height
        {
            self.screen_width = sw;
            self.screen_height = sh;
            self.parent_viewport.set_destination(sw as i32, sh as i32);
        }

        let rect = layout.overlay_rect();
        self.image_subsurface.set_position(rect.x, rect.y);

        let dst_w = rect.width as i32;
        let dst_h = rect.height as i32;
        let mut image_state_changed = force_image_commit;

        if (dst_w, dst_h) != self.last_image_dest
        {
            self.last_image_dest = (dst_w, dst_h);
            self.image_viewport.set_destination(dst_w, dst_h);
            image_state_changed = true;
        }

        let desired_transform = match layout.quarter_turn()
        {
            Some(quarter) => wl_transform_for_orientation(quarter, layout.flipped()),
            None => Transform::Normal,
        };
        if desired_transform != self.last_buffer_transform
        {
            self.last_buffer_transform = desired_transform;
            if self.image_surface.version() >= 2
            {
                self.image_surface.set_buffer_transform(desired_transform);
                tracing::debug!(
                    rotation_deg = layout.rotation(),
                    flipped = layout.flipped(),
                    ?desired_transform,
                    "Updated overlay buffer transform"
                );
            }
            else
            {
                tracing::warn!(
                    version = self.image_surface.version(),
                    "wl_surface version < 2 lacks buffer_transform support"
                );
            }
            image_state_changed = true;
        }

        if image_state_changed
        {
            self.image_surface.commit();
        }

        let handle_rect = layout.input_rect(false);
        self.chrome
            .sync_all(shm, (sw, sh), rect, handle_rect, input)?;

        if input.editing != self.last_editing
        {
            self.last_editing = input.editing;
            let interactivity = if input.editing
            {
                KeyboardInteractivity::OnDemand
            }
            else
            {
                KeyboardInteractivity::None
            };
            self.parent_layer.set_keyboard_interactivity(interactivity);
        }

        let input_rect = layout.input_rect(input.editing);
        let region = Region::new(compositor)
            .map_err(|err| WaylandError::ShmPool(format!("Failed updating input region: {err}")))?;
        region.add(
            input_rect.x,
            input_rect.y,
            input_rect.width as i32,
            input_rect.height as i32,
        );
        self.parent_layer
            .wl_surface()
            .set_input_region(Some(region.wl_region()));

        self.parent_layer.wl_surface().commit();
        Ok(())
    }

    /// Updates only the corner handle buffer opacity/attachment without altering geometry.
    pub(crate) fn sync_handle_only(
        &mut self,
        layout: &OverlayLayout,
        input: &InputState,
    ) -> Result<(), OverlayError>
    {
        self.chrome.sync_handle(layout.input_rect(false), input)
    }
}

/// Target SHM buffer dimensions and optional `display_cache` signature for a frame render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BufferTargetSpec
{
    // Target buffer width in physical pixels.
    width: u32,

    // Target buffer height in physical pixels.
    height: u32,

    // Cache signature when display-sized caching applies (`None` when streaming raw source frames).
    cache_key: Option<DisplayCacheKey>,
}

/// Scales `logical` by `buf_scale`, clamping each axis cleanly into `1..=max_size`.
fn clamped_physical_size(logical: (u32, u32), buf_scale: u32, max_size: (u32, u32)) -> (u32, u32)
{
    (
        logical.0.saturating_mul(buf_scale).clamp(1, max_size.0),
        logical.1.saturating_mul(buf_scale).clamp(1, max_size.1),
    )
}

/// Sizes `frame_ring` to `(target_w, target_h)` when needed, runs `fill`, and returns any retired ring.
fn with_target_ring(
    frame_ring: &mut ImageBufferRing,
    shm: &Shm,
    target_w: u32,
    target_h: u32,
    fill: impl FnOnce(&mut ImageBufferRing) -> Result<(), OverlayError>,
) -> Result<Option<ImageBufferRing>, OverlayError>
{
    if frame_ring.dimensions() != (target_w, target_h)
    {
        let mut replacement = ImageBufferRing::new(shm, target_w, target_h)?;
        fill(&mut replacement)?;
        return Ok(Some(std::mem::replace(frame_ring, replacement)));
    }

    fill(frame_ring)?;
    Ok(None)
}

/// Writes `frame_argb` directly into an SHM slot of `ring` (raw copy or fractional rotation).
fn fill_ring_slot(
    ring: &mut ImageBufferRing,
    surface: &WlSurface,
    layout: &OverlayLayout,
    frame_argb: &[u8],
) -> Result<(), OverlayError>
{
    if layout.quarter_turn().is_some()
    {
        return ring.upload_and_attach(surface, frame_argb);
    }

    let (frame_w, frame_h) = layout.frame_size();
    let (dst_w, dst_h) = ring.dimensions();
    ring.fill_and_attach(surface, |canvas| {
        rotate_argb8888_into(
            frame_argb,
            frame_w,
            frame_h,
            layout.rotation(),
            layout.flipped(),
            dst_w,
            dst_h,
            canvas,
        )
        .map_err(OverlayError::from)
    })
}
