//! Integration tests verifying display-resolution frame cache budgets and invalidation.

use animated_image_widget::{
    config::OverlayState,
    layout::OverlayLayout,
    wayland::{
        display_cache::{DisplayCacheKey, DisplayFrameCache, angle_key},
        frame_stage::{BufferTargetSpec, compute_buffer_target_spec, ring_needs_resync},
    },
};

fn make_key(width: u32, height: u32, angle: f64, flipped: bool) -> DisplayCacheKey
{
    DisplayCacheKey {
        width,
        height,
        angle_bits: angle_key(angle),
        flipped,
    }
}

#[test]
fn display_cache_enforces_budgets_streaming_state_and_invalidation()
{
    // Single-frame or streaming images and zero budgets disable slot storage cleanly.
    let mut cache = DisplayFrameCache::new(4 * 10 * 10 * 4);
    assert_eq!(cache.budget_bytes(), 1600);
    assert!(!cache.sync(make_key(10, 10, 45.0, false), 3, false));
    assert!(!cache.sync(make_key(10, 10, 45.0, false), 1, true));
    assert!(!cache.is_active());

    let mut disabled = DisplayFrameCache::new(0);
    assert!(!disabled.sync(make_key(10, 10, 45.0, false), 3, true));

    // Exact budget accepts all 4 frames (`4 * 10 * 10 * 4 = 1600` bytes), while 5 exceeds it.
    assert!(cache.would_fit(10, 10, 4, true));
    assert!(!cache.would_fit(10, 10, 5, true));
    assert!(!cache.would_fit(10, 10, 4, false));
    assert!(!cache.would_fit(10, 10, 1, true));
    assert!(!cache.would_fit(u32::MAX, u32::MAX, 2, true));

    let key = make_key(10, 10, 310.0, false);
    assert!(cache.sync(key, 4, true));
    assert!(cache.is_active());
    assert_eq!(cache.cached_bytes(), 0);

    // Mismatched byte lengths never populate slots; exact frames populate lazily.
    cache.store(0, vec![1_u8; 10]);
    assert!(cache.get(0).is_none());

    let frame0 = vec![7_u8; 400];
    cache.store(0, frame0.clone());
    assert_eq!(cache.get(0), Some(frame0.as_slice()));
    assert_eq!(cache.cached_bytes(), 400);

    // Re-syncing the identical key and normalizing `-0.0` keeps slots intact.
    assert!(cache.sync(key, 4, true));
    assert_eq!(cache.get(0), Some(frame0.as_slice()));
    assert_eq!(angle_key(-0.0), angle_key(0.0));

    // Changing angle or flip resets prior frames cleanly.
    assert!(cache.sync(make_key(10, 10, 315.0, false), 4, true));
    assert!(cache.get(0).is_none());
    assert_eq!(cache.cached_bytes(), 0);

    // Oversized frame sequences immediately deactivate and release retained memory.
    cache.store(1, vec![9_u8; 400]);
    assert!(!cache.sync(make_key(10, 10, 315.0, false), 5, true));
    assert!(!cache.is_active());
    assert_eq!(cache.cached_bytes(), 0);
}

