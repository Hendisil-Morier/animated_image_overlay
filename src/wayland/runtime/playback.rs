//! Animation frame deadline scheduling, SHM staging, and `--stats` tick recording.

// Monotonic microsecond intervals and error-retry timer durations.
use std::time::{Duration, Instant};

// SCTK frame callback user data.
use smithay_client_toolkit::compositor::FrameCallbackData;

// Parent `OverlayRuntime` struct and playback step types.
use super::state::OverlayRuntime;
use crate::playback::{PlaybackStep, TickOutcome};

// Recovery delay in milliseconds after a cache read or SHM frame upload error.
const FRAME_ERROR_RETRY_MS: u64 = 100;

impl OverlayRuntime
{
    /// Requests a Wayland frame callback on `image_surface` and commits immediately.
    pub(crate) fn queue_image_frame_callback(&mut self)
    {
        if let Some(scene) = &self.scene
        {
            self.frame_callback_pending = true;
            scene
                .image_surface
                .frame(&self.qh, FrameCallbackData(scene.image_surface.clone()));
            scene.image_surface.commit();
        }
    }

    /// Starts or resumes timer-paced animation ticks when configured and unpaused.
    pub(crate) fn schedule_playback_if_needed(&mut self)
    {
        if !self.configured || self.frame_callback_pending || self.playback.is_paused()
        {
            return;
        }

        self.tick_playback();
    }

    /// Advances playback with borrowed cache delays and presents or arms the next wait timer.
    pub(crate) fn tick_playback(&mut self)
    {
        self.cancel_frame_timer();
        if !self.configured || self.frame_callback_pending
        {
            return;
        }

        let now_us = self.start_instant.elapsed().as_secs_f64() * 1_000_000.0;
        let (step, wait_us, frame_count, decode_done) =
            self.cache.with_delays_us(|delays_us, decode_done| {
                let step = self
                    .playback
                    .advance_us_with_stats(now_us, delays_us, decode_done);
                let wait_us = if matches!(step.outcome, TickOutcome::NoChange)
                {
                    self.playback.us_until_next_frame(now_us)
                }
                else
                {
                    None
                };
                (step, wait_us, delays_us.len(), decode_done)
            });

        self.record_tick_stats(step, frame_count, decode_done);

        match step.outcome
        {
            TickOutcome::StopCallbacks =>
            {}
            TickOutcome::FrameChanged(idx) => self.present_frame_at(idx),
            TickOutcome::NoChange =>
            {
                if let Some(delay_us) = wait_us
                {
                    self.schedule_frame_timer(Duration::from_micros(delay_us.max(1)));
                }
            }
        }
    }

    /// Fetches frame `idx` from display cache or `FrameCache`, uploads it to SHM, and requests callback.
    fn present_frame_at(&mut self, idx: usize)
    {
        let Some(scene) = &mut self.scene
        else
        {
            return;
        };

        let stage_start = self.stats.is_some().then(Instant::now);
        let staged = scene.stage_frame_cached(&self.shm, &self.layout, &self.cache, idx);

        match staged
        {
            Ok(_retired_ring) =>
            {
                if let (Some(start), Some(stats)) = (stage_start, &mut self.stats)
                {
                    stats.record_presented(start.elapsed());
                }
                self.queue_image_frame_callback();
            }
            Err(err) =>
            {
                tracing::warn!(error = %err, frame_index = idx, "Failed presenting frame");
                self.schedule_frame_timer(Duration::from_millis(FRAME_ERROR_RETRY_MS));
            }
        }
    }

    /// Updates `--stats` tick counters and logs cold-decode completion the instant it finishes.
    fn record_tick_stats(&mut self, step: PlaybackStep, frame_count: usize, decode_done: bool)
    {
        let Some(stats) = &mut self.stats
        else
        {
            return;
        };
        stats.record_tick(step);
        if let Some(msg) = stats.check_decode_completion(Instant::now(), frame_count, decode_done)
        {
            println!("{msg}");
        }
    }

    /// Resamples and atomically commits the current playback frame for updated orientation or scale.
    pub(crate) fn refresh_active_oriented_frame(&mut self)
    {
        let Some(scene) = &mut self.scene
        else
        {
            return;
        };

        let frame_idx = self.playback.current_index();
        let refreshed = scene.refresh_frame_cached(
            &self.compositor,
            &self.shm,
            &self.layout,
            &self.input,
            &self.cache,
            frame_idx,
        );
        if let Err(err) = refreshed
        {
            tracing::warn!(
                error = %err,
                frame_index = frame_idx,
                "Failed refreshing rotated Wayland frame"
            );
        }
    }
}
