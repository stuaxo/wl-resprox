"""Shared plumbing for the small GTK4 test clients in this directory.

Why these exist (2026-08-03, see plan-desktop-resilience.md): gtk4-demo and
tilix work for crash testing, but neither is ours -- adding logging means
patching a real app's source, and neither one's rendering path is
something we control (gtk4-demo silently fell back to X11 once tonight,
and separately turned out to be dmabuf/GL-backed by default, both facts
only discovered after the fact via strace). These scripts are deliberately
minimal and self-reporting instead: each one logs, at startup, exactly
which GDK backend and which GSK renderer it ended up with, so there's
never any ambiguity about what's actually being exercised, and each one
logs every frame tick so a crash's exact timing relative to a client's own
render loop is visible without needing external tracing tools.

GSK_RENDERER must be set (in the environment) BEFORE gi/Gtk is imported --
GTK reads it once, lazily, the first time a renderer is actually needed,
but there's no supported way to change it after Gtk has started up, so
the two entry-point scripts set it as literally their first line.
"""

import logging
import os
import sys
import time

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
from gi.repository import Gdk, GLib, Gtk  # noqa: E402

logging.basicConfig(
    level=logging.INFO,
    # %(name)s -- the label run() is given -- so two of these running in
    # the same terminal (the common way to compare SHM vs. dmabuf
    # side by side) can still be told apart line by line.
    format="%(asctime)s.%(msecs)03d [%(name)s/%(levelname)s] %(message)s",
    datefmt="%H:%M:%S",
    stream=sys.stdout,
)
log = logging.getLogger("wl-res-test-client")


def log_startup_environment():
    """Self-reports the facts that would otherwise need strace to find --
    see this module's own doc comment for why that matters."""
    log.info("pid=%d GSK_RENDERER=%s WAYLAND_DISPLAY=%s XDG_SESSION_TYPE=%s",
              os.getpid(),
              os.environ.get("GSK_RENDERER", "<unset -- GTK picks its own default>"),
              os.environ.get("WAYLAND_DISPLAY", "<unset>"),
              os.environ.get("XDG_SESSION_TYPE", "<unset>"))


