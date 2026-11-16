//! Deterministic animation frame timing and suspend/stall recovery state machine.
//!
//! Mirrors Python `_on_tick` cadence rules at microsecond resolution: clamps delays to
//! a configurable `--max-fps` floor (`240 fps` / `4_167 us` by default, with a `1_000 us`
//! decoded cache floor for `0 ms` GIF/WebP quirks), holds on the final buffered frame while
//! background decoding is incomplete, loops cleanly once decoding finishes, and resets stale
//! deadlines after pauses or suspend jumps exceeding 1000 ms.

/// Microseconds per millisecond conversion constant.
const MICROS_PER_MILLI: u32 = 1_000;

/// Microseconds per second conversion constant.
const MICROS_PER_SECOND: u32 = 1_000_000;

/// Default maximum playback frame rate ceiling in frames per second when `--max-fps` is omitted.
pub const DEFAULT_MAX_FPS: u32 = 240;

/// Maximum configurable `--max-fps` ceiling matching the `1_000 us` cache storage floor.
pub const MAX_CONFIGURABLE_FPS: u32 = 1_000;

/// Minimum allowed display duration cached per frame in microseconds (`1_000 us` / `1000 fps`).
pub const CACHE_MIN_FRAME_DELAY_US: u32 = MICROS_PER_SECOND / MAX_CONFIGURABLE_FPS;

/// Default minimum allowed display duration per animation frame in milliseconds (`4 ms`).
pub const MIN_FRAME_DELAY_MS: u32 = 4;

/// Default minimum allowed display duration per animation frame in microseconds (`4_167 us` / `240 fps`).
pub const MIN_FRAME_DELAY_US: u32 = PlaybackClock::max_fps_to_delay_us(DEFAULT_MAX_FPS);

/// Fallback frame delay in milliseconds when source metadata omits timing information.
pub const DEFAULT_FRAME_DELAY_MS: u32 = 33;

/// Fallback frame delay in microseconds when source metadata omits timing information.
pub const DEFAULT_FRAME_DELAY_US: u32 = DEFAULT_FRAME_DELAY_MS * MICROS_PER_MILLI;

/// Elapsed deadline drift threshold (ms) triggering clock rebase instead of catch-up spinning.
pub const STALL_RECOVERY_MS: f64 = 1000.0;

// Elapsed deadline drift threshold in microseconds matching `STALL_RECOVERY_MS`.
const STALL_RECOVERY_US: f64 = STALL_RECOVERY_MS * 1000.0;

/// Decision produced when evaluating the animation clock at a compositor frame callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickOutcome
{
    /// Current frame remains valid; keep scheduling frame callbacks for upcoming transitions.
    NoChange,

    /// Active frame index changed to the contained zero-based frame number.
    FrameChanged(usize),

    /// No further frame callbacks are required (completed single-frame image or paused).
    StopCallbacks,
}

/// Detailed playback advance result exposing catch-up skips and decode-frontier holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackStep
{
    /// Frame presentation decision produced for this tick.
    pub outcome: TickOutcome,

    /// Count of intermediate frames skipped while catching up to monotonic deadlines.
    pub skipped_frames: u32,

    /// Set when playback held at the final buffered frame while decoding was incomplete.
    pub frontier_held: bool,
}

/// Tracks active animation frame index, monotonic microsecond deadline, and pause state.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaybackClock
{
    // Currently presented frame index within `delays_us`.
    index: usize,

    // Monotonic timestamp (us) when the next frame transition becomes due.
    next_deadline_us: Option<f64>,

    // Minimum frame duration in microseconds enforced during playback (`--max-fps` ceiling).
    min_delay_us: u32,

    // When true, advances pause until resumed by keyboard Space in edit mode.
    paused: bool,
}

impl Default for PlaybackClock
{
    /// Starts at frame 0 with an uninitialized deadline, active playback, and `DEFAULT_MAX_FPS` cap.
    fn default() -> Self
    {
        Self::new()
    }
}

