#!/usr/bin/env bash
# Launches a real GTK client against the live wl-res-gnome-shell-direct
# session on this machine (not a headless container -- see test-crash.sh
# for that tier), crashes the real gnome-shell, waits for recovery, and
# prints ONE compact summary instead of raw log dumps.
#
# Built chasing ADR-0008's dmabuf client-wedge bug: the first live
# investigation of that (2026-08-04) needed several separate manual
# journalctl/proc greps per run, and the proxy's own `wp_presentation
# .feedback ... dropping` warning alone can repeat 1,000+ times per
# crash (see ADR-0006's "Open issue" section) -- pasting that raw into an
# LLM's context is pure noise per line after the first. This collapses
# that class of repeated warning to one counted line per distinct
# message shape, and scopes mutter's own CRITICAL lines to only the
# freshly-restarted gnome-shell pid instead of grepping the wrapper log's
# entire multi-session history (which stretches back hours and includes
# unrelated crash-test runs).
#
# Usage:
#   scripts/live-crash-test.sh python3 scripts/gtk/dmabuf_gl.py
#   scripts/live-crash-test.sh python3 scripts/gtk/basic_shm.py
#
# Env overrides (defaults match the wl-res-gnome-shell-direct session):
#   WAYLAND_DISPLAY      proxy's public socket name (default: wayland-0)
#   COMPOSITOR_MATCH     pgrep -f pattern for the compositor process
#                        (default: "gnome-shell --mode")
#   SETTLE_SECONDS       wait after launch before crashing (default: 3)
#   RECOVERY_TIMEOUT     max seconds to wait for a new compositor pid and
#                        the proxy's own recovery burst (default: 20)
#   KEEP_CLIENT_RUNNING  if "1", don't kill the client at exit -- for
#                        manually poking at it afterward (default: unset)
#
# Leaves full raw logs on disk (paths printed at the end) -- the compact
# summary is deliberately lossy; go to the raw files for anything this
# doesn't surface.
set -uo pipefail