class TestWindow(Gtk.ApplicationWindow):
    """A window with a continuously-animating DrawingArea -- guarantees a
    steady stream of real attach/damage/frame/commit traffic (the same
    request shape that tripped the frame-callback stall found live
    tonight), not just whatever a demo app happens to be idling on when
    the crash test fires. Logs every Nth frame tick (not every single
    one -- at a real frame rate that's dozens of lines a second, drowning
    out the events that actually matter) plus renderer/backend identity
    the moment the window is realized (the earliest point a real GSK
    renderer -- and therefore a real answer to "GL or cairo, dmabuf or
    shm" -- actually exists).
    """

    LOG_EVERY_N_FRAMES = 30  # roughly once a second at the ~30fps timer below
    STALL_THRESHOLD_SECONDS = 5  # found live 2026-08-03: silence alone is ambiguous -- say so instead
    STALL_REMINDER_SECONDS = 5  # re-log while still stalled, so the log itself doesn't look dead
    STALL_QUIT_SECONDS = 30  # give up and exit rather than sit stalled indefinitely

    def __init__(self, app, title):
        super().__init__(application=app, title=title, default_width=320, default_height=240)
        self._frame_count = 0
        self._start_time = time.monotonic()
        self._last_tick_time = self._start_time
        self._stalled = False
        self._last_stall_reminder_time = None

        self.drawing_area = Gtk.DrawingArea()
        self.drawing_area.set_draw_func(self._on_draw)
        self.set_child(self.drawing_area)

        self.connect("realize", self._on_realize)
        self.connect("map", lambda *_: log.info("window mapped"))
        self.connect("unmap", lambda *_: log.info("window unmapped"))
        self.connect("close-request", self._on_close_request)

        # Quitting a recovered-after-crash window by clicking its own
        # titlebar close button is exactly the interaction most worth
        # doubting after a crash-recovery test -- Escape is a reliable
        # fallback that doesn't depend on window-manager chrome working.
        key_controller = Gtk.EventControllerKey()
        key_controller.connect("key-pressed", self._on_key_pressed)
        self.add_controller(key_controller)

        # add_tick_callback alone, self-chained via queue_draw() from
        # inside the callback, was found live NOT to reliably keep the
        # frame clock in continuous-update mode (fired once, then never
        # again) -- a plain timer driving queue_draw() is simpler and
        # deterministic. The tick callback stays registered purely to
        # LOG each real frame boundary (the same wl_surface.frame
        # callback mechanism found live 2026-08-03 to matter -- see
        # plan-desktop-resilience.md), not to drive redraws itself.
        self.drawing_area.add_tick_callback(self._on_tick)
        GLib.timeout_add(33, self._periodic_redraw)  # ~30fps
        GLib.timeout_add(1000, self._check_stall)  # independent of the tick/redraw path on purpose -- see its own doc comment

    def _periodic_redraw(self):
        self.drawing_area.queue_draw()
        return GLib.SOURCE_CONTINUE

    def _check_stall(self):
        # Deliberately its own timer, not piggybacked on _on_tick or
        # _periodic_redraw -- if the frame clock itself has stopped
        # ticking (e.g. waiting forever on a wl_callback.done that will
        # never arrive, the exact stall found live 2026-08-03 before
        # this session's frame() fix landed), anything driven BY the tick
        # would also be stuck and could never report that. This one is
        # driven by GLib's own main-loop timer, independent of Wayland
        # frame callbacks entirely, so it keeps running (and can keep
        # logging) even while the render loop itself is completely dead --
        # the whole point of this check is to make silence itself an
        # observable, timestamped event instead of nothing at all.
        since_last_tick = time.monotonic() - self._last_tick_time
        if since_last_tick >= self.STALL_THRESHOLD_SECONDS:
            if not self._stalled:
                self._stalled = True
                self._last_stall_reminder_time = time.monotonic()
                log.warning(
                    "STALLED: no frame tick for %.1fs (expected roughly every ~33ms) -- "
                    "render loop appears stuck, possibly waiting on a wl_callback.done "
                    "that will never arrive",
                    since_last_tick,
                )
            elif since_last_tick >= self.STALL_QUIT_SECONDS:
                # Give up rather than sit stalled indefinitely -- nothing
                # about a dead render loop is going to fix itself, and a
                # human doesn't need to come back and Ctrl-C it by hand.
                log.warning("GIVING UP: stalled for %.1fs, quitting", since_last_tick)
                self.get_application().quit()
                return GLib.SOURCE_REMOVE
            elif time.monotonic() - self._last_stall_reminder_time >= self.STALL_REMINDER_SECONDS:
                self._last_stall_reminder_time = time.monotonic()
                log.warning("STILL STALLED: no frame tick for %.1fs", since_last_tick)
        return GLib.SOURCE_CONTINUE

    def _on_realize(self, *_):
        native = self.get_native()
        surface = native.get_surface() if native else None
        renderer = native.get_renderer() if native else None
        display = Gdk.Display.get_default()
        log.info(
            "realized: gdk_backend=%s renderer=%s surface=%s",
            type(display).__name__ if display else "<none>",
            type(renderer).__name__ if renderer else "<none>",
            type(surface).__name__ if surface else "<none>",
        )

    def _on_tick(self, widget, frame_clock):
        now = time.monotonic()
        if self._stalled:
            log.warning("RECOVERED: frame tick resumed after %.1fs stall", now - self._last_tick_time)
            self._stalled = False
        self._last_tick_time = now
        self._frame_count += 1
        if self._frame_count % self.LOG_EVERY_N_FRAMES == 0:
            elapsed = now - self._start_time
            log.info("frame tick #%d (t+%.1fs)", self._frame_count, elapsed)
        return GLib.SOURCE_CONTINUE

    def _on_draw(self, area, cr, width, height):
        # Simple animated content -- a circle sweeping left to right --
        # just enough to force a genuinely new buffer contents each
        # frame, not a static image the toolkit might short-circuit.
        t = time.monotonic() - self._start_time
        x = (width / 2) * (1 + 0.8 * (t % 4 - 2) / 2)
        cr.set_source_rgb(0.15, 0.15, 0.15)
        cr.paint()
        cr.set_source_rgb(0.9, 0.3, 0.2)
        cr.arc(x, height / 2, min(width, height) / 8, 0, 2 * 3.14159)
        cr.fill()

    def _on_close_request(self, *_):
        log.info("close requested, total frames rendered: %d", self._frame_count)
        return False

    def _on_key_pressed(self, controller, keyval, keycode, state):
        if keyval == Gdk.KEY_Escape:
            log.info("Escape pressed, closing")
            self.close()
            return True
        return False


def run(app_id, title, label):
    global log
    log = logging.getLogger(label)  # see the %(name)s format set up above
    log_startup_environment()

    def on_activate(app):
        win = TestWindow(app, title)
        win.present()

    app = Gtk.Application(application_id=app_id)
    app.connect("activate", on_activate)
    return app.run(sys.argv)
