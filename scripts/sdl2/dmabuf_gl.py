#!/usr/bin/env python3
"""Minimal SDL2 test client, a real OpenGL context via SDL_GL_CreateContext
-- exercising the same class of GPU-backed buffer as
scripts/gtk/dmabuf_gl.py and scripts/qt/dmabuf_gl.py. See
scripts/sdl2/common.py's own doc comment for why this exists alongside the
GTK/Qt clients: SDL2's own Wayland/EGL integration is a third, genuinely
separate code path from GTK's and Qt's. wl_buffer (dmabuf or otherwise) is
deliberately outside the recreation graph today (see recreation.rs), so a
client that keeps reusing its own pre-crash GPU buffer after a reconnect
is expected to still hit that gap with this script -- consistent with the
other two clients' own doc comments.

COMPATIBILITY profile requested explicitly (not left as SDL's default)
specifically so plain immediate-mode glBegin/glEnd calls can be used below
-- the point of this client is exercising a real GPU-backed EGL/dmabuf
surface the same way the GTK/Qt clients do, not modern GL API coverage,
so there's no reason to add shader/VBO boilerplate a CORE profile would
require.

Run with: WAYLAND_DISPLAY=wayland-0 SDL_VIDEODRIVER=wayland python3 scripts/sdl2/dmabuf_gl.py
"""

import math
import sys

import sdl2
from OpenGL import GL

import common

WIDTH, HEIGHT = 320, 240


def main():
    common.log_startup_environment()

    if sdl2.SDL_Init(sdl2.SDL_INIT_VIDEO) != 0:
        common.log.error("SDL_Init failed: %s", sdl2.SDL_GetError().decode())
        return 1

    # Requested before window/context creation -- same ordering constraint
    # GSK_RENDERER-before-Gtk-import and QSurfaceFormat.setDefaultFormat
    # have in the other two clients (see their own doc comments).
    sdl2.SDL_GL_SetAttribute(sdl2.SDL_GL_CONTEXT_PROFILE_MASK, sdl2.SDL_GL_CONTEXT_PROFILE_COMPATIBILITY)
    sdl2.SDL_GL_SetAttribute(sdl2.SDL_GL_DOUBLEBUFFER, 1)

    window = sdl2.SDL_CreateWindow(
        b"wl-res test: dmabuf (GL)",
        sdl2.SDL_WINDOWPOS_UNDEFINED,
        sdl2.SDL_WINDOWPOS_UNDEFINED,
        WIDTH,
        HEIGHT,
        sdl2.SDL_WINDOW_SHOWN | sdl2.SDL_WINDOW_OPENGL,
    )
    if not window:
        common.log.error("SDL_CreateWindow failed: %s", sdl2.SDL_GetError().decode())
        return 1

    gl_ctx = sdl2.SDL_GL_CreateContext(window)
    if not gl_ctx:
        common.log.error("SDL_GL_CreateContext failed: %s", sdl2.SDL_GetError().decode())
        return 1
    sdl2.SDL_GL_MakeCurrent(window, gl_ctx)

    common.log_video_driver()
    common.log.info(
        "realized: widget=SDL_GLContext gl_version=%s gl_renderer=%s",
        GL.glGetString(GL.GL_VERSION).decode(errors="replace"),
        GL.glGetString(GL.GL_RENDERER).decode(errors="replace"),
    )

    def draw(elapsed):
        # Simple animated content -- a triangle sweeping left to right --
        # same reasoning as the other clients' own draw functions: force
        # genuinely new frame contents each redraw, not a static image the
        # toolkit might short-circuit.
        x = 0.8 * math.sin(elapsed)
        GL.glClearColor(0.15, 0.15, 0.2, 1.0)
        GL.glClear(GL.GL_COLOR_BUFFER_BIT)
        GL.glColor3f(0.2, 0.5, 0.9)
        GL.glBegin(GL.GL_TRIANGLES)
        GL.glVertex2f(x, 0.3)
        GL.glVertex2f(x - 0.25, -0.3)
        GL.glVertex2f(x + 0.25, -0.3)
        GL.glEnd()
        sdl2.SDL_GL_SwapWindow(window)

    common.run_loop(draw)

    sdl2.SDL_GL_DeleteContext(gl_ctx)
    sdl2.SDL_DestroyWindow(window)
    sdl2.SDL_Quit()
    return 0


if __name__ == "__main__":
    sys.exit(main())
