//! Reusable double/triple-buffered Wayland `SlotPool` frame ring avoiding steady-state memfd churn.

// SCTK SHM buffer pool and Wayland protocol surface/SHM types.
use smithay_client_toolkit::shm::{
    Shm,
    slot::{Buffer, SlotPool},
};
use wayland_client::protocol::{wl_shm, wl_surface::WlSurface};

// Typed overlay errors, Wayland SHM errors, and 4-byte ARGB pixel size.
use crate::{
    decode::BYTES_PER_PIXEL,
    error::{OverlayError, WaylandError},
};

// Maximum concurrent SHM buffers maintained in the image frame ring.
const MAX_FRAME_RING_SLOTS: usize = 3;

/// Formats a shared-memory or Wayland pool/region failure cleanly as `WaylandError::ShmPool`.
pub(crate) fn shm_pool_err(err: impl ToString) -> WaylandError
{
    WaylandError::ShmPool(err.to_string())
}

/// Computes `width * BYTES_PER_PIXEL` cleanly, failing on `i32` overflow.
pub(crate) fn checked_argb_stride(width: i32) -> Result<i32, WaylandError>
{
    width
        .checked_mul(BYTES_PER_PIXEL as i32)
        .ok_or_else(|| WaylandError::ShmPool(String::from("Frame stride overflow")))
}

/// Allocates one `wl_shm::Format::Argb8888` slot buffer in `pool` using standard 4-byte stride.
pub(crate) fn create_argb_buffer(
    pool: &mut SlotPool,
    width: i32,
    height: i32,
) -> Result<(Buffer, &mut [u8]), WaylandError>
{
    let stride = checked_argb_stride(width)?;
    pool.create_buffer(width, height, stride, wl_shm::Format::Argb8888)
        .map_err(shm_pool_err)
}

/// Reusable double/triple-buffered `SlotPool` holding frame-sized `ARGB8888` `wl_shm` slots.
pub(crate) struct ImageBufferRing
{
    // Memory-mapped Wayland shared-memory pool for image frames.
    pool: SlotPool,

    // Pre-allocated slot buffers rotated as the compositor releases each previous buffer.
    buffers: Vec<Buffer>,

    // Fixed frame buffer pixel width matching `FrameCache`.
    frame_width: i32,

    // Fixed frame buffer pixel height matching `FrameCache`.
    frame_height: i32,

    // Byte stride (`frame_width * 4`).
    stride: i32,

    // Cursor advancing round-robin to maximize time since last buffer submission.
    next_cursor: usize,
}

impl ImageBufferRing
{
    /// Allocates a pool sized for two frame buffers up front without runtime allocations later.
    pub(crate) fn new(shm: &Shm, frame_width: u32, frame_height: u32)
    -> Result<Self, OverlayError>
    {
        let fw = i32::try_from(frame_width.max(1)).map_err(|_| {
            WaylandError::ShmPool(format!("Frame width exceeds i32: {frame_width}"))
        })?;
        let fh = i32::try_from(frame_height.max(1)).map_err(|_| {
            WaylandError::ShmPool(format!("Frame height exceeds i32: {frame_height}"))
        })?;
        let stride = checked_argb_stride(fw)?;
        let frame_bytes = (fh as usize)
            .checked_mul(stride as usize)
            .ok_or_else(|| WaylandError::ShmPool(String::from("Frame buffer size overflow")))?;

        let initial_bytes = frame_bytes.saturating_mul(2).max(4096);
        let mut pool = SlotPool::new(initial_bytes, shm).map_err(shm_pool_err)?;

        let mut buffers = Vec::with_capacity(MAX_FRAME_RING_SLOTS);
        for _ in 0..2
        {
            let (buffer, _) = create_argb_buffer(&mut pool, fw, fh)?;
            buffers.push(buffer);
        }

        Ok(Self {
            pool,
            buffers,
            frame_width: fw,
            frame_height: fh,
            stride,
            next_cursor: 0,
        })
    }

    /// Returns the pixel dimensions `(width, height)` allocated in this SHM buffer ring.
    pub(crate) fn dimensions(&self) -> (u32, u32)
    {
        (self.frame_width as u32, self.frame_height as u32)
    }

    /// Copies premultiplied `argb` pixels into a released slot and attaches it to `surface`.
    pub(crate) fn upload_and_attach(
        &mut self,
        surface: &WlSurface,
        argb: &[u8],
    ) -> Result<(), OverlayError>
    {
        let slot_idx = self.acquire_free_slot_index()?;
        let buffer = &self.buffers[slot_idx];

        let canvas = buffer.canvas(&mut self.pool).ok_or_else(|| {
            WaylandError::ShmPool(String::from("Acquired SHM buffer slot unexpectedly locked"))
        })?;

        if canvas.len() != argb.len()
        {
            return Err(WaylandError::ShmPool(format!(
                "SHM slot byte length {} != frame byte length {}",
                canvas.len(),
                argb.len()
            ))
            .into());
        }

        canvas.copy_from_slice(argb);
        buffer.attach_to(surface).map_err(shm_pool_err)?;
        surface.damage_buffer(0, 0, self.frame_width, self.frame_height);
        Ok(())
    }

    /// Returns index of an inactive released buffer, growing up to `MAX_FRAME_RING_SLOTS` if needed.
    fn acquire_free_slot_index(&mut self) -> Result<usize, WaylandError>
    {
        let count = self.buffers.len();
        for offset in 0..count
        {
            let idx = (self.next_cursor + offset) % count;
            if self.buffers[idx].canvas(&mut self.pool).is_some()
            {
                self.next_cursor = (idx + 1) % count;
                return Ok(idx);
            }
        }

        if count < MAX_FRAME_RING_SLOTS
        {
            let (buffer, _) = self
                .pool
                .create_buffer(
                    self.frame_width,
                    self.frame_height,
                    self.stride,
                    wl_shm::Format::Argb8888,
                )
                .map_err(shm_pool_err)?;
            self.buffers.push(buffer);
            let idx = self.buffers.len() - 1;
            self.next_cursor = (idx + 1) % self.buffers.len();
            return Ok(idx);
        }

        Err(WaylandError::ShmPool(String::from(
            "All SHM buffers remain locked by compositor",
        )))
    }
}
