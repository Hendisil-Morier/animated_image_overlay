//! Display-resolution frame scaling, rotation, and SHM ring staging for the overlay subsurface.

// Lanczos3 resizer reused across oversized quarter-turn downscales.
use fast_image_resize::Resizer;
// SCTK SHM allocator abstraction.
use smithay_client_toolkit::shm::Shm;
// Wayland surface protocol handle for attaching staged frame buffers.
use wayland_client::protocol::wl_surface::WlSurface;

// Display-resolution cache and reusable SHM buffer ring.
use super::{
    display_cache::{DisplayCacheKey, DisplayFrameCache, angle_key},
    shm_ring::ImageBufferRing,
};
// Core frame store, pixel transforms, errors, and overlay geometry.
use crate::{
    cache::FrameCache,
    decode::{checked_argb_byte_len, resize_premultiplied_argb8888, rotate_argb8888_into},
    error::OverlayError,
    layout::OverlayLayout,
};

/// Stages source or display-sized frames into a double/triple-buffered SHM `ImageBufferRing`.
pub(crate) struct FrameStage
{
    // Reusable frame ring backing `image_surface` at source or scaled rotated bounds.
    frame_ring: ImageBufferRing,

    // Budgeted RAM cache of display-resolution frames avoiding repeated LZ4 and rotation work.
    display_cache: DisplayFrameCache,

    // Reusable Lanczos3 resizer downscaling oversized quarter-turn frames into `display_cache`.
    resizer: Resizer,
}

impl FrameStage
{
    /// Allocates an initial `ImageBufferRing` of `(init_w, init_h)` and a bounded display cache.
    pub(crate) fn new(
        shm: &Shm,
        init_w: u32,
        init_h: u32,
        display_cache_budget_bytes: usize,
    ) -> Result<Self, OverlayError>
    {
        Ok(Self {
            frame_ring: ImageBufferRing::new(shm, init_w, init_h)?,
            display_cache: DisplayFrameCache::new(display_cache_budget_bytes),
            resizer: Resizer::new(),
        })
    }

    /// Uploads and attaches animation frame `frame_idx`, reusing `display_cache` when within budget.
    ///
    /// Returns any retired `ImageBufferRing` so callers keep the old SHM pool alive until after
    /// committing `image_surface`.
    pub(crate) fn stage_frame_cached(
        &mut self,
        shm: &Shm,
        image_surface: &WlSurface,
        layout: &OverlayLayout,
        cache: &FrameCache,
        buf_scale: i32,
        frame_idx: usize,
    ) -> Result<Option<ImageBufferRing>, OverlayError>
    {
        let spec = self.target_spec(layout, cache, buf_scale);
        if let Some(key) = spec.cache_key
        {
            let (frame_count, decode_done, _) = cache.cache_profile();
            if self.display_cache.sync(key, frame_count, decode_done)
            {
                return self.stage_from_display_cache(
                    shm,
                    image_surface,
                    layout,
                    cache,
                    frame_idx,
                    spec,
                );
            }
        }

        self.display_cache.clear();
        cache
            .with_frame_argb(frame_idx, |argb| {
                with_target_ring(&mut self.frame_ring, shm, spec.width, spec.height, |ring| {
                    fill_ring_slot(ring, image_surface, layout, argb)
                })
            })
            .and_then(|inner| inner)
    }

    /// Serves a cached display frame or renders, caches, and uploads it into `self.frame_ring`.
    fn stage_from_display_cache(
        &mut self,
        shm: &Shm,
        image_surface: &WlSurface,
        layout: &OverlayLayout,
        cache: &FrameCache,
        frame_idx: usize,
        spec: BufferTargetSpec,
    ) -> Result<Option<ImageBufferRing>, OverlayError>
    {
        if let Some(hit) = self.display_cache.get(frame_idx)
        {
            return with_target_ring(&mut self.frame_ring, shm, spec.width, spec.height, |ring| {
                ring.upload_and_attach(image_surface, hit)
            });
        }

        let resizer = &mut self.resizer;
        let buf = cache.with_frame_argb(frame_idx, |argb| {
            render_display_frame_argb(argb, layout, spec.width, spec.height, resizer)
        })??;
        let retired =
            with_target_ring(&mut self.frame_ring, shm, spec.width, spec.height, |ring| {
                ring.upload_and_attach(image_surface, &buf)
            })?;
        self.display_cache.store(frame_idx, buf);
        Ok(retired)
    }

