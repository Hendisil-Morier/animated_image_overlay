import ctypes
import sys


def preload_layer_shell():
    """Load gtk4-layer-shell before GTK resolves Wayland symbols."""
    for lib in ("libgtk4-layer-shell.so", "libgtk4-layer-shell.so.0"):
        try:
            ctypes.CDLL(lib)
            return
        except OSError:
            continue
    sys.exit(
        "error: could not load libgtk4-layer-shell.so or libgtk4-layer-shell.so.0"
    )


preload_layer_shell()

import os
import signal
import traceback
import cairo
import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
gi.require_version("Gtk4LayerShell", "1.0")
from gi.repository import Gdk, Gio, GLib, Gtk
from gi.repository import Gtk4LayerShell as LayerShell

try:
    gi.require_version("GLibUnix", "2.0")
    from gi.repository import GLibUnix
    signal_add = GLibUnix.signal_add
except (ImportError, ValueError):
    signal_add = GLib.unix_signal_add

from cache import FrameStore
from config import load_saved_state, log, save_state_entry

HANDLE = 28              # size of the corner handle in pixels
MIN_SIZE = 3 * HANDLE    # smallest allowed overlay size on its short side
SAVE_DEBOUNCE_MS = 500
HANDLE_FADE_MS = 2000

CSS = """
window, window.background, fixed, picture, overlay {
    background: none;
    background-color: transparent;
    box-shadow: none;
}
.handle {
    background: rgba(255, 255, 255, 0.25);
    border-radius: 8px;
    opacity: 1.0;
    transition: opacity 250ms ease;
}
.handle:hover {
    background: rgba(255, 255, 255, 0.85);
}
.handle.idle {
    opacity: 0.0;
}
overlay.editing {
    outline: 1px dashed rgba(255, 255, 255, 0.45);
}
"""


def install_css():
    """Register overlay transparency and handle styles on the default display."""
    css = Gtk.CssProvider()
    css.connect("parsing-error", lambda _p, _sec, err: log(f"CSS parse error: {err.message}"))
    if hasattr(css, "load_from_string"):
        css.load_from_string(CSS)
    else:
        css.load_from_data(CSS.encode())
    Gtk.StyleContext.add_provider_for_display(
        Gdk.Display.get_default(), css, Gtk.STYLE_PROVIDER_PRIORITY_USER
    )


def _format_monitors(monitors, count):
    items = []
    for i in range(count):
        mon = monitors.get_item(i)
        name = (mon.get_connector() if mon is not None else None) or "unknown"
        items.append(f"{i}:{name}")
    return ", ".join(items)


def resolve_monitor(display, spec):
    """Find a Gdk.Monitor by zero-based index or connector name."""
    monitors = display.get_monitors()
    count = monitors.get_n_items()
    if count == 0:
        return None, "no monitors found"

    try:
        idx = int(spec)
    except ValueError:
        for i in range(count):
            mon = monitors.get_item(i)
            if mon is not None and mon.get_connector() == spec:
                return mon, None
        return None, f"monitor {spec!r} not found (available: {_format_monitors(monitors, count)})"

    if 0 <= idx < count:
        mon = monitors.get_item(idx)
        if mon is not None:
            return mon, None
    return None, f"monitor index {idx} out of range (available: {_format_monitors(monitors, count)})"


