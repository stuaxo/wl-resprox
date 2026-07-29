#!/usr/bin/env bash
# Enters the distrobox container (rootful), starts the nested compositor
# via entrypoint.sh, then drops you into an interactive shell inside the
# container for testing (gtk4-demo, wayland-info, etc.).
#
# Usage: ./scripts/start-guest.sh [host-wayland-socket]
#   host-wayland-socket defaults to wayland-0 — match whatever
#   scripts/start-host.sh printed.
#
# Note: the container is rootful, so this shells out to `sudo podman exec`
# directly instead of `distrobox enter --root` — expect a password prompt
# (cached per your normal sudo timeout, not every call).
#
# IMPORTANT: this deliberately does NOT use plain `distrobox enter --root`.
# That invokes `podman exec --user=stu` under the hood, which — for this
# container, for reasons not fully root-caused — does not apply stu's
# video/render group membership, so anything touching /dev/dri (like the
# nested labwc started by entrypoint.sh) fails with "Permission denied".
# `--user stu:render` works reliably. See the 2026-07-29 entry in
# docs/debugging-notes.md for the full investigation.
set -euo pipefail

CONTAINER_NAME="wayland-proxy-dev"
HOST_DISPLAY="${1:-wayland-0}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "Starting nested compositor in '${CONTAINER_NAME}' (host display: ${HOST_DISPLAY})..."
sudo podman exec --user stu:render \
  -e WAYLAND_DISPLAY="$HOST_DISPLAY" \
  -e XDG_RUNTIME_DIR="/run/user/1000" \
  "$CONTAINER_NAME" bash "$SCRIPT_DIR/entrypoint.sh"

echo ""
echo "Entering '${CONTAINER_NAME}' interactively for testing..."
sudo podman exec -it --user stu:render \
  -e WAYLAND_DISPLAY="$HOST_DISPLAY" \
  -e XDG_RUNTIME_DIR="/run/user/1000" \
  "$CONTAINER_NAME" bash