#[test]
fn compute_buffer_target_spec_selects_static_downscale_and_adr001_tiers()
{
    let display_cache = DisplayFrameCache::new(64 * 1024 * 1024);

    // 1. Static single-frame thumbnail at quarter turn downscales cleanly with zero cache key.
    let static_state = OverlayState {
        x: 0.0,
        y: 0.0,
        scale: 0.25,
        rotation: 90.0,
        flipped: true,
    };
    let static_layout = OverlayLayout::new(2000, 1000, 1920, 1080, static_state);
    assert_eq!(static_layout.overlay_size(), (250, 500));
    assert_eq!(
        compute_buffer_target_spec(&static_layout, &display_cache, 1, 1, true, true),
        BufferTargetSpec {
            width: 500,
            height: 250,
            cache_key: None,
        }
    );
    assert_eq!(
        compute_buffer_target_spec(&static_layout, &display_cache, 2, 1, true, true),
        BufferTargetSpec {
            width: 1000,
            height: 500,
            cache_key: None,
        }
    );

    // 2. Streaming incomplete animation (`decode_done = false`) stays at raw source dimensions.
    assert_eq!(
        compute_buffer_target_spec(&static_layout, &display_cache, 1, 1, false, true),
        BufferTargetSpec {
            width: 2000,
            height: 1000,
            cache_key: None,
        }
    );

    // 3. Small multi-frame loop (`fits_source_ram = true`) streams raw source frames zero-copy.
    assert_eq!(
        compute_buffer_target_spec(&static_layout, &display_cache, 1, 12, true, true),
        BufferTargetSpec {
            width: 2000,
            height: 1000,
            cache_key: None,
        }
    );

    // 4. Oversized multi-frame loop fitting `-c display` uses canonical `(0 deg, false)` cache key.
    assert_eq!(
        compute_buffer_target_spec(&static_layout, &display_cache, 1, 12, true, false),
        BufferTargetSpec {
            width: 500,
            height: 250,
            cache_key: Some(make_key(500, 250, 0.0, false)),
        }
    );

    // When `-c display` cannot hold all downscaled frames, fall back to source frame streaming.
    let tiny_cache = DisplayFrameCache::new(512 * 1024);
    assert_eq!(
        compute_buffer_target_spec(&static_layout, &tiny_cache, 1, 12, true, false),
        BufferTargetSpec {
            width: 2000,
            height: 1000,
            cache_key: None,
        }
    );

    // 5. Fractional rotation targets display-sized rotated dimensions with oriented cache key.
    let rotated_layout = OverlayLayout::new(
        1000,
        1000,
        1920,
        1080,
        OverlayState {
            x: 0.0,
            y: 0.0,
            scale: 0.25,
            rotation: 45.0,
            flipped: true,
        },
    );
    let expected_side = rotated_layout.overlay_size().0;
    assert_eq!(
        compute_buffer_target_spec(&rotated_layout, &display_cache, 1, 1, true, true),
        BufferTargetSpec {
            width: expected_side,
            height: expected_side,
            cache_key: Some(make_key(expected_side, expected_side, 45.0, true)),
        }
    );
}

#[test]
fn ring_needs_resync_triggers_on_screen_shrink_past_hysteresis_and_growth()
{
    let display_cache = DisplayFrameCache::new(64 * 1024 * 1024);

    // 1. Fractional-angle animated overlay shrinking across the 2/3 pixel hysteresis boundary.
    let mut rotated = OverlayLayout::new(
        1000,
        800,
        1920,
        1080,
        OverlayState {
            x: 0.0,
            y: 0.0,
            scale: 0.7,
            rotation: 310.0,
            flipped: false,
        },
    );
    let initial_spec = compute_buffer_target_spec(&rotated, &display_cache, 1, 10, true, false);
    let initial_ring = (initial_spec.width, initial_spec.height);
    assert!(!ring_needs_resync(initial_ring, initial_ring));

    // Gentle screen shrink stays above 2/3 pixels and avoids unnecessary SHM reallocation.
    rotated.set_screen_size(1920, 820);
    let gentle_spec = compute_buffer_target_spec(&rotated, &display_cache, 1, 10, true, false);
    assert!(!ring_needs_resync(
        (gentle_spec.width, gentle_spec.height),
        initial_ring
    ));

    // Steep monitor shrink crosses <= 2/3 pixel threshold and requires SHM buffer resync.
    rotated.set_screen_size(1280, 500);
    let shrunk_spec = compute_buffer_target_spec(&rotated, &display_cache, 1, 10, true, false);
    let shrunk_ring = (shrunk_spec.width, shrunk_spec.height);
    assert!(ring_needs_resync(shrunk_ring, initial_ring));

    // Growing beyond the shrunk ring triggers resync for sharpness.
    assert!(ring_needs_resync(initial_ring, shrunk_ring));

    // 2. Static downscaled single-frame overlay also resyncs when compositor configure shrinks screen.
    let mut static_layout = OverlayLayout::new(
        2000,
        1000,
        2560,
        1440,
        OverlayState {
            x: 0.0,
            y: 0.0,
            scale: 1.0,
            rotation: 0.0,
            flipped: false,
        },
    );
    let static_initial =
        compute_buffer_target_spec(&static_layout, &display_cache, 1, 1, true, true);
    static_layout.set_screen_size(1000, 600);
    let static_shrunk =
        compute_buffer_target_spec(&static_layout, &display_cache, 1, 1, true, true);
    assert!(static_shrunk.cache_key.is_none());
    assert!(ring_needs_resync(
        (static_shrunk.width, static_shrunk.height),
        (static_initial.width, static_initial.height)
    ));
}
