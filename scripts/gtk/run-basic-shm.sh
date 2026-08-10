#!/bin/sh
# See run-dmabuf-gl.sh's own doc comment -- same reasoning, for the
# wl_shm/cairo counterpart.
exec python3 "$(dirname "$0")/basic_shm.py"
