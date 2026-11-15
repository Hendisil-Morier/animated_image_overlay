//! On-disk `.lraw`/`.json` cache layout, key hashing, metadata validation, and 30-day pruning.

// Standard filesystem queries, hashing formatter, and wall-clock timestamps.
use std::{
    env,
    fmt::Write as _,
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

// Serialization for `.json` frame metadata markers.
use serde::{Deserialize, Serialize};
// Cryptographic digest for deterministic short cache filenames.
use sha2::{Digest, Sha256};

// Shared atomic writers, canonical key builder, and typed cache errors.
use crate::{
    atomic_fs,
    config::ConfigStore,
    error::{CacheError, OverlayError},
};

/// Default age in days before unused cache artifacts are removed during startup pruning.
pub const DEFAULT_PRUNE_AGE_DAYS: u64 = 30;

// Length in hex characters retained from SHA-256 digests for cache artifact filenames.
const CACHE_DIGEST_HEX_LEN: usize = 16;

// Cache payload format version prefix isolating Rust `.lraw` files from legacy `.zraw`.
const CACHE_KEY_PREFIX: &str = "v2-lraw-argb8888";

// Base application directory name inside XDG cache storage.
const CACHE_APP_DIR: &str = "apng-overlay";

// Number of seconds in one calendar day for 30-day pruning calculations.
const SECONDS_PER_DAY: u64 = 86_400;

/// Pair of `.lraw` frame blob and `.json` metadata marker paths inside `cache_dir`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachePaths
{
    /// Chunked LZ4-compressed `ARGB8888` frame data file.
    pub lraw_path: PathBuf,

    /// Metadata JSON file committed strictly after `lraw_path`.
    pub meta_path: PathBuf,
}

/// On-disk JSON metadata describing stored frame dimensions, delays, and chunk byte sizes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheMetadata
{
    /// Stored frame width in pixels.
    pub w: u32,

    /// Stored frame height in pixels.
    pub h: u32,

    /// Per-frame display durations in milliseconds (`>= 10 ms`).
    pub delays: Vec<u32>,

    /// Per-frame compressed byte lengths sequential inside `.lraw`.
    pub sizes: Vec<usize>,
}

/// Computes deterministic `.lraw` and `.json` cache paths for `(path, width, max_frames)`.
pub fn compute_cache_paths(
    cache_dir: &Path,
    image_path: &Path,
    base_width: u32,
    max_frames: usize,
) -> Result<CachePaths, OverlayError>
{
    let meta = fs::metadata(image_path).map_err(|source| CacheError::Io {
        path: image_path.to_path_buf(),
        source,
    })?;

    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|dur| dur.as_nanos())
        .unwrap_or(0);

    let key_path = ConfigStore::state_key(image_path)?;
    let mut hasher = Sha256::new();
    hasher.update(CACHE_KEY_PREFIX.as_bytes());
    hasher.update(b":");
    hasher.update(key_path.as_bytes());
    hasher.update(b":");
    hasher.update(mtime_ns.to_le_bytes());
    hasher.update(meta.len().to_le_bytes());
    hasher.update(base_width.to_le_bytes());
    hasher.update((max_frames as u64).to_le_bytes());

    let digest = hasher.finalize();
    let byte_count = (CACHE_DIGEST_HEX_LEN / 2).min(digest.len());
    let mut short_digest = String::with_capacity(byte_count * 2);
    for byte in &digest[..byte_count]
    {
        let _ = write!(short_digest, "{byte:02x}");
    }

    Ok(CachePaths {
        lraw_path: cache_dir.join(format!("{short_digest}.lraw")),
        meta_path: cache_dir.join(format!("{short_digest}.json")),
    })
}

/// Resolves the default user cache root (`$XDG_CACHE_HOME/apng-overlay` or `~/.cache/apng-overlay`).
pub fn default_cache_dir() -> Result<PathBuf, CacheError>
{
    let cache_root = if let Some(xdg) = env::var_os("XDG_CACHE_HOME")
        && !xdg.is_empty()
    {
        PathBuf::from(xdg)
    }
    else
    {
        let home = env::var_os("HOME")
            .filter(|path| !path.is_empty())
            .ok_or(CacheError::DirectoryUnavailable)?;
        PathBuf::from(home).join(".cache")
    };

    Ok(cache_root.join(CACHE_APP_DIR))
}

