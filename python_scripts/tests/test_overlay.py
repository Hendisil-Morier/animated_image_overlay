from concurrent.futures import ThreadPoolExecutor  # Concurrent worker threads for lock testing
from contextlib import redirect_stderr           # Stderr capture for argparse error checks
import io                                        # In-memory text stream for redirected stderr
import os                                        # Filesystem path and directory inspection
import sys                                       # Module path setup for root imports
import tempfile                                  # Isolated per-test temporary directories
import time                                      # Clock/age manipulation and artificial delays
import unittest                                  # Unit test framework and assertions
from unittest.mock import MagicMock              # Lightweight GTK/GDK object stubs
import zlib                                      # Compressed RGBA frame construction for cache tests

from PIL import Image

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import window
import cache
import config
import overlay
from gi.repository import Gio, GLib


class TestOverlay(unittest.TestCase):
    def test_parse_args_rejects_invalid_numbers(self):
        for bad in (
            ["img.png", "-w", "0"],
            ["img.png", "-n", "-1"],
            ["img.png", "-s", "0"],
            ["img.png", "-s", "nan"],
        ):
            with self.subTest(bad=bad), redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                overlay.parse_args(bad)

        args = overlay.parse_args(["img.png", "-n", "10", "-w", "640", "-m", "DP-1", "-s", "0.8"])
        self.assertEqual(args.max_frames, 10)
        self.assertEqual(args.base_width, 640)
        self.assertEqual(args.monitor, "DP-1")
        self.assertEqual(args.scale, 0.8)

    def test_config_atomic_write_and_validation(self):
        with tempfile.TemporaryDirectory() as tmp:
            cfg = os.path.join(tmp, "nested", "apng-overlay.json")
            orig_cfg = config.CONFIG_PATH
            config.CONFIG_PATH = cfg
            try:
                config.atomic_write_json(
                    cfg,
                    {"/a.png": {"x": 250, "y": "bad", "scale": True}},
                )
                state = config.load_saved_state("/a.png")
                self.assertEqual(state, {"x": 250, "y": 100, "scale": 0.6})

                with open(cfg, "w") as f:
                    f.write("{corrupt")
                self.assertEqual(config.load_saved_state("/a.png"), {"x": 100, "y": 100, "scale": 0.6})
            finally:
                config.CONFIG_PATH = orig_cfg

    def test_non_unique_application_flag(self):
        app = window.OverlayApp(overlay.parse_args(["img.png"]))
        self.assertTrue(bool(app.app.get_flags() & Gio.ApplicationFlags.NON_UNIQUE))

    def test_resolve_monitor(self):
        mon0 = MagicMock()
        mon0.get_connector.return_value = "eDP-1"
        mon1 = MagicMock()
        mon1.get_connector.return_value = "DP-1"

        monitors = MagicMock()
        monitors.get_n_items.return_value = 2
        monitors.get_item.side_effect = lambda i: [mon0, mon1][i]

        display = MagicMock()
        display.get_monitors.return_value = monitors

        self.assertEqual(window.resolve_monitor(display, "0"), (mon0, None))
        self.assertEqual(window.resolve_monitor(display, "DP-1"), (mon1, None))

        mon, err = window.resolve_monitor(display, "5")
        self.assertIsNone(mon)
        self.assertIn("out of range", err)

        mon, err = window.resolve_monitor(display, "HDMI-1")
        self.assertIsNone(mon)
        self.assertIn("not found", err)

        monitors.get_n_items.return_value = 0
        mon, err = window.resolve_monitor(display, "0")
        self.assertIsNone(mon)
        self.assertEqual(err, "no monitors found")

    def test_clamp_state(self):
        app = window.OverlayApp(overlay.parse_args(["img.png"]))
        app.store.fw, app.store.fh = 500, 500
        app.state = {"x": 5000, "y": -20, "scale": 10.0}

        geo = MagicMock(width=1000, height=800)
        app.monitor = MagicMock()
        app.monitor.get_geometry.return_value = geo

        app.clamp_state()
        self.assertAlmostEqual(app.state["scale"], 800 / 500)
        w, h = app.overlay_size()
        self.assertEqual(app.state["x"], 1000 - w)
        self.assertEqual(app.state["y"], 0)

        app.state["scale"] = 0.001
        app.clamp_state()
        self.assertGreaterEqual(min(app.overlay_size()), window.MIN_SIZE)

    def test_corrupt_cache_falls_back_to_source_decode(self):
        with tempfile.TemporaryDirectory() as tmp:
            img_path = os.path.join(tmp, "sample.png")
            Image.new("RGBA", (120, 80), (10, 20, 30, 255)).save(img_path)

            orig_cache = cache.CACHE_DIR
            cache.CACHE_DIR = os.path.join(tmp, "cache")
            try:
                raw_path, meta_path = cache.cache_paths(img_path, 500, 0)
                with open(meta_path, "w") as f:
                    f.write('{"w": 120, "h": 80, "delays": [33]}')  # missing sizes
                with open(raw_path, "wb") as f:
                    f.write(b"not-zlib")

                store = cache.FrameStore(img_path, 500, 0)
                store.prepare_first_frame()
                self.assertEqual((store.fw, store.fh), (120, 80))
                self.assertEqual(len(store.compressed_frames), 1)
                self.assertTrue(store.decode_done)
            finally:
                cache.CACHE_DIR = orig_cache

    def test_on_tick_timing_and_suspend_recovery(self):
        app = window.OverlayApp(overlay.parse_args(["img.png"]))
        app.store.fw, app.store.fh = 10, 10
        app.pic = MagicMock()

        # Single-frame done removes the tick callback.
        app.store.compressed_frames = [b"f0"]
        app.store.delays = [33]
        app.store.decode_done = True
        clock = MagicMock()
        self.assertEqual(app._on_tick(None, clock), GLib.SOURCE_REMOVE)

        # Streaming holds on latest decoded frame until decode finishes.
        app.store.compressed_frames = [b"f0", b"f1"]
        app.store.delays = [33, 33]
        app.store.decode_done = False
        app.store.texture_cache[0] = MagicMock()
        app.store.texture_cache[1] = MagicMock()

        clock.get_frame_time.return_value = 100_000  # 100 ms
        self.assertEqual(app._on_tick(None, clock), GLib.SOURCE_CONTINUE)
        self.assertEqual(app.anim_index, 0)

        clock.get_frame_time.return_value = 140_000  # 140 ms -> frame 1
        app._on_tick(None, clock)
        self.assertEqual(app.anim_index, 1)

        clock.get_frame_time.return_value = 180_000  # still streaming, hold at 1
        app._on_tick(None, clock)
        self.assertEqual(app.anim_index, 1)

        # Once decode finishes, wraps back to 0 and recovers from big clock jump.
        app.store.decode_done = True
        clock.get_frame_time.return_value = 5_000_000  # +5s stall
        app._on_tick(None, clock)
        self.assertEqual(app.anim_index, 0)
        self.assertEqual(app.anim_next, 5033.0)

    def test_quit_cleans_up_temp_cache_on_abort(self):
        with tempfile.TemporaryDirectory() as tmp:
            gif_path = os.path.join(tmp, "anim.gif")
            frames = [Image.new("RGBA", (64, 64), (i, 0, 0, 255)) for i in range(10)]
            frames[0].save(gif_path, save_all=True, append_images=frames[1:], duration=33, loop=0)

            orig_cache = cache.CACHE_DIR
            cache_dir = os.path.join(tmp, "cache")
            cache.CACHE_DIR = cache_dir
            try:
                app = window.OverlayApp(overlay.parse_args([gif_path]))
                app.store.prepare_first_frame()
                self.assertIsNotNone(app.store.worker_thread)
                app.quit()
                self.assertFalse(app.store.worker_thread.is_alive())
                leftovers = [name for name in os.listdir(cache_dir) if name.startswith(".tmp-")]
                self.assertEqual(leftovers, [])
            finally:
                cache.CACHE_DIR = orig_cache

    def test_prune_cache_and_hit_refresh(self):
        with tempfile.TemporaryDirectory() as tmp:
            img_path = os.path.join(tmp, "sample.png")
            Image.new("RGBA", (80, 60), (5, 10, 15, 255)).save(img_path)

            orig_cache = cache.CACHE_DIR
            cache_dir = os.path.join(tmp, "cache")
            cache.CACHE_DIR = cache_dir
            try:
                s1 = cache.FrameStore(img_path, 500, 0)
                s1.prepare_first_frame()
                raw_path, meta_path = cache.cache_paths(img_path, 500, 0)

                stale_json = os.path.join(cache_dir, "old123.json")
                stale_raw = os.path.join(cache_dir, "old123.raw")
                stale_zraw = os.path.join(cache_dir, "old123.zraw")
                keep_note = os.path.join(cache_dir, "notes.txt")
                for p in (stale_json, stale_raw, stale_zraw, keep_note):
                    with open(p, "wb") as f:
                        f.write(b"x")

                old_ts = time.time() - 31 * 86400
                for p in (raw_path, meta_path, stale_json, stale_raw, stale_zraw, keep_note):
                    os.utime(p, (old_ts, old_ts))

                # Cache hit refreshes active files before pruning removes stale ones.
                s2 = cache.FrameStore(img_path, 500, 0)
                s2.prepare_first_frame()

                self.assertTrue(os.path.exists(meta_path))
                self.assertTrue(os.path.exists(raw_path))
                self.assertTrue(os.path.exists(keep_note))
                self.assertFalse(os.path.exists(stale_json))
                self.assertFalse(os.path.exists(stale_raw))
                self.assertFalse(os.path.exists(stale_zraw))
            finally:
                cache.CACHE_DIR = orig_cache

    def test_texture_cache_cap_by_loop_length(self):
        frame = zlib.compress(b"\x00\x00\x00\xff" * 4, level=1)

        short_store = cache.FrameStore("img.png", 500, 0)
        short_store.fw, short_store.fh = 2, 2
        short_store.compressed_frames = [frame] * 10
        for i in range(10):
            short_store.get_texture(i)
        self.assertEqual(len(short_store.texture_cache), 10)

        long_store = cache.FrameStore("img.png", 500, 0)
        long_store.fw, long_store.fh = 2, 2
        long_store.compressed_frames = [frame] * (cache.TEXTURE_CACHE_LIMIT + 5)
        for i in range(len(long_store.compressed_frames)):
            long_store.get_texture(i)
        self.assertEqual(len(long_store.texture_cache), 2)

    def test_atomic_write_bytes_cleans_temp_on_error(self):
        """Verify failed atomic_write_bytes removes temp files without creating target."""
        with tempfile.TemporaryDirectory() as tmp:
            target = os.path.join(tmp, "broken.zraw")
            with self.assertRaises(TypeError):
                config.atomic_write_bytes(target, object())
            self.assertFalse(os.path.exists(target))
            self.assertEqual(os.listdir(tmp), [])

    def test_save_state_entry_serializes_concurrent_writes(self):
        """Verify concurrent save_state_entry calls preserve every key via flock."""
        with tempfile.TemporaryDirectory() as tmp:
            cfg = os.path.join(tmp, "nested", "apng-overlay.json")
            orig_cfg = config.CONFIG_PATH
            orig_atomic_json = config.atomic_write_json
            config.CONFIG_PATH = cfg

            def slow_atomic_write_json(path, data):
                time.sleep(0.01)
                orig_atomic_json(path, data)

            config.atomic_write_json = slow_atomic_write_json
            try:
                keys = [f"/img_{i}.png" for i in range(6)]

                def write_key(idx):
                    config.save_state_entry(
                        keys[idx],
                        {"x": idx * 10, "y": idx * 20, "scale": 1.0},
                    )

                with ThreadPoolExecutor(max_workers=6) as pool:
                    list(pool.map(write_key, range(6)))

                for idx, key in enumerate(keys):
                    self.assertEqual(
                        config.load_saved_state(key),
                        {"x": idx * 10, "y": idx * 20, "scale": 1.0},
                    )
            finally:
                config.atomic_write_json = orig_atomic_json
                config.CONFIG_PATH = orig_cfg

    def test_single_frame_cache_commits_raw_before_meta(self):
        """Verify single-frame caching commits .zraw before .json and leaves no temps on failure."""
        with tempfile.TemporaryDirectory() as tmp:
            img_path = os.path.join(tmp, "single.png")
            Image.new("RGBA", (64, 48), (12, 34, 56, 255)).save(img_path)

            orig_cache = cache.CACHE_DIR
            orig_write_meta = cache._write_cache_meta
            cache_dir = os.path.join(tmp, "cache")
            cache.CACHE_DIR = cache_dir
            events = []

            def failing_meta(*_args, **_kwargs):
                events.append(("meta_attempt", os.path.exists(raw_path)))
                raise OSError("simulated metadata failure")

            try:
                raw_path, meta_path = cache.cache_paths(img_path, 500, 0)
                cache._write_cache_meta = failing_meta
                with self.assertRaises(OSError):
                    cache.FrameStore(img_path, 500, 0).prepare_first_frame()

                self.assertEqual(events, [("meta_attempt", True)])
                self.assertFalse(os.path.exists(meta_path))
                self.assertEqual(
                    [n for n in os.listdir(cache_dir) if n.startswith(".tmp-")],
                    [],
                )

                cache._write_cache_meta = orig_write_meta
                store = cache.FrameStore(img_path, 500, 0)
                store.prepare_first_frame()
                self.assertTrue(os.path.exists(raw_path))
                self.assertTrue(os.path.exists(meta_path))
            finally:
                cache._write_cache_meta = orig_write_meta
                cache.CACHE_DIR = orig_cache


if __name__ == "__main__":
    unittest.main()
