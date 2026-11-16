//! Audio container probing via `ffprobe` and buffered-versus-streaming RAM budget classification.

// I/O errors, paths, and subprocess spawning.
use std::{
    io::ErrorKind,
    path::Path,
    process::{Command, Stdio},
};

// JSON metadata deserialization for `ffprobe -print_format json`.
use serde::Deserialize;

// Typed audio subsystem error hierarchy.
use crate::error::AudioError;

/// Maximum uncompressed `f32` PCM bytes (~`64 MiB`, ~3 min stereo @ 48 kHz) loaded completely into RAM.
pub const AUDIO_RAM_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;

// Sample byte size per single decoded interleaved `f32` channel sample.
const PCM_F32_BYTES: usize = 4;

/// Audio sample storage strategy derived from container stream duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioStorageMode
{
    /// Preload the complete track into RAM for instantaneous zero-gap loop wrap and seekless replay.
    Buffered,

    /// Stream long tracks continuously via a bounded lookahead PCM ring buffer (~1 s RAM).
    Streaming,
}

/// Validated container metadata describing the primary playable audio stream.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioProbe
{
    /// Zero-based container stream index (`-map 0:<idx>`).
    pub stream_index: usize,

    /// Source reported duration in seconds when present in stream or container headers.
    pub duration_secs: Option<f64>,
}

impl AudioProbe
{
    /// Probes `path` via `ffprobe` and returns `None` cleanly when no audio stream is present.
    pub fn probe_optional(path: &Path) -> Result<Option<Self>, AudioError>
    {
        let output = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-print_format",
                "json",
                "-select_streams",
                "a:0",
                "-show_format",
                "-show_streams",
            ])
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|err| {
                if err.kind() == ErrorKind::NotFound
                {
                    AudioError::UnavailableTool { tool: "ffprobe" }
                }
                else
                {
                    AudioError::ProbeFailed {
                        path: path.to_path_buf(),
                        reason: err.to_string(),
                    }
                }
            })?;

        if !output.status.success()
        {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(AudioError::ProbeFailed {
                path: path.to_path_buf(),
                reason: if stderr.is_empty()
                {
                    format!("ffprobe exited with status {}", output.status)
                }
                else
                {
                    stderr
                },
            });
        }

        let parsed: FfprobeAudioOutput =
            serde_json::from_slice(&output.stdout).map_err(|err| AudioError::ProbeFailed {
                path: path.to_path_buf(),
                reason: format!("Invalid ffprobe JSON metadata: {err}"),
            })?;

        let Some(stream) = parsed.streams.first()
        else
        {
            return Ok(None);
        };

        let duration_secs = stream
            .duration
            .as_deref()
            .and_then(parse_positive_f64)
            .or_else(|| {
                parsed
                    .format
                    .as_ref()
                    .and_then(|fmt| fmt.duration.as_deref())
                    .and_then(parse_positive_f64)
            });

        Ok(Some(Self {
            stream_index: stream.index,
            duration_secs,
        }))
    }

    /// Probes `path` via `ffprobe` and requires an active audio stream to exist.
    pub fn probe_required(path: &Path) -> Result<Self, AudioError>
    {
        Self::probe_optional(path)?.ok_or_else(|| AudioError::ProbeFailed {
            path: path.to_path_buf(),
            reason: String::from("Input audio file lacked a decodable audio stream"),
        })
    }

    /// Chooses `Buffered` for short tracks fitting `AUDIO_RAM_CACHE_MAX_BYTES`, `Streaming` otherwise.
    pub fn storage_mode(
        &self,
        sample_rate: u32,
        channels: u16,
        cap_duration_secs: Option<f64>,
    ) -> AudioStorageMode
    {
        let effective_secs = match (self.duration_secs, cap_duration_secs)
        {
            (Some(src), Some(cap)) => Some(src.min(cap)),
            (None, Some(cap)) => Some(cap),
            (Some(src), None) => Some(src),
            (None, None) => None,
        };

        let Some(secs) = effective_secs.filter(|s| s.is_finite() && *s > 0.0)
        else
        {
            return AudioStorageMode::Streaming;
        };

        let bytes_per_sec = f64::from(sample_rate) * f64::from(channels) * (PCM_F32_BYTES as f64);
        let estimated_bytes = secs * bytes_per_sec;
        if estimated_bytes.is_finite() && estimated_bytes <= (AUDIO_RAM_CACHE_MAX_BYTES as f64)
        {
            AudioStorageMode::Buffered
        }
        else
        {
            AudioStorageMode::Streaming
        }
    }
}

/// Top-level JSON output parsed cleanly from `ffprobe -select_streams a:0`.
#[derive(Debug, Deserialize)]
struct FfprobeAudioOutput
{
    // Format-level metadata fallback supplying container duration.
    format: Option<FfprobeAudioFormat>,

    // Selected audio streams array (`len <= 1` with `-select_streams a:0`).
    #[serde(default)]
    streams: Vec<FfprobeAudioStream>,
}

/// Format duration wrapper parsed from `ffprobe`.
#[derive(Debug, Deserialize)]
struct FfprobeAudioFormat
{
    // Decimal seconds string describing overall media duration.
    duration: Option<String>,
}

/// Selected audio stream descriptor parsed from `ffprobe`.
#[derive(Debug, Deserialize)]
struct FfprobeAudioStream
{
    // Zero-based container stream index.
    index: usize,

    // Stream-specific duration string overriding container fallback when available.
    duration: Option<String>,
}

/// Parses a decimal seconds string into a positive finite `f64`.
fn parse_positive_f64(raw: &str) -> Option<f64>
{
    raw.trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v > 0.0)
}
