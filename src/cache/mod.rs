//! LZ4 `.lraw` + `.json` frame cache with 30-day pruning and configurable RAM LRU budget.

// On-disk cache key hashing, `.json` metadata validation, and 30-day file pruning.
pub mod disk;

// Synchronized in-memory compressed storage and configurable uncompressed RAM LRU cache.
pub mod memory;

// Warm `.lraw` cache loading and batched background chunk reader.
pub mod reader;

// Streaming decode consumer persisting `.lraw` payload before `.json` metadata.
pub mod sink;

// High-level `FrameCache` coordinator bridging disk cache, decoder worker, and compositor.
pub mod store;

// Disk cache path/metadata types, directory lookup, and 30-day pruning entry points.
pub use disk::{
    CacheMetadata, CachePaths, DEFAULT_PRUNE_AGE_DAYS, cache_dir_from_env, compute_cache_paths,
    default_cache_dir, prune_cache,
};
// RAM LRU budget constant and compositor wakeup hook type.
pub use memory::{DEFAULT_RAM_CACHE_BUDGET_BYTES, WakeupCallback};
// Primary overlay frame store API.
pub use store::FrameCache;
