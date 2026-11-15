//! Synchronized in-memory compressed chunks, 128 MiB uncompressed RAM LRU cache, and batch signals.

// Collections and thread-safe mutex coordination.
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, MutexGuard},
};

/// Default upper limit in bytes for keeping uncompressed `ARGB8888` frames in RAM (128 MiB).
pub const DEFAULT_RAM_CACHE_BUDGET_BYTES: usize = 128 * 1024 * 1024;

/// Optional wakeup callback invoked when background decoding appends frames or finishes.
pub type WakeupCallback = Arc<dyn Fn() + Send + Sync>;

/// Byte offset and compressed chunk length of one LZ4 frame stored inside `.lraw`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChunkSpan
{
    /// Byte offset from the beginning of the `.lraw` file after the magic header.
    pub(crate) offset: u64,

    /// Compressed chunk length in bytes.
    pub(crate) len: usize,
}

/// Interior synchronized storage holding frame chunk locations and the conditional LRU RAM cache.
pub(crate) struct SharedFrames
{
    /// Stored base frame width in pixels.
    pub(crate) width: u32,

    /// Stored base frame height in pixels.
    pub(crate) height: u32,

    /// Exact expected byte length of one uncompressed `ARGB8888` frame.
    pub(crate) frame_byte_len: usize,

    /// Temporary in-memory LZ4-compressed frames retained only while decoding cold without committed `.lraw`.
    pub(crate) compressed_frames: Vec<Vec<u8>>,

    /// Byte spans into the committed `.lraw` file for lazy chunk streaming.
    pub(crate) disk_spans: Vec<ChunkSpan>,

    /// Normalized millisecond delays parallel to `compressed_frames` or `disk_spans`.
    pub(crate) delays_ms: Vec<u32>,

    /// Set once source decoding or disk cache validation finishes.
    pub(crate) decode_done: bool,

    /// One-shot decoded frame 0 preserved across open until first render when RAM caching is disabled.
    pub(crate) initial_frame0: Option<Arc<Vec<u8>>>,

    /// Uncompressed premultiplied `ARGB8888` frames kept within `ram_budget_bytes`.
    pub(crate) ram_slots: HashMap<usize, Arc<Vec<u8>>>,

    // Least-recently-used eviction order (front = oldest, back = most recently used).
    lru_order: VecDeque<usize>,

    /// Total uncompressed pixel bytes currently retained inside `ram_slots`.
    pub(crate) ram_used_bytes: usize,

    // Maximum byte budget allowed for `ram_slots`.
    ram_budget_bytes: usize,
}

impl SharedFrames
{
    /// Initializes synchronized frame storage pre-populated with frame 0 from a cold decode stream.
    #[expect(clippy::too_many_arguments)]
    pub(crate) fn with_first_frame(
        width: u32,
        height: u32,
        frame_byte_len: usize,
        ram_budget_bytes: usize,
        first_compressed: Vec<u8>,
        first_delay_ms: u32,
        first_argb: Vec<u8>,
        decode_done: bool,
    ) -> Arc<Mutex<Self>>
    {
        let mut state = Self {
            width,
            height,
            frame_byte_len,
            compressed_frames: vec![first_compressed],
            disk_spans: Vec::new(),
            delays_ms: vec![first_delay_ms],
            decode_done,
            initial_frame0: None,
            ram_slots: HashMap::new(),
            lru_order: VecDeque::new(),
            ram_used_bytes: 0,
            ram_budget_bytes,
        };
        state.seed_first_frame(first_argb);
        Arc::new(Mutex::new(state))
    }

    /// Initializes synchronized frame storage directly from a validated `.lraw` + `.json` warm cache.
    pub(crate) fn with_warm_disk(
        width: u32,
        height: u32,
        frame_byte_len: usize,
        ram_budget_bytes: usize,
        disk_spans: Vec<ChunkSpan>,
        delays_ms: Vec<u32>,
        first_argb: Vec<u8>,
    ) -> Arc<Mutex<Self>>
    {
        let mut state = Self {
            width,
            height,
            frame_byte_len,
            compressed_frames: Vec::new(),
            disk_spans,
            delays_ms,
            decode_done: true,
            initial_frame0: None,
            ram_slots: HashMap::new(),
            lru_order: VecDeque::new(),
            ram_used_bytes: 0,
            ram_budget_bytes,
        };
        state.seed_first_frame(first_argb);
        Arc::new(Mutex::new(state))
    }

