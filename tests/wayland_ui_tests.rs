//! Integration tests verifying monitor selection, chrome HiDPI geometry, Wayland buffer
//! orientations, and `--stats` runtime telemetry formatting and CPU/RAM sampling.

use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use animated_image_widget::{
    layout::Rotation,
    playback::{PlaybackStep, TickOutcome},
    wayland::{
        MonitorDescriptor, OverlayStats, StatsSnapshot,
        chrome::{
            ChromeGeometry, HANDLE_FADED_ALPHA, HANDLE_HOVER_ALPHA, HANDLE_IDLE_ALPHA,
            fill_rounded_handle_argb, needs_chrome_rebuild,
        },
        resolve_monitor_index, wl_transform_for_orientation,
    },
};
use wayland_client::protocol::wl_output::Transform;

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

#[test]
fn chrome_geometry_and_rebuild_policy_scale_hidpi_and_monitor_growth()
{
    let g1 = ChromeGeometry::for_scale(0);
    assert_eq!(g1.scale, 1);
    assert_eq!(g1.handle_px, 28);
    assert_eq!(g1.handle_radius_px, 8);
    assert_eq!(g1.outline_thickness_px, 1);
    assert_eq!(g1.dash_period_px, 12);

    let g2 = ChromeGeometry::for_scale(2);
    assert_eq!(g2.scale, 2);
    assert_eq!(g2.handle_px, 56);
    assert_eq!(g2.handle_radius_px, 16);
    assert_eq!(g2.outline_thickness_px, 2);
    assert_eq!(g2.dash_period_px, 24);
    assert!(g2.pool_byte_budget(1920, 1080) >= g2.handle_buffer_bytes() * 3);
    assert_eq!(g2.edge_buffer_size(1920, true), (3840, 2));
    assert_eq!(g2.edge_buffer_size(1080, false), (2, 2160));
    assert_eq!(g2.edge_viewport_crop(420, 1920, true), (840.0, 2.0, 420, 1));
    assert_eq!(
        g2.edge_viewport_crop(2400, 1920, true),
        (3840.0, 2.0, 1920, 1)
    );

    // Unchanged scale with equal or smaller dimensions skips pool rebuilding.
    assert_eq!(needs_chrome_rebuild(2, 2560, 1440, 2, 1920, 1080), None);
    // Larger configured dimensions trigger monotonic growth without changing scale.
    assert_eq!(
        needs_chrome_rebuild(2, 1920, 1080, 2, 2560, 1440),
        Some((2, 2560, 1440))
    );
    // Changing scale rebuilds even when dimensions remain within bounds.
    assert_eq!(
        needs_chrome_rebuild(1, 1920, 1080, 2, 1920, 1080),
        Some((2, 1920, 1080))
    );
}

#[test]
fn orientation_maps_to_expected_wayland_buffer_transforms()
{
    assert_eq!(
        wl_transform_for_orientation(Rotation::Deg0, false),
        Transform::Normal
    );
    assert_eq!(
        wl_transform_for_orientation(Rotation::Deg90, false),
        Transform::_90
    );
    assert_eq!(
        wl_transform_for_orientation(Rotation::Deg180, false),
        Transform::_180
    );
    assert_eq!(
        wl_transform_for_orientation(Rotation::Deg270, false),
        Transform::_270
    );
    assert_eq!(
        wl_transform_for_orientation(Rotation::Deg0, true),
        Transform::Flipped
    );
    assert_eq!(
        wl_transform_for_orientation(Rotation::Deg90, true),
        Transform::Flipped90
    );
    assert_eq!(
        wl_transform_for_orientation(Rotation::Deg180, true),
        Transform::Flipped180
    );
    assert_eq!(
        wl_transform_for_orientation(Rotation::Deg270, true),
        Transform::Flipped270
    );
}

