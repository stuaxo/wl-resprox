#!/usr/bin/env bash
# Builds and starts a wayland-proxy-dev-<wm> container directly with
# podman -- no Distrobox involved. See scripts/containers/<wm>/Containerfile's
# own header comment for why: Distrobox's own container-init script was
# the source of a recurring "First time user password setup" prompt on
# every session (harmless -- fails fast rather than hanging -- but
# noisy, and the root cause of the passwordless-sudo issues documented
# in docs/debugging-notes.md's 2026-07-29/2026-07-30 entries). Building
# the image ourselves means we control user/sudoers setup entirely at
# build time and never run that script at all.
#
# Usage: ./scripts/setup-env.sh [--wm=<name>]
#   --wm=<name>   which compositor's container to build (default labwc).
#                 Must match a scripts/containers/<name>/ directory.
set -euo pipefail

WM="labwc"
for arg in "$@"; do
  case "$arg" in
    --wm=*) WM="${arg#--wm=}" ;;
    *) echo "ERROR: unrecognized argument '$arg'" >&2; exit 1 ;;
  esac
done

CONTAINER_NAME="wayland-proxy-dev-${WM}"
IMAGE_TAG="wayland-proxy-dev-${WM}:latest"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
CONTAINER_PROJECT_ROOT="/workspace"
WM_DIR="${SCRIPT_DIR}/containers/${WM}"

if [[ ! -f "${WM_DIR}/Containerfile" ]]; then
  echo "ERROR: no ${WM_DIR}/Containerfile -- unknown --wm value '${WM}'?" >&2
  exit 1
fi

# UID/GID values are host-specific facts, not hardcoded -- read live
# rather than assumed, so this works on a host with different values
# (each Containerfile's own ARG defaults exist only as a fallback
# documentation of *this* host's actual values, confirmed via the same
# commands below).
HOST_UID="$(id -u)"
HOST_GID="$(id -g)"
HOST_VIDEO_GID="$(getent group video | cut -d: -f3)"
HOST_RENDER_GID="$(getent group render | cut -d: -f3)"
if [[ -z "$HOST_VIDEO_GID" || -z "$HOST_RENDER_GID" ]]; then
  echo "ERROR: couldn't resolve the host's video/render group GIDs (getent group video render)." >&2
  exit 1
fi
HOST_RUNTIME_DIR="/run/user/${HOST_UID}"
if [[ ! -d "$HOST_RUNTIME_DIR" ]]; then
  echo "ERROR: ${HOST_RUNTIME_DIR} doesn't exist. Needs a real login session (systemd-logind/PAM) on this host." >&2
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
  -f "${WM_DIR}/Containerfile" "${WM_DIR}"

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
# -v "$PROJECT_ROOT:$CONTAINER_PROJECT_ROOT": only the project directory,
# at a fixed generic path -- deliberately NOT the whole $HOME the way
# Distrobox shared it. A whole-$HOME mount means anything the container
# writes under $HOME lands in the host's real home directory, and on any
# host where the container's (now-fixed, generic) user happens to share a
# name with the host account, it also silently shadows the image's own
# home-directory dotfiles with the host's (confirmed live: a ~/.bashrc
# customization baked into the image never took effect on this dev host,
# because both happened to be named the same). Every script references
# the fixed container-side path directly now, not a host-mirrored one.
# -v /dev:/dev: full device passthrough, matching Distrobox's default --
# makes the device *nodes* visible, but Podman's device cgroup ACL still
# blocks actually opening them regardless of Unix file permissions/group
# membership being otherwise correct (confirmed: labwc failed to open
# /dev/dri/renderD128 with EPERM -- "Operation not permitted", not EACCES
# "Permission denied" -- the tell that this is a cgroup restriction, not a
# permissions one). --device grants the actual cgroup access; Distrobox
# was evidently already doing the equivalent under the hood.
# -v "$HOST_RUNTIME_DIR:$HOST_RUNTIME_DIR": shares XDG_RUNTIME_DIR with the
# host, same path both sides -- this is what lets a nested compositor
# inside the container (entrypoint.sh) see the HOST compositor's Wayland
# socket (scripts/start-host.sh's), and is exactly what Distrobox did
# automatically that this replacement setup was missing (confirmed live:
# without this, entrypoint.sh's nested compositor fails with "Could not
# connect to remote display: No such file or directory" -- it never saw
# the host socket at all).
# --init: without it, `sleep infinity` (the CMD below) IS pid 1, and
# pid 1 has no default SIGCHLD handling of its own -- every backgrounded
# process anything inside the container ever forks and doesn't
# explicitly wait() for (labwc/sway/kwin/gnome-shell restarts across
# repeated test-crash.sh runs, dbus-daemon --fork, gnome-shell's own
# heavy child-process fan-out: gvfsd, evolution-data-server, dconf,
# at-spi, xdg-desktop-portal, ...) becomes a permanent zombie once it
# exits, since nothing ever reaps it. Confirmed live this isn't just
# cosmetic ("diagnose.sh's own count_zombie_labwc calls it harmless" was
# true for the small numbers a few manual test runs leave behind, but
# not at scale): repeated `test-crash.sh --l1` runs against the mutter
# container piled up 1483 zombies against this container's 2048
# pids-limit, and gtk4-demo started failing outright with `fork: retry:
# Resource temporarily unavailable` once fork() itself couldn't get a
# new pid. `--init` installs Podman's own bundled subreaper
# (`podman-init`) as the real pid 1, which reaps every terminated child
# it isn't itself waiting on -- confirmed live afterward, zero zombies
# survive a backgrounded child exiting. See docs/debugging-notes.md's
# 2026-07-31 entry for the full story and the live before/after check.
sudo podman run -d --name "${CONTAINER_NAME}" \
  --init \
  --network host \
  --group-add "${HOST_VIDEO_GID}" --group-add "${HOST_RENDER_GID}" \
  --device /dev/dri \
  -v "${PROJECT_ROOT}:${CONTAINER_PROJECT_ROOT}" \
  -v /dev:/dev \
  -v "${HOST_RUNTIME_DIR}:${HOST_RUNTIME_DIR}" \
  "${IMAGE_TAG}" sleep infinity

echo "Verifying passwordless sudo and render group membership..."
sudo podman exec --user dev "${CONTAINER_NAME}" bash -c '
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
echo "  ./scripts/start-guest.sh --wm=${WM}"
echo '(needs a host Wayland session first — see scripts/start-host.sh)'
echo '======================================'
