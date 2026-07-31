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
# forward the error" rule (docs/implementation-constraints.md) holds. A
# passing run today would be surprising -- crash recovery isn't
# implemented yet -- this harness exists so that work has something to
# iterate against.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
CLIENT_APP="${1:-gtk4-demo}"

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
# Snapshot sockets that already exist before starting, and only accept a
# name that's genuinely new -- $RUNTIME_DIR is shared with the host and
# any other running compositor (see scripts/setup-env.sh), so it can
# already hold several wayland-N entries, including stale ones a crashed
# compositor never unlinked. Scanning for "any non-proxy socket" (the
# previous approach) picked up exactly one of those live, and the proxy
# then failed with "Connection refused" trying to reach a dead socket.
existing_sockets="$(ls "$RUNTIME_DIR"/wayland-*[0-9] 2>/dev/null || true)"
WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 labwc -C /dev/null > "$COMPOSITOR_LOG" 2>&1 &
COMPOSITOR_PID=$!

# 20s budget (80 x 0.25s), not 5s -- confirmed via strace that a slow
# start here isn't a hang: labwc walks several theme directories probing
# for window-decoration icons (all missing, normal fallback behavior) and
# does real GPU work even in headless mode (DRM_IOCTL_AMDGPU_CS -- it
# still renders via /dev/dri, just doesn't scan out to a display). On a
# busy/shared host -- confirmed live: other labwc/Xwayland instances
# entirely outside this container, competing for the same GPU and
# XWayland X-display-number slots -- that startup can take longer than
# 5s, which isn't a fault of anything here, just contention.
COMPOSITOR_DISPLAY=""
for _ in $(seq 1 80); do
    for sock in "$RUNTIME_DIR"/wayland-*[0-9]; do
        [[ -e "$sock" ]] || continue
        name="$(basename "$sock")"
        if [[ "$name" != "$PROXY_DISPLAY" ]] && ! grep -qxF "$sock" <<< "$existing_sockets"; then
            COMPOSITOR_DISPLAY="$name"
        fi
    done
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
