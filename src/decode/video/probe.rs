//! Video container signature checks and `ffprobe` stream metadata parsing.

// Filesystem reads, standard I/O errors, paths, and subprocess spawning.
use std::{
    fs::File,
    io::{ErrorKind, Read},
    path::Path,
    process::{Command, Stdio},
};

// JSON metadata deserialization for `ffprobe -print_format json`.
use serde::Deserialize;

// Checked pixel byte math, cache errors, and normalized delay timing clamps.
use crate::{decode::checked_argb_byte_len, error::CacheError, playback::PlaybackClock};

/// Default uncompressed RGBA ceiling (`512 MiB`) for storing short video loops in `.lraw`.
pub(crate) const DEFAULT_VIDEO_CACHE_BUDGET_BYTES: usize = 512 * 1024 * 1024;

// Upper frame count ceiling preventing massive `.lraw` writes even for small overlays.
const MAX_CACHED_VIDEO_FRAMES: usize = 1_800;

// Maximum initial header bytes read when sniffing container magic signatures.
const MAGIC_HEADER_BYTES: usize = 200;

// MPEG-TS packet stride between consecutive `0x47` sync bytes.
const MPEG_TS_STRIDE: usize = 188;

/// Probed metadata for a container's primary non-artwork video stream.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VideoProbe
{
    /// Zero-based stream index inside the container selecting the video track (`-map 0:<idx>`).
    pub(crate) video_stream_index: usize,

    /// Display-adjusted frame width in pixels after honoring container rotation metadata.
    pub(crate) display_width: u32,

    /// Display-adjusted frame height in pixels after honoring container rotation metadata.
    pub(crate) display_height: u32,

    /// Best-effort estimate of total frame count reported by stream or duration * FPS.
    pub(crate) estimated_frames: Option<u64>,

    /// Fallback per-frame microsecond delay derived from `avg_frame_rate` or `r_frame_rate`.
    pub(crate) fallback_delay_us: u32,

    /// Hardware-accelerated GPU decoding backend passed to `ffmpeg -hwaccel <mode>`.
    pub(crate) gpu: Option<String>,
}

impl VideoProbe
{
    /// Computes aspect-preserving `(target_w, target_h)` bounded by `base_width` without upscaling.
    pub(crate) fn scaled_dimensions(&self, base_width: u32) -> (u32, u32)
    {
        let target_w = base_width.min(self.display_width).max(1);
        let target_h = ((f64::from(self.display_height) * f64::from(target_w))
            / f64::from(self.display_width))
        .round()
        .max(1.0) as u32;
        (target_w, target_h)
    }

    /// Returns `true` when this video clip fits the bounded short-loop `.lraw` cache policy.
    pub(crate) fn should_cache(
        &self,
        base_width: u32,
        max_frames: usize,
        video_cache_bytes: usize,
    ) -> bool
    {
        if video_cache_bytes == 0
        {
            return false;
        }

        let estimated = self.estimated_frames.and_then(|n| usize::try_from(n).ok());
        let frame_count = match (max_frames, estimated)
        {
            (cap, Some(est)) if cap > 0 => cap.min(est),
            (cap, None) if cap > 0 => cap,
            (0, Some(est)) => est,
            (0, None) => return false,
            _ => return false,
        };

        if frame_count == 0 || frame_count > MAX_CACHED_VIDEO_FRAMES
        {
            return false;
        }

        let (target_w, target_h) = self.scaled_dimensions(base_width);
        let Ok(per_frame_bytes) = checked_argb_byte_len(target_w, target_h)
        else
        {
            return false;
        };

        per_frame_bytes
            .checked_mul(frame_count)
            .is_some_and(|total| total <= video_cache_bytes)
    }
}

/// Top-level JSON payload emitted by `ffprobe -show_format -show_streams -print_format json`.
#[derive(Debug, Deserialize)]
struct FfprobeOutput
{
    // Container-level metadata block holding duration fallback.
    format: Option<FfprobeFormat>,

    // Ordered array of video, audio, and auxiliary streams.
    #[serde(default)]
    streams: Vec<FfprobeStream>,
}

/// Container-level format fields parsed from `ffprobe` JSON.
#[derive(Debug, Deserialize)]
struct FfprobeFormat
{
    // Total container duration formatted as decimal seconds string.
    duration: Option<String>,
}

