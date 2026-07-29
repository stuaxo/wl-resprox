#!/usr/bin/env bash
# Runs INSIDE the distrobox container. Starts the nested labwc compositor
# and reports the new Wayland socket it creates. Called by start-guest.sh —
# not meant to be run directly on the host.
set -euo pipefail

echo "Guest sees WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-<unset>}"
echo "Sockets visible before starting nested compositor:"
ls "$XDG_RUNTIME_DIR"/wayland-* 2>/dev/null || echo "  (none)"

echo "Starting nested labwc..."
labwc -C /dev/null &

sleep 2

echo "Sockets visible after starting nested compositor:"
ls "$XDG_RUNTIME_DIR"/wayland-*

echo ""
echo "If a new wayland-N socket appeared above (distinct from \$WAYLAND_DISPLAY),"
echo "the nested compositor is up. Test it with, e.g.:"
echo "  WAYLAND_DISPLAY=<new-socket> gtk4-demo"
