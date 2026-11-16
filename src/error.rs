//! Domain error hierarchy for the animated Wayland overlay.
//!
//! Each subsystem exposes a dedicated error enum built with `thiserror`,
//! consolidated under `OverlayError` at the application boundary.

// Standard path type for diagnostic error payloads.
use std::path::PathBuf;

// Derive macro for structured Display and Error implementations.
use thiserror::Error;

/// Errors returned by persistent JSON configuration loading and saving.
#[derive(Debug, Error)]
pub enum ConfigError
{
    /// Failed to resolve the user's home or XDG config directory.
    #[error("Unable to resolve config directory from environment")]
    DirectoryUnavailable,

    /// Target configuration path does not have a valid parent directory.
    #[error("Config path lacks a parent directory: {path}")]
    MissingParent
    {
        /// Configuration file path missing a parent directory.
        path: PathBuf,
    },

    /// Filesystem I/O error while accessing the configuration file or directory.
    #[error("Config I/O failure at {path}: {source}")]
    Io
    {
        /// Filesystem path involved in the failed operation.
        path: PathBuf,

        /// Underlying OS I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Advisory file lock acquisition or release failure.
    #[error("Config lock failure at {path}: {source}")]
    Lock
    {
        /// Lock file path that could not be acquired or released.
        path: PathBuf,

        /// Underlying OS I/O error.
        #[source]
        source: std::io::Error,
    },

    /// JSON serialization error when persisting updated state entries.
    #[error("Config JSON serialization failed for {path}: {source}")]
    Serialize
    {
        /// Target configuration file path.
        path: PathBuf,

        /// Underlying serializer failure.
        #[source]
        source: serde_json::Error,
    },
}

/// Errors returned by source image decoding and disk/RAM frame cache management.
#[derive(Debug, Error)]
pub enum CacheError
{
    /// Failed to resolve the user's home or XDG cache directory.
    #[error("Unable to resolve cache directory from environment")]
    DirectoryUnavailable,

    /// Cache artifact path lacks a valid parent directory.
    #[error("Cache path lacks a parent directory: {path}")]
    MissingParent
    {
        /// Artifact path missing a parent directory.
        path: PathBuf,
    },

    /// Filesystem I/O failure while reading, writing, or pruning cache files.
    #[error("Cache I/O failure at {path}: {source}")]
    Io
    {
        /// Filesystem path involved in the failed cache operation.
        path: PathBuf,

        /// Underlying OS I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Metadata serialization or deserialization failure.
    #[error("Cache metadata JSON error at {path}: {source}")]
    Json
    {
        /// Metadata `.json` file path.
        path: PathBuf,

        /// Underlying JSON parser or serializer error.
        #[source]
        source: serde_json::Error,
    },

    /// Metadata schema validation failure (dimensions, delays, or offsets mismatch).
    #[error("Corrupt cache metadata at {path}: {reason}")]
    CorruptMetadata
    {
        /// Metadata `.json` file path.
        path: PathBuf,

        /// Specific invariant violation detected in the metadata payload.
        reason: String,
    },

    /// Failed to decode a source image or animation frame.
    #[error("Image decode failure for {path}: {reason}")]
    Decode
    {
        /// Source image file path.
        path: PathBuf,

        /// Human-readable decoder error description.
        reason: String,
    },

    /// Failed to compress or decompress cached pixel data via LZ4.
    #[error("LZ4 cache codec error at {path} (frame {frame_index}): {reason}")]
    Codec
    {
        /// Cache `.lraw` data file path.
        path: PathBuf,

        /// Zero-based frame index associated with the failure.
        frame_index: usize,

        /// Underlying LZ4 codec failure message.
        reason: String,
    },

    /// Pixel stride or buffer byte count overflowed `usize`.
    #[error("Image dimensions overflow buffer size: {width}x{height}")]
    DimensionOverflow
    {
        /// Target pixel width.
        width: u32,

        /// Target pixel height.
        height: u32,
    },

    /// Asynchronous cache decoding was stopped cleanly due to application shutdown.
    #[error("Background frame decoding cancelled during shutdown")]
    Cancelled,
}

/// Errors returned by Wayland protocol binding, SHM buffer management, and event loop setup.
#[derive(Debug, Error)]
pub enum WaylandError
{
    /// Failed to establish a connection to the active Wayland display server.
    #[error("Failed to connect to Wayland display: {0}")]
    Connection(String),

    /// Required Wayland global or protocol interface was not advertised by the compositor.
    #[error("Missing required Wayland protocol `{interface}`: {detail}")]
    MissingProtocol
    {
        /// Wayland interface name (such as `zwlr_layer_shell_v1`).
        interface: &'static str,

        /// Context describing why binding failed.
        detail: String,
    },

    /// Requested monitor index or connector name does not match any active output.
    #[error("Monitor `{selector}` did not match any active Wayland output")]
    MonitorNotFound
    {
        /// User-provided monitor index or connector name.
        selector: String,
    },

    /// Shared-memory pool creation or buffer allocation failure.
    #[error("Wayland shared-memory pool failure: {0}")]
    ShmPool(String),

    /// Wayland event dispatch or `calloop` event loop runtime error.
    #[error("Wayland event loop error: {0}")]
    EventLoop(String),
}

/// Errors returned by audio stream probing, `ffmpeg` PCM extraction, and `cpal` playback.
#[derive(Debug, Error)]
pub enum AudioError
{
    /// Required external media tool (`ffmpeg` or `ffprobe`) was missing from `PATH`.
    #[error("Media audio decoding requires `{tool}` installed on PATH")]
    UnavailableTool
    {
        /// Missing CLI executable name.
        tool: &'static str,
    },

    /// Audio stream metadata probing failed or yielded malformed output.
    #[error("Audio probe failed for {path}: {reason}")]
    ProbeFailed
    {
        /// Audio or video container file path.
        path: PathBuf,

        /// Failure detail reported by probing.
        reason: String,
    },

    /// Extracting PCM samples via `ffmpeg` failed or returned truncated data.
    #[error("Audio decode failure for {path}: {reason}")]
    DecodeFailed
    {
        /// Audio or video container file path.
        path: PathBuf,

        /// Human-readable decoder failure detail.
        reason: String,
    },

    /// System audio output device or stream format is unavailable.
    #[error("Audio output device unavailable: {0}")]
    DeviceUnavailable(String),

    /// Playback stream construction or runtime execution failed.
    #[error("Audio stream failure: {0}")]
    StreamFailed(String),
}

/// Top-level error type unifying CLI validation, logging, config, cache, audio, and Wayland failures.
#[derive(Debug, Error)]
pub enum OverlayError
{
    /// Invalid CLI flag combination or out-of-range numeric parameter.
    #[error("Invalid command-line argument: {0}")]
    CliValidation(String),

    /// Target image file does not exist or cannot be opened as a regular file.
    #[error("Input image file not found: {0}")]
    MissingInputFile(PathBuf),

    /// Companion or override audio file does not exist or cannot be opened as a regular file.
    #[error("Input audio file not found: {0}")]
    MissingAudioFile(PathBuf),

    /// Failed to initialize console or non-blocking file diagnostics.
    #[error("Failed to initialize logging subsystem: {0}")]
    LoggingInit(String),

    /// Configuration subsystem failure.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// Cache or image decoding subsystem failure.
    #[error(transparent)]
    Cache(#[from] CacheError),

    /// Audio probing, decoding, or playback subsystem failure.
    #[error(transparent)]
    Audio(#[from] AudioError),

    /// Wayland display, protocol, or event loop failure.
    #[error(transparent)]
    Wayland(#[from] WaylandError),
}
