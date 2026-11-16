//! Ordered APNG full-canvas compositor applying `DisposeOp` and `BlendOp` transitions.

// Error context placeholder path when patch buffer sizes mismatch metadata.
use std::path::PathBuf;

// Exact Porter-Duff straight-alpha pixel blender.
use image::{Pixel, Rgba};

// Decoded patch payloads, frame control metadata, pixel sizing, and error types.
use super::{patch::DecodedApngPatch, reader::FrameControl};
use crate::{
    decode::pixel::{BYTES_PER_PIXEL, RawRgbaFrame, checked_argb_byte_len},
    error::CacheError,
};

// APNG `dispose_op`: preserve canvas untouched before the next frame.
const DISPOSE_NONE: u8 = 0;

// APNG `dispose_op`: clear the rendered patch rectangle to transparent black.
const DISPOSE_BACKGROUND: u8 = 1;

// APNG `dispose_op`: restore the canvas patch rectangle from prior state.
const DISPOSE_PREVIOUS: u8 = 2;

// APNG `blend_op`: overwrite destination pixels directly.
const BLEND_SOURCE: u8 = 0;

// APNG `blend_op`: Porter-Duff straight-alpha `OVER` blend onto destination pixels.
const BLEND_OVER: u8 = 1;

/// Applies ordered APNG `DisposeOp` and `BlendOp` updates over a reusable full RGBA canvas.
pub struct ApngCompositor
{
    // Full canvas pixel width.
    canvas_w: u32,

    // Full canvas pixel height.
    canvas_h: u32,

    // Active compositing canvas stored as straight RGBA pixels.
    canvas: Vec<u8>,

    // Region pixels backed up only when a frame declares `DISPOSE_PREVIOUS`.
    saved_previous_patch: Vec<u8>,

    // Rectangle and disposal rule left behind by the previously rendered frame.
    pending_dispose: Option<FrameControl>,
}

impl ApngCompositor
{
    /// Creates an empty transparent canvas compositor sized `(canvas_w, canvas_h)`.
    pub fn new(canvas_w: u32, canvas_h: u32) -> Result<Self, CacheError>
    {
        let byte_len = checked_argb_byte_len(canvas_w, canvas_h)?;
        Ok(Self {
            canvas_w,
            canvas_h,
            canvas: vec![0_u8; byte_len],
            saved_previous_patch: Vec::new(),
            pending_dispose: None,
        })
    }

    /// Applies disposal for the previous frame, blends `patch`, and returns full-canvas `RawRgbaFrame`.
    pub(crate) fn apply_patch(
        &mut self,
        patch: DecodedApngPatch,
    ) -> Result<RawRgbaFrame, CacheError>
    {
        // W3C APNG requires executing the prior frame's disposal (e.g. `DISPOSE_BACKGROUND` clear)
        // before snapshotting the canvas for an incoming frame marked `DISPOSE_PREVIOUS`
        // (unlike `image-0.25.10`'s `ApngDecoder::mix_next_frame`, which snapshots prior to disposal).
        if let Some(prev) = self.pending_dispose.take()
        {
            self.dispose_previous_region(prev);
        }

        let fc = patch.fctl;
        if fc.dispose_op == DISPOSE_PREVIOUS
        {
            self.snapshot_region(fc);
        }

        self.blend_patch(fc, &patch.rgba)?;
        self.pending_dispose = Some(fc);

        Ok(RawRgbaFrame {
            width: self.canvas_w,
            height: self.canvas_h,
            delay_us: patch.delay_us,
            rgba: self.canvas.clone(),
        })
    }

    /// Applies `patch` onto the canvas and returns `(delay_us, straight_rgba)` for external tests.
    pub fn apply_patch_rgba(
        &mut self,
        patch: DecodedApngPatch,
    ) -> Result<(u32, Vec<u8>), CacheError>
    {
        let raw = self.apply_patch(patch)?;
        Ok((raw.delay_us, raw.rgba))
    }