/// Per-stream descriptors emitted by `ffprobe` JSON.
#[derive(Debug, Deserialize)]
struct FfprobeStream
{
    // Container stream index used for explicit `-map 0:<index>`.
    index: usize,

    // Stream media kind (`"video"`, `"audio"`, `"subtitle"`, etc.).
    codec_type: Option<String>,

    // Raw coded pixel width prior to display rotation.
    width: Option<u32>,

    // Raw coded pixel height prior to display rotation.
    height: Option<u32>,

    // Rational average frame rate (e.g. `"30000/1001"`).
    avg_frame_rate: Option<String>,

    // Nominal stream base frame rate fallback when `avg_frame_rate` is `0/0`.
    r_frame_rate: Option<String>,

    // Decimal stream frame count string when indexed cleanly in container headers.
    nb_frames: Option<String>,

    // Stream-level duration formatted as decimal seconds string.
    duration: Option<String>,

    // Stream flags identifying static MP4/MKV cover art (`attached_pic == 1`).
    #[serde(default)]
    disposition: FfprobeDisposition,

    // Optional stream tags (legacy `"rotate"` metadata).
    tags: Option<FfprobeTags>,

    // Side data descriptors carrying modern MP4 displaymatrix rotation angles.
    #[serde(default)]
    side_data_list: Vec<FfprobeSideData>,
}

/// Disposition bits emitted per stream by `ffprobe`.
#[derive(Debug, Default, Deserialize)]
struct FfprobeDisposition
{
    // Non-zero when stream holds attached cover art rather than motion video.
    #[serde(default)]
    attached_pic: u8,
}

/// Optional stream tag dictionary parsed for legacy rotation strings.
#[derive(Debug, Deserialize)]
struct FfprobeTags
{
    // Legacy container rotation degrees string (e.g. `"90"`).
    rotate: Option<String>,
}

/// Per-stream side data entry checked for displaymatrix rotation angles.
#[derive(Debug, Deserialize)]
struct FfprobeSideData
{
    // Rotation angle in clockwise/counter-clockwise degrees (`-90`, `90`, `180`, etc.).
    rotation: Option<f64>,
}

/// Returns `true` when `path` matches known video file extensions or container magic bytes.
pub(crate) fn has_video_signature(path: &Path) -> Result<bool, CacheError>
{
    if let Some(ext) = path.extension().and_then(|s| s.to_str())
        && matches!(
            ext.to_ascii_lowercase().as_str(),
            "mp4" | "m4v" | "mov" | "webm" | "mkv" | "avi" | "flv" | "ogv" | "ts" | "mts" | "m2ts"
        )
    {
        return Ok(true);
    }

    let mut file = File::open(path).map_err(|source| CacheError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut header = [0_u8; MAGIC_HEADER_BYTES];
    let n = file.read(&mut header).map_err(|source| CacheError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(matches_video_magic(&header[..n]))
}

/// Probes `path` via `ffprobe` to verify a decodable video stream and extract geometry and timing.
pub(crate) fn probe_video(path: &Path) -> Result<VideoProbe, CacheError>
{
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| {
            let reason = if err.kind() == ErrorKind::NotFound
            {
                String::from("Video playback requires `ffprobe` and `ffmpeg` installed on PATH")
            }
            else
            {
                format!("Failed spawning ffprobe: {err}")
            };
            CacheError::Decode {
                path: path.to_path_buf(),
                reason,
            }
        })?;

    if !output.status.success()
    {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let reason = if stderr.is_empty()
        {
            format!("ffprobe exited with status {}", output.status)
        }
        else
        {
            format!("ffprobe failed: {stderr}")
        };
        return Err(CacheError::Decode {
            path: path.to_path_buf(),
            reason,
        });
    }

    let parsed: FfprobeOutput =
        serde_json::from_slice(&output.stdout).map_err(|err| CacheError::Decode {
            path: path.to_path_buf(),
            reason: format!("Invalid ffprobe JSON metadata: {err}"),
        })?;

    build_video_probe(path, parsed)
}

/// Converts deserialized `ffprobe` metadata into a validated `VideoProbe`.
fn build_video_probe(path: &Path, output: FfprobeOutput) -> Result<VideoProbe, CacheError>
{
    let video = output
        .streams
        .iter()
        .find(|stream| {
            stream.codec_type.as_deref() == Some("video")
                && stream.disposition.attached_pic == 0
                && stream.width.unwrap_or(0) > 0
                && stream.height.unwrap_or(0) > 0
        })
        .ok_or_else(|| CacheError::Decode {
            path: path.to_path_buf(),
            reason: String::from("Container did not contain a valid video stream"),
        })?;

    let coded_w = video.width.unwrap_or(1);
    let coded_h = video.height.unwrap_or(1);
    let (display_width, display_height) = apply_display_rotation(video, coded_w, coded_h);

    let fps = parse_stream_fps(video);
    let fallback_delay_us =
        PlaybackClock::normalize_cache_delay_us(fps.map(|rate| 1_000_000.0 / rate));

    let duration_secs = video
        .duration
        .as_deref()
        .and_then(parse_positive_f64)
        .or_else(|| {
            output
                .format
                .as_ref()
                .and_then(|fmt| fmt.duration.as_deref())
                .and_then(parse_positive_f64)
        });

    let nb_frames = video
        .nb_frames
        .as_deref()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&count| count > 0);

    let estimated_frames = nb_frames.or_else(|| {
        duration_secs.zip(fps).and_then(|(secs, rate)| {
            let approx = (secs * rate).ceil();
            (approx.is_finite() && approx >= 1.0).then_some(approx as u64)
        })
    });

    Ok(VideoProbe {
        video_stream_index: video.index,
        display_width,
        display_height,
        estimated_frames,
        fallback_delay_us,
        gpu: Some(String::from("auto")),
    })
}

