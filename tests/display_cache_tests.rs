//! Integration tests verifying display-resolution frame cache budgets and invalidation.

use animated_image_widget::wayland::display_cache::{
    DisplayCacheKey, DisplayFrameCache, angle_key,
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

    // Exact budget accepts all 4 frames (`4 * 10 * 10 * 4 = 1600` bytes).
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