#[test]
fn overlay_stats_formats_fps_cache_state_and_one_shot_decode_completion()
{
    let t0 = Instant::now();
    let mut cold = OverlayStats::new(t0, false);

    cold.record_tick(PlaybackStep {
        outcome: TickOutcome::FrameChanged(1),
        skipped_frames: 2,
        frontier_held: true,
    });
    cold.record_presented(Duration::from_micros(100));
    cold.record_presented(Duration::from_micros(300));

    let line1 = cold.take_interval_report(
        t0 + Duration::from_secs(1),
        StatsSnapshot {
            frame_count: 20,
            decode_done: false,
            paused: false,
            fits_source_ram: true,
            ram_used_bytes: 10 * 1024 * 1024,
            is_live_ring: false,
            display_cache_active: false,
            display_cache_bytes: 0,
            cpu_time: Duration::from_millis(250),
            logical_cpus: 16,
            process_ram_bytes: 42 * 1024 * 1024,
        },
    );
    assert!(
        line1.contains("fps=2.0 cpu=25.0% host=1.56% ram=42.0MiB skips=2 holds=1 stage_us=200/300")
    );
    assert!(line1.contains("cache=ram(10.0MiB) disp=off decode=decoding(20f)"));
    assert!(!line1.contains("frame="));

    // Window counters and CPU baseline advance cleanly, and `decode=` omits once decoding finishes.
    let line2 = cold.take_interval_report(
        t0 + Duration::from_secs(2),
        StatsSnapshot {
            frame_count: 180,
            decode_done: true,
            paused: true,
            fits_source_ram: false,
            ram_used_bytes: 0,
            is_live_ring: false,
            display_cache_active: true,
            display_cache_bytes: 32 * 1024 * 1024,
            cpu_time: Duration::from_millis(2_250),
            logical_cpus: 16,
            process_ram_bytes: 58 * 1024 * 1024,
        },
    );
    assert!(line2.contains(
        "fps=0.0 paused cpu=200.0% host=12.50% ram=58.0MiB skips=0 holds=0 stage_us=0/0"
    ));
    assert!(line2.contains("cache=lraw disp=32.0MiB"));
    assert!(!line2.contains("decode="));
    assert!(!line2.contains("frame="));

    // Cold decode completion emits exactly once when `decode_done` flips true.
    assert!(
        cold.check_decode_completion(t0 + Duration::from_secs(2), 180, true)
            .is_some_and(|msg| msg.contains("cold decode finished: 180 frames in 2.00s"))
    );
    assert!(
        cold.check_decode_completion(t0 + Duration::from_secs(3), 180, true)
            .is_none()
    );

    // Warm cache launches omit cold-decode completion and redundant `decode=warm` tag.
    let mut warm = OverlayStats::new(t0, true);
    assert!(warm.check_decode_completion(t0, 60, true).is_none());
    let warm_line = warm.format_report(
        Duration::from_secs(1),
        StatsSnapshot {
            frame_count: 60,
            decode_done: true,
            paused: false,
            fits_source_ram: true,
            ram_used_bytes: 0,
            is_live_ring: false,
            display_cache_active: false,
            display_cache_bytes: 0,
            cpu_time: Duration::from_millis(7),
            logical_cpus: 16,
            process_ram_bytes: 24 * 1024 * 1024,
        },
    );
    assert!(warm_line.contains("cpu=0.7% host=0.04% ram=24.0MiB"));
    assert!(!warm_line.contains("decode="));

    let live_line = warm.format_report(
        Duration::from_secs(1),
        StatsSnapshot {
            frame_count: 120,
            decode_done: true,
            paused: false,
            fits_source_ram: false,
            ram_used_bytes: 12 * 1024 * 1024,
            is_live_ring: true,
            display_cache_active: false,
            display_cache_bytes: 0,
            cpu_time: Duration::from_millis(7),
            logical_cpus: 16,
            process_ram_bytes: 36 * 1024 * 1024,
        },
    );
    assert!(live_line.contains("cache=live(12.0MiB) disp=off"));
}

