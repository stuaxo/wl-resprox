#!/usr/bin/env bash
# Cross-compositor swap test: like test-crash.sh, but the compositor that
# comes back after the crash is a DIFFERENT window manager, in a
# DIFFERENT container, than the one that crashed. Run from the HOST (not
# inside any container) -- see plan-test-harness.md's "Cross-compositor
# swap" section for why this pairing matters (same-compositor restart
# only proves recreation against one implementation's protocol
# strictness; a real desktop swap is the untested risk).
#
# Usage: ./scripts/test-crash-swap.sh --from=<wm> --to=<wm> [client-app]
#   --from=<wm>   compositor that starts first and gets crashed. Its
#                 container (wayland-proxy-dev-<wm>) must already exist
#                 and be running (./scripts/setup-env.sh --wm=<wm>).
#   --to=<wm>     compositor started fresh afterward, on the same freed
#                 socket name -- same requirement.
#   client-app    defaults to gtk4-demo, runs inside the --to container.
#
# What it does (same shape as test-crash.sh, spread across two
# containers instead of one):
#   1. Starts --from's compositor, headless, inside its own container.
#   2. Starts the wayland-proxy binary ON THE HOST, pointed at it -- the
#      proxy has no GUI/GTK dependency (unlike the client), so unlike
#      the compositor and client it doesn't need to run in any
#      container at all; running it on the host keeps this script from
#      needing a third container just to hold it.
#   3. Starts the client, inside --to's container, connected through the
#      proxy.
#   4. kill -9's --from's compositor -- the crash.
#   5. Starts --to's compositor, headless, inside ITS container, and
#      confirms it lands on the exact same freed socket name (the proxy
#      only ever reconnects to the fixed path it started with, see
#      src/main.rs -- if the new compositor picked a different slot,
#      the proxy would never see it, which would look like a hang, not
#      a clean failure).
#   6. Checks whether the client survived, same criteria as test-crash.sh.
#
# All three roles (from-compositor, proxy, to-compositor+client) are
# tracked via run-registry.sh into ONE shared run directory under
# $XDG_RUNTIME_DIR/wayland-proxy-runs/<run-id>/ -- this is the scenario
# that module was actually built for: a pid only means something within
# its own container's pid namespace, and without a registry recording
# which container each one belongs to, cleaning up (or even just
# understanding what's still running) after a failed cross-container run
# is real archaeology. See run-registry.sh's own header for the details.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

FROM="" TO="" CLIENT_APP="gtk4-demo"
for arg in "$@"; do
    case "$arg" in
        --from=*) FROM="${arg#--from=}" ;;
        --to=*) TO="${arg#--to=}" ;;
        *) CLIENT_APP="$arg" ;;
    esac
done
if [[ -z "$FROM" || -z "$TO" ]]; then
    echo "Usage: $0 --from=<wm> --to=<wm> [client-app]" >&2
    exit 1
fi

FROM_CONTAINER="wayland-proxy-dev-${FROM}"
TO_CONTAINER="wayland-proxy-dev-${TO}"
for c in "$FROM_CONTAINER" "$TO_CONTAINER"; do
    sudo podman container exists "$c" || {
        echo "ERROR: container '$c' doesn't exist -- run ./scripts/setup-env.sh --wm=<name> first." >&2
        exit 1
    }
    running="$(sudo podman inspect -f '{{.State.Running}}' "$c" 2>/dev/null)"
    [[ "$running" == "true" ]] || {
        echo "ERROR: container '$c' exists but isn't running -- ./scripts/setup-env.sh --wm=<name> starts it." >&2
        exit 1
    }
done

# Same fixed uid this project's other host-side scripts already assume
# (start-guest.sh, diagnose.sh's pull_guest_diagnostics) -- this host's
# actual login uid, baked into every container's /etc/passwd at build
# time by setup-env.sh's own `id -u`.
CONTAINER_RUNTIME_DIR="/run/user/1000"
RUNTIME_DIR="${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR must be set -- run this from the host, not inside a container}"
PROXY_DISPLAY="wayland-proxy-0" # fixed name the proxy binds, see src/main.rs

# shellcheck source=./run-registry.sh disable=SC1091
source "$SCRIPT_DIR/run-registry.sh"
# shellcheck source=./socket-wait.sh disable=SC1091
source "$SCRIPT_DIR/socket-wait.sh"
run_dir_init

PROXY_LOG="$(mktemp)"

PROXY_PID=""

# shellcheck disable=SC2329,SC2317
cleanup() {
    local exit_status=$?
    echo ""
    echo "Cleaning up..."
    run_cleanup
    rm -f "$PROXY_LOG"
    if [[ "$exit_status" -eq 0 ]]; then
        rm -rf "$RUN_DIR"
    else
        echo "Run directory kept for inspection: $RUN_DIR"
    fi
}
trap cleanup EXIT

fail() {
    echo "FAIL: $1"
    echo "--- proxy log (host) ---"; cat "$PROXY_LOG"
    echo "--- ${FROM_CONTAINER} compositor log ---"; sudo podman exec "$FROM_CONTAINER" cat /tmp/swap-compositor.log 2>/dev/null
    echo "--- ${TO_CONTAINER} compositor log ---"; sudo podman exec "$TO_CONTAINER" cat /tmp/swap-compositor.log 2>/dev/null
    echo "--- ${TO_CONTAINER} client log ---"; sudo podman exec "$TO_CONTAINER" cat /tmp/swap-client.log 2>/dev/null
    exit 1
}

