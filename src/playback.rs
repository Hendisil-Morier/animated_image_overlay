//! Deterministic animation frame timing and suspend/stall recovery state machine.
//!
//! Mirrors Python `_on_tick` cadence rules: clamps delays to a 10 ms floor,
//! holds on the final buffered frame while background decoding is incomplete,
//! loops cleanly once decoding finishes, and resets stale deadlines after pauses
//! or suspend jumps exceeding 1000 ms.

/// Minimum allowed display duration per animation frame in milliseconds.
pub const MIN_FRAME_DELAY_MS: u32 = 10;

/// Fallback frame delay in milliseconds when source metadata omits timing information.
pub const DEFAULT_FRAME_DELAY_MS: u32 = 33;

/// Elapsed deadline drift threshold (ms) triggering clock rebase instead of catch-up spinning.
pub const STALL_RECOVERY_MS: f64 = 1000.0;

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

/// Tracks active animation frame index, monotonic deadline, and pause state.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaybackClock
{
    // Currently presented frame index within `delays_ms`.
    index: usize,

    // Monotonic timestamp (ms) when the next frame transition becomes due.
    next_deadline_ms: Option<f64>,

    // When true, advances pause until resumed by keyboard Space in edit mode.
    paused: bool,
}

impl Default for PlaybackClock
{
    /// Starts at frame 0 with an uninitialized deadline and active playback.
    fn default() -> Self
    {
        Self::new()
    }
}

impl PlaybackClock
{
    /// Constructs a new playback clock starting at frame index 0.
    pub fn new() -> Self
    {
        Self {
            index: 0,
            next_deadline_ms: None,
            paused: false,
        }
    }

    /// Clamps a decoded delay to the 10 ms minimum floor (mapping 0 ms GIF/WebP quirks safely).
    pub fn normalize_delay_ms(delay_ms: Option<f64>) -> u32
    {
        let ms = delay_ms
            .filter(|val| val.is_finite())
            .map(|val| val.round().clamp(0.0, f64::from(u32::MAX)) as u32)
            .unwrap_or(DEFAULT_FRAME_DELAY_MS);
        ms.max(MIN_FRAME_DELAY_MS)
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
        self.next_deadline_ms = None;
        self.paused
    }

    /// Advances playback against monotonic `now_ms`, available `delays_ms`, and `decode_done`.
    pub fn advance(&mut self, now_ms: f64, delays_ms: &[u32], decode_done: bool) -> TickOutcome
    {
        if self.paused
        {
            self.next_deadline_ms = None;
            return TickOutcome::StopCallbacks;
        }

        if delays_ms.len() <= 1
        {
            if decode_done
            {
                return TickOutcome::StopCallbacks;
            }
            return TickOutcome::NoChange;
        }

        if self.index >= delays_ms.len()
        {
            self.index = 0;
        }

        let initial_delay = f64::from(delays_ms[self.index].max(MIN_FRAME_DELAY_MS));
        let deadline = self.next_deadline_ms.get_or_insert(now_ms + initial_delay);

        if now_ms - *deadline > STALL_RECOVERY_MS
        {
            *deadline = now_ms;
        }

        let prev_index = self.index;
        while now_ms >= *deadline
        {
            let mut next_index = self.index + 1;
            if next_index >= delays_ms.len()
            {
                if !decode_done
                {
                    let hold_delay = f64::from(delays_ms[self.index].max(MIN_FRAME_DELAY_MS));
                    *deadline = now_ms + hold_delay;
                    break;
                }
                next_index = 0;
            }

            self.index = next_index;
            *deadline += f64::from(delays_ms[self.index].max(MIN_FRAME_DELAY_MS));
        }

        if self.index != prev_index
        {
            TickOutcome::FrameChanged(self.index)
        }
        else
        {
            TickOutcome::NoChange
        }
    }

    /// Returns the stored next-frame deadline in milliseconds if initialized.
    pub fn next_deadline_ms(&self) -> Option<f64>
    {
        self.next_deadline_ms
    }
}
