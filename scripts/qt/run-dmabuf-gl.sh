#!/bin/sh
# See scripts/gtk/run-dmabuf-gl.sh's own doc comment -- same
# shlex.quote single-token constraint, same reasoning.
exec python3 "$(dirname "$0")/dmabuf_gl.py"
