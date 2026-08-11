#!/bin/sh
# See scripts/gtk/run-basic-shm.sh's own doc comment -- same
# shlex.quote single-token constraint, same reasoning.
exec python3 "$(dirname "$0")/basic_shm.py"
