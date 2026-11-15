//! Integration tests for animation playback scheduling, hold-on-stream, wrap, and stall recovery.

use animated_image_widget::playback::{
    DEFAULT_FRAME_DELAY_US, MIN_FRAME_DELAY_MS, MIN_FRAME_DELAY_US, PlaybackClock, TickOutcome,
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
    assert_eq!(
        PlaybackClock::normalize_delay_ms(Some(0.0)),
        MIN_FRAME_DELAY_MS
    );
    assert_eq!(PlaybackClock::normalize_delay_ms(None), 33);

    let mut clock = PlaybackClock::new();
    let delays = [0, 0];

    assert_eq!(clock.advance(10.0, &delays, true), TickOutcome::NoChange);
    assert_eq!(clock.next_deadline_ms(), Some(20.0));

    assert!(clock.toggle_pause());
    assert_eq!(
        clock.advance(100.0, &delays, true),
        TickOutcome::StopCallbacks
    );

    assert!(!clock.toggle_pause());
    assert_eq!(clock.advance(100.0, &delays, true), TickOutcome::NoChange);
    assert_eq!(clock.next_deadline_ms(), Some(110.0));
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
