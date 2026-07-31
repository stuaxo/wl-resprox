#!/usr/bin/env bash
# Automated crash-recovery test harness. Runs entirely inside the
# wayland-proxy-dev container (needs a real Wayland session -- GTK/EGL --
# so it has the same GPU-access requirement as everything else in scripts/).
# From an interactive shell there (scripts/start-guest.sh drops you into
# one already at the project root):
#
#   bash scripts/test-crash.sh
#
# What it does:
#   1. Starts a headless compositor (the "server" the proxy fronts).
#   2. Starts the wayland-proxy binary pointed at it.
#   3. Starts a GTK client connected through the proxy.
#   4. kill -9's the compositor -- the actual crash.
#   5. Checks whether the GTK client process is still alive, and prints a
#      SUCCESS/FAIL line accordingly.
#
# This does NOT check that the client is still usable/rendering after the
# crash, only that the proxy kept its socket open instead of tearing the
# connection down -- i.e. that Phase 5's "freeze the client socket, don't
# forward the error" rule (docs/implementation-constraints.md) holds.
# Doesn't restart the compositor either, so it doesn't exercise
# reconnect/recreation -- see plan-test-harness.md for the fuller
# testing-levels picture and why this stays a narrow L0 check.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
CLIENT_APP="${1:-gtk4-demo}"
COMPOSITOR="${COMPOSITOR:?COMPOSITOR must be set -- baked into the image by scripts/containers/<wm>/Containerfile}"

RUNTIME_DIR="${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR must be set}"
PROXY_DISPLAY="wayland-proxy-0" # fixed name the proxy binds, see src/main.rs

COMPOSITOR_LOG="$(mktemp)"
PROXY_LOG="$(mktemp)"
CLIENT_LOG="$(mktemp)"

COMPOSITOR_PID=""
PROXY_PID=""
CLIENT_PID=""

# Invoked indirectly via `trap cleanup EXIT` below -- shellcheck's
# reachability analysis can't trace that, hence disabling both "never
# invoked" (SC2329) and, on some versions, "unreachable" for the whole
# body (SC2317).
# shellcheck disable=SC2329,SC2317
cleanup() {
    echo ""
    echo "Cleaning up..."
    for pid in "$CLIENT_PID" "$PROXY_PID" "$COMPOSITOR_PID"; do
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
            kill -9 "$pid" 2>/dev/null || true
        fi
    done
    rm -f "$COMPOSITOR_LOG" "$PROXY_LOG" "$CLIENT_LOG"
}
trap cleanup EXIT

fail() {
    echo "FAIL: $1"
    echo "--- compositor log ---"; cat "$COMPOSITOR_LOG"
    echo "--- proxy log ---"; cat "$PROXY_LOG"
    echo "--- client log ---"; cat "$CLIENT_LOG"
    exit 1
}

echo "== Building proxy =="
( cd "$PROJECT_DIR" && cargo build --quiet ) || fail "cargo build failed"

echo "== Starting headless compositor =="
# Snapshot (name, live-pid) pairs before starting, not just names --
# $RUNTIME_DIR is shared with the host and any other running compositor
# (see scripts/setup-env.sh), so it can already hold several wayland-N
# entries, including stale ones a crashed compositor never unlinked.
# Comparing names alone has two distinct failure modes, both confirmed
# live: scanning for "any non-proxy socket" picks up a stale one nothing
# is listening on ("Connection refused" in the proxy); scanning for "any
# name absent from the before-snapshot" misses a *legitimate* new
# compositor that reused a stale name nothing currently holds a lock on
# (wlroots' own auto-selection reuses any unlocked slot, dead or never
# used) -- looked like a hang for a long time before this was found.
# Comparing the full (name, pid) pair catches both: a name is "new" only
# if it's live now and wasn't live with that same pid before.
snapshot_live_sockets() {
    local sock name pid
    for sock in "$RUNTIME_DIR"/wayland-*[0-9]; do
        [[ -e "$sock" ]] || continue
        name="$(basename "$sock")"
        pid="$(fuser "$sock" 2>/dev/null | xargs)"
        [[ -n "$pid" ]] && echo "${name}=${pid}"
    done
}
existing_sockets="$(snapshot_live_sockets)"
case "$COMPOSITOR" in
    labwc)
        WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 \
            labwc -C "$SCRIPT_DIR/containers/labwc/labwc-config" > "$COMPOSITOR_LOG" 2>&1 &
        ;;
    sway)
        WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 \
            sway -c "$SCRIPT_DIR/containers/sway/sway-config" > "$COMPOSITOR_LOG" 2>&1 &
        ;;
    *)
        fail "no launch case for COMPOSITOR='$COMPOSITOR' -- add one here"
        ;;
