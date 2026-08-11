#!/usr/bin/env python3
"""Minimal SDL2 test client, SDL_RENDERER_SOFTWARE -- raster (software/SHM)
rendering, the SDL counterpart to scripts/gtk/basic_shm.py and
scripts/qt/basic_shm.py. See scripts/sdl2/common.py's own doc comment for
why this exists alongside the GTK/Qt clients.

SDL_RENDERER_SOFTWARE is requested explicitly rather than left as
SDL_CreateRenderer's own default choice -- same "don't trust an opaque
toolkit default" reasoning as GSK_RENDERER/QSurfaceFormat being set
explicitly in the other two clients (see scripts/gtk/common.py).

Run with: WAYLAND_DISPLAY=wayland-0 SDL_VIDEODRIVER=wayland python3 scripts/sdl2/basic_shm.py
"""

import ctypes
import math
import sys

import sdl2

import common

WIDTH, HEIGHT = 320, 240


def main():
    common.log_startup_environment()

    if sdl2.SDL_Init(sdl2.SDL_INIT_VIDEO) != 0:
        common.log.error("SDL_Init failed: %s", sdl2.SDL_GetError().decode())
        return 1

    window = sdl2.SDL_CreateWindow(
        b"wl-res test: basic (SHM/software)",
        sdl2.SDL_WINDOWPOS_UNDEFINED,
        sdl2.SDL_WINDOWPOS_UNDEFINED,
        WIDTH,
        HEIGHT,
        sdl2.SDL_WINDOW_SHOWN,
    )
    if not window:
        common.log.error("SDL_CreateWindow failed: %s", sdl2.SDL_GetError().decode())
        return 1

    renderer = sdl2.SDL_CreateRenderer(window, -1, sdl2.SDL_RENDERER_SOFTWARE)
    if not renderer:
        common.log.error("SDL_CreateRenderer failed: %s", sdl2.SDL_GetError().decode())
        return 1

    common.log_video_driver()
    common.log.info("realized: renderer=software widget=SDL_Renderer")

    def draw(elapsed):
        # Simple animated content -- a square sweeping left to right --
        # just enough to force genuinely new buffer contents each frame,
        # same reasoning as the GTK/Qt clients' own draw functions. SDL2's
        # core API has no filled-circle primitive without SDL2_gfx, so a
        # square stands in for it here.
        sdl2.SDL_SetRenderDrawColor(renderer, 38, 38, 38, 255)
        sdl2.SDL_RenderClear(renderer)

        size = min(WIDTH, HEIGHT) // 4
        x = int((WIDTH / 2) * (1 + 0.8 * math.sin(elapsed)) - size / 2)
        y = int(HEIGHT / 2 - size / 2)
        rect = sdl2.SDL_Rect(x, y, size, size)
        sdl2.SDL_SetRenderDrawColor(renderer, 230, 77, 51, 255)
        sdl2.SDL_RenderFillRect(renderer, ctypes.byref(rect))

        sdl2.SDL_RenderPresent(renderer)

    common.run_loop(draw)

    sdl2.SDL_DestroyRenderer(renderer)
    sdl2.SDL_DestroyWindow(window)
    sdl2.SDL_Quit()
    return 0


if __name__ == "__main__":
    sys.exit(main())
