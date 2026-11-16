//! Subprocess `ffmpeg` PAM/RGBA + `showinfo` VFR video stream reader with deterministic reaping.

// Buffered I/O, channels, paths, subprocess management, and stderr reader thread.
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, ErrorKind},
    path::{Path, PathBuf},
    process::{Child, ChildStderr, ChildStdout, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
};

// Video stream probe metadata and shared raw RGBA pixel container.
use super::probe::VideoProbe;
use crate::{
    decode::{BYTES_PER_PIXEL, checked_argb_byte_len, pixel::RawRgbaFrame},
    error::CacheError,
    playback::PlaybackClock,
};

// Maximum trailing non-showinfo diagnostic lines preserved for error reporting.
const MAX_STDERR_TAIL: usize = 16;

/// Microsecond presentation timestamp and optional packet duration parsed from `ffmpeg` `showinfo`.
#[derive(Debug, Clone, Copy)]
struct ShowInfoTiming
{
    // Frame presentation timestamp in seconds from stream start.
    pts_secs: Option<f64>,

    // Nominal frame display duration in seconds when carried in packet metadata.
    duration_secs: Option<f64>,
}

/// Parsed binary PAM `P7` RGBA header preceding raw `[R, G, B, A]` pixels.
struct PamFrame
{
    // Decoded pixel width in `P7 WIDTH`.
    width: u32,

    // Decoded pixel height in `P7 HEIGHT`.
    height: u32,

    // Interleaved straight RGBA pixels (`width * height * 4` bytes).
    rgba: Vec<u8>,
}

/// Owns a running `ffmpeg` child process and its `stderr` parser thread, reaping both on drop.
struct FfmpegChildGuard
{
    // Child subprocess handle killed if iterator drops before natural EOF.
    child: Option<Child>,

    // Joined `stderr` line reader delivering `ShowInfoTiming` and trailing errors.
    stderr_thread: Option<JoinHandle<Vec<String>>>,
}

impl FfmpegChildGuard
{
    /// Waits cleanly for child termination on EOF and joins `stderr` diagnostics.
    fn finish(&mut self, path: &Path) -> Result<(), CacheError>
    {
        let tail = self
            .stderr_thread
            .take()
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();

        let Some(mut child) = self.child.take()
        else
        {
            return Ok(());
        };

        let status = child.wait().map_err(|source| CacheError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if status.success()
        {
            return Ok(());
        }

        let summary = if tail.is_empty()
        {
            format!("ffmpeg exited with status {status}")
        }
        else
        {
            format!("ffmpeg failed ({status}): {}", tail.join(" | "))
        };
        Err(CacheError::Decode {
            path: path.to_path_buf(),
            reason: summary,
        })
    }
}

impl Drop for FfmpegChildGuard
{
    /// Terminates and reaps any still-running `ffmpeg` subprocess before returning.
    fn drop(&mut self)
    {
        if let Some(mut child) = self.child.take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(handle) = self.stderr_thread.take()
        {
            let _ = handle.join();
        }
    }
}

/// Streaming video frame iterator yielding scaled straight-RGBA frames and microsecond delays.
pub(crate) struct VideoFrameIterator
{
    // Source file path preserved for contextual decode errors.
    path: PathBuf,

    // Scaled output target width enforced in `-vf scale` without upscaling.
    expected_dims: Option<(u32, u32)>,

    // Buffered `stdout` reader yielding sequential `P7` PAM frames.
    stdout: BufReader<ChildStdout>,

    // Frame timing queue delivered 1:1 from the `stderr` `showinfo` thread.
    timing_rx: Receiver<ShowInfoTiming>,

    // One-frame lookahead enabling exact `pts[i+1] - pts[i]` VFR microsecond delays.
    pending_frame: Option<(PamFrame, ShowInfoTiming)>,

    // Fallback frame duration in microseconds when packet timing omits duration and deltas.
    fallback_delay_us: u32,

    // Set once EOF or fatal stream failure finishes iteration.
    finished: bool,

