#!/usr/bin/env bash
# Reproduces the GDM-greeter-has-no-mouse-pointer bug documented in
# docs/KNOWN_BUGS.md: ends a graphical session via `loginctl
# terminate-session` (rather than a full reboot) and leaves the next
# greeter with no visible cursor.
#
# DESTRUCTIVE: this actually logs the target session out. Requires
# --yes. Run from a session that will SURVIVE the logout (a plain TTY
# login, an SSH session, tmux detached from the graphical session --
# not from a terminal running inside the session being terminated,
# or this script kills its own controlling terminal mid-run).
#
# This does NOT self-verify -- checking for the cursor needs eyes on
# the physical console or a VNC/screenshot view of it afterward.
#
# Usage: repro-logout-loses-mouse-pointer.sh --yes [session-id]
# Without a session-id, targets the first seat0 session of class
# "user" loginctl reports -- verify that's the right one on a
# multi-session machine before trusting the default.
set -euo pipefail

if [[ "${1:-}" != "--yes" ]]; then
    echo "Refusing to run without --yes -- this logs out a real session." >&2
    echo "Usage: $0 --yes [session-id]" >&2
    exit 1
fi
shift

SESSION_ID="${1:-}"
if [[ -z "$SESSION_ID" ]]; then
    SESSION_ID="$(loginctl list-sessions --no-legend | awk '$4=="seat0" && $6=="user"{print $1; exit}')"
fi
if [[ -z "$SESSION_ID" ]]; then
    echo "ERROR: couldn't auto-detect a seat0/user session -- pass one explicitly (see: loginctl list-sessions)." >&2
    exit 1
fi

echo "Terminating session $SESSION_ID..."
loginctl terminate-session "$SESSION_ID"

echo "$(date -Iseconds) sent. A fresh GDM greeter should appear shortly."
echo "Check it now (physically, or via VNC/screenshot): is there a visible mouse cursor?"
echo "Known bug: there often isn't one, even though the greeter otherwise responds to input."