esac
COMPOSITOR_PID=$!

# 20s budget (80 x 0.25s), not 5s -- confirmed via strace that a slow
# start here isn't necessarily a hang: labwc walks several theme
# directories probing for window-decoration icons (all missing, normal
# fallback behavior) and does real GPU work even in headless mode
# (DRM_IOCTL_AMDGPU_CS -- it still renders via /dev/dri, just doesn't
# scan out to a display), which can take longer than 5s on a busy/shared
# host. Most of what looked like this in practice, though, turned out to
# be a real detection bug (see snapshot_live_sockets above and the
# 2026-07-31 debugging-notes.md entry), not host contention -- keeping
# the generous budget regardless, since the slow-start case above is
# real too, just not the dominant one it first appeared to be.
COMPOSITOR_DISPLAY=""
for _ in $(seq 1 80); do
    while IFS= read -r entry; do
        [[ -z "$entry" ]] && continue
        name="${entry%%=*}"
        [[ "$name" == "$PROXY_DISPLAY" ]] && continue
        if ! grep -qxF "$entry" <<< "$existing_sockets"; then
            COMPOSITOR_DISPLAY="$name"
        fi
    done <<< "$(snapshot_live_sockets)"
    [[ -n "$COMPOSITOR_DISPLAY" ]] && break
    sleep 0.25
done
[[ -n "$COMPOSITOR_DISPLAY" ]] || fail "headless compositor never created a socket"
echo "Compositor socket: $COMPOSITOR_DISPLAY (pid $COMPOSITOR_PID)"

echo "== Starting proxy (-> $COMPOSITOR_DISPLAY) =="
rm -f "$RUNTIME_DIR/$PROXY_DISPLAY"
WAYLAND_DISPLAY="$COMPOSITOR_DISPLAY" "$PROJECT_DIR/target/debug/wayland-proxy" \
    > "$PROXY_LOG" 2>&1 &
PROXY_PID=$!

for _ in $(seq 1 20); do
    [[ -e "$RUNTIME_DIR/$PROXY_DISPLAY" ]] && break
    kill -0 "$PROXY_PID" 2>/dev/null || fail "proxy exited before creating its socket"
    sleep 0.25
done
[[ -e "$RUNTIME_DIR/$PROXY_DISPLAY" ]] || fail "proxy never created $PROXY_DISPLAY"
echo "Proxy socket: $PROXY_DISPLAY (pid $PROXY_PID)"

echo "== Starting client ($CLIENT_APP) through the proxy =="
WAYLAND_DISPLAY="$PROXY_DISPLAY" "$CLIENT_APP" > "$CLIENT_LOG" 2>&1 &
CLIENT_PID=$!

sleep 2
kill -0 "$CLIENT_PID" 2>/dev/null || fail "$CLIENT_APP exited before the crash even happened"
echo "Client is up (pid $CLIENT_PID). Giving it a moment to settle..."
sleep 1

echo "== Crashing the compositor (kill -9 $COMPOSITOR_PID) =="
kill -9 "$COMPOSITOR_PID" 2>/dev/null
wait "$COMPOSITOR_PID" 2>/dev/null
COMPOSITOR_PID=""

sleep 2

echo ""
if kill -0 "$CLIENT_PID" 2>/dev/null; then
    echo "SUCCESS: $CLIENT_APP (pid $CLIENT_PID) survived the compositor crash."
    exit 0
else
    echo "FAIL: $CLIENT_APP did not survive the compositor crash."
    echo "--- proxy log ---"
    cat "$PROXY_LOG"
    echo "--- client log ---"
    cat "$CLIENT_LOG"
    exit 1
fi