impl PlaybackClock
{
    /// Constructs a new playback clock starting at frame index 0 with the default `240 fps` cap.
    pub fn new() -> Self
    {
        Self::with_max_fps(DEFAULT_MAX_FPS)
    }

    /// Constructs a new playback clock starting at frame index 0 capped to `max_fps`.
    pub fn with_max_fps(max_fps: u32) -> Self
    {
        Self {
            index: 0,
            next_deadline_us: None,
            min_delay_us: Self::max_fps_to_delay_us(max_fps),
            paused: false,
        }
    }

    /// Returns the configured minimum per-frame display duration in microseconds.
    pub fn min_delay_us(&self) -> u32
    {
        self.min_delay_us
    }

    /// Converts a frame rate limit in FPS to the nearest microsecond minimum frame delay.
    pub const fn max_fps_to_delay_us(max_fps: u32) -> u32
    {
        if max_fps == 0
        {
            return DEFAULT_FRAME_DELAY_US;
        }
        let rounded = MICROS_PER_SECOND.saturating_add(max_fps / 2) / max_fps;
        if rounded < 1 { 1 } else { rounded }
    }

    /// Clamps a decoded microsecond delay to the `MIN_FRAME_DELAY_US` (`4_167 us`, `240 fps`) floor.
    pub fn normalize_delay_us(delay_us: Option<f64>) -> u32
    {
        clamp_rounded_delay(delay_us, DEFAULT_FRAME_DELAY_US, MIN_FRAME_DELAY_US)
    }

    /// Clamps a decoded microsecond delay to the `1_000 us` (`1000 fps`) cache storage guard.
    pub fn normalize_cache_delay_us(delay_us: Option<f64>) -> u32
    {
        clamp_rounded_delay(delay_us, DEFAULT_FRAME_DELAY_US, CACHE_MIN_FRAME_DELAY_US)
    }

    /// Clamps a decoded delay to the `MIN_FRAME_DELAY_MS` (`4 ms`) floor.
    pub fn normalize_delay_ms(delay_ms: Option<f64>) -> u32
    {
        clamp_rounded_delay(delay_ms, DEFAULT_FRAME_DELAY_MS, MIN_FRAME_DELAY_MS)
    }

    /// Converts a microsecond frame delay to the nearest whole millisecond.
    pub fn delay_us_to_ms(delay_us: u32) -> u32
    {
        delay_us.saturating_add(MICROS_PER_MILLI / 2) / MICROS_PER_MILLI
    }

    /// Returns the currently displayed frame index.
    pub fn current_index(&self) -> usize
    {
        self.index
    }

    /// Returns `true` when playback is paused by the user.
    pub fn is_paused(&self) -> bool
    {
        self.paused
    }

    /// Toggles pause state and resets the deadline when resuming so no catch-up skip occurs.
    pub fn toggle_pause(&mut self) -> bool
    {
        self.paused = !self.paused;
        self.next_deadline_us = None;
        self.paused
    }

    /// Advances playback against monotonic `now_us`, available `delays_us`, and `decode_done`.
    pub fn advance_us(&mut self, now_us: f64, delays_us: &[u32], decode_done: bool) -> TickOutcome
    {
        self.advance_us_with_stats(now_us, delays_us, decode_done)
            .outcome
    }

    /// Advances playback at microsecond resolution while returning skip and frontier-hold metrics.
    pub fn advance_us_with_stats(
        &mut self,
        now_us: f64,
        delays_us: &[u32],
        decode_done: bool,
    ) -> PlaybackStep
    {
        let min_delay_us = self.min_delay_us;
        self.advance_with_unit(now_us, delays_us.len(), decode_done, |idx| {
            f64::from(delays_us[idx].max(min_delay_us))
        })
    }

    /// Advances playback against monotonic `now_ms`, available `delays_ms`, and `decode_done`.
    pub fn advance(&mut self, now_ms: f64, delays_ms: &[u32], decode_done: bool) -> TickOutcome
    {
        let scale = f64::from(MICROS_PER_MILLI);
        let min_delay_us = f64::from(self.min_delay_us);
        self.advance_with_unit(now_ms * scale, delays_ms.len(), decode_done, |idx| {
            (f64::from(delays_ms[idx]) * scale).max(min_delay_us)
        })
        .outcome
    }

