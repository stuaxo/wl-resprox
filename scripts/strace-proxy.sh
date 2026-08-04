#!/usr/bin/env bash
set -euo pipefail

# Traces a systemd --user managed wayland-proxy unit's own sendmsg/recvmsg
# syscalls -- fd/SCM_RIGHTS ground truth for "what did we actually put on
# the wire," independent of what our own Rust code believes it did.
# Built chasing the "invalid arguments for wl_shm#N.create_pool" bug (see
# docs/adr/adr-0006-recreate-buffers-via-fd-handover.md's "Open issue"
# section and the matching 2026-08-04 entry in docs/debugging-notes.md)
# -- kept as a permanent tool since the same two obstacles below will
# recur for any future "what did the proxy actually send/receive"
# question, not just this one bug.
#
# Two real obstacles this works around:
#
# 1. yama ptrace_scope=1 (this distro's default) refuses `strace -p
#    <pid>` against an unrelated systemd-started process, even same-user
#    -- confirmed live: `ptrace(PTRACE_SEIZE, ...)`: Operation not
#    permitted. There's no way around this short of root/sudo. Attaching
#    strace as the *parent* (launching the exact same ExecStart command
#    under strace, instead of attaching to the systemd-started one)
#    sidesteps the restriction entirely -- so this script stops the
#    systemd unit and launches its own copy under strace in its place.
#
# 2. If whatever you're reproducing involves an actual compositor crash
#    (pkill -9 gnome-shell), the session wrapper script will notice the
#    unit is inactive (since this script just stopped it) and restart
#    it -- a SECOND, unstraced proxy ends up listening on the same
#    public socket alongside this one. This does NOT invalidate a
#    capture for an already-connected test client: an existing
#    connection (accepted before the second instance starts) keeps
#    talking to THIS straced process regardless of who owns the
#    *listening* socket afterward -- confirmed live by cross-referencing
#    the traced process's own stdout log against the strace output, same
#    client pid throughout. A client started fresh AFTER the crash,
#    though, may go through the untraced second instance instead -- if
#    that matters, start your test client BEFORE crashing, not after.
#
# Always restarts the real systemd unit on exit (trap, fires even on
# Ctrl-C), so a debugging session never accidentally leaves the session
# on an unmanaged proxy with no SIGUSR1 rebind reachable for the next
# *real* crash.
#
# Usage: scripts/strace-proxy.sh [unit-name] [output-file]
#   unit-name   defaults to wayland-proxy-gnome-shell-direct.service
#   output-file defaults to /tmp/wayland-proxy-strace-<timestamp>.log
#
# Then, in another terminal: reproduce whatever you're chasing, Ctrl-C
# this script when done, and grep the output file. A good starting grep
# for "was an fd actually attached to this specific message": search for
# the message's own distinctive byte sequence (e.g. a size argument in
# hex) and check whether that sendmsg's msg_control shows
# cmsg_type=SCM_RIGHTS or msg_controllen=0.

UNIT="${1:-wayland-proxy-gnome-shell-direct.service}"
OUT="${2:-/tmp/wayland-proxy-strace-$(date +%s).log}"

EXEC_START=$(systemctl --user show -p ExecStart "$UNIT" | sed -n 's/.*argv\[\]=\(.*\) ; ignore_errors.*/\1/p')
if [ -z "$EXEC_START" ]; then
    echo "couldn't extract ExecStart from $UNIT -- is the unit name right? (systemctl --user status $UNIT)" >&2
    exit 1
fi

echo "unit:   $UNIT"
echo "cmd:    $EXEC_START"
echo "output: $OUT"
echo

cleanup() {
    echo
    # strace's tracee can survive its tracer dying/being killed abruptly
    # (confirmed live in a smoke test: SIGKILLing just strace left the
    # traced proxy running on, orphaned and un-managed, fighting the
    # freshly systemctl-restarted one for the same public socket).
    # Explicit, unconditional cleanup by PID -- not just relying on
    # signal propagation to strace's children -- so this is safe
    # regardless of exactly how the script was terminated (a real
    # terminal's Ctrl-C sends SIGINT to the whole foreground process
    # group and would clean this up on its own, but `kill <this
    # script's pid>` from anywhere else, e.g. another terminal or tool,
    # does NOT reach strace or the proxy at all -- confirmed live both
    # ways).
    if [ -n "${STRACE_PID:-}" ] && kill -0 "$STRACE_PID" 2>/dev/null; then
        echo "stopping strace (pid $STRACE_PID)..."
        kill -KILL "$STRACE_PID" 2>/dev/null || true
    fi
    if [ -n "${PROXY_PID:-}" ] && kill -0 "$PROXY_PID" 2>/dev/null; then
        echo "stopping the traced proxy (pid $PROXY_PID)..."
        kill -KILL "$PROXY_PID" 2>/dev/null || true
    fi
    echo "restarting $UNIT to restore normal (non-traced) crash-resilience..."
    systemctl --user restart "$UNIT"
}
trap cleanup EXIT

systemctl --user stop "$UNIT"

echo "starting under strace -- reproduce your scenario now, then Ctrl-C this script."
# Deliberately NOT `exec`: an exec'd strace REPLACES this shell's own
# process image, and bash traps do not survive exec -- the cleanup trap
# above would simply never fire when strace is killed/Ctrl-C'd, silently
# leaving the session on an unmanaged proxy. Caught this the hard way, in
# a smoke test, before it ever shipped: `kill 88918` (strace's pid) on an
# earlier exec'd version left both strace and the traced proxy running
# untouched. Backgrounding it (rather than a plain foreground command)
# instead is what makes $! available to capture below, needed for the
# explicit cleanup above -- it does NOT change how a real terminal's
# Ctrl-C behaves (bash without `set -m`, the default in a plain script,
# keeps a `&`-started job in the SAME process group as the script
# itself, so the terminal's own SIGINT still reaches everything at once
# exactly as if this were a single foreground command).
# shellcheck disable=SC2086
strace -f -tt -yy -v -s 400 -e trace=sendmsg,recvmsg -o "$OUT" $EXEC_START &
STRACE_PID=$!
# The proxy is strace's traced child, not this script's direct child --
# $! only ever gives strace's own pid -- so find it once strace has had
# a moment to actually exec it, purely as a best-effort belt-and-braces
# extra for cleanup() above (killing $STRACE_PID alone is normally
# enough, since SIGKILLing strace also delivers PTRACE_O_EXITKILL to a
# process it's actively tracing in most configurations -- this is for
# the cases where it doesn't).
sleep 0.3
PROXY_PID=$(pgrep -P "$STRACE_PID" | head -1 || true)
wait "$STRACE_PID"
