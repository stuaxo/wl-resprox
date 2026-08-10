"""Shared plumbing for the small Qt6/PySide6 test clients in this
directory. Mirrors scripts/gtk/common.py's own design and reasoning --
see that module's doc comment for the full "why these exist" background
(2026-08-03: real apps' rendering-path behavior is a black box, adding
logging means patching something we don't own).

Added 2026-08-10, specifically because that same day's investigation
found gtk4-demo -- not our proxy -- was the actual cause of a dmabuf
crash-recovery failure against kwin/sway (see docs/KNOWN_BUGS.md-adjacent
notes and the 2026-08-10 dmabuf-recreation investigation): a real GTK4
app's own opaque buffer-allocation choices produced a false failure
signal our own controlled GTK test client didn't reproduce. Qt is a
second, genuinely different toolkit -- qtwayland's own dmabuf/EGL
integration is a different code path than GTK's Wayland backend, not
just a relabeling of the same one -- so a Qt-specific protocol-usage
quirk has a real chance of existing that no amount of GTK-only coverage
would ever catch. These clients are that coverage's Qt half.

One real difference from the GTK clients, worth being honest about
rather than papering over: GTK4's own frame clock is explicitly wired to
wl_surface.frame() (add_tick_callback fires from a real frame callback
done event) with no supported way to drive it from a plain timer instead
-- see scripts/gtk/common.py's own comment on this. Qt's QWidget/
QOpenGLWidget redraw scheduling is not observably wired to a Wayland
frame callback the same way at the PySide6 level (a plain QTimer-driven
update() does not itself block on wl_callback.done); the stall detector
below is therefore a weaker signal here -- "the app's own timer stopped
firing" rather than "a specific Wayland promise was never answered".
Genuinely useful for that difference alone (a live GTK/Qt divergence
already known to matter), but don't read a clean Qt stall-free run as
proof a frame() callback was actually answered the way it would for the
GTK clients.
"""

import logging
import os
import sys
import time

# WL_TEST_LOG_LEVEL: same knob as scripts/gtk/common.py's own, same
# reasoning -- see that module's doc comment for why it exists.
_level_name = os.environ.get("WL_TEST_LOG_LEVEL", "INFO").upper()
_level = getattr(logging, _level_name, None)
if not isinstance(_level, int):
    _level = logging.INFO

logging.basicConfig(
    level=_level,
    format="%(asctime)s.%(msecs)03d [%(name)s/%(levelname)s] %(message)s",
    datefmt="%H:%M:%S",
    stream=sys.stdout,
)
log = logging.getLogger("wl-res-test-client")


def log_startup_environment():
    """Self-reports the facts that would otherwise need strace to find --
    see scripts/gtk/common.py's own doc comment for why that matters."""
    log.info(
        "pid=%d QT_QPA_PLATFORM=%s WAYLAND_DISPLAY=%s XDG_SESSION_TYPE=%s",
        os.getpid(),
        os.environ.get("QT_QPA_PLATFORM", "<unset -- Qt picks its own default>"),
        os.environ.get("WAYLAND_DISPLAY", "<unset>"),
        os.environ.get("XDG_SESSION_TYPE", "<unset>"),
    )


class StallTracker:
    """Shared stall-detection logic, factored out of the GTK version's
    TestWindow so both the QWidget (raster) and QOpenGLWidget (GL)
    windows below can use identical thresholds/behavior without
    duplicating the timer bookkeeping. See this module's own doc comment
    for the caveat on what "stall" actually means here vs. the GTK
    clients' own frame-callback-tied version.
    """

    STALL_THRESHOLD_SECONDS = 5
    STALL_REMINDER_SECONDS = 5
    STALL_QUIT_SECONDS = 30

    def __init__(self, on_give_up):
        self._on_give_up = on_give_up
        self._last_tick_time = time.monotonic()
        self._stalled = False
        self._last_stall_reminder_time = None

    def tick(self):
        now = time.monotonic()
        if self._stalled:
            log.warning("RECOVERED: redraw resumed after %.1fs stall", now - self._last_tick_time)
            self._stalled = False
        self._last_tick_time = now

    def check_stall(self):
        since_last_tick = time.monotonic() - self._last_tick_time
        if since_last_tick < self.STALL_THRESHOLD_SECONDS:
            return
        if not self._stalled:
            self._stalled = True
            self._last_stall_reminder_time = time.monotonic()
            log.warning(
                "STALLED: no redraw for %.1fs (expected roughly every ~33ms) -- "
                "render loop appears stuck",
                since_last_tick,
            )
        elif since_last_tick >= self.STALL_QUIT_SECONDS:
            log.warning("GIVING UP: stalled for %.1fs, quitting", since_last_tick)
            self._on_give_up()
        elif time.monotonic() - self._last_stall_reminder_time >= self.STALL_REMINDER_SECONDS:
            self._last_stall_reminder_time = time.monotonic()
            log.warning("STILL STALLED: no redraw for %.1fs", since_last_tick)
