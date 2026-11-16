//! Integration tests for animation playback scheduling, hold-on-stream, wrap, and stall recovery.

use animated_image_widget::playback::{
    CACHE_MIN_FRAME_DELAY_US, DEFAULT_FRAME_DELAY_US, DEFAULT_MAX_FPS, MIN_FRAME_DELAY_MS,
    MIN_FRAME_DELAY_US, PlaybackClock, PlaybackStep, TickOutcome,
};

#[test]
fn playback_stops_callbacks_for_completed_single_frame_images()
{
    let mut clock = PlaybackClock::new();
    assert_eq!(
        clock.advance(100.0, &[33], true),
        TickOutcome::StopCallbacks
    );

    assert_eq!(clock.advance(100.0, &[33], false), TickOutcome::NoChange);
}

#[test]
fn playback_holds_while_streaming_wraps_on_completion_and_recovers_from_stall()
{
    let mut clock = PlaybackClock::new();
    let delays = [33, 33];

    assert_eq!(clock.advance(100.0, &delays, false), TickOutcome::NoChange);
    assert_eq!(clock.current_index(), 0);
    assert_eq!(clock.next_deadline_ms(), Some(133.0));

    assert_eq!(
        clock.advance(140.0, &delays, false),
        TickOutcome::FrameChanged(1)
    );
    assert_eq!(clock.current_index(), 1);
    assert_eq!(clock.next_deadline_ms(), Some(166.0));

    assert_eq!(clock.advance(180.0, &delays, false), TickOutcome::NoChange);
    assert_eq!(clock.current_index(), 1);
    assert_eq!(clock.next_deadline_ms(), Some(213.0));

    assert_eq!(
        clock.advance(5000.0, &delays, true),
        TickOutcome::FrameChanged(0)
    );
    assert_eq!(clock.current_index(), 0);
    assert_eq!(clock.next_deadline_ms(), Some(5033.0));
}

#[test]
fn playback_enforces_minimum_delay_and_pause_resume()
{
    assert_eq!(MIN_FRAME_DELAY_MS, 4);
    assert_eq!(MIN_FRAME_DELAY_US, 4_167);
    assert_eq!(
        PlaybackClock::normalize_delay_ms(Some(0.0)),
        MIN_FRAME_DELAY_MS
    );
    assert_eq!(PlaybackClock::normalize_delay_ms(None), 33);

    let mut default_clock = PlaybackClock::new();
    let delays = [0, 0];
    assert_eq!(
        default_clock.advance(10.0, &delays, true),
        TickOutcome::NoChange
    );
    assert_eq!(default_clock.next_deadline_ms(), Some(14.167));

    let mut capped_100 = PlaybackClock::with_max_fps(100);
    assert_eq!(
        capped_100.advance(10.0, &delays, true),
        TickOutcome::NoChange
    );
    assert_eq!(capped_100.next_deadline_ms(), Some(20.0));

    assert!(capped_100.toggle_pause());
    assert_eq!(
        capped_100.advance(100.0, &delays, true),
        TickOutcome::StopCallbacks
    );

    assert!(!capped_100.toggle_pause());
    assert_eq!(
        capped_100.advance(100.0, &delays, true),
        TickOutcome::NoChange
    );
    assert_eq!(capped_100.next_deadline_ms(), Some(110.0));
}

#[test]
fn playback_reports_remaining_ms_for_timer_wait_and_clears_while_paused()
{
    let mut clock = PlaybackClock::new();
    let delays = [80, 120];

    assert_eq!(clock.ms_until_next_frame(0.0), None);
    assert_eq!(clock.advance(50.0, &delays, false), TickOutcome::NoChange);
    assert_eq!(clock.ms_until_next_frame(50.0), Some(80));
    assert_eq!(clock.ms_until_next_frame(74.2), Some(56));
    assert_eq!(clock.ms_until_next_frame(130.0), Some(0));

    assert!(clock.toggle_pause());
    assert_eq!(clock.ms_until_next_frame(90.0), None);
}

#[test]
fn playback_preserves_60fps_microsecond_deadlines_and_wait_times()
{
    assert_eq!(
        PlaybackClock::normalize_delay_us(Some(0.0)),
        MIN_FRAME_DELAY_US
    );
    assert_eq!(
        PlaybackClock::normalize_delay_us(None),
        DEFAULT_FRAME_DELAY_US
    );
    assert_eq!(
        PlaybackClock::normalize_delay_us(Some(50_000.0 / 3.0)),
        16_667
    );

    let mut clock = PlaybackClock::new();
    let delays_us = [16_667, 16_667, 16_667];

    assert_eq!(clock.us_until_next_frame(0.0), None);
    assert_eq!(
        clock.advance_us(0.0, &delays_us, true),
        TickOutcome::NoChange
    );
    assert_eq!(clock.next_deadline_us(), Some(16_667.0));
    assert_eq!(clock.us_until_next_frame(1_000.4), Some(15_667));

    assert_eq!(
        clock.advance_us(16_667.0, &delays_us, true),
        TickOutcome::FrameChanged(1)
    );
    assert_eq!(clock.next_deadline_us(), Some(33_334.0));

    assert_eq!(
        clock.advance_us(33_334.0, &delays_us, true),
        TickOutcome::FrameChanged(2)
    );
    assert_eq!(clock.next_deadline_us(), Some(50_001.0));
}

