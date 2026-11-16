//! Synchronized audio stream probing, `ffmpeg` PCM extraction, lock-free mixing, and `cpal` output.

// Subprocess `ffmpeg` PCM extraction and bounded lookahead audio ring buffers.
mod decode;

// Real-time `cpal` audio output stream lifecycle and background audio decode loader.
mod output;

// Audio stream metadata probing and buffered-versus-streaming policy.
pub mod probe;

// Lock-free PCM mixer, click-free fade slew, and monotonic A/V sync clock state.
pub mod sync;

// Re-export primary runtime audio engine, clock/sync controls, and telemetry snapshots.
pub use output::{AudioEngine, write_cpal_output};
pub use probe::{AudioProbe, AudioStorageMode};
pub use sync::{AVSyncMode, AudioStatsSnapshot, AudioSyncCore, AudioTrackData};
