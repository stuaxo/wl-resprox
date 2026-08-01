#!/usr/bin/env bash
# Runs INSIDE a wayland-proxy-dev-<wm> container. Starts the nested
# compositor (whichever $COMPOSITOR the image bakes in -- see
# scripts/containers/<wm>/Containerfile) and reports the new Wayland
# socket it creates. Called by start-guest.sh — not meant to be run
# directly on the host.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
COMPOSITOR="${COMPOSITOR:?COMPOSITOR must be set -- baked into the image by scripts/containers/<wm>/Containerfile}"

# shellcheck source=./run-registry.sh disable=SC1091
source "$SCRIPT_DIR/run-registry.sh"
# shellcheck source=./compositor-launch.sh disable=SC1091
source "$SCRIPT_DIR/compositor-launch.sh"
run_dir_init
# Unlike test-crash.sh, this script doesn't clean up after itself (the
# whole point is to leave the nested compositor running for the
# interactive shell start-guest.sh drops you into next) -- so nothing
# here calls run_cleanup. Tracking pids is still worth it purely for
# `cat`-ability: `$XDG_RUNTIME_DIR/wayland-proxy-runs/current/chain`
# gives a manual debugging session one place to check "what's actually
# running and what's its pid" instead of `pgrep`-ing around, which is
# exactly the friction that motivated this (see run-registry.sh's own
# header and the 2026-07-31 mutter entry in docs/debugging-notes.md for
# the DBUS_SESSION_BUS_ADDRESS-shuttling-between-podman-exec-calls
# version of the same problem).

echo "Guest sees WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-<unset>}, COMPOSITOR=${COMPOSITOR}"
echo "Sockets visible before starting nested compositor:"
ls "$XDG_RUNTIME_DIR"/wayland-*[0-9] 2>/dev/null || echo "  (none)"

echo "Starting nested ${COMPOSITOR}..."
launch_compositor "$COMPOSITOR" || {
    echo "ERROR: no launch case for COMPOSITOR='$COMPOSITOR' -- add one to scripts/compositor-launch.sh." >&2
    exit 1
}

# 5s, not 2s -- sway confirmed live to need more than 2s here at least
# once; labwc is usually faster but this costs little either way.
sleep 5

echo "Sockets visible after starting nested compositor:"
ls "$XDG_RUNTIME_DIR"/wayland-*[0-9]

echo ""
echo "If a new wayland-N socket appeared above (distinct from \$WAYLAND_DISPLAY),"
echo "the nested compositor is up. Test it with, e.g.:"
echo "  WAYLAND_DISPLAY=<new-socket> gtk4-demo"
echo ""
echo "Note: a name can reappear identically above even when it's now a"
echo "genuinely new, live compositor (a stale socket from something else"
echo "getting reused) -- if this listing looks unchanged but you expected"
echo "a new one, verify with the WAYLAND_DISPLAY test above rather than"
echo "trusting the listing alone (see the 2026-07-31 test-crash.sh entry"
echo "in docs/debugging-notes.md for the full story)."
