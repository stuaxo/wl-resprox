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
# testing-levels picture and why this stays a narrow L0 check by default.
#
# Pass --l1 to go further: after confirming the client survives the
# crash, restarts the SAME compositor on the same freed socket and
# additionally asserts protocol-level recovery from the proxy's own log
# -- zero "unresolvable interface" warnings (an interfaces.rs gap, see
# architecture-notes.md) and a "recreated xdg_toplevel" line (proof the
# reconnect actually recreated the toplevel chain, not just that the
# client process happens to still be alive). This is the exact
# RUST_LOG=debug-and-grep check that's been run by hand for every Phase 9
# container and swap pair so far (see docs/debugging-notes.md); --l1
# automates it as the per-WM unit Phase 10's matrix runner needs, since
# "L0-only doesn't count as verified" per plan-test-harness.md. Default
# (no --l1) behavior is unchanged, so existing "L0 pass (N/N)" results
# elsewhere in this project's docs stay meaningful as exactly what they
# say -- L0 only.
#
# Every pid/socket this script starts is also recorded via
# run-registry.sh, under $XDG_RUNTIME_DIR/wayland-proxy-runs/<run-id>/
# (printed at startup, also reachable via that directory's `current`
# symlink) -- `cat` the `chain` file there for a human-readable log of
# what belongs to what, or point diagnose.sh at it. Not required reading
# to use this script; it exists for when something gets stuck and the
# usual `ps`/`fuser` archaeology isn't enough.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
CLIENT_APP="gtk4-demo"
CHECK_L1=false
for arg in "$@"; do
    case "$arg" in
        --l1) CHECK_L1=true ;;
        *) CLIENT_APP="$arg" ;;
    esac
done
COMPOSITOR="${COMPOSITOR:?COMPOSITOR must be set -- baked into the image by scripts/containers/<wm>/Containerfile}"

RUNTIME_DIR="${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR must be set}"
PROXY_DISPLAY="wayland-proxy-0" # fixed name the proxy binds, see src/main.rs

# shellcheck source=./run-registry.sh disable=SC1091
source "$SCRIPT_DIR/run-registry.sh"
# shellcheck source=./compositor-launch.sh disable=SC1091
source "$SCRIPT_DIR/compositor-launch.sh"
# shellcheck source=./socket-wait.sh disable=SC1091
source "$SCRIPT_DIR/socket-wait.sh"
run_dir_init

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
    local exit_status=$? # must be captured before any other command runs
    echo ""
    echo "Cleaning up..."
    run_cleanup
    rm -f "$COMPOSITOR_LOG" "$PROXY_LOG" "$CLIENT_LOG"
    # Only discard the run directory on a clean pass -- on failure, keep
    # it (chain file + per-role pid/container/socket records) for
    # postmortem inspection instead of erasing the one thing this exists
    # for right when it'd actually be useful.
    if [[ "$exit_status" -eq 0 ]]; then
        rm -rf "$RUN_DIR"
    else
        echo "Run directory kept for inspection: $RUN_DIR"
    fi
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
# 20s budget (80 x 0.25s, see socket-wait.sh), not 5s -- confirmed via
# strace that a slow start here isn't necessarily a hang: labwc walks
# several theme directories probing for window-decoration icons (all
# missing, normal fallback behavior) and does real GPU work even in
# headless mode (DRM_IOCTL_AMDGPU_CS -- it still renders via /dev/dri,
# just doesn't scan out to a display), which can take longer than 5s on
# a busy/shared host. Most of what looked like this in practice, though,
# turned out to be a real detection bug (see socket-wait.sh and the
# 2026-07-31 debugging-notes.md entry), not host contention -- keeping
# the generous budget regardless, since the slow-start case above is
# real too, just not the dominant one it first appeared to be.
existing_sockets="$(snapshot_live_sockets)"
launch_compositor "$COMPOSITOR" "$COMPOSITOR_LOG" || fail "no launch case for COMPOSITOR='$COMPOSITOR' -- add one to scripts/compositor-launch.sh"
COMPOSITOR_DISPLAY="$(wait_for_new_socket "$existing_sockets")"
[[ -n "$COMPOSITOR_DISPLAY" ]] || fail "headless compositor never created a socket"
run_link_socket compositor "$RUNTIME_DIR/$COMPOSITOR_DISPLAY"
echo "Compositor socket: $COMPOSITOR_DISPLAY (pid $COMPOSITOR_PID)"

