//! Integration tests verifying triple-click edit toggling, pointer drags, scroll scaling,
//! middle-click quit, hover fade timers, edit-mode keyboard shortcuts, and monitor selection.

use animated_image_widget::{
    config::OverlayState,
    layout::{KEYBOARD_NUDGE_PX, OverlayLayout},
    wayland::{
        ClickTracker, InputState, KeyShortcut, MonitorDescriptor, TRIPLE_CLICK_WINDOW_MS,
        map_edit_keysym, resolve_monitor_index,
    },
};
use smithay_client_toolkit::seat::{
    keyboard::Keysym,
    pointer::{AxisScroll, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT},
};

fn sample_layout() -> OverlayLayout
{
    OverlayLayout::new(
        400,
        300,
        1920,
        1080,
        OverlayState {
            x: 200.0,
            y: 150.0,
            scale: 1.0,
        },
    )
}

#[test]
fn triple_click_toggles_only_within_time_window_and_distance_threshold()
{
    let mut tracker = ClickTracker::default();

    // Two fast clicks + slow third click -> does not trigger triple-click.
    assert!(!tracker.register_left_press(100, (500.0, 500.0)));
    assert!(!tracker.register_left_press(250, (502.0, 501.0)));
    assert!(!tracker.register_left_press(250 + TRIPLE_CLICK_WINDOW_MS + 10, (501.0, 501.0)));
    assert_eq!(tracker.pending_count(), 1);

    // Move too far (> 4 px) resets sequence.
    assert!(!tracker.register_left_press(800, (520.0, 501.0)));
    assert_eq!(tracker.pending_count(), 1);

    // Three quick taps within 4 px succeed on third press and reset afterward.
    assert!(!tracker.register_left_press(900, (522.0, 502.0)));
    assert!(tracker.register_left_press(1050, (521.0, 500.0)));
    assert_eq!(tracker.pending_count(), 0);
}

#[test]
fn pointer_drag_move_resize_scroll_and_middle_click_update_layout_and_save()
{
    let mut layout = sample_layout();
    let mut input = InputState::default();

    // Triple-click corner handle to enter edit mode.
    let handle_pos = (590.0, 440.0);
    assert_eq!(
        input
            .on_button_press(BTN_LEFT, 100, handle_pos, &layout)
            .edit_mode_changed,
        None
    );
    input.on_button_release(BTN_LEFT);
    assert_eq!(
        input
            .on_button_press(BTN_LEFT, 220, handle_pos, &layout)
            .edit_mode_changed,
        None
    );
    input.on_button_release(BTN_LEFT);
    let toggled = input.on_button_press(BTN_LEFT, 340, handle_pos, &layout);
    assert_eq!(toggled.edit_mode_changed, Some(true));
    assert!(input.editing);

    // Left-drag repositions overlay; release triggers debounced save.
    input.on_button_press(BTN_LEFT, 1000, (300.0, 300.0), &layout);
    let moved = input.on_pointer_motion((350.0, 270.0), &mut layout);
    assert!(moved.layout_changed);
    assert_eq!(layout.state().x, 250.0);
    assert_eq!(layout.state().y, 120.0);
    assert!(input.on_button_release(BTN_LEFT).schedule_save);

    // Right-drag scales preserving top-left corner.
    input.on_button_press(BTN_RIGHT, 1200, (400.0, 300.0), &layout);
    input.on_pointer_motion((600.0, 300.0), &mut layout);
    assert!((layout.state().scale - 1.5).abs() < 1e-9);
    assert!(input.on_button_release(BTN_RIGHT).schedule_save);

    // Scroll zooms and schedules save.
    let before_scroll = layout.state().scale;
    let scrolled = input.on_scroll(
        AxisScroll {
            discrete: -1,
            ..AxisScroll::default()
        },
        &mut layout,
    );
    assert!(scrolled.layout_changed);
    assert!(scrolled.schedule_save);
    assert!(layout.state().scale > before_scroll);

    // Middle-click inside overlay body in edit mode does not quit; middle-click on corner handle quits.
    let center = (300.0, 250.0);
    assert!(
        !input
            .on_button_press(BTN_MIDDLE, 1500, center, &layout)
            .quit
    );
    let rect = layout.input_rect(false);
    let on_handle = (f64::from(rect.x) + 5.0, f64::from(rect.y) + 5.0);
    assert!(
        input
            .on_button_press(BTN_MIDDLE, 1600, on_handle, &layout)
            .quit
    );
}