/// Removes cache files older than `max_age_days`, deleting `.json` markers before data blobs.
pub fn prune_cache(cache_dir: &Path, max_age_days: u64)
{
    let retention = Duration::from_secs(max_age_days.saturating_mul(SECONDS_PER_DAY));
    let Some(cutoff) = SystemTime::now().checked_sub(retention)
    else
    {
        return;
    };

    let Ok(entries) = fs::read_dir(cache_dir)
    else
    {
        return;
    };

    let mut targets: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_prunable_cache_file(path))
        .collect();

    // Delete `.json` markers before `.lraw`/`.zraw`/`.raw` so orphans never look valid.
    targets.sort_by_key(|path| {
        let is_json = path.extension().and_then(|s| s.to_str()) == Some("json");
        (!is_json, path.clone())
    });

    for path in targets
    {
        let Ok(meta) = fs::metadata(&path)
        else
        {
            continue;
        };
        let Ok(modified) = meta.modified()
        else
        {
            continue;
        };
        if modified < cutoff
        {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Validates `.json` cache metadata schema and verifies `.lraw` covers the declared chunk sizes.
pub(crate) fn load_and_validate_meta(paths: &CachePaths) -> Result<CacheMetadata, CacheError>
{
    let raw_json = fs::read(&paths.meta_path).map_err(|source| CacheError::Io {
        path: paths.meta_path.clone(),
        source,
    })?;

    let meta: CacheMetadata =
        serde_json::from_slice(&raw_json).map_err(|source| CacheError::Json {
            path: paths.meta_path.clone(),
            source,
        })?;

    if meta.w == 0 || meta.h == 0 || meta.delays.is_empty() || meta.delays.len() != meta.sizes.len()
    {
        return Err(CacheError::CorruptMetadata {
            path: paths.meta_path.clone(),
            reason: String::from("Invalid dimensions or mismatched delays/sizes metadata"),
        });
    }

    let mut total_bytes: u64 = 0;
    for &size in &meta.sizes
    {
        if size == 0
        {
            return Err(CacheError::CorruptMetadata {
                path: paths.meta_path.clone(),
                reason: String::from("Zero-byte frame chunk size"),
            });
        }
        total_bytes = total_bytes
            .checked_add(u64::try_from(size).unwrap_or(u64::MAX))
            .ok_or_else(|| CacheError::CorruptMetadata {
                path: paths.meta_path.clone(),
                reason: String::from("Chunk size total overflow"),
            })?;
    }

    let lraw_len = fs::metadata(&paths.lraw_path)
        .map_err(|source| CacheError::Io {
            path: paths.lraw_path.clone(),
            source,
        })?
        .len();

    if lraw_len < total_bytes
    {
        return Err(CacheError::CorruptMetadata {
            path: paths.lraw_path.clone(),
            reason: format!("Truncated .lraw file ({lraw_len} < {total_bytes})"),
        });
    }

    Ok(meta)
}

/// Atomically serializes and commits `.json` cache metadata after `.lraw` is safely on disk.
pub(crate) fn write_cache_metadata(
    meta_path: &Path,
    width: u32,
    height: u32,
    delays: &[u32],
    sizes: &[usize],
) -> Result<(), CacheError>
{
    let meta = CacheMetadata {
        w: width,
        h: height,
        delays: delays.to_vec(),
        sizes: sizes.to_vec(),
    };

    let bytes = serde_json::to_vec(&meta).map_err(|source| CacheError::Json {
        path: meta_path.to_path_buf(),
        source,
    })?;

    atomic_fs::write_bytes(meta_path, ".json", &bytes).map_err(|source| CacheError::Io {
        path: meta_path.to_path_buf(),
        source,
    })
}

/// Updates a cached artifact's mtime to now using stdlib `File::set_modified`.
pub(crate) fn touch_modified(path: &Path)
{
    if let Ok(file) = OpenOptions::new().write(true).open(path)
    {
        let _ = file.set_modified(SystemTime::now());
    }
}

/// Returns `true` when `path` is a regular file with a recognized cache artifact extension.
fn is_prunable_cache_file(path: &Path) -> bool
{
    if !path.is_file()
    {
        return false;
    }

    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("json" | "raw" | "zraw" | "lraw")
    )
}
