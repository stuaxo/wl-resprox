#!/usr/bin/env bash
# Per-WM smoke test: setup-env.sh -> test-crash.sh --l1 -> diagnose.sh
# --errors-only -> teardown-env.sh, asserting clean at each step. Run
# from the HOST. This is the per-WM unit scripts/test-matrix.sh loops
# over for Phase 10's automated matrix runner.
#
# Deviates from plan-test-harness.md's original Phase 10 sketch
# ("setup-env.sh -> start-guest.sh -> test-crash.sh -> ...") in one way:
# skips start-guest.sh. That script's nested-compositor step needs a
# real, already-running HOST Wayland session (scripts/start-host.sh),
# which an automated/headless matrix run won't have -- and every Phase 9
# container has already been verified exclusively through test-crash.sh's
# own fully self-contained headless path (no host compositor needed at
# all), so that's the flow this reuses rather than forcing a
# host-dependent interactive step into automation it was never built
# for. --l1 (not plain test-crash.sh) because plan-test-harness.md's own
# pass criteria is "L1 minimum -- L0-only doesn't count as verified."
#
# Usage: ./scripts/self-test.sh --wm=<name>
# Exit code: 0 if every step passed, 1 otherwise. Always tears the
# container down on the way out, pass or fail -- a failed run's evidence
# is the output this script already prints (setup/test-crash.sh's own
# FAIL block, diagnose.sh's error lines), not a container left running
# for later inspection; unlike test-crash.sh's run-directory-on-failure
# convention, there's no cheap way to "keep just the interesting part"
# of an entire container.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

WM=""
for arg in "$@"; do
    case "$arg" in
        --wm=*) WM="${arg#--wm=}" ;;
        *) echo "ERROR: unrecognized argument '$arg'" >&2; exit 1 ;;
    esac
done
[[ -n "$WM" ]] || { echo "Usage: $0 --wm=<name>" >&2; exit 1; }

CONTAINER_NAME="wayland-proxy-dev-${WM}"

# Invoked indirectly via `trap teardown EXIT` below -- shellcheck's
# reachability analysis can't trace that, same as test-crash.sh's cleanup().
# shellcheck disable=SC2329,SC2317
teardown() {
    echo "== [$WM] Tearing down =="
    "$SCRIPT_DIR/teardown-env.sh" --wm="$WM"
}
trap teardown EXIT

echo "== [$WM] Building and starting container =="
if ! "$SCRIPT_DIR/setup-env.sh" --wm="$WM"; then
    echo "FAIL [$WM]: setup-env.sh"
    exit 1
fi

echo "== [$WM] Running test-crash.sh --l1 =="
if ! sudo podman exec --user dev -e XDG_RUNTIME_DIR=/run/user/1000 "$CONTAINER_NAME" \
    bash -c 'cd /workspace && bash test-crash.sh --l1'; then
    echo "FAIL [$WM]: test-crash.sh --l1"
    exit 1
fi

echo "== [$WM] Running diagnose.sh --errors-only =="
if ! "$SCRIPT_DIR/diagnose.sh" --errors-only "$CONTAINER_NAME"; then
    echo "FAIL [$WM]: diagnose.sh --errors-only"
    exit 1
fi

echo "SUCCESS [$WM]: setup + L1 crash/reconnect + diagnose all clean."
exit 0