    /// Ensures the SHM frame ring matches `oriented_target_size(layout)` and renders or copies the
    /// supplied frame straight into the active SHM slot attached to `image_surface`.
    pub(crate) fn stage_oriented_buffer(
        &mut self,
        shm: &Shm,
        image_surface: &WlSurface,
        layout: &OverlayLayout,
        buf_scale: i32,
        frame_argb: &[u8],
    ) -> Result<Option<ImageBufferRing>, OverlayError>
    {
        let (target_w, target_h) = oriented_target_size(layout, buf_scale);
        with_target_ring(&mut self.frame_ring, shm, target_w, target_h, |ring| {
            fill_ring_slot(ring, image_surface, layout, frame_argb)
        })
    }

    /// Reports when the active SHM ring should resize for sharpness, shrinking, or cache mode changes.
    pub(crate) fn needs_buffer_resync(
        &self,
        layout: &OverlayLayout,
        cache: &FrameCache,
        buf_scale: i32,
    ) -> bool
    {
        let spec = self.target_spec(layout, cache, buf_scale);
        let (cur_w, cur_h) = self.frame_ring.dimensions();
        if spec.width > cur_w || spec.height > cur_h
        {
            return true;
        }

        let want_pixels = u64::from(spec.width) * u64::from(spec.height);
        let cur_pixels = u64::from(cur_w) * u64::from(cur_h);
        want_pixels * 3 <= cur_pixels * 2
    }

    /// Computes desired SHM dimensions and optional `display_cache` key for `layout` and `cache`.
    fn target_spec(
        &self,
        layout: &OverlayLayout,
        cache: &FrameCache,
        buf_scale: i32,
    ) -> BufferTargetSpec
    {
        let clamped_scale = buf_scale.max(1) as u32;
        let (ov_w, ov_h) = layout.overlay_size();
        if let Some(quarter) = layout.quarter_turn()
        {
            let (frame_w, frame_h) = layout.frame_size();
            let (scaled_w, scaled_h) = clamped_physical_size(
                quarter.orient_dimensions(ov_w, ov_h),
                clamped_scale,
                (frame_w, frame_h),
            );
            let (frame_count, decode_done, fits_source_ram) = cache.cache_profile();
            // Gate quarter-turn downscaling on both oversized sources and fit inside `-c display`:
            // small animations stream faster via source LRU copy, while huge overlays offload scaling
            // to compositor GPU sampling rather than burning CPU Lanczos3 per frame.
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
            clamped_physical_size((ov_w, ov_h), clamped_scale, layout.oriented_frame_size());
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

/// Renders one display-sized `ARGB8888` frame via Lanczos3 resize (quarter turns) or rotation.
fn render_display_frame_argb(
    argb: &[u8],
    layout: &OverlayLayout,
    dst_w: u32,
    dst_h: u32,
    resizer: &mut Resizer,
) -> Result<Vec<u8>, OverlayError>
{
    let (frame_w, frame_h) = layout.frame_size();
    if layout.quarter_turn().is_some()
    {
        return Ok(resize_premultiplied_argb8888(
            argb, frame_w, frame_h, dst_w, dst_h, resizer,
        )?);
    }

    let mut dst = vec![0_u8; checked_argb_byte_len(dst_w, dst_h)?];
    rotate_argb8888_into(
        argb,
        frame_w,
        frame_h,
        layout.rotation(),
        layout.flipped(),
        dst_w,
        dst_h,
        &mut dst,
    )?;
    Ok(dst)
}

/// Returns initial SHM dimensions for frame 0 before `display_cache` or full decode completes.
fn oriented_target_size(layout: &OverlayLayout, buf_scale: i32) -> (u32, u32)
{
    if layout.quarter_turn().is_some()
    {
        return layout.frame_size();
    }

    let clamped_scale = buf_scale.max(1) as u32;
    clamped_physical_size(
        layout.overlay_size(),
        clamped_scale,
        layout.oriented_frame_size(),
    )
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
