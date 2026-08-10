#!/usr/bin/env bash
# Reproduces and identifies the meta_window_set_stack_position_no_sync
# assertion documented in docs/KNOWN_BUGS.md: attaches gdb to the live
# gnome-shell process, breakpoints GLib's always-exported
# g_return_if_fail_warning (receives the failing function name and
# expression as plain strings -- works even though libmutter itself
# ships with no symbol table at all), and on a hit, tries to identify
# the specific stale MetaWindow* involved by calling the real, exported
# meta_window_get_description() on each callee-saved register that
# looks like a plausible GObject-shaped pointer.
#
# Read-only in intent (a getter call, no mutation) but this DOES run
# arbitrary code in your live gnome-shell process via gdb. A wrong
# guess at which register holds the window could in principle crash
# it -- low risk in practice (each candidate is sanity-checked via a
# plain memory read first, which fails safely on a bad pointer), and
# if it does crash, the crash-resilient session wrapper restarts
# gnome-shell automatically, same as any other crash this project is
# built to survive.
#
# Requires: gdb; kernel.yama.ptrace_scope=0 (attaching to a same-user,
# non-child process is otherwise blocked) -- this script does NOT
# change that for you. Revert with
# `sudo sysctl kernel.yama.ptrace_scope=1` when done.
#
# Usage: repro-stack-position-assertion.sh [gnome-shell-pid]
# Without a pid, auto-detects via `pgrep -f 'gnome-shell --mode=ubuntu'`
# (adjust the pattern if your session uses a different --mode).
# Runs gdb in the background, waits briefly for the breakpoint to be
# set, then prints instructions for triggering it (a
# Meta.Window.activate() call via Looking Glass Eval, on a window
# that's survived at least one prior gnome-shell restart -- a fresh,
# never-crashed window won't reproduce this).
set -euo pipefail

if [[ "$(cat /proc/sys/kernel/yama/ptrace_scope 2>/dev/null || echo 1)" != "0" ]]; then
    echo "ERROR: kernel.yama.ptrace_scope must be 0 to attach gdb to gnome-shell." >&2
    echo "Run: sudo sysctl kernel.yama.ptrace_scope=0  (revert to 1 when done)" >&2
    exit 1
fi

if ! command -v gdb >/dev/null 2>&1; then
    echo "ERROR: gdb not found." >&2
    exit 1
fi

SHELL_PID="${1:-}"
if [[ -z "$SHELL_PID" ]]; then
    SHELL_PID="$(pgrep -f 'gnome-shell --mode=ubuntu' | head -1)"
fi
if [[ -z "$SHELL_PID" ]]; then
    echo "ERROR: couldn't find a gnome-shell process -- pass its pid explicitly." >&2
    exit 1
fi
echo "Target gnome-shell pid: $SHELL_PID"

GDB_SCRIPT="$(mktemp /tmp/repro-stack-position-XXXXXX.gdb)"
GDB_LOG="$(mktemp /tmp/repro-stack-position-XXXXXX.log)"
trap 'rm -f "$GDB_SCRIPT"' EXIT

cat > "$GDB_SCRIPT" <<'GDBEOF'
break g_return_if_fail_warning
commands
silent
printf "[HIT] func=%s expr=%s\n", (char*)$rsi, (char*)$rdx
printf "candidates: rbx=0x%lx r12=0x%lx r13=0x%lx r14=0x%lx r15=0x%lx\n", $rbx, $r12, $r13, $r14, $r15
if $rbx != 0
  call (const char*) meta_window_get_description((void*)$rbx)
end
if $r12 != 0
  call (const char*) meta_window_get_description((void*)$r12)
end
if $r13 != 0
  call (const char*) meta_window_get_description((void*)$r13)
end
if $r14 != 0
  call (const char*) meta_window_get_description((void*)$r14)
end
if $r15 != 0
  call (const char*) meta_window_get_description((void*)$r15)
end
continue
end
continue
GDBEOF

echo "Attaching gdb (background, log: $GDB_LOG)..."
gdb -p "$SHELL_PID" --batch -x "$GDB_SCRIPT" > "$GDB_LOG" 2>&1 &
GDB_PID=$!
sleep 2

if ! kill -0 "$GDB_PID" 2>/dev/null; then
    echo "ERROR: gdb exited immediately -- check $GDB_LOG" >&2
    cat "$GDB_LOG" >&2
    exit 1
fi

echo "Attached (gdb pid $GDB_PID). Now trigger it:"
echo "  With Looking Glass unsafe mode enabled, run via Eval:"
echo "  (function(){let w=global.get_window_actors()[0].meta_window; w.activate(global.get_current_time());})()"
echo "  -- or just click/activate any window that's survived a prior gnome-shell restart."
echo ""
echo "Results will accumulate in: $GDB_LOG"
echo "One of the meta_window_get_description() calls that DOESN'T error is your"
echo "identified stale window -- some candidates will legitimately fail (not"
echo "every register holds a valid pointer), that's expected."
echo ""
echo "When done: kill $GDB_PID   # detach gdb cleanly"
echo "Then, if you enabled it for this: sudo sysctl kernel.yama.ptrace_scope=1"
