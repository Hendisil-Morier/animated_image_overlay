//! Integration tests verifying triple-click edit toggling, pointer drags, scroll scaling,
//! middle-click quit, hover fade timers, and edit-mode keyboard shortcuts.

use animated_image_widget::{
    config::OverlayState,
    layout::{FlipAxis, KEYBOARD_NUDGE_PX, OverlayLayout, Rotation},
    wayland::{
        ClickTracker, InputEffects, InputState, KeyShortcut, PointerCursor, TRIPLE_CLICK_WINDOW_MS,
        chrome::{HANDLE_FADED_ALPHA, HANDLE_HOVER_ALPHA, HANDLE_IDLE_ALPHA, handle_opacity_alpha},
        map_edit_keysym,
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
            rotation: 0.0,
            flipped: false,
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
    let left_release = input.on_button_release(BTN_LEFT);
    assert!(left_release.schedule_save);
    assert!(!left_release.layout_changed);

    // Right-drag scales preserving top-left corner; release marks layout updated for final resync.
    input.on_button_press(BTN_RIGHT, 1200, (400.0, 300.0), &layout);
    assert!(input.is_resizing());
    input.on_pointer_motion((600.0, 300.0), &mut layout);
    assert!((layout.state().scale - 1.5).abs() < 1e-9);
    let right_release = input.on_button_release(BTN_RIGHT);
    assert!(!input.is_resizing());
    assert!(right_release.schedule_save);
    assert!(right_release.layout_changed);

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
    assert_eq!(handle_opacity_alpha(&input), HANDLE_IDLE_ALPHA);

    // Fade timeout dims handle to a subtle visible resting opacity when passive and idle.
    assert!(input.on_fade_timeout().handle_visual_changed);
    assert!(!input.handle_visible);
    let faded_alpha = handle_opacity_alpha(&input);
    assert_eq!(faded_alpha, HANDLE_FADED_ALPHA);
    assert!(faded_alpha > 0 && faded_alpha < HANDLE_IDLE_ALPHA);

    // Hovering restores full active highlight and cancels fade timer.
    let enter = input.on_pointer_enter();
    assert!(enter.handle_visual_changed);
    assert!(enter.cancel_fade_timer);
    assert!(input.handle_visible);
    assert_eq!(handle_opacity_alpha(&input), HANDLE_HOVER_ALPHA);

    // While hovered, fade timeout is a no-op.
    assert!(!input.on_fade_timeout().handle_visual_changed);

    // Leaving arms fade timer and drops back to idle opacity.
    let leave = input.on_pointer_leave();
    assert!(leave.reset_fade_timer);
    assert_eq!(handle_opacity_alpha(&input), HANDLE_IDLE_ALPHA);

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
    assert!(!input.on_key_repeat(Keysym::space, &mut layout).toggle_pause);
    assert!(!input.on_key_repeat(Keysym::q, &mut layout).quit);
    assert!(
        input
            .on_key_repeat(Keysym::Left, &mut layout)
            .layout_changed
    );
    assert!(
        !input
            .on_key_repeat(Keysym::Escape, &mut layout)
            .layout_changed
    );

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
    assert_eq!(
        map_edit_keysym(Keysym::r),
        Some(KeyShortcut::Rotate {
            delta_deg: 90,
            repeatable: false
        })
    );
    assert_eq!(
        map_edit_keysym(Keysym::R),
        Some(KeyShortcut::Rotate {
            delta_deg: -90,
            repeatable: false
        })
    );
    assert_eq!(
        map_edit_keysym(Keysym::bracketleft),
        Some(KeyShortcut::Rotate {
            delta_deg: -5,
            repeatable: true
        })
    );
    assert_eq!(
        map_edit_keysym(Keysym::bracketright),
        Some(KeyShortcut::Rotate {
            delta_deg: 5,
            repeatable: true
        })
    );
    assert_eq!(
        map_edit_keysym(Keysym::_0),
        Some(KeyShortcut::ResetRotation)
    );
    assert_eq!(
        map_edit_keysym(Keysym::f),
        Some(KeyShortcut::Flip(FlipAxis::Horizontal))
    );
    assert_eq!(
        map_edit_keysym(Keysym::F),
        Some(KeyShortcut::Flip(FlipAxis::Vertical))
    );
    assert_eq!(map_edit_keysym(Keysym::q), Some(KeyShortcut::Quit));
    assert_eq!(map_edit_keysym(Keysym::m), Some(KeyShortcut::ToggleMute));
    assert_eq!(
        map_edit_keysym(Keysym::period),
        Some(KeyShortcut::AdjustVolume { steps: 1 })
    );
    assert_eq!(
        map_edit_keysym(Keysym::comma),
        Some(KeyShortcut::AdjustVolume { steps: -1 })
    );

    // Audio shortcuts need edit mode, schedule a save, and volume repeats while held.
    assert_eq!(
        input.on_key_press(Keysym::m, &mut layout),
        InputEffects::default()
    );
    input.editing = true;
    let mute = input.on_key_press(Keysym::m, &mut layout);
    assert!(mute.toggle_mute && mute.schedule_save);
    let louder = input.on_key_press(Keysym::period, &mut layout);
    assert_eq!(louder.volume_steps, 1);
    assert!(louder.schedule_save);
    assert_eq!(
        input.on_key_repeat(Keysym::comma, &mut layout).volume_steps,
        -1
    );
    assert!(!input.on_key_repeat(Keysym::m, &mut layout).toggle_mute);
}

#[test]
fn rotate_shortcuts_work_only_in_edit_mode_and_manage_repeats_and_redraws()
{
    let mut layout = sample_layout();
    let mut input = InputState::default();

    // Ignored in passive playback.
    let passive = input.on_key_press(Keysym::r, &mut layout);
    assert!(!passive.layout_changed);
    assert_eq!(layout.rotation(), 0.0);

    input.editing = true;

    let cw = input.on_key_press(Keysym::r, &mut layout);
    assert!(cw.layout_changed);
    assert!(!cw.frame_redraw_needed);
    assert!(cw.schedule_save);
    assert_eq!(layout.rotation(), 90.0);
    assert_eq!(layout.quarter_turn(), Some(Rotation::Deg90));
    assert_eq!(layout.overlay_size(), (300, 400));

    // Held r/R repeat events are ignored so quarter-turn jumps stay single-step.
    assert!(!input.on_key_repeat(Keysym::r, &mut layout).layout_changed);
    assert_eq!(layout.rotation(), 90.0);

    let ccw = input.on_key_press(Keysym::R, &mut layout);
    assert!(ccw.layout_changed);
    assert!(!ccw.frame_redraw_needed);
    assert!(ccw.schedule_save);
    assert_eq!(layout.rotation(), 0.0);
    assert_eq!(layout.overlay_size(), (400, 300));

    // Fine rotation keys advance 5 degrees per press or repeat and request CPU redraws.
    let fine = input.on_key_press(Keysym::bracketright, &mut layout);
    assert!(fine.layout_changed);
    assert!(fine.frame_redraw_needed);
    assert!(fine.schedule_save);
    assert_eq!(layout.rotation(), 5.0);

    let fine_repeat = input.on_key_repeat(Keysym::bracketright, &mut layout);
    assert!(fine_repeat.layout_changed);
    assert!(fine_repeat.frame_redraw_needed);
    assert_eq!(layout.rotation(), 10.0);

    // Stepping back onto 0 degrees restores hardware buffer sizing cleanly.
    let _ = input.on_key_press(Keysym::bracketleft, &mut layout);
    let back_to_zero = input.on_key_repeat(Keysym::bracketleft, &mut layout);
    assert!(back_to_zero.frame_redraw_needed);
    assert_eq!(layout.rotation(), 0.0);
    assert_eq!(layout.quarter_turn(), Some(Rotation::Deg0));

    // Reset via '0' restores upright orientation while ignoring hold-repeat.
    let _ = input.on_key_press(Keysym::bracketright, &mut layout);
    assert!(!input.on_key_repeat(Keysym::_0, &mut layout).layout_changed);
    let reset = input.on_key_press(Keysym::_0, &mut layout);
    assert!(reset.layout_changed);
    assert!(reset.frame_redraw_needed);
    assert!(reset.schedule_save);
    assert_eq!(layout.rotation(), 0.0);

    // Repeating reset at 0 degrees avoids redundant CPU resampling.
    let noop_reset = input.on_key_press(Keysym::_0, &mut layout);
    assert!(!noop_reset.frame_redraw_needed);
}

#[test]
fn flip_shortcuts_work_only_in_edit_mode_and_ignore_repeats()
{
    let mut layout = sample_layout();
    let mut input = InputState::default();

    let passive = input.on_key_press(Keysym::f, &mut layout);
    assert!(!passive.layout_changed);
    assert!(!layout.flipped());

    input.editing = true;

    let horiz = input.on_key_press(Keysym::f, &mut layout);
    assert!(horiz.layout_changed);
    assert!(!horiz.frame_redraw_needed);
    assert!(horiz.schedule_save);
    assert!(layout.flipped());
    assert_eq!(layout.rotation(), 0.0);

    // Held f/F repeat events are ignored so mirroring does not oscillate rapidly.
    assert!(!input.on_key_repeat(Keysym::f, &mut layout).layout_changed);
    assert!(layout.flipped());

    let vert = input.on_key_press(Keysym::F, &mut layout);
    assert!(vert.layout_changed);
    assert!(!vert.frame_redraw_needed);
    assert!(vert.schedule_save);
    assert!(!layout.flipped());
    assert_eq!(layout.rotation(), 180.0);
    assert_eq!(layout.overlay_size(), (400, 300));

    // Flipping at a fractional angle triggers CPU redraw instead of hardware transform.
    let _ = input.on_key_press(Keysym::bracketright, &mut layout);
    let frac_flip = input.on_key_press(Keysym::f, &mut layout);
    assert!(frac_flip.frame_redraw_needed);
    assert!(layout.flipped());
}

#[test]
fn passive_handle_drag_moves_overlay_and_updates_cursor_and_input_region()
{
    let mut layout = sample_layout();
    let mut input = InputState::default();
    assert!(!input.editing);

    // Passive input region restricts hit-testing to the bottom-right 28x28 corner handle.
    let handle_rect = layout.input_rect(false);
    assert_eq!(handle_rect.width, 28);
    assert_eq!(handle_rect.height, 28);

    // Points outside the passive handle keep the default cursor and lie outside `input_rect(false)`.
    let outside = (250.0, 200.0);
    assert!(!layout.handle_contains(outside.0, outside.1));
    assert_eq!(
        input.desired_cursor(outside, &layout),
        PointerCursor::Default
    );

    // Hovering and dragging inside the passive handle moves overlay + input region.
    let handle_center = (
        f64::from(handle_rect.x) + f64::from(handle_rect.width) / 2.0,
        f64::from(handle_rect.y) + f64::from(handle_rect.height) / 2.0,
    );
    let _enter = input.on_pointer_enter();
    assert_eq!(
        input.desired_cursor(handle_center, &layout),
        PointerCursor::Grab
    );

    let press = input.on_button_press(BTN_LEFT, 1000, handle_center, &layout);
    assert!(press.cancel_fade_timer);
    assert_eq!(
        input.desired_cursor(handle_center, &layout),
        PointerCursor::Grabbing
    );

    let target = (handle_center.0 + 45.0, handle_center.1 - 30.0);
    let moved = input.on_pointer_motion(target, &mut layout);
    assert!(moved.layout_changed);
    assert_eq!(layout.state().x, 245.0);
    assert_eq!(layout.state().y, 120.0);

    let shifted_handle = layout.input_rect(false);
    assert_eq!(shifted_handle.x, handle_rect.x + 45);
    assert_eq!(shifted_handle.y, handle_rect.y - 30);

    let released = input.on_button_release(BTN_LEFT);
    assert!(released.schedule_save);
    assert_eq!(input.desired_cursor(target, &layout), PointerCursor::Grab);
}

#[test]
fn pointer_detach_and_chorded_buttons_preserve_consistent_drag_state()
{
    let mut layout = sample_layout();
    let mut input = InputState::default();

    // Start a left-button move drag; chorded BTN_RIGHT press and release must not clobber Move.
    let _enter = input.on_pointer_enter();
    let _press_left = input.on_button_press(BTN_LEFT, 1000, (300.0, 300.0), &layout);
    let _chord_right = input.on_button_press(BTN_RIGHT, 1050, (310.0, 300.0), &layout);
    assert_eq!(
        input.desired_cursor((310.0, 300.0), &layout),
        PointerCursor::Grabbing
    );
    assert!(!input.on_button_release(BTN_RIGHT).schedule_save);
    let moved = input.on_pointer_motion((340.0, 320.0), &mut layout);
    assert!(moved.layout_changed);
    assert_eq!(layout.state().x, 240.0);
    assert_eq!(layout.state().y, 170.0);

    // Detaching pointer mid-drag clears hover and drag, schedules save, and rearms fade timer.
    let detached = input.on_pointer_detach();
    assert!(!input.hovered);
    assert!(!input.is_dragging());
    assert_eq!(input.click_tracker.pending_count(), 0);
    assert!(detached.schedule_save);
    assert!(detached.reset_fade_timer);

    // Start a right-button resize drag; chorded BTN_LEFT must preserve Resize without editing toggle.
    let _enter2 = input.on_pointer_enter();
    let _press_right = input.on_button_press(BTN_RIGHT, 2000, (400.0, 300.0), &layout);
    let chord_left = input.on_button_press(BTN_LEFT, 2050, (400.0, 300.0), &layout);
    assert_eq!(chord_left.edit_mode_changed, None);
    assert_eq!(
        input.desired_cursor((400.0, 300.0), &layout),
        PointerCursor::ResizeBottomRight
    );
    assert!(!input.on_button_release(BTN_LEFT).schedule_save);
    assert!(input.on_button_release(BTN_RIGHT).schedule_save);
}

#[test]
fn startup_edit_mode_enables_shortcuts_and_keeps_handle_visible()
{
    let mut layout = sample_layout();
    let mut input = InputState::with_editing(true);

    assert!(input.editing);
    assert!(input.handle_visible);
    assert_eq!(
        input.desired_cursor((220.0, 170.0), &layout),
        PointerCursor::Grab
    );

    // Fade timeouts never hide the corner handle while starting in edit mode.
    assert!(!input.on_fade_timeout().handle_visual_changed);
    assert!(input.handle_visible);

    // Keyboard shortcuts respond immediately without triple-clicking first.
    let nudged = input.on_key_press(Keysym::Down, &mut layout);
    assert!(nudged.layout_changed);
    assert!(nudged.schedule_save);
    assert_eq!(layout.state().y, 150.0 + KEYBOARD_NUDGE_PX);
}

#[test]
fn input_effects_coalesce_scene_flags_without_dropping_transitions()
{
    let mut pending = InputEffects::default();
    assert!(!pending.has_scene_updates());

    pending.merge_scene_flags(InputEffects {
        handle_visual_changed: true,
        edit_mode_changed: Some(true),
        cancel_fade_timer: true,
        ..InputEffects::default()
    });
    assert!(pending.has_scene_updates());
    assert!(pending.handle_visual_changed);
    assert_eq!(pending.edit_mode_changed, Some(true));
    assert!(!pending.cancel_fade_timer);

    pending.merge_scene_flags(InputEffects {
        layout_changed: true,
        frame_redraw_needed: true,
        edit_mode_changed: Some(false),
        schedule_save: true,
        ..InputEffects::default()
    });
    assert!(pending.layout_changed);
    assert!(pending.frame_redraw_needed);
    assert!(pending.handle_visual_changed);
    assert_eq!(pending.edit_mode_changed, Some(false));
    assert!(!pending.schedule_save);
}