class OverlayApp:
    """Floating Wayland layer-shell overlay for animated images."""

    def __init__(self, args):
        self.args = args
        self.key = os.path.abspath(args.file)
        self.state = load_saved_state(self.key)
        if args.scale is not None:
            self.state["scale"] = args.scale

        self.store = FrameStore(args.file, args.base_width, args.max_frames)
        self.exit_code = 0
        self.monitor = None
        self.win = None
        self.pic = None
        self.handle = None
        self.overlay = None
        self.fixed = None

        self.last_size = (0, 0)
        self.anim_index = 0
        self.anim_next = None
        self.editing = False
        self.dragging = False
        self.hovered = False

        self.save_timer_id = 0
        self.fade_timer_id = 0

        self.app = Gtk.Application(
            application_id="local.apng.overlay",
            flags=Gio.ApplicationFlags.NON_UNIQUE,
        )
        self.app.connect("activate", self.on_activate)

    def run(self):
        """Hook Unix signals and enter the GTK application main loop."""
        log(f"GTK {Gtk.get_major_version()}.{Gtk.get_minor_version()}.{Gtk.get_micro_version()}")
        log(f"layer shell supported: {LayerShell.is_supported()}")

        signal_add(GLib.PRIORITY_DEFAULT, signal.SIGINT, self.quit)
        signal_add(GLib.PRIORITY_DEFAULT, signal.SIGTERM, self.quit)
        signal_add(GLib.PRIORITY_DEFAULT, signal.SIGUSR1, self.toggle_edit_mode)

        self.app.run(None)
        return self.exit_code

    def overlay_size(self):
        return int(self.store.fw * self.state["scale"]), int(self.store.fh * self.state["scale"])

    def clamp_state(self):
        """Keep scale and position inside sane limits for this image and screen."""
        geo = self.monitor.get_geometry()
        sw, sh = max(1, geo.width), max(1, geo.height)
        max_scale = min(sw / self.store.fw, sh / self.store.fh)
        min_scale = min(max_scale, MIN_SIZE / min(self.store.fw, self.store.fh))
        self.state["scale"] = min(max_scale, max(min_scale, self.state["scale"]))
        w, h = self.overlay_size()
        self.state["x"] = min(max(0, self.state["x"]), max(0, sw - w))
        self.state["y"] = min(max(0, self.state["y"]), max(0, sh - h))

    def update_region(self, w, h):
        surface = self.win.get_surface()
        if surface is None:
            return
        x, y = int(self.state["x"]), int(self.state["y"])
        if self.editing:
            rect = cairo.RectangleInt(x, y, w, h)
        else:
            rect = cairo.RectangleInt(x + w - HANDLE, y + h - HANDLE, HANDLE, HANDLE)
        surface.set_input_region(cairo.Region(rect))

    def apply_layout(self):
        w, h = self.overlay_size()
        if (w, h) != self.last_size:
            self.pic.set_size_request(w, h)
            self.last_size = (w, h)
        self.fixed.move(self.overlay, int(self.state["x"]), int(self.state["y"]))
        self.update_region(w, h)

    def log_layout(self):
        w, h = self.overlay_size()
        log(f"layout {w}x{h} at ({int(self.state['x'])},{int(self.state['y'])})")

    def schedule_save(self):
        if self.save_timer_id:
            GLib.source_remove(self.save_timer_id)
        self.save_timer_id = GLib.timeout_add(SAVE_DEBOUNCE_MS, self._flush_save)

    def _flush_save(self):
        self.save_timer_id = 0
        save_state_entry(self.key, self.state)
        self.log_layout()
        return GLib.SOURCE_REMOVE

    def _show_handle(self):
        if self.handle is None:
            return
        self.handle.remove_css_class("idle")
        if self.fade_timer_id:
            GLib.source_remove(self.fade_timer_id)
            self.fade_timer_id = 0
        if not self.editing and not self.dragging and not self.hovered:
            self.fade_timer_id = GLib.timeout_add(HANDLE_FADE_MS, self._fade_handle)

    def _fade_handle(self):
        self.fade_timer_id = 0
        if not self.editing and not self.dragging and not self.hovered and self.handle is not None:
            self.handle.add_css_class("idle")
        return GLib.SOURCE_REMOVE

    def toggle_edit_mode(self):
        """Toggle full-image drag/resize input versus corner-handle-only input."""
        self.editing = not self.editing
        if self.overlay is not None:
            if self.editing:
                self.overlay.add_css_class("editing")
            else:
                self.overlay.remove_css_class("editing")
            self._show_handle()
            self.update_region(*self.overlay_size())
        log(f"edit mode: {self.editing}")
        return GLib.SOURCE_CONTINUE

    def _on_tick(self, _widget, clock):
        frames = self.store.compressed_frames
        delays = self.store.delays
        if len(frames) <= 1:
            if self.store.decode_done:
                return GLib.SOURCE_REMOVE
            return GLib.SOURCE_CONTINUE

        now = clock.get_frame_time() / 1000.0  # ms
        if self.anim_next is None:
            self.anim_next = now + delays[0]

        if now - self.anim_next > 1000.0:
            self.anim_next = now

        prev_index = self.anim_index
        while now >= self.anim_next:
            next_index = self.anim_index + 1
            if next_index >= len(frames):
                if not self.store.decode_done:
                    self.anim_next = now + delays[self.anim_index]
                    break
                next_index = 0
            self.anim_index = next_index
            self.anim_next += delays[self.anim_index]

        if self.anim_index != prev_index:
            self.pic.set_paintable(self.store.get_texture(self.anim_index))
        return GLib.SOURCE_CONTINUE

    def _bind_inputs(self):
        # Gestures sit on the fixed surface, whose coordinates never move,
        # so the offsets they report follow the pointer exactly.
        start_x = 0.0
        start_y = 0.0
        start_scale = 1.0

        def refresh():
            self.clamp_state()
            self.apply_layout()
            self._show_handle()

        def attach_drag(button, on_begin, on_update):
            gesture = Gtk.GestureDrag()
            gesture.set_button(button)

            def begin(g, x, y):
                self.dragging = True
                self._show_handle()
                on_begin(g, x, y)

            def end(_g, _dx, _dy):
                self.dragging = False
                self._show_handle()
                self.schedule_save()

            gesture.connect("drag-begin", begin)
            gesture.connect("drag-update", on_update)
            gesture.connect("drag-end", end)
            self.fixed.add_controller(gesture)

        def on_move_begin(_gesture, _x, _y):
            nonlocal start_x, start_y
            start_x, start_y = self.state["x"], self.state["y"]

        def on_move(_gesture, dx, dy):
            self.state["x"] = start_x + dx
            self.state["y"] = start_y + dy
            refresh()

        def on_resize_begin(_gesture, _x, _y):
            nonlocal start_scale
            start_scale = self.state["scale"]

        # Right button resizes, keeping top-left fixed and growing toward bottom-right.
        def on_resize(_gesture, dx, _dy):
            w0 = self.store.fw * start_scale
            self.state["scale"] = start_scale * (w0 + dx) / w0
            refresh()

        attach_drag(1, on_move_begin, on_move)
        attach_drag(3, on_resize_begin, on_resize)

        scroll = Gtk.EventControllerScroll.new(Gtk.EventControllerScrollFlags.VERTICAL)

        def on_scroll(_controller, _dx, dy):
            self.state["scale"] *= 1.05 ** (-dy)
            refresh()
            self.schedule_save()
            return True

        scroll.connect("scroll", on_scroll)
        self.fixed.add_controller(scroll)

        # Middle-clicking the handle quits cleanly.
        middle_click = Gtk.GestureClick()
        middle_click.set_button(2)
        middle_click.connect("pressed", lambda *_args: self.quit())
        self.handle.add_controller(middle_click)

        motion = Gtk.EventControllerMotion()

        def on_enter(_ctrl, _x, _y):
            self.hovered = True
            self._show_handle()

        def on_leave(_ctrl):
            self.hovered = False
            self._show_handle()

        motion.connect("enter", on_enter)
        motion.connect("motion", lambda *_args: self._show_handle())
        motion.connect("leave", on_leave)
        self.fixed.add_controller(motion)

    def on_activate(self, app):
        try:
            if not LayerShell.is_supported():
                print(
                    "error: gtk4-layer-shell is not supported on this display server "
                    "(requires a wlroots-compatible Wayland compositor, not X11 or GNOME Shell).",
                    file=sys.stderr,
                )
                self.exit_code = 1
                self.quit()
                return

            display = Gdk.Display.get_default()
            monitor, err = resolve_monitor(display, self.args.monitor)
            if monitor is None:
                print(f"error: {err}", file=sys.stderr)
                self.exit_code = 1
                self.quit()
                return
            self.monitor = monitor

            self.store.prepare_first_frame()
            install_css()

            win = Gtk.ApplicationWindow(application=app)
            LayerShell.init_for_window(win)
            LayerShell.set_layer(win, LayerShell.Layer.OVERLAY)
            for edge in (
                LayerShell.Edge.TOP,
                LayerShell.Edge.BOTTOM,
                LayerShell.Edge.LEFT,
                LayerShell.Edge.RIGHT,
            ):
                LayerShell.set_anchor(win, edge, True)  # cover the whole screen
            LayerShell.set_exclusive_zone(win, -1)      # ignore bars and panels
            LayerShell.set_monitor(win, self.monitor)
            log(f"is_layer_window: {LayerShell.is_layer_window(win)}")

            pic = Gtk.Picture.new_for_paintable(self.store.get_texture(0))
            pic.set_can_shrink(True)
            pic.set_content_fit(Gtk.ContentFit.FILL)

            handle = Gtk.Box()
            handle.add_css_class("handle")
            handle.set_halign(Gtk.Align.END)
            handle.set_valign(Gtk.Align.END)
            handle.set_size_request(HANDLE, HANDLE)

            overlay = Gtk.Overlay()
            overlay.set_child(pic)
            overlay.add_overlay(handle)
            if self.editing:
                overlay.add_css_class("editing")

            fixed = Gtk.Fixed()
            fixed.put(overlay, self.state["x"], self.state["y"])
            win.set_child(fixed)

            self.win = win
            self.pic = pic
            self.handle = handle
            self.overlay = overlay
            self.fixed = fixed

            self._bind_inputs()
            win.connect("map", lambda *_: GLib.idle_add(self.apply_layout))

            self.clamp_state()
            self.apply_layout()
            self.log_layout()
            win.present()
            self._show_handle()

            if not (self.store.decode_done and len(self.store.compressed_frames) <= 1):
                pic.add_tick_callback(self._on_tick)

            log("activate: done")
        except Exception:
            traceback.print_exc()
            self.exit_code = 1
            self.quit()

    def quit(self):
        """Stop background workers, flush pending config writes, and exit GTK."""
        self.store.stop()
        if self.save_timer_id:
            GLib.source_remove(self.save_timer_id)
            self._flush_save()
        if self.fade_timer_id:
            GLib.source_remove(self.fade_timer_id)
            self.fade_timer_id = 0
        self.app.quit()
        return GLib.SOURCE_REMOVE