    // Child process and diagnostic reader cleaned up on drop.
    child_guard: FfmpegChildGuard,
}

/// Spawns `ffmpeg` to decode scaled straight-RGBA PAM frames and microsecond VFR timestamps.
pub(crate) fn open_video_frames(
    path: &Path,
    probe: &VideoProbe,
    base_width: u32,
    max_frames: usize,
) -> Result<VideoFrameIterator, CacheError>
{
    let target_width = base_width.min(probe.display_width).max(1);
    let filter =
        format!("scale='min({target_width},iw)':-1:flags=lanczos,format=rgba,showinfo=checksum=0");
    let map_spec = format!("0:{}", probe.video_stream_index);

    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-nostats", "-loglevel", "info", "-i"])
        .arg(path)
        .args([
            "-map",
            &map_spec,
            "-vf",
            &filter,
            "-fps_mode",
            "passthrough",
        ]);
    if max_frames > 0
    {
        cmd.args(["-frames:v", &max_frames.to_string()]);
    }
    cmd.args(["-c:v", "pam", "-f", "image2pipe", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|err| {
        let reason = if err.kind() == ErrorKind::NotFound
        {
            String::from("Video playback requires `ffmpeg` installed on PATH")
        }
        else
        {
            format!("Failed spawning ffmpeg: {err}")
        };
        CacheError::Decode {
            path: path.to_path_buf(),
            reason,
        }
    })?;

    let stdout = child.stdout.take().ok_or_else(|| CacheError::Decode {
        path: path.to_path_buf(),
        reason: String::from("Failed capturing ffmpeg stdout stream"),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| CacheError::Decode {
        path: path.to_path_buf(),
        reason: String::from("Failed capturing ffmpeg stderr stream"),
    })?;

    let (timing_tx, timing_rx) = mpsc::channel();
    let stderr_thread = thread::Builder::new()
        .name(String::from("aiw-ffmpeg-meta"))
        .spawn(move || read_stderr_timings(stderr, timing_tx))
        .map_err(|source| CacheError::Io {
            path: path.to_path_buf(),
            source,
        })?;

    Ok(VideoFrameIterator {
        path: path.to_path_buf(),
        expected_dims: None,
        stdout: BufReader::new(stdout),
        timing_rx,
        pending_frame: None,
        fallback_delay_us: probe.fallback_delay_us,
        finished: false,
        child_guard: FfmpegChildGuard {
            child: Some(child),
            stderr_thread: Some(stderr_thread),
        },
    })
}

impl VideoFrameIterator
{
    /// Reads the next `PamFrame` and pairs it with its matching `ShowInfoTiming`.
    fn read_next_pair(&mut self) -> Result<Option<(PamFrame, ShowInfoTiming)>, CacheError>
    {
        let Some(frame) = read_pam_frame(&self.path, &mut self.stdout)?
        else
        {
            return Ok(None);
        };

        if let Some((exp_w, exp_h)) = self.expected_dims
            && (frame.width != exp_w || frame.height != exp_h)
        {
            return Err(CacheError::Decode {
                path: self.path.clone(),
                reason: format!(
                    "Video resolution changed mid-stream ({exp_w}x{exp_h} -> {}x{})",
                    frame.width, frame.height
                ),
            });
        }
        self.expected_dims
            .get_or_insert((frame.width, frame.height));

        let timing = self.timing_rx.recv().unwrap_or(ShowInfoTiming {
            pts_secs: None,
            duration_secs: None,
        });
        Ok(Some((frame, timing)))
    }

    /// Computes microsecond delay between `cur` and optional lookahead `next` timestamps.
    fn compute_delay_us(
        &self,
        cur: ShowInfoTiming,
        next: Option<ShowInfoTiming>,
    ) -> Result<u32, CacheError>
    {
        if let (Some(c_pts), Some(n_pts)) = (cur.pts_secs, next.and_then(|n| n.pts_secs))
        {
            let delta = n_pts - c_pts;
            if delta.is_finite() && delta > 0.0
            {
                return Ok(PlaybackClock::normalize_cache_delay_us(Some(
                    delta * 1_000_000.0,
                )));
            }
        }

        if let Some(dur) = cur.duration_secs.filter(|d| d.is_finite() && *d > 0.0)
        {
            return Ok(PlaybackClock::normalize_cache_delay_us(Some(
                dur * 1_000_000.0,
            )));
        }

        Ok(self.fallback_delay_us)
    }

    /// Combines a `PamFrame` and resolved `delay_us` into a standard `RawRgbaFrame`.
    fn into_raw_frame(frame: PamFrame, delay_us: u32) -> RawRgbaFrame
    {
        RawRgbaFrame {
            width: frame.width,
            height: frame.height,
            delay_us,
            rgba: frame.rgba,
        }
    }
}

impl Iterator for VideoFrameIterator
{
    type Item = Result<RawRgbaFrame, CacheError>;

    /// Yields scaled video frames sequentially with 1-frame lookahead for VFR microsecond deltas.
    fn next(&mut self) -> Option<Self::Item>
    {
        if self.finished
        {
            return None;
        }

        if self.pending_frame.is_none()
        {
            match self.read_next_pair()
            {
                Ok(Some(first)) => self.pending_frame = Some(first),
                Ok(None) =>
                {
                    self.finished = true;
                    return self.child_guard.finish(&self.path).err().map(Err);
                }
                Err(err) =>
                {
                    self.finished = true;
                    return Some(Err(err));
                }
            }
        }

        let (cur_frame, cur_timing) = self.pending_frame.take()?;
        let next_timing = match self.read_next_pair()
        {
            Ok(Some(next_pair)) =>
            {
                let timing = next_pair.1;
                self.pending_frame = Some(next_pair);
                Some(timing)
            }
            Ok(None) =>
            {
                self.finished = true;
                if let Err(err) = self.child_guard.finish(&self.path)
                {
                    return Some(Err(err));
                }
                None
            }
            Err(err) =>
            {
                self.finished = true;
                return Some(Err(err));
            }
        };

        let delay_us = match self.compute_delay_us(cur_timing, next_timing)
        {
            Ok(us) => us,
            Err(err) =>
            {
                self.finished = true;
                return Some(Err(err));
            }
        };

        Some(Ok(Self::into_raw_frame(cur_frame, delay_us)))
    }
}

/// Parses one binary PAM `P7` frame (`RGB_ALPHA`) from `reader`, returning `None` cleanly at EOF.
fn read_pam_frame<R: BufRead>(path: &Path, reader: &mut R) -> Result<Option<PamFrame>, CacheError>
{
    let mut line = String::new();
    let first_bytes = reader
        .read_line(&mut line)
        .map_err(|source| CacheError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if first_bytes == 0
    {
        return Ok(None);
    }

    if line.trim_end() != "P7"
    {
        return Err(CacheError::Decode {
            path: path.to_path_buf(),
            reason: format!("Unexpected PAM frame header '{}'", line.trim_end()),
        });
    }

    let mut width: Option<u32> = None;
    let mut height: Option<u32> = None;
    let mut depth: Option<u32> = None;
    let mut maxval: Option<u32> = None;
    let mut endhdr = false;

    while !endhdr
    {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|source| CacheError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        if n == 0
        {
            return Err(CacheError::Decode {
                path: path.to_path_buf(),
                reason: String::from("Truncated PAM frame header from ffmpeg"),
            });
        }

        let trimmed = line.trim();
        if trimmed == "ENDHDR"
        {
            endhdr = true;
            continue;
        }
        if let Some((key, val)) = trimmed.split_once(' ')
        {
            match key
            {
                "WIDTH" => width = val.trim().parse().ok(),
                "HEIGHT" => height = val.trim().parse().ok(),
                "DEPTH" => depth = val.trim().parse().ok(),
                "MAXVAL" => maxval = val.trim().parse().ok(),
                _ =>
                {}
            }
        }
    }

    let width = width.filter(|&w| w > 0).ok_or_else(|| CacheError::Decode {
        path: path.to_path_buf(),
        reason: String::from("Invalid or zero PAM frame width"),
    })?;
    let height = height
        .filter(|&h| h > 0)
        .ok_or_else(|| CacheError::Decode {
            path: path.to_path_buf(),
            reason: String::from("Invalid or zero PAM frame height"),
        })?;
    if depth != Some(BYTES_PER_PIXEL as u32) || maxval != Some(255)
    {
        return Err(CacheError::Decode {
            path: path.to_path_buf(),
            reason: format!("Unsupported PAM pixel format depth={depth:?} maxval={maxval:?}"),
        });
    }

    let byte_len = checked_argb_byte_len(width, height)?;
    let mut rgba = vec![0_u8; byte_len];
    reader
        .read_exact(&mut rgba)
        .map_err(|err| CacheError::Decode {
            path: path.to_path_buf(),
            reason: format!("Truncated PAM pixel payload from ffmpeg: {err}"),
        })?;

    Ok(Some(PamFrame {
        width,
        height,
        rgba,
    }))
}

/// Streams `ffmpeg` stderr lines, sending `showinfo` timings and retaining trailing error text.
fn read_stderr_timings(stderr: ChildStderr, tx: mpsc::Sender<ShowInfoTiming>) -> Vec<String>
{
    let reader = BufReader::new(stderr);
    let mut tail = VecDeque::with_capacity(MAX_STDERR_TAIL);

    for line in reader.lines().map_while(Result::ok)
    {
        if line.starts_with("[Parsed_showinfo_")
        {
            if let Some(timing) = parse_showinfo_line(&line)
            {
                let _ = tx.send(timing);
            }
            continue;
        }

        let trimmed = line.trim();
        if trimmed.is_empty()
        {
            continue;
        }
        if tail.len() >= MAX_STDERR_TAIL
        {
            tail.pop_front();
        }
        tail.push_back(trimmed.to_owned());
    }

    tail.into_iter().collect()
}

/// Extracts `pts_time` and `duration_time` values from one `Parsed_showinfo` summary line.
fn parse_showinfo_line(line: &str) -> Option<ShowInfoTiming>
{
    if !line.contains(" pts_time:")
    {
        return None;
    }

    Some(ShowInfoTiming {
        pts_secs: extract_showinfo_field(line, "pts_time:"),
        duration_secs: extract_showinfo_field(line, "duration_time:"),
    })
}

/// Parses a `key:<float>` token from a `showinfo` log line, returning `None` for `N/A`.
fn extract_showinfo_field(line: &str, prefix: &str) -> Option<f64>
{
    let start = line.find(prefix)? + prefix.len();
    let token = line[start..].split_whitespace().next()?;
    token.parse::<f64>().ok().filter(|val| val.is_finite())
}
