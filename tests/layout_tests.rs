//! Integration tests verifying overlay bounds clamping, drag math, scroll scaling, and hit regions.

use animated_image_widget::{
    config::OverlayState,
    layout::{
        FlipAxis, HANDLE_SIZE, LogicalRect, MIN_OVERLAY_SIZE, OverlayLayout, RotateDirection,
        Rotation, SCROLL_ZOOM_BASE,
    },
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
            rotation: Rotation::Deg0,
            flipped: false,
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
            rotation: Rotation::Deg0,
            flipped: false,
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
            rotation: Rotation::Deg0,
            flipped: false,
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
            rotation: Rotation::Deg0,
            flipped: false,
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

#[test]
fn rotation_enum_cycles_full_turn_and_swaps_quarter_turn_axes()
{
    let mut rot = Rotation::Deg0;
    rot = rot.rotated(RotateDirection::Clockwise);
    assert_eq!(rot, Rotation::Deg90);
    assert_eq!(rot.orient_dimensions(500, 700), (700, 500));

    rot = rot.rotated(RotateDirection::Clockwise);
    assert_eq!(rot, Rotation::Deg180);
    assert_eq!(rot.orient_dimensions(500, 700), (500, 700));

    rot = rot.rotated(RotateDirection::Clockwise);
    assert_eq!(rot, Rotation::Deg270);
    assert_eq!(rot.orient_dimensions(500, 700), (700, 500));

    rot = rot.rotated(RotateDirection::Clockwise);
    assert_eq!(rot, Rotation::Deg0);

    assert_eq!(
        rot.rotated(RotateDirection::CounterClockwise),
        Rotation::Deg270
    );
    assert_eq!(
        Rotation::Deg90.rotated(RotateDirection::CounterClockwise),
        Rotation::Deg0
    );
}

#[test]
fn layout_rotate_swaps_bounds_preserves_center_and_uses_oriented_resize()
{
    let mut layout = OverlayLayout::new(
        400,
        200,
        1920,
        1080,
        OverlayState {
            x: 500.0,
            y: 400.0,
            scale: 1.0,
            rotation: Rotation::Deg0,
            flipped: false,
        },
    );

    layout.rotate(RotateDirection::Clockwise);
    assert_eq!(layout.rotation(), Rotation::Deg90);
    assert_eq!(layout.oriented_frame_size(), (200, 400));
    assert_eq!(layout.overlay_size(), (200, 400));
    assert_eq!(layout.state().x, 600.0);
    assert_eq!(layout.state().y, 300.0);

    let start_scale = layout.state().scale;
    layout.on_resize(start_scale, 100.0);
    assert!((layout.state().scale - 1.5).abs() < 1e-9);
    assert_eq!(layout.overlay_size(), (300, 600));

    layout.rotate(RotateDirection::CounterClockwise);
    assert_eq!(layout.rotation(), Rotation::Deg0);
    assert_eq!(layout.overlay_size(), (600, 300));
}

#[test]
fn layout_rotate_clamps_wide_overlay_against_screen_height()
{
    let mut layout = OverlayLayout::new(
        1800,
        600,
        1920,
        1080,
        OverlayState {
            x: 0.0,
            y: 100.0,
            scale: 1.0,
            rotation: Rotation::Deg0,
            flipped: false,
        },
    );

    layout.rotate(RotateDirection::Clockwise);
    let expected_scale = 1080.0 / 1800.0;
    assert!((layout.state().scale - expected_scale).abs() < 1e-9);
    let (w, h) = layout.overlay_size();
    assert!(w <= 1920 && h <= 1080);
    assert!(layout.state().x >= 0.0 && layout.state().y >= 0.0);
}

#[test]
fn layout_restores_saved_rotation_on_startup_and_clamps_bounds()
{
    let layout = OverlayLayout::new(
        1800,
        600,
        1920,
        1080,
        OverlayState {
            x: 2000.0,
            y: 200.0,
            scale: 1.0,
            rotation: Rotation::Deg270,
            flipped: false,
        },
    );

    assert_eq!(layout.rotation(), Rotation::Deg270);
    assert!(!layout.flipped());
    assert_eq!(layout.state().rotation, Rotation::Deg270);
    assert_eq!(layout.oriented_frame_size(), (600, 1800));
    let expected_scale = 1080.0 / 1800.0;
    assert!((layout.state().scale - expected_scale).abs() < 1e-9);
    let (w, h) = layout.overlay_size();
    assert!(w <= 1920 && h <= 1080);
    assert_eq!(layout.state().x, f64::from(1920 - w));
    assert_eq!(layout.state().y, 0.0);
}

#[test]
fn layout_flip_and_rotate_match_screen_space_dihedral_group()
{
    fn rot_cw(g: [[char; 2]; 2]) -> [[char; 2]; 2]
    {
        [[g[1][0], g[0][0]], [g[1][1], g[0][1]]]
    }
    fn rot_ccw(g: [[char; 2]; 2]) -> [[char; 2]; 2]
    {
        [[g[0][1], g[1][1]], [g[0][0], g[1][0]]]
    }
    fn flip_h(g: [[char; 2]; 2]) -> [[char; 2]; 2]
    {
        [[g[0][1], g[0][0]], [g[1][1], g[1][0]]]
    }
    fn flip_v(g: [[char; 2]; 2]) -> [[char; 2]; 2]
    {
        [g[1], g[0]]
    }
    fn render(rotation: Rotation, flipped: bool) -> [[char; 2]; 2]
    {
        let mut grid = [['A', 'B'], ['C', 'D']];
        let steps = match rotation
        {
            Rotation::Deg0 => 0,
            Rotation::Deg90 => 1,
            Rotation::Deg180 => 2,
            Rotation::Deg270 => 3,
        };
        for _ in 0..steps
        {
            grid = rot_cw(grid);
        }
        if flipped
        {
            grid = flip_h(grid);
        }
        grid
    }

    enum Op
    {
        Rot(RotateDirection),
        Flip(FlipAxis),
    }

    let ops = [
        Op::Flip(FlipAxis::Horizontal),
        Op::Rot(RotateDirection::Clockwise),
        Op::Flip(FlipAxis::Vertical),
        Op::Rot(RotateDirection::CounterClockwise),
        Op::Rot(RotateDirection::CounterClockwise),
        Op::Flip(FlipAxis::Horizontal),
        Op::Flip(FlipAxis::Vertical),
        Op::Rot(RotateDirection::Clockwise),
    ];

    let mut layout = OverlayLayout::new(400, 300, 1920, 1080, OverlayState::default());
    let mut expected = [['A', 'B'], ['C', 'D']];

    for op in ops
    {
        match op
        {
            Op::Rot(dir) =>
            {
                layout.rotate(dir);
                expected = match dir
                {
                    RotateDirection::Clockwise => rot_cw(expected),
                    RotateDirection::CounterClockwise => rot_ccw(expected),
                };
            }
            Op::Flip(axis) =>
            {
                layout.flip(axis);
                expected = match axis
                {
                    FlipAxis::Horizontal => flip_h(expected),
                    FlipAxis::Vertical => flip_v(expected),
                };
            }
        }
        assert_eq!(render(layout.rotation(), layout.flipped()), expected);
    }

    // Flipping keeps size and position untouched while preserving center under later rotation.
    let rect_before = layout.overlay_rect();
    layout.flip(FlipAxis::Horizontal);
    assert_eq!(layout.overlay_rect(), rect_before);
}
