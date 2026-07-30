#!/usr/bin/env bash
# Stops and removes the wayland-proxy-dev container. Reverses
# scripts/setup-env.sh. Re-run setup-env.sh to rebuild.
#
# Usage: ./scripts/teardown-env.sh [--image]
#   --image   also remove the built image (wayland-proxy-dev:latest).
#             Without it, setup-env.sh's next build reuses cached layers.
set -euo pipefail

CONTAINER_NAME="wayland-proxy-dev"
IMAGE_TAG="wayland-proxy-dev:latest"

REMOVE_IMAGE=false
if [[ "${1:-}" == "--image" ]]; then
  REMOVE_IMAGE=true
fi

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