if [[ $# -eq 0 ]]; then
    echo "usage: $0 <client command...>" >&2
    exit 2
fi

WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-0}"
COMPOSITOR_MATCH="${COMPOSITOR_MATCH:-gnome-shell --mode}"
SETTLE_SECONDS="${SETTLE_SECONDS:-3}"
RECOVERY_TIMEOUT="${RECOVERY_TIMEOUT:-20}"
RUNTIME_DIR="${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR must be set}"

CLIENT_LOG="$(mktemp /tmp/live-crash-test.client.XXXXXX.log)"

# The proxy unit's own journal is the source of truth for what it did;
# there's exactly one running at a time in this session (the other two
# session units Conflicts= against it -- see packaging/*.service).
PROXY_UNIT="$(systemctl --user list-units --state=running --no-legend 'wayland-proxy*' | awk '{print $1}' | head -1)"
[[ -n "$PROXY_UNIT" ]] || { echo "no running wayland-proxy-*.service found -- is a session up?" >&2; exit 1; }

# Plain-text log, not journald -- gnome-shell itself isn't a systemd
# unit in this session (see the wrapper script's own header), so its
# stderr (mutter's CRITICAL/WARNING lines) only exists here.
WRAPPER_LOG="$(ls -t "$RUNTIME_DIR"/*-wrapper.log 2>/dev/null | head -1)"
[[ -n "$WRAPPER_LOG" ]] || { echo "no *-wrapper.log found in $RUNTIME_DIR" >&2; exit 1; }
wrapper_lines_before="$(wc -l < "$WRAPPER_LOG")"

old_compositor_pid="$(pgrep -f "$COMPOSITOR_MATCH" | head -1)"
[[ -n "$old_compositor_pid" ]] || { echo "no process matching '$COMPOSITOR_MATCH' -- is gnome-shell running?" >&2; exit 1; }

echo "== Launching: $* =="
test_start="$(date -Iseconds)"
WAYLAND_DISPLAY="$WAYLAND_DISPLAY" nohup "$@" >"$CLIENT_LOG" 2>&1 &
CLIENT_PID=$!

cleanup() {
    if [[ "${KEEP_CLIENT_RUNNING:-}" != "1" ]]; then
        kill -9 "$CLIENT_PID" 2>/dev/null
    fi
}
trap cleanup EXIT

sleep "$SETTLE_SECONDS"
kill -0 "$CLIENT_PID" 2>/dev/null || { echo "FAIL: client (pid $CLIENT_PID) exited before the crash -- see $CLIENT_LOG" >&2; exit 1; }
echo "Client up (pid $CLIENT_PID), settled ${SETTLE_SECONDS}s. Crashing compositor (pid $old_compositor_pid)..."

kill -9 "$old_compositor_pid" 2>/dev/null

echo "== Waiting for a new compositor + proxy recovery (up to ${RECOVERY_TIMEOUT}s) =="
new_compositor_pid=""
deadline=$((SECONDS + RECOVERY_TIMEOUT))
while (( SECONDS < deadline )); do
    candidate="$(pgrep -f "$COMPOSITOR_MATCH" | head -1)"
    if [[ -n "$candidate" && "$candidate" != "$old_compositor_pid" ]]; then
        new_compositor_pid="$candidate"
        break
    fi
    sleep 0.5
done
[[ -n "$new_compositor_pid" ]] || echo "WARNING: no new compositor pid seen within ${RECOVERY_TIMEOUT}s -- session may still be restarting"

recovered=false
while (( SECONDS < deadline )); do
    if journalctl --user -u "$PROXY_UNIT" --since "$test_start" --no-pager 2>/dev/null \
        | grep -q "pid=$CLIENT_PID}: wayland_proxy: connection unfrozen"; then
        recovered=true
        break
    fi
    sleep 0.5
done
$recovered || echo "WARNING: proxy log never showed 'connection unfrozen' for this client within ${RECOVERY_TIMEOUT}s"

# Give the client a moment post-recovery before judging its pulse --
# ADR-0008's basic_shm.py run took ~5s to resume ticking after a clean
# recovery, so sampling immediately at t+0 would misreport a healthy
# client as hung.
sleep 3

echo ""
echo "=================== SUMMARY ==================="

echo ""
echo "--- proxy: structural events for pid=$CLIENT_PID (recreated/synthesized/errors) ---"
journalctl --user -u "$PROXY_UNIT" --since "$test_start" --no-pager 2>/dev/null \
    | grep "pid=$CLIENT_PID}" \
    | grep -E "recreated|synthesized|connection (lost|reconnected|unfrozen)|COMPOSITOR ERROR|invalid arguments" \
    || echo "(none found)"

echo ""
echo "--- proxy: dropped/warning messages for pid=$CLIENT_PID, collapsed by template ---"
journalctl --user -u "$PROXY_UNIT" --since "$test_start" --no-pager 2>/dev/null \
    | grep "pid=$CLIENT_PID}" | grep "WARN" \
    | sed -E 's/[0-9]+/N/g; s/\(bytes=[0-9a-f]*\)/(bytes=...)/' \
    | sort | uniq -c | sort -rn \
    || echo "(none found)"

echo ""
echo "--- mutter stderr: CRITICAL/WARNING lines from the NEW gnome-shell (pid=$new_compositor_pid) only ---"
if [[ -n "$new_compositor_pid" ]]; then
    tail -n +"$((wrapper_lines_before + 1))" "$WRAPPER_LOG" \
        | grep -E "CRITICAL|WARNING" | grep "gnome-shell:$new_compositor_pid" \
        || echo "(none)"
else
    echo "(skipped -- no new compositor pid captured)"
fi

echo ""
echo "--- client pulse (pid=$CLIENT_PID) ---"
if ! kill -0 "$CLIENT_PID" 2>/dev/null; then
    echo "VERDICT: client process is gone (crashed or exited) -- see $CLIENT_LOG"
else
    last_log_time="$(date -r "$CLIENT_LOG" +%s 2>/dev/null || echo 0)"
    now="$(date +%s)"
    log_age=$((now - last_log_time))

    # Resample the main thread's wait channel a few times -- a genuinely
    # alive main loop (even one just waiting on frame callbacks) should
    # show SOME variation over a couple of seconds; a wedged one (see
    # ADR-0008) sits in the exact same syscall every time.
    wchans=""
    for _ in 1 2 3 4 5; do
        w="$(cat "/proc/$CLIENT_PID/task/$CLIENT_PID/wchan" 2>/dev/null || echo "?")"
        wchans="$wchans $w"
        sleep 0.4
    done
    stuck=true
    first_wchan="$(echo "$wchans" | awk '{print $1}')"
    for w in $wchans; do [[ "$w" == "$first_wchan" ]] || stuck=false; done

    grep -qE "GIVING UP|STALLED" "$CLIENT_LOG" && stall_logged=true || stall_logged=false

    echo "last log line age: ${log_age}s"
    echo "main-thread wchan samples:$wchans"
    if $stall_logged; then
        echo "VERDICT: client's OWN stall detector fired -- see $CLIENT_LOG for STALLED/GIVING UP lines"
    elif $stuck && (( log_age > 10 )); then
        echo "VERDICT: possibly HUNG -- main thread stuck in '$first_wchan' across all samples, no log output in ${log_age}s"
    else
        echo "VERDICT: appears alive (log age ${log_age}s, wchan varied or recent)"
    fi
fi

echo ""
echo "================================================="
echo "Raw logs: client=$CLIENT_LOG  wrapper=$WRAPPER_LOG  proxy unit=$PROXY_UNIT (journalctl --user -u $PROXY_UNIT --since '$test_start')"
if [[ "${KEEP_CLIENT_RUNNING:-}" == "1" ]]; then
    echo "KEEP_CLIENT_RUNNING=1 -- client pid $CLIENT_PID left running for manual inspection."
fi
exit 0
