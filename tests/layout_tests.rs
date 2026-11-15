//! Integration tests verifying overlay bounds clamping, drag math, scroll scaling, and hit regions.

use animated_image_widget::{
    config::OverlayState,
    layout::{HANDLE_SIZE, LogicalRect, MIN_OVERLAY_SIZE, OverlayLayout, SCROLL_ZOOM_BASE},
};

#[test]
fn layout_clamps_scale_and_coordinates_within_monitor()
{
    let mut layout = OverlayLayout::new(
        500,
        500,
        1000,
        800,
        OverlayState {
            x: 5000.0,
            y: -20.0,
            scale: 10.0,
        },
    );

    let state = layout.state();
    assert!((state.scale - (800.0 / 500.0)).abs() < 1e-9);
    let (w, h) = layout.overlay_size();
    assert_eq!(state.x, f64::from(1000 - w));
    assert_eq!(state.y, 0.0);
    assert_eq!((w, h), (800, 800));

    layout.on_resize(state.scale, -5000.0);
    let (min_w, min_h) = layout.overlay_size();
    assert!(min_w.min(min_h) >= MIN_OVERLAY_SIZE);
}

#[test]
fn layout_drag_move_and_resize_preserve_top_left_and_bounds()
{
    let mut layout = OverlayLayout::new(
        400,
        200,
        1200,
        900,
        OverlayState {
            x: 100.0,
            y: 120.0,
            scale: 1.0,
        },
    );

    layout.on_move(100.0, 120.0, 50.0, -30.0);
    assert_eq!(layout.state().x, 150.0);
    assert_eq!(layout.state().y, 90.0);

    let start_scale = layout.state().scale;
    layout.on_resize(start_scale, 200.0);
    assert!((layout.state().scale - 1.5).abs() < 1e-9);
    assert_eq!(layout.overlay_size(), (600, 300));
    assert_eq!(layout.state().x, 150.0);
    assert_eq!(layout.state().y, 90.0);
}

#[test]
fn layout_scroll_zoom_matches_exponential_factor()
{
    let mut layout = OverlayLayout::new(
        500,
        500,
        1920,
        1080,
        OverlayState {
            x: 50.0,
            y: 50.0,
            scale: 0.5,
        },
    );

    layout.on_scroll(-2.0);
    let expected = 0.5 * SCROLL_ZOOM_BASE.powf(2.0);
    assert!((layout.state().scale - expected).abs() < 1e-9);
}

#[test]
fn layout_computes_passive_handle_and_edit_input_regions()
{
    let layout = OverlayLayout::new(
        400,
        300,
        1920,
        1080,
        OverlayState {
            x: 100.0,
            y: 200.0,
            scale: 1.0,
        },
    );

    assert_eq!(
        layout.input_rect(false),
        LogicalRect {
            x: 100 + 400 - HANDLE_SIZE as i32,
            y: 200 + 300 - HANDLE_SIZE as i32,
            width: HANDLE_SIZE,
            height: HANDLE_SIZE,
        }
    );

    assert_eq!(
        layout.input_rect(true),
        LogicalRect {
            x: 100,
            y: 200,
            width: 400,
            height: 300,
        }
    );

    assert!(layout.handle_contains(490.0, 490.0));
    assert!(!layout.handle_contains(150.0, 250.0));
}