#[test]
fn playback_respects_default_240fps_and_custom_max_fps_ceilings()
{
    assert_eq!(DEFAULT_MAX_FPS, 240);
    assert_eq!(PlaybackClock::default().min_delay_us(), MIN_FRAME_DELAY_US);
    assert_eq!(PlaybackClock::max_fps_to_delay_us(240), 4_167);
    assert_eq!(PlaybackClock::max_fps_to_delay_us(144), 6_944);
    assert_eq!(PlaybackClock::max_fps_to_delay_us(120), 8_333);
    assert_eq!(PlaybackClock::max_fps_to_delay_us(100), 10_000);
    assert_eq!(PlaybackClock::max_fps_to_delay_us(60), 16_667);

    assert_eq!(
        PlaybackClock::normalize_delay_us(Some(0.0)),
        MIN_FRAME_DELAY_US
    );
    assert_eq!(
        PlaybackClock::normalize_cache_delay_us(Some(0.0)),
        CACHE_MIN_FRAME_DELAY_US
    );
    assert_eq!(
        PlaybackClock::normalize_cache_delay_us(Some(2_777.8)),
        2_778
    );

    // Default 240 fps cap allows 120 fps (`8_333 us`) and 240 fps (`4_167 us`) natively.
    let mut clock_default = PlaybackClock::new();
    let delays_120 = [8_333, 8_333, 8_333];
    assert_eq!(
        clock_default.advance_us(0.0, &delays_120, true),
        TickOutcome::NoChange
    );
    assert_eq!(clock_default.next_deadline_us(), Some(8_333.0));
    assert_eq!(
        clock_default.advance_us(8_333.0, &delays_120, true),
        TickOutcome::FrameChanged(1)
    );
    assert_eq!(clock_default.next_deadline_us(), Some(16_666.0));

    // `-f 60` slows a 120 fps animation down to 60 fps (`16_667 us`), while keeping 30 fps untouched.
    let mut clock_60 = PlaybackClock::with_max_fps(60);
    assert_eq!(
        clock_60.advance_us(0.0, &delays_120, true),
        TickOutcome::NoChange
    );
    assert_eq!(clock_60.next_deadline_us(), Some(16_667.0));

    // `-f 360` enables sub-240 fps delays (`2_778 us`) while clamping `1_000 us` (0 ms quirk) up to `2_778 us`.
    let mut clock_360 = PlaybackClock::with_max_fps(360);
    assert_eq!(
        clock_360.advance_us(0.0, &[1_000, 2_778], true),
        TickOutcome::NoChange
    );
    assert_eq!(clock_360.next_deadline_us(), Some(2_778.0));
}

#[test]
fn playback_step_tracks_catchup_skips_and_decode_frontier_holds()
{
    let mut clock = PlaybackClock::new();

    // Waiting on frame 0 while background decode has 1 frame reports frontier_held = true.
    assert_eq!(
        clock.advance_us_with_stats(0.0, &[8_333], false),
        PlaybackStep {
            outcome: TickOutcome::NoChange,
            skipped_frames: 0,
            frontier_held: true,
        }
    );

    let delays = [8_333_u32; 5];
    assert_eq!(
        clock.advance_us_with_stats(0.0, &delays, false),
        PlaybackStep {
            outcome: TickOutcome::NoChange,
            skipped_frames: 0,
            frontier_held: false,
        }
    );

    // Jumping 3 frame intervals in one tick advances once and reports 2 skipped intermediate frames.
    assert_eq!(
        clock.advance_us_with_stats(25_000.0, &delays, true),
        PlaybackStep {
            outcome: TickOutcome::FrameChanged(3),
            skipped_frames: 2,
            frontier_held: false,
        }
    );

    // Reaching the buffered tail before decoding finishes marks frontier_held = true.
    assert_eq!(
        clock.advance_us_with_stats(50_000.0, &delays, false),
        PlaybackStep {
            outcome: TickOutcome::FrameChanged(4),
            skipped_frames: 0,
            frontier_held: true,
        }
    );
}
