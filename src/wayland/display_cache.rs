//! Bounded in-memory cache of display-resolution `ARGB8888` overlay frames.

// Checked byte-size arithmetic preventing integer overflow on large buffer targets.
use crate::decode::checked_argb_byte_len;

/// Cache invalidation signature pairing physical buffer geometry and orientation transform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayCacheKey
{
    /// Physical display buffer width in pixels.
    pub width: u32,

    /// Physical display buffer height in pixels.
    pub height: u32,

    /// Canonical IEEE 754 bit representation of the clockwise angle in degrees (`0.0` when raw).
    pub angle_bits: u64,

    /// Horizontal mirror flag baked into the cached pixels (`false` when compositor handles flip).
    pub flipped: bool,
}

/// Converts a finite rotation angle to canonical IEEE 754 bits, normalizing `-0.0` to `+0.0`.
pub fn angle_key(angle_deg: f64) -> u64
{
    let normalized = if angle_deg == 0.0 { 0.0 } else { angle_deg };
    normalized.to_bits()
}

/// Stores lazily populated display-sized `ARGB8888` frames within a strict byte ceiling.
pub struct DisplayFrameCache
{
    // Configured memory limit in bytes (`0` disables display caching).
    budget_bytes: usize,

    // Geometry and orientation key matched by `slots`, or `None` when disabled/uninitialized.
    active_key: Option<DisplayCacheKey>,

    // Exact expected byte length (`width * height * 4`) of each cached slot.
    expected_frame_bytes: usize,

    // Per-frame slot storage cleared whenever the key changes or total bytes exceed budget.
    slots: Vec<Option<Vec<u8>>>,
}

impl DisplayFrameCache
{
    /// Creates an empty display frame cache bounded by `budget_bytes`.
    pub fn new(budget_bytes: usize) -> Self
    {
        Self {
            budget_bytes,
            active_key: None,
            expected_frame_bytes: 0,
            slots: Vec::new(),
        }
    }

    /// Returns the configured byte ceiling for display-resolution frame storage.
    pub fn budget_bytes(&self) -> usize
    {
        self.budget_bytes
    }

    /// Returns `true` when display caching is active for the current key and frame count.
    pub fn is_active(&self) -> bool
    {
        self.active_key.is_some() && !self.slots.is_empty()
    }

    /// Sums heap pixel bytes currently populated in cached display slots.
    pub fn cached_bytes(&self) -> usize
    {
        self.slots
            .iter()
            .filter_map(|slot| slot.as_ref().map(Vec::len))
            .sum()
    }

    /// Validates `key`, `frame_count`, and `decode_done` against `budget_bytes`, resizing slots as needed.
    ///
    /// Releases all retained frame buffers immediately if the animation exceeds budget, has fewer
    /// than two frames, or is still streaming an unknown frame count.
    pub fn sync(&mut self, key: DisplayCacheKey, frame_count: usize, decode_done: bool) -> bool
    {
        if !decode_done || frame_count <= 1 || self.budget_bytes == 0
        {
            self.clear();
            return false;
        }

        let Ok(frame_bytes) = checked_argb_byte_len(key.width, key.height)
        else
        {
            self.clear();
            return false;
        };

        let Some(total_bytes) = frame_count.checked_mul(frame_bytes)
        else
        {
            self.clear();
            return false;
        };

        if total_bytes > self.budget_bytes
        {
            self.clear();
            return false;
        }

        if self.active_key == Some(key) && self.slots.len() == frame_count
        {
            return true;
        }

        self.active_key = Some(key);
        self.expected_frame_bytes = frame_bytes;
        self.slots = vec![None; frame_count];
        true
    }

    /// Drops all cached frame buffers and resets active key state, shrinking backing storage.
    pub fn clear(&mut self)
    {
        self.active_key = None;
        self.expected_frame_bytes = 0;
        self.slots.clear();
        self.slots.shrink_to_fit();
    }

    /// Returns borrowed premultiplied `ARGB8888` bytes when frame `index` is cached.
    pub fn get(&self, index: usize) -> Option<&[u8]>
    {
        self.slots.get(index).and_then(|slot| slot.as_deref())
    }

    /// Stores a rendered `ARGB8888` frame buffer for `index` if active and byte-size matches.
    pub fn store(&mut self, index: usize, argb: Vec<u8>)
    {
        if self.active_key.is_none() || argb.len() != self.expected_frame_bytes
        {
            return;
        }
        if let Some(slot) = self.slots.get_mut(index)
        {
            *slot = Some(argb);
        }
    }
}
