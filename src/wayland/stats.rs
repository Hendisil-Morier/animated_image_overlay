//! Periodic playback, staging latency, and cache memory telemetry for `--stats`.

// Monotonic intervals for per-second FPS and staging duration measurement.
use std::time::{Duration, Instant};

// Step-level skip and frontier-hold metrics from `PlaybackClock`.
use crate::playback::PlaybackStep;

/// Periodic reporting cadence in milliseconds when `--stats` is enabled.
pub const STATS_INTERVAL_MS: u64 = 1_000;

// IEC binary megabyte divisor for formatting cache usage in MiB.
const BYTES_PER_MIB: f64 = 1024.0 * 1024.0;

/// Instantaneous playback, decode, and cache memory state sampled at each stats report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatsSnapshot
{
    /// Zero-based index of the currently active frame.
    pub frame_index: usize,

    /// Total number of frames currently known by the frame store.
    pub frame_count: usize,

    /// Set once background decoding or warm cache verification has completed.
    pub decode_done: bool,

    /// Set while user pause is active in interactive edit mode.
    pub paused: bool,

    /// Set when the full uncompressed loop fits inside `-r` / `-c ram`.
    pub fits_source_ram: bool,

    /// Total uncompressed bytes currently pinned inside the source RAM LRU cache.
    pub ram_used_bytes: usize,

    /// Set when `DisplayFrameCache` is actively serving display-sized frames.
    pub display_cache_active: bool,

    /// Total bytes currently held inside `DisplayFrameCache`.
    pub display_cache_bytes: usize,
}

/// Accumulates frame presentation, skip/hold, staging timing, and cold-decode metrics.
#[derive(Debug, Clone)]
pub struct OverlayStats
{
    // Start instant of the active reporting interval.
    window_start: Instant,

    // Startup instant used to report total cold-decode completion duration.
    startup_instant: Instant,

    // True when launch began with an uncached background decode stream.
    cold_decode_started: bool,

    // Guard ensuring cold-decode completion summary prints at most once.
    cold_decode_reported: bool,

    // Frames staged cleanly to `image_surface` during the active window.
    presented_frames: u64,

    // Intermediate animation frames skipped during deadline catch-up steps.
    catchup_skips: u64,

    // Ticks where playback waited on `!decode_done` at the decoded frontier.
    frontier_holds: u64,

    // Count of frame staging timing samples collected in the active window.
    stage_samples: u64,

    // Cumulative frame staging duration in microseconds across `stage_samples`.
    stage_total_us: u64,

    // Peak single-frame staging duration in microseconds observed in the active window.
    stage_max_us: u64,
}

impl OverlayStats
{
    /// Creates a new telemetry accumulator starting at `now` and recording cold/warm status.
    pub fn new(now: Instant, initial_decode_done: bool) -> Self
    {
        Self {
            window_start: now,
            startup_instant: now,
            cold_decode_started: !initial_decode_done,
            cold_decode_reported: initial_decode_done,
            presented_frames: 0,
            catchup_skips: 0,
            frontier_holds: 0,
            stage_samples: 0,
            stage_total_us: 0,
            stage_max_us: 0,
        }
    }

    /// Records catch-up skips and frontier holds returned by one playback clock step.
    pub fn record_tick(&mut self, step: PlaybackStep)
    {
        self.catchup_skips = self
            .catchup_skips
            .saturating_add(u64::from(step.skipped_frames));
        if step.frontier_held
        {
            self.frontier_holds = self.frontier_holds.saturating_add(1);
        }
    }

    /// Records one completed frame upload and its staging latency.
    pub fn record_presented(&mut self, stage_duration: Duration)
    {
        let stage_us = u64::try_from(stage_duration.as_micros()).unwrap_or(u64::MAX);
        self.presented_frames = self.presented_frames.saturating_add(1);
        self.stage_samples = self.stage_samples.saturating_add(1);
        self.stage_total_us = self.stage_total_us.saturating_add(stage_us);
        self.stage_max_us = self.stage_max_us.max(stage_us);
    }

    /// Emits a one-shot summary line the first time a cold background decode completes.
    pub fn check_decode_completion(
        &mut self,
        now: Instant,
        frame_count: usize,
        decode_done: bool,
    ) -> Option<String>
    {
        if !self.cold_decode_started || self.cold_decode_reported || !decode_done
        {
            return None;
        }

        self.cold_decode_reported = true;
        let elapsed_secs = now
            .saturating_duration_since(self.startup_instant)
            .as_secs_f64();
        let rate = if elapsed_secs > 0.0
        {
            frame_count as f64 / elapsed_secs
        }
        else
        {
            0.0
        };

        Some(format!(
            "[stats] cold decode finished: {frame_count} frames in {elapsed_secs:.2}s ({rate:.1} frames/s)"
        ))
    }

    /// Formats the current interval summary against `now` and resets per-window counters.
    pub fn take_interval_report(&mut self, now: Instant, snap: StatsSnapshot) -> String
    {
        let elapsed = now.saturating_duration_since(self.window_start);
        let report = self.format_report(elapsed, snap);
        self.reset_window(now);
        report
    }

    /// Formats accumulated window metrics over `elapsed` paired with instantaneous `snap` state.
    pub fn format_report(&self, elapsed: Duration, snap: StatsSnapshot) -> String
    {
        let secs = elapsed.as_secs_f64();
        let fps = if secs > 0.0
        {
            self.presented_frames as f64 / secs
        }
        else
        {
            0.0
        };
        let stage_avg_us = self
            .stage_total_us
            .checked_div(self.stage_samples)
            .unwrap_or(0);

        let cache_label = if snap.fits_source_ram
        {
            let ram_mib = snap.ram_used_bytes as f64 / BYTES_PER_MIB;
            format!("ram({ram_mib:.1}MiB)")
        }
        else
        {
            String::from("lraw")
        };

        let disp_label = if snap.display_cache_active
        {
            let disp_mib = snap.display_cache_bytes as f64 / BYTES_PER_MIB;
            format!("{disp_mib:.1}MiB")
        }
        else
        {
            String::from("off")
        };

        let decode_label = if !snap.decode_done
        {
            format!("decoding({}f)", snap.frame_count)
        }
        else if !self.cold_decode_started
        {
            format!("warm({}f)", snap.frame_count)
        }
        else
        {
            format!("done({}f)", snap.frame_count)
        };

        let state_tag = if snap.paused { " paused" } else { "" };
        let display_idx = if snap.frame_count == 0
        {
            0
        }
        else
        {
            snap.frame_index.saturating_add(1).min(snap.frame_count)
        };

        format!(
            "[stats] fps={fps:.1}{state_tag} frame={display_idx}/{} skips={} holds={} stage_us={stage_avg_us}/{} cache={cache_label} disp={disp_label} decode={decode_label}",
            snap.frame_count, self.catchup_skips, self.frontier_holds, self.stage_max_us
        )
    }

    /// Clears per-interval presentation, skip/hold, and staging latency accumulators.
    fn reset_window(&mut self, now: Instant)
    {
        self.window_start = now;
        self.presented_frames = 0;
        self.catchup_skips = 0;
        self.frontier_holds = 0;
        self.stage_samples = 0;
        self.stage_total_us = 0;
        self.stage_max_us = 0;
    }
}
