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

/// Desired SHM buffer dimensions and optional `display_cache` signature for a frame render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferTargetSpec
{
    /// Target buffer width in physical pixels.
    pub width: u32,

    /// Target buffer height in physical pixels.
    pub height: u32,

    /// Cache signature when display-sized caching applies (`None` when streaming raw source frames).
    pub cache_key: Option<DisplayCacheKey>,
}

/// Computes desired SHM buffer dimensions and optional `display_cache` key for `layout`.
pub fn compute_buffer_target_spec(
    layout: &OverlayLayout,
    display_cache: &DisplayFrameCache,
    buf_scale: i32,
    frame_count: usize,
    decode_done: bool,
    fits_source_ram: bool,
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
        if (scaled_w, scaled_h) != (frame_w, frame_h)
        {
            // Completed single-frame overlays have no animation ticks, so downscale cleanly
            // with Lanczos3 directly into SHM without occupying DisplayFrameCache slots.
            if decode_done && frame_count == 1
            {
                return BufferTargetSpec {
                    width: scaled_w,
                    height: scaled_h,
                    cache_key: None,
                };
            }

            // Multi-frame quarter-turn loops downscale only when source frames exceed RAM budget
            // and the scaled sequence fits inside `-c display`.
            if !fits_source_ram
                && display_cache.would_fit(scaled_w, scaled_h, frame_count, decode_done)
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

/// Reports whether an SHM ring of `ring_size` should reallocate for `spec_size`.
pub fn ring_needs_resync(spec_size: (u32, u32), ring_size: (u32, u32)) -> bool
{
    if spec_size.0 > ring_size.0 || spec_size.1 > ring_size.1
    {
        return true;
    }

    let want_pixels = u64::from(spec_size.0) * u64::from(spec_size.1);
    let cur_pixels = u64::from(ring_size.0) * u64::from(ring_size.1);
    want_pixels * 3 <= cur_pixels * 2
}

/// Stages source or display-sized frames into a double/triple-buffered SHM `ImageBufferRing`.
pub(crate) struct FrameStage
{
    // Reusable frame ring backing `image_surface` at source or scaled rotated bounds.
    frame_ring: ImageBufferRing,

    // Budgeted RAM cache of display-resolution frames avoiding repeated LZ4 and rotation work.
    display_cache: DisplayFrameCache,

    // Reusable Lanczos3 resizer downscaling oversized quarter-turn frames into `display_cache` or SHM.
    resizer: Resizer,
}

impl FrameStage
{
    /// Allocates an initial `ImageBufferRing` sized to the startup target spec and creates `DisplayFrameCache`.
    pub(crate) fn new(
        shm: &Shm,
        layout: &OverlayLayout,
        cache_profile: (usize, bool, bool),
        buf_scale: i32,
        display_cache_budget_bytes: usize,
    ) -> Result<Self, OverlayError>
    {
        let display_cache = DisplayFrameCache::new(display_cache_budget_bytes);
        let (frame_count, decode_done, fits_source_ram) = cache_profile;
        let spec = compute_buffer_target_spec(
            layout,
            &display_cache,
            buf_scale,
            frame_count,
            decode_done,
            fits_source_ram,
        );
        Ok(Self {
            frame_ring: ImageBufferRing::new(shm, spec.width, spec.height)?,
            display_cache,
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
        let cache_profile = cache.cache_profile();
        let (frame_count, decode_done, _) = cache_profile;
        let spec = self.target_spec(layout, buf_scale, cache_profile);
        if let Some(key) = spec.cache_key
            && self.display_cache.sync(key, frame_count, decode_done)
            && let Some(hit) = self.display_cache.get(frame_idx)
        {
            return with_target_ring(&mut self.frame_ring, shm, spec.width, spec.height, |ring| {
                ring.upload_and_attach(image_surface, hit)
            });
        }

        cache
            .with_frame_argb(frame_idx, |argb| {
                self.stage_frame_with_spec(
                    shm,
                    image_surface,
                    layout,
                    spec,
                    (frame_count, decode_done),
                    frame_idx,
                    argb,
                )
            })
            .and_then(|inner| inner)
    }

    /// Uploads caller-supplied `frame_argb` for `frame_idx` through the same target-spec pipeline.
    #[expect(clippy::too_many_arguments)]
    pub(crate) fn stage_frame_slice(
        &mut self,
        shm: &Shm,
        image_surface: &WlSurface,
        layout: &OverlayLayout,
        buf_scale: i32,
        cache_profile: (usize, bool, bool),
        frame_idx: usize,
        frame_argb: &[u8],
    ) -> Result<Option<ImageBufferRing>, OverlayError>
    {
        let spec = self.target_spec(layout, buf_scale, cache_profile);
        self.stage_frame_with_spec(
            shm,
            image_surface,
            layout,
            spec,
            (cache_profile.0, cache_profile.1),
            frame_idx,
            frame_argb,
        )
    }

    /// Stages `frame_argb` according to `spec`, populating `display_cache` when active.
    #[expect(clippy::too_many_arguments)]
    fn stage_frame_with_spec(
        &mut self,
        shm: &Shm,
        image_surface: &WlSurface,
        layout: &OverlayLayout,
        spec: BufferTargetSpec,
        cache_state: (usize, bool),
        frame_idx: usize,
        frame_argb: &[u8],
    ) -> Result<Option<ImageBufferRing>, OverlayError>
    {
        let (frame_count, decode_done) = cache_state;
        if let Some(key) = spec.cache_key
            && self.display_cache.sync(key, frame_count, decode_done)
        {
            if let Some(hit) = self.display_cache.get(frame_idx)
            {
                return with_target_ring(
                    &mut self.frame_ring,
                    shm,
                    spec.width,
                    spec.height,
                    |ring| ring.upload_and_attach(image_surface, hit),
                );
            }

            let rendered = render_display_frame_argb(
                frame_argb,
                layout,
                spec.width,
                spec.height,
                &mut self.resizer,
            )?;
            let retired =
                with_target_ring(&mut self.frame_ring, shm, spec.width, spec.height, |ring| {
                    ring.upload_and_attach(image_surface, &rendered)
                })?;
            self.display_cache.store(frame_idx, rendered);
            return Ok(retired);
        }

        self.display_cache.clear();
        with_target_ring(&mut self.frame_ring, shm, spec.width, spec.height, |ring| {
            fill_ring_slot(ring, image_surface, layout, frame_argb, &mut self.resizer)
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
        let spec = self.target_spec(layout, buf_scale, cache.cache_profile());
        ring_needs_resync((spec.width, spec.height), self.frame_ring.dimensions())
    }

    /// Reports whether display-resolution caching is active and how many bytes it holds.
    pub(crate) fn display_cache_stats(&self) -> (bool, usize)
    {
        (
            self.display_cache.is_active(),
            self.display_cache.cached_bytes(),
        )
    }

    /// Convenience wrapper invoking `compute_buffer_target_spec` with `self.display_cache`.
    fn target_spec(
        &self,
        layout: &OverlayLayout,
        buf_scale: i32,
        cache_profile: (usize, bool, bool),
    ) -> BufferTargetSpec
    {
        compute_buffer_target_spec(
            layout,
            &self.display_cache,
            buf_scale,
            cache_profile.0,
            cache_profile.1,
            cache_profile.2,
        )
    }
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

/// Writes `frame_argb` directly into an SHM slot of `ring` (raw copy, Lanczos3 downscale, or rotation).
fn fill_ring_slot(
    ring: &mut ImageBufferRing,
    surface: &WlSurface,
    layout: &OverlayLayout,
    frame_argb: &[u8],
    resizer: &mut Resizer,
) -> Result<(), OverlayError>
{
    let (frame_w, frame_h) = layout.frame_size();
    let (dst_w, dst_h) = ring.dimensions();
    if layout.quarter_turn().is_some()
    {
        if (dst_w, dst_h) == (frame_w, frame_h)
        {
            return ring.upload_and_attach(surface, frame_argb);
        }

        let downscaled =
            resize_premultiplied_argb8888(frame_argb, frame_w, frame_h, dst_w, dst_h, resizer)?;
        return ring.upload_and_attach(surface, &downscaled);
    }

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
