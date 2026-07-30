#!/usr/bin/env bash
# Builds and starts the wayland-proxy-dev container directly with podman --
# no Distrobox involved. See scripts/Containerfile's own header comment for
# why: Distrobox's own container-init script was the source of a recurring
# "First time user password setup" prompt on every session (harmless --
# fails fast rather than hanging -- but noisy, and the root cause of the
# passwordless-sudo issues documented in docs/debugging-notes.md's
# 2026-07-29/2026-07-30 entries). Building the image ourselves means we
# control user/sudoers setup entirely at build time and never run that
# script at all.
set -euo pipefail

CONTAINER_NAME="wayland-proxy-dev"
IMAGE_TAG="wayland-proxy-dev:latest"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# UID/GID values are host-specific facts, not hardcoded -- read live
# rather than assumed, so this works on a host with different values
# (Containerfile's own ARG defaults exist only as a fallback documentation
# of *this* host's actual values, confirmed via the same commands below).
HOST_UID="$(id -u)"
HOST_GID="$(id -g)"
HOST_VIDEO_GID="$(getent group video | cut -d: -f3)"
HOST_RENDER_GID="$(getent group render | cut -d: -f3)"
if [[ -z "$HOST_VIDEO_GID" || -z "$HOST_RENDER_GID" ]]; then
  echo "ERROR: couldn't resolve the host's video/render group GIDs (getent group video render)." >&2
  exit 1
fi

echo "Building ${IMAGE_TAG} (uid=${HOST_UID} gid=${HOST_GID} video=${HOST_VIDEO_GID} render=${HOST_RENDER_GID})..."
# --network host for the same reason the container run below uses it:
# this host's plain Podman bridge networking is broken (apt-get inside the
# build loops "Ign" on every mirror otherwise) -- see docs/debugging-notes.md.
sudo podman build \
  --network host \
  -t "${IMAGE_TAG}" \
  --build-arg USER_UID="${HOST_UID}" \
  --build-arg USER_GID="${HOST_GID}" \
  --build-arg VIDEO_GID="${HOST_VIDEO_GID}" \
  --build-arg RENDER_GID="${HOST_RENDER_GID}" \
  -f "${SCRIPT_DIR}/Containerfile" "${SCRIPT_DIR}"

if sudo podman container exists "${CONTAINER_NAME}"; then
  echo "Removing existing ${CONTAINER_NAME} container..."
  sudo podman rm -f "${CONTAINER_NAME}"
fi

echo "Starting ${CONTAINER_NAME}..."
# --network host: see the build step's comment above.
# --group-add: belt-and-braces alongside the video/render GIDs already
# baked into /etc/group at the matching host values in the Containerfile
# -- confirmed in isolation (see docs/debugging-notes.md) that a plain
# `podman run --group-add ...` container reliably passes supplementary
# groups through to `podman exec --user=<name>` (no `:group` override
# needed), unlike the Distrobox-managed container this replaces, which
# never did regardless of this flag for reasons never fully root-caused.
# -v "$HOME:$HOME": shares the whole project tree (and everything else in
# $HOME) between host and container, matching what Distrobox did
# automatically -- this is what lets every `podman exec` in this
# project's scripts just cd into the same absolute project path on both
# sides.
# -v /dev:/dev: full device passthrough, matching Distrobox's default --
# makes the device *nodes* visible, but Podman's device cgroup ACL still
# blocks actually opening them regardless of Unix file permissions/group
# membership being otherwise correct (confirmed: labwc failed to open
# /dev/dri/renderD128 with EPERM -- "Operation not permitted", not EACCES
# "Permission denied" -- the tell that this is a cgroup restriction, not a
# permissions one). --device grants the actual cgroup access; Distrobox
# was evidently already doing the equivalent under the hood.
sudo podman run -d --name "${CONTAINER_NAME}" \
  --network host \
  --group-add "${HOST_VIDEO_GID}" --group-add "${HOST_RENDER_GID}" \
  --device /dev/dri \
  -v "${HOME}:${HOME}" \
  -v /dev:/dev \
  "${IMAGE_TAG}" sleep infinity

# /run/user/<uid> (XDG_RUNTIME_DIR, where every Wayland socket in this
# project lives) is normally created by a login session manager
# (systemd-logind, PAM) or, previously, by Distrobox's own init --
# nothing does that here, so /run/user/1000 doesn't exist yet and `stu`
# can't create it directly (/run itself is root:root 0755). One-time fix,
# as root, right after start; persists for the container's lifetime since
# it's a long-running `sleep infinity` container, not restarted per-command.
echo "Creating XDG_RUNTIME_DIR (/run/user/${HOST_UID})..."
# --user root: the image's own default exec user is stu (see the
# Containerfile's `USER stu`), which can't create anything directly under
# root:root 0755 /run -- this one step needs root.
sudo podman exec --user root "${CONTAINER_NAME}" \
  bash -c "mkdir -p /run/user/${HOST_UID} && chown ${HOST_UID}:${HOST_GID} /run/user/${HOST_UID} && chmod 700 /run/user/${HOST_UID}"

echo "Verifying passwordless sudo and render group membership..."
sudo podman exec --user stu "${CONTAINER_NAME}" bash -c '
  set -e
  sudo -n true
  echo "sudo: passwordless OK"
  groups | grep -qw render && groups | grep -qw video
  echo "groups: $(groups)"
'

echo ''
echo '======================================'
echo 'Environment successfully provisioned!'
echo 'To start testing, run:'
echo "  ./scripts/start-guest.sh"
echo '(needs a host Wayland session first — see scripts/start-host.sh)'
echo '======================================'
