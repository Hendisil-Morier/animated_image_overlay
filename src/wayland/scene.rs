//! Wayland layer-shell parent, image subsurface, viewport scaling, and input-region updates.

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

// Chrome subsurfaces, SHM frame ring, monitor metadata, layout, input state, and errors.
use super::{
    chrome::OverlayChrome,
    input::InputState,
    monitor::MonitorDescriptor,
    shm_ring::{ImageBufferRing, create_argb_buffer, shm_pool_err},
};
use crate::{
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

    // Reusable frame ring backing `image_surface`.
    frame_ring: ImageBufferRing,

    // Corner handle and edit-mode dashed border subsurfaces.
    chrome: OverlayChrome,

    // Current known monitor width for lazy parent viewport updates.
    screen_width: u32,

    // Current known monitor height for lazy parent viewport updates.
    screen_height: u32,

    // Cached last committed edit mode to avoid redundant `set_keyboard_interactivity` commits.
    last_editing: bool,

    // Cached last committed rotation applied to `image_surface`.
    last_rotation: Rotation,

    // Cached last committed horizontal flip state applied to `image_surface`.
    last_flipped: bool,

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
            chrome,
            screen_width: sw,
            screen_height: sh,
            last_editing: false,
            last_rotation: Rotation::default(),
            last_flipped: false,
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

        self.frame_ring
            .upload_and_attach(&self.image_surface, frame0_argb)?;

        self.sync_layout(compositor, shm, layout, input)?;
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

    /// Uploads and attaches a new animation frame onto the desynchronized image subsurface.
    ///
    /// Callers commit `image_surface` separately after queuing any `wl_surface::frame` callback.
    pub(crate) fn stage_frame(&mut self, frame_argb: &[u8]) -> Result<(), OverlayError>
    {
        self.frame_ring
            .upload_and_attach(&self.image_surface, frame_argb)
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
        let mut image_state_changed = false;

        if (dst_w, dst_h) != self.last_image_dest
        {
            self.last_image_dest = (dst_w, dst_h);
            self.image_viewport.set_destination(dst_w, dst_h);
            image_state_changed = true;
        }

        let desired_rotation = layout.rotation();
        let desired_flipped = layout.flipped();
        if desired_rotation != self.last_rotation || desired_flipped != self.last_flipped
        {
            self.last_rotation = desired_rotation;
            self.last_flipped = desired_flipped;
            if self.image_surface.version() >= 2
            {
                let transform = wl_transform_for_orientation(desired_rotation, desired_flipped);
                self.image_surface.set_buffer_transform(transform);
                tracing::debug!(
                    ?desired_rotation,
                    desired_flipped,
                    ?transform,
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
