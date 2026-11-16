//! Periodic playback, CPU utilization, overall process RAM, staging latency, and cache telemetry for `--stats`.

// Cached logical CPU lookup, procfs RSS reads, and monotonic intervals for `--stats`.
use std::{
    fs,
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};

// Safe `CLOCK_PROCESS_CPUTIME_ID` query for multi-thread process user and kernel CPU time.
use rustix::time::{ClockId, clock_gettime};

// Step-level skip and frontier-hold metrics from `PlaybackClock`.
use crate::playback::PlaybackStep;

/// Periodic reporting cadence in milliseconds when `--stats` is enabled.
pub const STATS_INTERVAL_MS: u64 = 1_000;

// IEC binary megabyte divisor for formatting cache and overall RAM usage in MiB.
const BYTES_PER_MIB: f64 = 1024.0 * 1024.0;

/// Instantaneous decode, CPU, and memory state sampled at each stats report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatsSnapshot
{
    /// Total number of frames currently known by the frame store while decoding.
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

    /// Cumulative process CPU time (`user + kernel` across all threads) at snapshot capture.
    pub cpu_time: Duration,

    /// Logical CPU core count used to normalize host-wide CPU utilization.
    pub logical_cpus: usize,

    /// Overall process physical RAM usage (`VmRSS`) in bytes sampled at snapshot capture.
    pub process_ram_bytes: usize,
}

/// Accumulates frame presentation, CPU usage, skip/hold, staging timing, and cold-decode metrics.
#[derive(Debug, Clone)]
pub struct OverlayStats
{
    // Start instant of the active reporting interval.
    window_start: Instant,

    // Cumulative process CPU time observed at `window_start`.
    window_cpu_start: Duration,

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
    /// Creates a new telemetry accumulator starting at `now` with a zero CPU baseline.
    pub fn new(now: Instant, initial_decode_done: bool) -> Self
    {
        Self::with_cpu(now, initial_decode_done, Duration::ZERO)
    }

    /// Creates a new telemetry accumulator starting at `now` and anchored at `initial_cpu_time`.
    pub fn with_cpu(now: Instant, initial_decode_done: bool, initial_cpu_time: Duration) -> Self
    {
        Self {
            window_start: now,
            window_cpu_start: initial_cpu_time,
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

    /// Samples cumulative process CPU time (`user + kernel` across all threads) via `CLOCK_PROCESS_CPUTIME_ID`.
    pub fn sample_cpu_time() -> Duration
    {
        let ts = clock_gettime(ClockId::ProcessCPUTime);
        let secs = u64::try_from(ts.tv_sec).unwrap_or(0);
        let nanos = u32::try_from(ts.tv_nsec).unwrap_or(0);
        if nanos >= 1_000_000_000
        {
            return Duration::ZERO;
        }
        Duration::new(secs, nanos)
    }

    /// Returns the cached logical CPU count (`>= 1`) used to normalize host CPU utilization.
    pub fn logical_cpu_count() -> usize
    {
        static LOGICAL_CPUS: OnceLock<usize> = OnceLock::new();
        *LOGICAL_CPUS.get_or_init(|| {
            thread::available_parallelism()
                .map_or(1, usize::from)
                .max(1)
        })
    }

    /// Samples overall process physical RAM usage (`VmRSS`) in bytes from `/proc/self/status`.
    pub fn sample_process_ram_bytes() -> usize
    {
        fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|text| Self::parse_vm_rss_bytes(&text))
            .unwrap_or(0)
    }

    /// Parses the `VmRSS` entry in KiB from Linux `/proc/self/status` text into bytes.
    pub fn parse_vm_rss_bytes(status: &str) -> Option<usize>
    {
        for line in status.lines()
        {
            let Some(rest) = line.strip_prefix("VmRSS:")
            else
            {
                continue;
            };
            let kib: usize = rest.split_whitespace().next()?.parse().ok()?;
            return kib.checked_mul(1024);
        }
        None
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
        self.reset_window(now, snap.cpu_time);
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
        let (cpu_core_pct, cpu_host_pct) = self.cpu_utilization_pct(secs, snap);
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

        let decode_tag = if !snap.decode_done
        {
            format!(" decode=decoding({}f)", snap.frame_count)
        }
        else
        {
            String::new()
        };

        let process_ram_mib = snap.process_ram_bytes as f64 / BYTES_PER_MIB;
        let state_tag = if snap.paused { " paused" } else { "" };

        format!(
            "[stats] fps={fps:.1}{state_tag} cpu={cpu_core_pct:.1}% host={cpu_host_pct:.2}% ram={process_ram_mib:.1}MiB skips={} holds={} stage_us={stage_avg_us}/{} cache={cache_label} disp={disp_label}{decode_tag}",
            self.catchup_skips, self.frontier_holds, self.stage_max_us
        )
    }

    /// Computes `(core_pct, host_pct)` over `secs` from cumulative process CPU time deltas.
    fn cpu_utilization_pct(&self, secs: f64, snap: StatsSnapshot) -> (f64, f64)
    {
        if secs <= 0.0
        {
            return (0.0, 0.0);
        }

        let cpu_delta_secs = snap
            .cpu_time
            .saturating_sub(self.window_cpu_start)
            .as_secs_f64();
        let cpu_core_pct = (cpu_delta_secs / secs) * 100.0;
        let core_divisor = snap.logical_cpus.max(1) as f64;
        (cpu_core_pct, cpu_core_pct / core_divisor)
    }

    /// Clears per-interval presentation, skip/hold, and staging accumulators and updates baselines.
    fn reset_window(&mut self, now: Instant, cpu_time: Duration)
    {
        self.window_start = now;
        self.window_cpu_start = cpu_time;
        self.presented_frames = 0;
        self.catchup_skips = 0;
        self.frontier_holds = 0;
        self.stage_samples = 0;
        self.stage_total_us = 0;
        self.stage_max_us = 0;
    }
}
