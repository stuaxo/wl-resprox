"""Shared plumbing for the small SDL2 test clients in this directory.
Mirrors scripts/gtk/common.py's and scripts/qt/common.py's own design and
reasoning -- see scripts/gtk/common.py's doc comment for the full "why
these exist" background.

Added 2026-08-11, as a third independent toolkit alongside GTK4 and Qt6:
SDL2's own Wayland video driver (src/video/wayland/ upstream) is a
genuinely separate implementation from both GTK's and Qt's Wayland
backends -- not a relabeling of either -- so it's real additional
coverage for the same class of toolkit-specific protocol-usage quirk the
Qt clients already caught once (see scripts/qt/common.py's own doc
comment). SDL2, not SDL3: python3-sdl2 is a proper Ubuntu apt package
(matching the same clean-install pattern as python3-gi and
python3-pyside6.* already used here); the only Python SDL3 binding found
(pysdl3) is PyPI-only and pre-1.0. SDL3's own Wayland driver evolved from
SDL2's rather than being a ground-up rewrite, so SDL2 gives the same
"third independent implementation" value without the venv/pip complexity.

One real structural difference from the GTK/Qt clients, worth being
honest about: SDL2 has no callback-driven main loop the way GLib's or
Qt's own event loops do -- there's no signal/slot or tick-callback
mechanism to hook into, just SDL_PollEvent() drained by hand. run_loop()
below is the direct SDL equivalent of TestWindow's timer/tick-callback
setup in the other two clients, just as one plain loop instead of several
independent timers.

Two mutter-specific issues found live 2026-08-11 and root-caused, NEITHER
a proxy bug:

1. SDL_Init() itself used to fail ("video driver did not add any
   displays") against the container harness's headless mutter/gnome-
   shell, while GTK's and Qt's own clients ran fine against the same
   container -- confirmed via wayland-info that bare `--headless --no-x11`
   advertises ZERO wl_output globals when a real DRM render node is
   present (unlike labwc/sway/kwin's virtual backends, which each still
   create one fake output). GTK/Qt tolerate a zero-output compositor;
   SDL2's video driver hard-requires at least one enumerable display.
   Fixed at the source: scripts/compositor-launch.sh now passes gnome-
   shell's own `--virtual-monitor` flag, which adds a real wl_output --
   see that file's own comment.

2. With a real output finally present, window mapping started reaching a
   SECOND, previously-dormant crash: SDL2's Wayland backend uses libdecor
   for client-side decorations, and libdecor auto-selects its GTK-
   rendered plugin (`libdecor-0-plugin-1-gtk`) when available -- which
   segfaults this container's process trying to load a window-icon
   fallback through a broken sandboxed SVG loader (bwrap/glycin-svg).
   GTK/Qt clients never hit this because they use their own native
   decoration path, not libdecor's fallback. Confirmed via
   SDL_VIDEO_WAYLAND_ALLOW_LIBDECOR=0 (disables libdecor outright,
   these test clients have no need for real window chrome) resolving it
   cleanly -- set below, before SDL_Init(), same "explicit beats an
   opaque toolkit default" reasoning as GSK_RENDERER/QSurfaceFormat in
   the other two clients' own common.py.

Both fixes are additive/opt-out only -- neither changes behavior for any
already-passing labwc/kwin/sway/GTK/Qt combination.
"""

import ctypes
import logging
import os
import sys
import time

# Must be set before SDL_Init() -- see this module's own doc comment,
# point 2, for why. setdefault, not a bare assignment: lets anyone who
# actually wants to exercise the libdecor path deliberately still set
# SDL_VIDEO_WAYLAND_ALLOW_LIBDECOR=1 themselves before running.
os.environ.setdefault("SDL_VIDEO_WAYLAND_ALLOW_LIBDECOR", "0")

import sdl2  # noqa: E402

# WL_TEST_LOG_LEVEL: same knob as scripts/gtk/common.py's and
# scripts/qt/common.py's own, same reasoning -- see scripts/gtk/common.py's
# doc comment for why it exists.
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
        "pid=%d SDL_VIDEODRIVER=%s WAYLAND_DISPLAY=%s XDG_SESSION_TYPE=%s",
        os.getpid(),
        os.environ.get("SDL_VIDEODRIVER", "<unset -- SDL picks its own default>"),
        os.environ.get("WAYLAND_DISPLAY", "<unset>"),
        os.environ.get("XDG_SESSION_TYPE", "<unset>"),
    )


def log_video_driver():
    """Called once the window/context actually exists -- the earliest
    point SDL_GetCurrentVideoDriver() has a real answer, same "don't
    trust the toolkit's default silently" reasoning as the GTK/Qt
    clients' own realize-time logging (gtk4-demo silently fell back to
    X11 once, see scripts/gtk/common.py)."""
    driver = sdl2.SDL_GetCurrentVideoDriver()
    log.info("realized: sdl_video_driver=%s", driver.decode() if driver else "<none>")


class StallTracker:
    """Identical to scripts/qt/common.py's own -- same thresholds, same
    reasoning. SDL's redraw loop here is hand-driven (see run_loop below),
    so "stalled" means the same thing it does for the GTK clients: the
    loop itself is a plain wall-clock timer independent of any Wayland
    frame callback, so it keeps ticking (and can keep logging) even if
    something downstream is stuck waiting on a wl_callback.done that will
    never arrive.
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


LOG_EVERY_N_FRAMES = 30  # roughly once a second at the ~30fps redraw cadence below
REDRAW_INTERVAL_SECONDS = 33 / 1000


def run_loop(draw_fn):
    """Drives the shared poll/redraw/stall-check/quit loop for both
    clients in this directory. draw_fn(elapsed_seconds) is called at
    ~30fps and is expected to actually present the frame itself (the
    renderer/GL-swap call differs per client, see basic_shm.py and
    dmabuf_gl.py) -- kept out of here so this stays toolkit-content-free,
    just the event/timing plumbing both share.
    """
    state = {"quit": False, "frame_count": 0}
    stall = StallTracker(on_give_up=lambda: state.__setitem__("quit", True))
    start_time = time.monotonic()
    last_redraw = 0.0
    last_stall_check = 0.0

    event = sdl2.SDL_Event()
    while not state["quit"]:
        while sdl2.SDL_PollEvent(ctypes.byref(event)) != 0:
            if event.type == sdl2.SDL_QUIT:
                log.info("quit requested")
                state["quit"] = True
            elif event.type == sdl2.SDL_KEYDOWN and event.key.keysym.sym == sdl2.SDLK_ESCAPE:
                log.info("Escape pressed, closing")
                state["quit"] = True
            elif event.type == sdl2.SDL_WINDOWEVENT:
                if event.window.event == sdl2.SDL_WINDOWEVENT_SHOWN:
                    log.info("window mapped")
                elif event.window.event == sdl2.SDL_WINDOWEVENT_HIDDEN:
                    log.info("window unmapped")

        now = time.monotonic()
        if now - last_redraw >= REDRAW_INTERVAL_SECONDS:
            last_redraw = now
            stall.tick()
            state["frame_count"] += 1
            if state["frame_count"] % LOG_EVERY_N_FRAMES == 0:
                log.info("frame tick #%d (t+%.1fs)", state["frame_count"], now - start_time)
            draw_fn(now - start_time)

        if now - last_stall_check >= 1.0:
            last_stall_check = now
            stall.check_stall()

        sdl2.SDL_Delay(5)

    log.info("close requested, total frames rendered: %d", state["frame_count"])
