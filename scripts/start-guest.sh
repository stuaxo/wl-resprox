#!/usr/bin/env bash
# Enters the wayland-proxy-dev container (a plain podman container, built
# and started by scripts/setup-env.sh -- see scripts/Containerfile),
# starts the nested compositor via entrypoint.sh, then drops you into an
# interactive shell inside the container for testing (gtk4-demo,
# wayland-info, etc.).
#
# Usage: ./scripts/start-guest.sh [host-wayland-socket]
#   host-wayland-socket defaults to wayland-0 — match whatever
#   scripts/start-host.sh printed.
#
# Note: this shells out to `sudo podman exec` -- expect a password prompt
# (cached per your normal sudo timeout, not every call).
#
# `--user stu:render` (not just `--user stu`) is explicit here even though
# scripts/setup-env.sh's --group-add plus the matching video/render GIDs
# baked into the image should already make plain `--user stu` pick up
# full supplementary groups (unlike the Distrobox-managed container this
# setup replaced, which never did regardless -- see the 2026-07-29 entry
# in docs/debugging-notes.md for that investigation). Kept explicit as the
# known-safe choice rather than re-relying on unverified group resolution.
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