/// Checks leading magic bytes for ISO BMFF, EBML/WebM/MKV, RIFF AVI, FLV, Ogg, or MPEG-TS.
fn matches_video_magic(bytes: &[u8]) -> bool
{
    let is_ftyp = bytes.len() >= 8 && &bytes[4..8] == b"ftyp";
    let is_ebml = bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]);
    let is_avi = bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"AVI ";
    let is_flv = bytes.starts_with(b"FLV");
    let is_ogg = bytes.starts_with(b"OggS");
    let is_ts = bytes.len() > MPEG_TS_STRIDE && bytes[0] == 0x47 && bytes[MPEG_TS_STRIDE] == 0x47;

    is_ftyp || is_ebml || is_avi || is_flv || is_ogg || is_ts
}

/// Swaps width and height when container rotation side data or tags specify a quarter turn.
fn apply_display_rotation(stream: &FfprobeStream, width: u32, height: u32) -> (u32, u32)
{
    let angle = stream
        .side_data_list
        .iter()
        .find_map(|sd| sd.rotation)
        .or_else(|| {
            stream
                .tags
                .as_ref()
                .and_then(|tags| tags.rotate.as_deref())
                .and_then(|val| val.trim().parse::<f64>().ok())
        });

    let swaps = angle.is_some_and(|deg| {
        let quarter = deg.rem_euclid(180.0);
        (quarter - 90.0).abs() < 1.0
    });

    if swaps
    {
        (height, width)
    }
    else
    {
        (width, height)
    }
}

/// Parses rational `avg_frame_rate` or `r_frame_rate` (`"num/den"`) into frames per second.
fn parse_stream_fps(stream: &FfprobeStream) -> Option<f64>
{
    stream
        .avg_frame_rate
        .as_deref()
        .and_then(parse_rational)
        .or_else(|| stream.r_frame_rate.as_deref().and_then(parse_rational))
}

/// Evaluates an `"N/D"` rational string into a positive finite `f64`.
fn parse_rational(raw: &str) -> Option<f64>
{
    let (num_str, den_str) = raw.trim().split_once('/')?;
    let num: f64 = num_str.parse().ok()?;
    let den: f64 = den_str.parse().ok()?;
    if !num.is_finite() || !den.is_finite() || num <= 0.0 || den <= 0.0
    {
        return None;
    }
    Some(num / den)
}

/// Parses a decimal string into a positive finite floating-point number.
fn parse_positive_f64(raw: &str) -> Option<f64>
{
    raw.trim()
        .parse::<f64>()
        .ok()
        .filter(|val| val.is_finite() && *val > 0.0)
}
