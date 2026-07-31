#!/usr/bin/env bash
# Stops and removes a wayland-proxy-dev-<wm> container. Reverses
# scripts/setup-env.sh. Re-run setup-env.sh to rebuild.
#
# Usage: ./scripts/teardown-env.sh [--wm=<name>] [--image]
#   --wm=<name>   which compositor's container to remove (default labwc).
#   --image       also remove the built image. Without it, setup-env.sh's
#                 next build reuses cached layers.
set -euo pipefail

WM="labwc"
REMOVE_IMAGE=false
for arg in "$@"; do
  case "$arg" in
    --wm=*) WM="${arg#--wm=}" ;;
    --image) REMOVE_IMAGE=true ;;
    *) echo "ERROR: unrecognized argument '$arg'" >&2; exit 1 ;;
  esac
done

CONTAINER_NAME="wayland-proxy-dev-${WM}"
IMAGE_TAG="wayland-proxy-dev-${WM}:latest"

if sudo podman container exists "${CONTAINER_NAME}"; then
  echo "Stopping and removing ${CONTAINER_NAME}..."
  sudo podman rm -f "${CONTAINER_NAME}"
else
  echo "${CONTAINER_NAME}: no container to remove."
fi

if [[ "${REMOVE_IMAGE}" == true ]]; then
  if sudo podman image exists "${IMAGE_TAG}"; then
    echo "Removing ${IMAGE_TAG}..."
    sudo podman rmi "${IMAGE_TAG}"
  else
    echo "${IMAGE_TAG}: no image to remove."
  fi
fi

echo "Done."
