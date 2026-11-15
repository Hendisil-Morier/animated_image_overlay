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

/// Interior synchronized storage holding compressed frames and the LRU uncompressed cache.
pub(crate) struct SharedFrames
{
    /// Stored base frame width in pixels.
    pub(crate) width: u32,

    /// Stored base frame height in pixels.
    pub(crate) height: u32,

    /// Exact expected byte length of one uncompressed `ARGB8888` frame.
    pub(crate) frame_byte_len: usize,

    /// LZ4-compressed frames in playback order.
    pub(crate) compressed_frames: Vec<Vec<u8>>,

    /// Normalized millisecond delays parallel to `compressed_frames`.
    pub(crate) delays_ms: Vec<u32>,

    /// Set once source decoding or disk cache streaming finishes.
    pub(crate) decode_done: bool,

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
    /// Initializes synchronized frame storage pre-populated with frame 0 in compressed and RAM state.
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
            delays_ms: vec![first_delay_ms],
            decode_done,
            ram_slots: HashMap::new(),
            lru_order: VecDeque::new(),
            ram_used_bytes: 0,
            ram_budget_bytes,
        };
        state.insert_ram_frame(0, Arc::new(first_argb));
        Arc::new(Mutex::new(state))
    }

    /// Inserts or promotes an uncompressed frame in `ram_slots` within `ram_budget_bytes`.
    pub(crate) fn insert_ram_frame(&mut self, index: usize, pixels: Arc<Vec<u8>>)
    {
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
    }

    if (has_items || done)
        && let Some(notify) = wakeup
    {
        notify();
    }
}

/// Sets `decode_done = true` safely and triggers any registered event-loop wakeup callback.
pub(crate) fn mark_decode_done(shared: &Arc<Mutex<SharedFrames>>, wakeup: Option<&WakeupCallback>)
{
    lock_mutex(shared).decode_done = true;
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