    /// Returns the stored next-frame deadline in microseconds if initialized.
    pub fn next_deadline_us(&self) -> Option<f64>
    {
        self.next_deadline_us
    }

    /// Returns the stored next-frame deadline in milliseconds if initialized.
    pub fn next_deadline_ms(&self) -> Option<f64>
    {
        self.next_deadline_us
            .map(|us| us / f64::from(MICROS_PER_MILLI))
    }

    /// Returns whole microseconds remaining until the next frame transition, if scheduled and unpaused.
    pub fn us_until_next_frame(&self, now_us: f64) -> Option<u64>
    {
        if self.paused
        {
            return None;
        }

        let deadline = self.next_deadline_us?;
        let remaining = deadline - now_us;
        if !remaining.is_finite() || remaining <= 0.0
        {
            return Some(0);
        }

        Some(remaining.ceil().clamp(0.0, f64::from(u32::MAX)) as u64)
    }

    /// Returns whole milliseconds remaining until the next frame transition, if scheduled and unpaused.
    pub fn ms_until_next_frame(&self, now_ms: f64) -> Option<u64>
    {
        let scale = f64::from(MICROS_PER_MILLI);
        self.us_until_next_frame(now_ms * scale)
            .map(|us| us.div_ceil(u64::from(MICROS_PER_MILLI)))
    }

    // Shared microsecond clock advancement parameterized over delay unit conversion.
    fn advance_with_unit(
        &mut self,
        now_us: f64,
        frame_count: usize,
        decode_done: bool,
        mut delay_us_at: impl FnMut(usize) -> f64,
    ) -> PlaybackStep
    {
        if self.paused
        {
            self.next_deadline_us = None;
            return PlaybackStep {
                outcome: TickOutcome::StopCallbacks,
                skipped_frames: 0,
                frontier_held: false,
            };
        }

        if frame_count <= 1
        {
            if decode_done
            {
                return PlaybackStep {
                    outcome: TickOutcome::StopCallbacks,
                    skipped_frames: 0,
                    frontier_held: false,
                };
            }
            return PlaybackStep {
                outcome: TickOutcome::NoChange,
                skipped_frames: 0,
                frontier_held: true,
            };
        }

        if self.index >= frame_count
        {
            self.index = 0;
        }

        let initial_delay = delay_us_at(self.index);
        let deadline = self.next_deadline_us.get_or_insert(now_us + initial_delay);

        if now_us - *deadline > STALL_RECOVERY_US
        {
            *deadline = now_us;
        }

        let prev_index = self.index;
        let mut advances = 0_u32;
        let mut frontier_held = false;
        while now_us >= *deadline
        {
            let mut next_index = self.index + 1;
            if next_index >= frame_count
            {
                if !decode_done
                {
                    let hold_delay = delay_us_at(self.index);
                    *deadline = now_us + hold_delay;
                    frontier_held = true;
                    break;
                }
                next_index = 0;
            }

            self.index = next_index;
            advances = advances.saturating_add(1);
            *deadline += delay_us_at(self.index);
        }

        let outcome = if self.index != prev_index
        {
            TickOutcome::FrameChanged(self.index)
        }
        else
        {
            TickOutcome::NoChange
        };

        PlaybackStep {
            outcome,
            skipped_frames: advances.saturating_sub(1),
            frontier_held,
        }
    }
}

/// Rounds a finite optional delay to `u32`, applying `fallback` when missing and clamping to `floor`.
fn clamp_rounded_delay(val: Option<f64>, fallback: u32, floor: u32) -> u32
{
    val.filter(|v| v.is_finite())
        .map(|v| v.round().clamp(0.0, f64::from(u32::MAX)) as u32)
        .unwrap_or(fallback)
        .max(floor)
}