echo "== Starting proxy (-> $COMPOSITOR_DISPLAY) =="
rm -f "$RUNTIME_DIR/$PROXY_DISPLAY"
WAYLAND_DISPLAY="$COMPOSITOR_DISPLAY" "$PROJECT_DIR/target/debug/wayland-proxy" \
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
echo "Proxy socket: $PROXY_DISPLAY (pid $PROXY_PID)"

echo "== Starting client ($CLIENT_APP) through the proxy =="
WAYLAND_DISPLAY="$PROXY_DISPLAY" "$CLIENT_APP" > "$CLIENT_LOG" 2>&1 &
CLIENT_PID=$!
run_track client "$CLIENT_PID"

sleep 2
kill -0 "$CLIENT_PID" 2>/dev/null || fail "$CLIENT_APP exited before the crash even happened"
echo "Client is up (pid $CLIENT_PID). Giving it a moment to settle..."
sleep 1

echo "== Crashing the compositor (kill -9 $COMPOSITOR_PID) =="
kill -9 "$COMPOSITOR_PID" 2>/dev/null
wait "$COMPOSITOR_PID" 2>/dev/null
COMPOSITOR_PID=""

sleep 2

if [[ "$CHECK_L1" != true ]]; then
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
fi

kill -0 "$CLIENT_PID" 2>/dev/null || fail "$CLIENT_APP did not survive the compositor crash (never even reached the reconnect attempt)"

echo "== --l1: restarting $COMPOSITOR on the same socket ($COMPOSITOR_DISPLAY) =="
existing_sockets="$(snapshot_live_sockets)"
launch_compositor "$COMPOSITOR" "$COMPOSITOR_LOG" "compositor-restarted" || fail "no launch case for COMPOSITOR='$COMPOSITOR'"
RESTARTED_DISPLAY="$(wait_for_new_socket "$existing_sockets")"
[[ -n "$RESTARTED_DISPLAY" ]] || fail "restarted $COMPOSITOR never created a socket"
run_link_socket compositor-restarted "$RUNTIME_DIR/$RESTARTED_DISPLAY"
if [[ "$RESTARTED_DISPLAY" != "$COMPOSITOR_DISPLAY" ]]; then
    fail "restarted $COMPOSITOR landed on $RESTARTED_DISPLAY, not the freed $COMPOSITOR_DISPLAY -- the proxy is still pointed at $COMPOSITOR_DISPLAY and will never see it"
fi

sleep 2

echo ""
kill -0 "$CLIENT_PID" 2>/dev/null || fail "$CLIENT_APP did not survive the $COMPOSITOR restart"
# gtk_shell1 is a confirmed-safe, permanent Gap 2 (GNOME/GTK-internal
# protocol, not generated by any dependency crate this project draws
# from -- see interfaces.rs's own module doc and architecture-notes.md's
# Gap 2 list). Excluded here rather than treated as a failure; any OTHER
# unresolvable interface is a real gap. Keep this exclusion list and
# architecture-notes.md's Gap 2 list in sync if a second permanent one
# is ever confirmed.
if grep "unresolvable interface" "$PROXY_LOG" | grep -qv 'unresolvable interface Some("gtk_shell1")'; then
    fail "proxy log contains unresolvable-interface warnings -- likely a new interfaces.rs gap (see architecture-notes.md's Gap 1/Gap 2), grep the log for the specific interface name"
fi
if ! grep -q "recreated xdg_toplevel" "$PROXY_LOG"; then
    fail "proxy log never shows the toplevel chain being recreated after reconnect -- L1 requires protocol-level recovery, not just process survival"
fi

echo "SUCCESS: $CLIENT_APP (pid $CLIENT_PID) survived the $COMPOSITOR crash+restart -- zero unresolvable-interface warnings, toplevel chain recreated (L1 verified)."
exit 0
