#!/usr/bin/env bash
# Enters a wayland-proxy-dev-<wm> container (a plain podman container,
# built and started by scripts/setup-env.sh -- see
# scripts/containers/<wm>/Containerfile), starts the nested compositor
# via entrypoint.sh, then drops you into an interactive shell inside the
# container for testing (gtk4-demo, wayland-info, etc.).
#
# Usage: ./scripts/start-guest.sh [--wm=<name>] [host-wayland-socket]
#   --wm=<name>          which compositor's container to enter (default labwc).
#   host-wayland-socket  defaults to wayland-0 — match whatever
#                        scripts/start-host.sh printed.
#
# Note: this shells out to `sudo podman exec` -- expect a password prompt
# (cached per your normal sudo timeout, not every call).
#
# `--user dev:render` (not just `--user dev`) is explicit here even though
# scripts/setup-env.sh's --group-add plus the matching video/render GIDs
# baked into the image should already make plain `--user dev` pick up
# full supplementary groups (unlike the Distrobox-managed container this
# setup replaced, which never did regardless -- see the 2026-07-29 entry
# in docs/debugging-notes.md for that investigation). Kept explicit as the
# known-safe choice rather than re-relying on unverified group resolution.
set -euo pipefail

WM="labwc"
HOST_DISPLAY=""
for arg in "$@"; do
  case "$arg" in
    --wm=*) WM="${arg#--wm=}" ;;
    *) HOST_DISPLAY="$arg" ;;
  esac
done
HOST_DISPLAY="${HOST_DISPLAY:-wayland-0}"

CONTAINER_NAME="wayland-proxy-dev-${WM}"
# Fixed, not derived from this script's own host-side location -- the
# container only ever mounts the project directory at /workspace (see
# setup-env.sh), not a host-mirrored path.
CONTAINER_PROJECT_ROOT="/workspace"

echo "Starting nested compositor in '${CONTAINER_NAME}' (host display: ${HOST_DISPLAY})..."
sudo podman exec --user dev:render \
  -e WAYLAND_DISPLAY="$HOST_DISPLAY" \
  -e XDG_RUNTIME_DIR="/run/user/1000" \
  "$CONTAINER_NAME" bash "${CONTAINER_PROJECT_ROOT}/scripts/entrypoint.sh"

echo ""
echo "Entering '${CONTAINER_NAME}' interactively for testing..."
sudo podman exec -it --user dev:render \
  -e WAYLAND_DISPLAY="$HOST_DISPLAY" \
  -e XDG_RUNTIME_DIR="/run/user/1000" \
  -w "${CONTAINER_PROJECT_ROOT}" \
  "$CONTAINER_NAME" bash
