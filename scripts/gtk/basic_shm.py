#!/usr/bin/env python3
"""Minimal GTK4 test client pinned to the cairo (software/SHM) renderer.

No dmabuf/GL involved at all -- every buffer is plain shared memory
(wl_shm_pool.create_buffer), which the recreation graph has never needed
to touch (wl_shm buffers are trivially reallocated by the client on its
own). Use this as the "should already work reliably" baseline when
crash-testing -- see dmabuf_gl.py for the GPU-backed counterpart that
exercises the harder, still-open buffer-recreation gap
(plan-desktop-resilience.md).

Run with: WAYLAND_DISPLAY=wayland-0 python3 scripts/gtk/basic_shm.py
"""
import os

os.environ["GSK_RENDERER"] = "cairo"  # must be set before gi/Gtk is imported -- see common.py

import common  # noqa: E402

if __name__ == "__main__":
    raise SystemExit(common.run("org.wlresproxy.test.BasicShm", "wl-res test: basic (SHM/cairo)", "basic_shm"))