    /// Returns whether all currently known frames fit decoded inside `ram_budget_bytes`.
    pub(crate) fn fits_ram_budget(&self) -> bool
    {
        self.delays_ms
            .len()
            .checked_mul(self.frame_byte_len)
            .is_some_and(|total| total <= self.ram_budget_bytes)
    }

    /// Releases `ram_slots` immediately when total decoded animation size exceeds `ram_budget_bytes`.
    pub(crate) fn evict_ram_if_oversized(&mut self)
    {
        if !self.fits_ram_budget() && (!self.ram_slots.is_empty() || self.ram_used_bytes > 0)
        {
            self.ram_slots = HashMap::new();
            self.lru_order = VecDeque::new();
            self.ram_used_bytes = 0;
        }
    }

    /// Computes total heap bytes currently pinned in `compressed_frames`.
    pub(crate) fn compressed_ram_bytes(&self) -> usize
    {
        self.compressed_frames.iter().map(Vec::len).sum()
    }

    // Stores initial decoded frame 0 into `ram_slots` only when the loop fits in RAM budget.
    fn seed_first_frame(&mut self, first_argb: Vec<u8>)
    {
        let frame0 = Arc::new(first_argb);
        if self.fits_ram_budget()
        {
            self.insert_ram_frame(0, frame0);
        }
        else
        {
            self.initial_frame0 = Some(frame0);
        }
    }

    /// Inserts or promotes an uncompressed frame in `ram_slots` within `ram_budget_bytes`.
    pub(crate) fn insert_ram_frame(&mut self, index: usize, pixels: Arc<Vec<u8>>)
    {
        if !self.fits_ram_budget()
        {
            self.evict_ram_if_oversized();
            return;
        }

        let byte_len = pixels.len();
        if byte_len > self.ram_budget_bytes
        {
            return;
        }

        if self.ram_slots.contains_key(&index)
        {
            self.promote_lru(index);
            return;
        }

        while self.ram_used_bytes + byte_len > self.ram_budget_bytes
        {
            let Some(evict_idx) = self.lru_order.pop_front()
            else
            {
                break;
            };
            if let Some(removed) = self.ram_slots.remove(&evict_idx)
            {
                self.ram_used_bytes = self.ram_used_bytes.saturating_sub(removed.len());
            }
        }

        self.ram_used_bytes += byte_len;
        self.ram_slots.insert(index, pixels);
        self.lru_order.push_back(index);
    }

    /// Moves an already-cached frame index to the MRU tail.
    pub(crate) fn promote_lru(&mut self, index: usize)
    {
        if let Some(pos) = self.lru_order.iter().position(|&item| item == index)
        {
            self.lru_order.remove(pos);
            self.lru_order.push_back(index);
        }
    }
}

/// Flushes a batch of `(lz4_chunk, delay_ms)` into `SharedFrames`, optionally marking completion.
/// Appends a batch of compressed chunks and delays to shared state, pruning RAM cache if oversized.
pub(crate) fn append_batch(
    shared: &Arc<Mutex<SharedFrames>>,
    batch: &mut Vec<(Vec<u8>, u32)>,
    done: bool,
    wakeup: Option<&WakeupCallback>,
)
{
    let has_items = !batch.is_empty();
    if has_items || done
    {
        let mut guard = lock_mutex(shared);
        for (chunk, delay) in batch.drain(..)
        {
            guard.compressed_frames.push(chunk);
            guard.delays_ms.push(delay);
        }
        if done
        {
            guard.decode_done = true;
        }
        guard.evict_ram_if_oversized();
    }

    if (has_items || done)
        && let Some(notify) = wakeup
    {
        notify();
    }
}

/// Switches shared playback state from remaining batch chunks to on-disk `.lraw` spans.
pub(crate) fn finish_with_disk(
    shared: &Arc<Mutex<SharedFrames>>,
    remaining_batch: &mut Vec<(Vec<u8>, u32)>,
    disk_spans: Vec<ChunkSpan>,
    wakeup: Option<&WakeupCallback>,
)
{
    {
        let mut guard = lock_mutex(shared);
        for (_chunk, delay) in remaining_batch.drain(..)
        {
            guard.delays_ms.push(delay);
        }
        guard.disk_spans = disk_spans;
        guard.compressed_frames = Vec::new();
        guard.decode_done = true;
        guard.evict_ram_if_oversized();
    }

    if let Some(notify) = wakeup
    {
        notify();
    }
}

/// Acquires any mutex cleanly without panicking on mutex poison.
pub(crate) fn lock_mutex<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T>
{
    match mutex.lock()
    {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
