#!/usr/bin/env python3
"""Minimal GTK4 test client pinned to the GL renderer -- dmabuf-backed
buffers, exercising the same class of GPU buffer as gtk4-demo (which is
what actually caught the "invalid arguments for wl_surface#N.frame" fatal
disconnect live tonight, see plan-desktop-resilience.md). wl_buffer
(dmabuf or otherwise) is deliberately outside the recreation graph today,
so a client that keeps reusing its own pre-crash GPU buffers (rather than
allocating fresh ones) after a reconnect is expected to still hit that
gap with this script -- that's the point: a small, self-logging
reproduction instead of needing strace on a real app to see what's
happening.

Run with: WAYLAND_DISPLAY=wayland-0 python3 scripts/gtk/dmabuf_gl.py
"""
import os

os.environ["GSK_RENDERER"] = "gl"  # must be set before gi/Gtk is imported -- see common.py

import common  # noqa: E402

if __name__ == "__main__":
    raise SystemExit(common.run("org.wlresproxy.test.DmabufGl", "wl-res test: dmabuf (GL)"))