# Per ADR-0003, the harness never builds the proxy from source itself.
# On the host (unlike inside a container) that means the release binary
# `cargo deb` already produced as a build byproduct, not a system-wide
# `dpkg -i` -- this is a dev machine, not a disposable container. Both
# containers already had to exist (checked above), which means
# setup-env.sh already ran `cargo deb` for at least one of them, so this
# should already be here; the check exists to fail clearly rather than
# with a confusing "no such file" if it somehow isn't.
PROXY_BIN="$PROJECT_DIR/target/release/wayland-proxy"
[[ -x "$PROXY_BIN" ]] || fail "$PROXY_BIN not found -- run ./scripts/setup-env.sh --wm=<name> first (it builds this as a byproduct of packaging the .deb)"

echo "== Starting ${FROM}'s compositor in ${FROM_CONTAINER} =="
before="$(snapshot_live_sockets)"
sudo podman exec -d --user dev -e XDG_RUNTIME_DIR="$CONTAINER_RUNTIME_DIR" -e RUN_DIR="$RUN_DIR" \
    "$FROM_CONTAINER" bash -c "
        SCRIPT_DIR=/workspace
        source /workspace/run-registry.sh
        source /workspace/compositor-launch.sh
        launch_compositor '$FROM' /tmp/swap-compositor.log 'compositor-${FROM}'
    " || fail "couldn't start ${FROM} in ${FROM_CONTAINER}"

FROM_DISPLAY="$(wait_for_new_socket "$before")"
[[ -n "$FROM_DISPLAY" ]] || fail "${FROM} never created a socket in ${FROM_CONTAINER}"
run_link_socket "compositor-${FROM}" "$RUNTIME_DIR/$FROM_DISPLAY"
echo "${FROM} socket: $FROM_DISPLAY"

echo "== Starting proxy on host (-> $FROM_DISPLAY) =="
rm -f "$RUNTIME_DIR/$PROXY_DISPLAY"
WAYLAND_DISPLAY="$FROM_DISPLAY" "$PROXY_BIN" \
    > "$PROXY_LOG" 2>&1 &
PROXY_PID=$!
run_track proxy "$PROXY_PID"

for _ in $(seq 1 20); do
    [[ -e "$RUNTIME_DIR/$PROXY_DISPLAY" ]] && break
    kill -0 "$PROXY_PID" 2>/dev/null || fail "proxy exited before creating its socket"
    sleep 0.25
done
[[ -e "$RUNTIME_DIR/$PROXY_DISPLAY" ]] || fail "proxy never created $PROXY_DISPLAY"
run_link_socket proxy "$RUNTIME_DIR/$PROXY_DISPLAY"
echo "Proxy socket: $PROXY_DISPLAY (pid $PROXY_PID, on host)"

echo "== Starting client ($CLIENT_APP) in ${TO_CONTAINER}, through the proxy =="
sudo podman exec -d --user dev -e XDG_RUNTIME_DIR="$CONTAINER_RUNTIME_DIR" \
    -e WAYLAND_DISPLAY="$PROXY_DISPLAY" -e RUN_DIR="$RUN_DIR" \
    "$TO_CONTAINER" bash -c "
        source /workspace/run-registry.sh
        '$CLIENT_APP' > /tmp/swap-client.log 2>&1 &
        run_track client \"\$!\"
    " || fail "couldn't start ${CLIENT_APP} in ${TO_CONTAINER}"

sleep 2
run_is_alive client || fail "$CLIENT_APP exited before the crash even happened"
echo "Client is up in ${TO_CONTAINER}. Giving it a moment to settle..."
sleep 1

echo "== Crashing ${FROM} (in ${FROM_CONTAINER}) =="
FROM_COMPOSITOR_PID="$(sed -n '2p' "$RUN_DIR/compositor-${FROM}.pid")"
sudo podman exec "$FROM_CONTAINER" kill -9 "$FROM_COMPOSITOR_PID" 2>/dev/null

sleep 2

echo "== Starting ${TO}'s compositor in ${TO_CONTAINER} (expecting it to reclaim $FROM_DISPLAY) =="
before="$(snapshot_live_sockets)"
sudo podman exec -d --user dev -e XDG_RUNTIME_DIR="$CONTAINER_RUNTIME_DIR" -e RUN_DIR="$RUN_DIR" \
    "$TO_CONTAINER" bash -c "
        SCRIPT_DIR=/workspace
        source /workspace/run-registry.sh
        source /workspace/compositor-launch.sh
        launch_compositor '$TO' /tmp/swap-compositor.log 'compositor-${TO}'
    " || fail "couldn't start ${TO} in ${TO_CONTAINER}"

TO_DISPLAY="$(wait_for_new_socket "$before")"
[[ -n "$TO_DISPLAY" ]] || fail "${TO} never created a socket in ${TO_CONTAINER}"
run_link_socket "compositor-${TO}" "$RUNTIME_DIR/$TO_DISPLAY"
echo "${TO} socket: $TO_DISPLAY"

if [[ "$TO_DISPLAY" != "$FROM_DISPLAY" ]]; then
    fail "${TO} landed on ${TO_DISPLAY}, not the freed ${FROM_DISPLAY} -- the proxy is still pointed at ${FROM_DISPLAY} and will never see it. Socket auto-selection didn't reuse the slot this time (see docs/debugging-notes.md for the reuse mechanism this depends on)."
fi

sleep 2

echo ""
if run_is_alive client; then
    echo "SUCCESS: $CLIENT_APP survived the ${FROM} -> ${TO} swap."
    exit 0
else
    echo "FAIL: $CLIENT_APP did not survive the ${FROM} -> ${TO} swap."
    echo "--- proxy log (host) ---"
    cat "$PROXY_LOG"
    echo "--- ${TO_CONTAINER} client log ---"
    sudo podman exec "$TO_CONTAINER" cat /tmp/swap-client.log 2>/dev/null
    exit 1
fi
