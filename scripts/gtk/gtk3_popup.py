#!/usr/bin/env python3
"""Minimal GTK3 test client exercising a right-click-style context menu
(xdg_popup + grab) -- every other client in this directory is GTK4, and
no test here has ever created a popup surface at all: Recreatable (see
recreation.rs) has no popup variant, and nothing exercises
xdg_popup.grab's serial requirement across a reconnect.

Found live 2026-08-07: right-click stopped working on a real GTK3 app
(Tilix) on a real desktop after several crash-recovery cycles. Traced to
a Mutter-internal bug (meta_window_set_stack_position_no_sync), not
wl-resprox -- see docs/adr/ -- but that investigation turned up this as
a genuine, previously-untested gap regardless: nothing here had ever
opened a popup at all, through the proxy or otherwise.

No mouse in a headless container, so this doesn't wait for a real
right-click: focus-in (GNOME auto-focuses newly-mapped/recreated
toplevels, the same real, compositor-issued event
docs/adr/adr-0009-clipboard-persistence.md's probes relied on for a
legitimate grab serial) opens the menu automatically, using that
focus-in event's own serial for the grab -- exactly what a real
right-click's button-press event would provide instead. Fires once at
startup and again after any reconnect (a recreated toplevel gets its
own fresh focus-in), so a single run covers "does a popup grab work at
all" and "does it still work after the compositor restarts" with no
extra scripting.

Confirmed live 2026-08-07 against scripts/containers/mutter's own
`gnome-shell --headless --no-x11`: `wl_seat` advertises zero
capabilities there (no pointer, no keyboard -- no input device backs it
at all), so no focus-in ever fires and this script's own popup/grab
path is UNREACHABLE in that specific container. Not a bug in this
script -- there is no real input event of any kind to derive a grab
serial from in a headless compositor, full stop (see ADR-0009: a
fabricated serial is rejected outright). Confirmed via `wayland-info`
showing an empty `capabilities:` line for wl_seat. Still useful there
for what test-crash.sh already checks (does a GTK3 client survive a
crash/reconnect at all -- the first GTK3 coverage this project has had,
GTK4 was the only toolkit ever tested here before); the popup/grab
question itself needs either a real desktop session or a headless setup
with virtual-input support this project's containers don't have yet.

Run with: WAYLAND_DISPLAY=wayland-0 python3 scripts/gtk/gtk3_popup.py
"""
import logging
import os
import sys
import time

import gi

gi.require_version("Gtk", "3.0")
gi.require_version("Gdk", "3.0")
from gi.repository import Gdk, GLib, Gtk  # noqa: E402

_FORMAT = "%(asctime)s.%(msecs)03d [%(name)s/%(levelname)s] %(message)s"
_DATEFMT = "%H:%M:%S"
logging.basicConfig(level=logging.INFO, format=_FORMAT, datefmt=_DATEFMT, stream=sys.stdout)
log = logging.getLogger("gtk3-popup")

# Also to a fixed path, independent of stdout -- test-crash.sh redirects
# stdout to its own mktemp'd CLIENT_LOG and deletes it on a passing run
# (see its cleanup trap), which would otherwise lose exactly the
# menu-shown/hidden evidence this script exists to produce.
_file_handler = logging.FileHandler("/tmp/gtk3_popup_client.log", mode="w")
_file_handler.setFormatter(logging.Formatter(_FORMAT, datefmt=_DATEFMT))
logging.getLogger().addHandler(_file_handler)

MENU_AUTOCLOSE_MS = 1500
LOG_EVERY_N_FRAMES = 30


class PopupTestWindow(Gtk.Window):
    def __init__(self):
        super().__init__(title="wl-res test: gtk3 popup")
        self.set_default_size(320, 240)
        self._frame_count = 0
        self._start_time = time.monotonic()
        self._focus_count = 0

        self.drawing_area = Gtk.DrawingArea()
        self.drawing_area.connect("draw", self._on_draw)
        self.add(self.drawing_area)

        self.menu = Gtk.Menu()
        for label in ("Copy", "Paste", "Profile"):
            self.menu.append(Gtk.MenuItem(label=label))
        self.menu.show_all()
        self.menu.connect("show", lambda *_: log.info("menu shown (popup grab accepted)"))
        self.menu.connect("hide", lambda *_: log.info("menu hidden"))

        self.connect("realize", self._on_realize)
        self.connect("map-event", lambda *_: log.info("window mapped"))
        self.connect("focus-in-event", self._on_focus_in)
        self.connect("button-press-event", self._on_button_press)
        self.connect("key-press-event", self._on_key_press)
        self.add_events(Gdk.EventMask.BUTTON_PRESS_MASK | Gdk.EventMask.FOCUS_CHANGE_MASK)

        self.drawing_area.add_tick_callback(self._on_tick)
        GLib.timeout_add(33, self._periodic_redraw)

    def _periodic_redraw(self):
        self.drawing_area.queue_draw()
        return GLib.SOURCE_CONTINUE

    def _on_realize(self, *_):
        window = self.get_window()
        display = Gdk.Display.get_default()
        log.info(
            "realized: gdk_backend=%s gdk_window=%s",
            type(display).__name__ if display else "<none>",
            type(window).__name__ if window else "<none>",
        )

    def _on_tick(self, widget, frame_clock):
        self._frame_count += 1
        if self._frame_count % LOG_EVERY_N_FRAMES == 0:
            log.info("frame tick #%d (t+%.1fs)", self._frame_count, time.monotonic() - self._start_time)
        return GLib.SOURCE_CONTINUE

    def _on_draw(self, area, cr):
        alloc = area.get_allocation()
        t = time.monotonic() - self._start_time
        x = (alloc.width / 2) * (1 + 0.8 * (t % 4 - 2) / 2)
        cr.set_source_rgb(0.15, 0.15, 0.15)
        cr.paint()
        cr.set_source_rgb(0.2, 0.5, 0.9)
        cr.arc(x, alloc.height / 2, min(alloc.width, alloc.height) / 8, 0, 2 * 3.14159)
        cr.fill()

    def _open_menu(self, event, reason):
        self._focus_count += 1
        log.info("opening context menu via %s (#%d) -- real xdg_popup + grab, same as a right-click", reason, self._focus_count)
        self.menu.popup_at_pointer(event)
        GLib.timeout_add(MENU_AUTOCLOSE_MS, self._close_menu)

    def _close_menu(self):
        self.menu.popdown()
        return GLib.SOURCE_REMOVE

    def _on_focus_in(self, widget, event):
        # Fires on initial map AND again after any reconnect (the
        # recreated toplevel gets its own fresh focus-in) -- see this
        # module's own doc comment for why that's exactly the coverage
        # wanted here, with no extra scripting.
        self._open_menu(event, "focus-in")
        return False

    def _on_button_press(self, widget, event):
        if event.button == 3:
            self._open_menu(event, "right-click")
            return True
        return False

    def _on_key_press(self, widget, event):
        if event.keyval == Gdk.KEY_m:
            self._open_menu(event, "keypress")
            return True
        if event.keyval == Gdk.KEY_Escape:
            log.info("Escape pressed, closing")
            self.close()
            return True
        return False


def main():
    log.info(
        "pid=%d WAYLAND_DISPLAY=%s XDG_SESSION_TYPE=%s",
        os.getpid(),
        os.environ.get("WAYLAND_DISPLAY", "<unset>"),
        os.environ.get("XDG_SESSION_TYPE", "<unset>"),
    )
    win = PopupTestWindow()
    win.connect("destroy", Gtk.main_quit)
    win.show_all()
    Gtk.main()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