    /// Clears or restores the previous frame's patch rectangle according to its `dispose_op`.
    fn dispose_previous_region(&mut self, prev: FrameControl)
    {
        let stride = (self.canvas_w as usize) * BYTES_PER_PIXEL;
        let row_bytes = (prev.width as usize) * BYTES_PER_PIXEL;
        let x_byte = (prev.x_offset as usize) * BYTES_PER_PIXEL;

        match prev.dispose_op
        {
            DISPOSE_NONE =>
            {}
            DISPOSE_BACKGROUND =>
            {
                for row in 0..prev.height as usize
                {
                    let start = ((prev.y_offset as usize) + row) * stride + x_byte;
                    self.canvas[start..start + row_bytes].fill(0);
                }
            }
            DISPOSE_PREVIOUS =>
            {
                if self.saved_previous_patch.len() < row_bytes * (prev.height as usize)
                {
                    return;
                }
                for row in 0..prev.height as usize
                {
                    let dst_start = ((prev.y_offset as usize) + row) * stride + x_byte;
                    let src_start = row * row_bytes;
                    self.canvas[dst_start..dst_start + row_bytes].copy_from_slice(
                        &self.saved_previous_patch[src_start..src_start + row_bytes],
                    );
                }
            }
            _ =>
            {}
        }
    }

    /// Saves the existing canvas patch before drawing a frame marked with `DISPOSE_PREVIOUS`.
    fn snapshot_region(&mut self, fc: FrameControl)
    {
        let stride = (self.canvas_w as usize) * BYTES_PER_PIXEL;
        let row_bytes = (fc.width as usize) * BYTES_PER_PIXEL;
        let total = row_bytes * (fc.height as usize);
        self.saved_previous_patch.resize(total, 0);

        for row in 0..fc.height as usize
        {
            let src_start =
                ((fc.y_offset as usize) + row) * stride + (fc.x_offset as usize) * BYTES_PER_PIXEL;
            let dst_start = row * row_bytes;
            self.saved_previous_patch[dst_start..dst_start + row_bytes]
                .copy_from_slice(&self.canvas[src_start..src_start + row_bytes]);
        }
    }

    /// Writes `patch_rgba` into `(fc.x_offset, fc.y_offset, fc.width, fc.height)` via `blend_op`.
    fn blend_patch(&mut self, fc: FrameControl, patch_rgba: &[u8]) -> Result<(), CacheError>
    {
        let canvas_stride = (self.canvas_w as usize) * BYTES_PER_PIXEL;
        let row_bytes = (fc.width as usize) * BYTES_PER_PIXEL;
        let x_byte = (fc.x_offset as usize) * BYTES_PER_PIXEL;

        let expected = row_bytes * (fc.height as usize);
        if patch_rgba.len() != expected
        {
            return Err(CacheError::Decode {
                path: PathBuf::from("<apng-patch>"),
                reason: format!("Patch size {} != expected {expected}", patch_rgba.len()),
            });
        }

        if fc.blend_op == BLEND_SOURCE
        {
            for row in 0..fc.height as usize
            {
                let dst_start = ((fc.y_offset as usize) + row) * canvas_stride + x_byte;
                let src_start = row * row_bytes;
                self.canvas[dst_start..dst_start + row_bytes]
                    .copy_from_slice(&patch_rgba[src_start..src_start + row_bytes]);
            }
            return Ok(());
        }

        if fc.blend_op == BLEND_OVER
        {
            for row in 0..fc.height as usize
            {
                let dst_start = ((fc.y_offset as usize) + row) * canvas_stride + x_byte;
                let src_start = row * row_bytes;
                let dst_row = &mut self.canvas[dst_start..dst_start + row_bytes];
                let src_row = &patch_rgba[src_start..src_start + row_bytes];
                blend_over_row(dst_row, src_row);
            }
            return Ok(());
        }

        Err(CacheError::Decode {
            path: PathBuf::from("<apng-patch>"),
            reason: format!("Unsupported APNG blend_op {}", fc.blend_op),
        })
    }
}

/// Blends straight-RGBA `src_row` over `dst_row` using `image::Rgba::blend` for exact parity.
fn blend_over_row(dst_row: &mut [u8], src_row: &[u8])
{
    for (dst_px, src_px) in dst_row
        .as_chunks_mut::<BYTES_PER_PIXEL>()
        .0
        .iter_mut()
        .zip(src_row.as_chunks::<BYTES_PER_PIXEL>().0.iter())
    {
        match src_px[3]
        {
            0 =>
            {}
            255 => *dst_px = *src_px,
            _ =>
            {
                let mut blended = Rgba(*dst_px);
                blended.blend(&Rgba(*src_px));
                *dst_px = blended.0;
            }
        }
    }
}
