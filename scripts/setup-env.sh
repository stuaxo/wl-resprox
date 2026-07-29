#!/usr/bin/env bash
# Sets up an Ubuntu-based Distrobox container for Wayland proxy development

set -euo pipefail

CONTAINER_NAME="wayland-proxy-dev"
IMAGE="ubuntu:26.04"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLAYBOOK_PATH="${SCRIPT_DIR}/playbook.yml"

echo "Creating Distrobox container '${CONTAINER_NAME}' using ${IMAGE}..."
# --root: rootful container. Root inside == real root on the host, so no
# user-namespace UID/GID remapping happens — /dev/dri and other bind
# mounts keep their REAL host GIDs instead of showing as nobody:nogroup,
# which is what blocked GPU/render access under a rootless container.
# Package installs and the container filesystem remain fully separate
# from the host either way; this only affects identity mapping.
#
# NOTE: we previously also added --unshare-all here (for a private /tmp,
# to dodge an XWayland/X11-socket collision) plus an explicit
# --volume /dev/dri:/dev/dri (since --unshare-all's --unshare-devsys
# stops sharing /dev). That combination broke the container's networking
# entirely (apt-get update looping "Ign" on every mirror) — likely
# --unshare-netns not getting working DNS/routing in this rootful setup.
# Dropped both: --root alone already fixes the GID problem, host /dev and
# /tmp share normally by default, and a fresh container without
# accumulated crash debris may not hit the /tmp/.X11-unix collision at
# all. If it resurfaces, clean it up manually rather than reintroducing
# --unshare-all (see docs/debugging-notes.md).
# Note: Distrobox's rootful containers already default to --network host
# under the hood (confirmed via `distrobox create --root --dry-run`) — no
# extra flag needed here. Earlier confusion: manual `sudo podman run`
# tests done directly (bypassing Distrobox) use plain Podman's own
# default networking instead, a private bridge that turned out to be
# broken on this host — a separate default from Distrobox's, which was
# never actually the problem.
#
# --additional-flags "--group-add 991 --group-add 44": bakes host's render
# (991) and video (44) GIDs into the container at CREATE time. Kept because
# it's harmless and DOES work as expected for a bare `podman run` container
# (verified in isolation) — but empirically it does NOT make plain
# `distrobox enter --root` sessions pick up render/video group membership
# for THIS (distrobox-managed) container; root cause not identified, see
# the 2026-07-29 entry in docs/debugging-notes.md. `podman exec
# --user=stu` (what `distrobox enter --root` does under the hood) only
# ever sets the primary GID from /etc/passwd for this container, ignoring
# supplementary groups from /etc/group regardless of this flag.
#
# PRACTICAL UPSHOT: anything that touches /dev/dri (nested labwc, GTK apps
# rendering via EGL) still needs the group named explicitly at exec time,
# e.g. `sudo podman exec --user stu:render wayland-proxy-dev <command>` —
# plain `distrobox enter --root` is NOT sufficient on its own. GIDs above
# are host-specific (this host: video=44, render=991 — confirmed via
# `getent group video render` on the host); update if run on different
# hardware.
distrobox create --root --name "${CONTAINER_NAME}" --image "${IMAGE}" \
  --additional-flags "--group-add 991 --group-add 44" --yes

# Provisioning runs as container ROOT via `podman exec`, not via
# `distrobox enter --root` (which execs as `stu`) — deliberately.
# Distrobox's own container-init script clears stu's password
# unconditionally on first boot and never sets up a working credential or
# NOPASSWD sudoers rule for rootful containers, so any `sudo` call as stu
# hits a password prompt that NOTHING can satisfy (there's no correct
# password — none was ever set). See the 2026-07-29 entry in
# docs/debugging-notes.md. Running provisioning as root sidesteps this
# entirely; playbook.yml's own "Allow stu passwordless sudo" task fixes it
# permanently for your own later interactive `sudo` use.
echo "Starting container..."
sudo podman start "${CONTAINER_NAME}"

echo "Waiting for container to finish its own init before provisioning..."
for _ in $(seq 1 30); do
  sudo podman exec "${CONTAINER_NAME}" true 2>/dev/null && break
  sleep 1
done
for _ in $(seq 1 60); do
  sudo podman exec "${CONTAINER_NAME}" pgrep -x apt-get > /dev/null 2>&1 || break
  sleep 2
done

echo "Installing Ansible and running playbook (as container root)..."
sudo podman exec "${CONTAINER_NAME}" bash -c "
  apt-get update -y
  apt-get install -y ansible
  ansible-playbook '${PLAYBOOK_PATH}'
"

echo ''
echo '======================================'
echo 'Environment successfully provisioned!'
echo 'To start testing, run:'
echo "  ./scripts/start-guest.sh"
echo '(needs a host Wayland session first — see scripts/start-host.sh)'
echo '======================================'