#[test]
fn overlay_stats_tracks_cpu_baselines_and_degenerate_inputs()
{
    let t0 = Instant::now();
    let mut stats = OverlayStats::with_cpu(t0, true, Duration::from_millis(500));

    let anchored = stats.take_interval_report(
        t0 + Duration::from_secs(1),
        StatsSnapshot {
            frame_count: 10,
            decode_done: true,
            paused: false,
            fits_source_ram: true,
            ram_used_bytes: 0,
            is_live_ring: false,
            display_cache_active: false,
            display_cache_bytes: 0,
            cpu_time: Duration::from_millis(900),
            logical_cpus: 4,
            process_ram_bytes: 16 * 1024 * 1024,
        },
    );
    assert!(anchored.contains("cpu=40.0% host=10.00% ram=16.0MiB"));

    // Regressed CPU counters and zero logical core counts saturate safely without NaN or underflow.
    let safe = stats.format_report(
        Duration::from_secs(1),
        StatsSnapshot {
            frame_count: 10,
            decode_done: true,
            paused: false,
            fits_source_ram: true,
            ram_used_bytes: 0,
            is_live_ring: false,
            display_cache_active: false,
            display_cache_bytes: 0,
            cpu_time: Duration::from_millis(200),
            logical_cpus: 0,
            process_ram_bytes: 0,
        },
    );
    assert!(safe.contains("cpu=0.0% host=0.00% ram=0.0MiB"));

    let zero_elapsed = stats.format_report(
        Duration::ZERO,
        StatsSnapshot {
            frame_count: 10,
            decode_done: true,
            paused: false,
            fits_source_ram: true,
            ram_used_bytes: 0,
            is_live_ring: false,
            display_cache_active: false,
            display_cache_bytes: 0,
            cpu_time: Duration::from_millis(2_000),
            logical_cpus: 8,
            process_ram_bytes: 0,
        },
    );
    assert!(zero_elapsed.contains("cpu=0.0% host=0.00%"));

    // `/proc/self/status` `VmRSS` parser reads KiB accurately and tolerates missing/malformed data.
    assert_eq!(
        OverlayStats::parse_vm_rss_bytes("Name:\ta\nVmRSS:\t   26212 kB\nRssAnon:\t12 kB\n"),
        Some(26_212 * 1024)
    );
    assert_eq!(OverlayStats::parse_vm_rss_bytes("VmSize:\t100 kB\n"), None);
    assert_eq!(OverlayStats::parse_vm_rss_bytes("VmRSS:\tbad kB\n"), None);
    assert!(OverlayStats::sample_process_ram_bytes() > 0);

    // Live OS CPU clock remains monotonic across process work and reports at least 1 core.
    assert!(OverlayStats::logical_cpu_count() >= 1);
    let c0 = OverlayStats::sample_cpu_time();
    let mut acc = 0_u64;
    for i in 0..100_000_u64
    {
        acc = acc.wrapping_add(black_box(i));
    }
    black_box(acc);
    let c1 = OverlayStats::sample_cpu_time();
    assert!(c1 >= c0);
}

#[test]
fn handle_rasterizer_draws_drop_shadow_and_contrast_boundary()
{
    for (scale, alpha) in [
        (1, HANDLE_FADED_ALPHA),
        (1, HANDLE_IDLE_ALPHA),
        (1, HANDLE_HOVER_ALPHA),
        (2, HANDLE_HOVER_ALPHA),
    ]
    {
        let geom = ChromeGeometry::for_scale(scale);
        let size = geom.handle_px;
        let mut argb = vec![0_u8; (size as usize) * (size as usize) * 4];

        fill_rounded_handle_argb(&mut argb, size, geom.handle_radius_px, alpha);

        let mut shadow_pixels = 0;
        let mut pill_pixels = 0;
        let mut min_white_bg_val = 255_u32;

        for py in 0..size
        {
            for px in 0..size
            {
                let idx = (py * size + px) as usize * 4;
                let b = argb[idx];
                let g = argb[idx + 1];
                let r = argb[idx + 2];
                let a = argb[idx + 3];

                // Valid premultiplied ARGB8888 invariant: channels must never exceed alpha.
                assert_eq!(b, g);
                assert_eq!(g, r);
                assert!(r <= a, "Premultiplied color {r} exceeds alpha {a}");

                if r == 0 && a > 0
                {
                    shadow_pixels += 1;
                }
                if r > 0
                {
                    pill_pixels += 1;
                }

                // Simulate Wayland compositor drawing over pure white background (255, 255, 255):
                // C_out = C_premul + 255 * (1 - A / 255) = r + 255 - a
                let composited_white = u32::from(r) + 255 - u32::from(a);
                if composited_white < min_white_bg_val
                {
                    min_white_bg_val = composited_white;
                }
            }
        }

        // Shadow pixels must exist outside the white pill to ensure visibility on bright images.
        assert!(
            shadow_pixels > 0,
            "Must have shadow pixels outside the pill (scale={scale}, alpha={alpha})"
        );
        // The white pill itself must be present in the center.
        assert!(
            pill_pixels > 0,
            "Must have white pill pixels in the handle body (scale={scale}, alpha={alpha})"
        );
        // The composited result on pure white must visibly darken (not blend to 255).
        assert!(
            min_white_bg_val < 250,
            "Handle must create visible contrast on pure white (got min value {min_white_bg_val})"
        );

        // Top-left outer buffer corner must gracefully fade to zero.
        let top_left_idx = 0;
        assert_eq!(argb[top_left_idx + 3], 0);
    }
}
