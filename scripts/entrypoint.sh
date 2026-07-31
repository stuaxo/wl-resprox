#!/usr/bin/env bash
# Runs INSIDE a wayland-proxy-dev-<wm> container. Starts the nested
# compositor (whichever $COMPOSITOR the image bakes in -- see
# scripts/containers/<wm>/Containerfile) and reports the new Wayland
# socket it creates. Called by start-guest.sh — not meant to be run
# directly on the host.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
COMPOSITOR="${COMPOSITOR:?COMPOSITOR must be set -- baked into the image by scripts/containers/<wm>/Containerfile}"

echo "Guest sees WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-<unset>}, COMPOSITOR=${COMPOSITOR}"
echo "Sockets visible before starting nested compositor:"
ls "$XDG_RUNTIME_DIR"/wayland-* 2>/dev/null || echo "  (none)"

echo "Starting nested ${COMPOSITOR}..."
case "$COMPOSITOR" in
  labwc)
    labwc -C "$SCRIPT_DIR/containers/labwc/labwc-config" &
    ;;
  sway)
    sway -c "$SCRIPT_DIR/containers/sway/sway-config" &
    ;;
  *)
    echo "ERROR: no launch case for COMPOSITOR='$COMPOSITOR' -- add one here." >&2
    exit 1
    ;;
esac

sleep 2

echo "Sockets visible after starting nested compositor:"
ls "$XDG_RUNTIME_DIR"/wayland-*

echo ""
echo "If a new wayland-N socket appeared above (distinct from \$WAYLAND_DISPLAY),"
echo "the nested compositor is up. Test it with, e.g.:"
echo "  WAYLAND_DISPLAY=<new-socket> gtk4-demo"