#[test]
fn hover_and_edit_state_govern_handle_fade()
{
    let layout = sample_layout();
    let mut input = InputState::default();
    assert!(input.handle_visible);

    // Fade timeout hides handle when passive, unhovered, and idle.
    assert!(input.on_fade_timeout().handle_visual_changed);
    assert!(!input.handle_visible);

    // Hovering restores handle and cancels fade timer.
    let enter = input.on_pointer_enter();
    assert!(enter.handle_visual_changed);
    assert!(enter.cancel_fade_timer);
    assert!(input.handle_visible);

    // While hovered, fade timeout is a no-op.
    assert!(!input.on_fade_timeout().handle_visual_changed);

    // Leaving arms fade timer.
    let leave = input.on_pointer_leave();
    assert!(leave.reset_fade_timer);

    // Middle-click while passive exits cleanly.
    assert!(
        input
            .on_button_press(BTN_MIDDLE, 200, (590.0, 440.0), &layout)
            .quit
    );
}

#[test]
fn keyboard_shortcuts_operate_only_in_edit_mode()
{
    let mut layout = sample_layout();
    let mut input = InputState::default();

    // Ignored in passive playback (`KeyboardInteractivity::None` / `!editing`).
    assert!(
        !input
            .on_key_press(Keysym::Right, &mut layout)
            .layout_changed
    );
    assert!(!input.on_key_press(Keysym::q, &mut layout).quit);

    input.editing = true;

    let move_right = input.on_key_press(Keysym::Right, &mut layout);
    assert!(move_right.layout_changed);
    assert!(move_right.schedule_save);
    assert_eq!(layout.state().x, 200.0 + KEYBOARD_NUDGE_PX);

    let zoom_in = input.on_key_press(Keysym::plus, &mut layout);
    assert!(zoom_in.layout_changed);
    assert!(zoom_in.schedule_save);
    assert!(layout.state().scale > 1.0);

    assert!(input.on_key_press(Keysym::space, &mut layout).toggle_pause);

    let exit_edit = input.on_key_press(Keysym::Escape, &mut layout);
    assert_eq!(exit_edit.edit_mode_changed, Some(false));
    assert!(!input.editing);

    assert_eq!(
        map_edit_keysym(Keysym::j),
        Some(KeyShortcut::Move { dir_x: 0, dir_y: 1 })
    );
    assert_eq!(
        map_edit_keysym(Keysym::k),
        Some(KeyShortcut::Move {
            dir_x: 0,
            dir_y: -1
        })
    );
    assert_eq!(
        map_edit_keysym(Keysym::h),
        Some(KeyShortcut::Move {
            dir_x: -1,
            dir_y: 0
        })
    );
    assert_eq!(
        map_edit_keysym(Keysym::l),
        Some(KeyShortcut::Move { dir_x: 1, dir_y: 0 })
    );
    assert_eq!(
        map_edit_keysym(Keysym::minus),
        Some(KeyShortcut::Scale { steps: -1 })
    );
    assert_eq!(map_edit_keysym(Keysym::q), Some(KeyShortcut::Quit));
}

#[test]
fn resolve_monitor_index_matches_index_and_connector_or_explains_failure()
{
    let monitors = vec![
        MonitorDescriptor {
            index: 0,
            name: Some(String::from("eDP-1")),
            width: 1920,
            height: 1080,
        },
        MonitorDescriptor {
            index: 1,
            name: Some(String::from("DP-1")),
            width: 2560,
            height: 1440,
        },
    ];

    assert_eq!(resolve_monitor_index(&monitors, "0").unwrap(), 0);
    assert_eq!(resolve_monitor_index(&monitors, "DP-1").unwrap(), 1);

    let err_oob = resolve_monitor_index(&monitors, "5")
        .unwrap_err()
        .to_string();
    assert!(err_oob.contains("out of range"));
    assert!(err_oob.contains("eDP-1"));

    let err_name = resolve_monitor_index(&monitors, "HDMI-1")
        .unwrap_err()
        .to_string();
    assert!(err_name.contains("HDMI-1"));
    assert!(err_name.contains("1:DP-1"));

    let err_empty = resolve_monitor_index(&[], "0").unwrap_err().to_string();
    assert!(err_empty.contains("no monitors found"));
}
