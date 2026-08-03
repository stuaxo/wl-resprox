#!/bin/sh
# Exec= target for wl-res-gnome-shell-direct.desktop -- the INTERIM
# crash-resilient GNOME session, mirroring wl-res-labwc-session-wrapper.sh's
# architecture directly: this script IS gdm-wayland-session's own child,
# running gnome-shell as ITS direct child in a plain restart loop -- no
# systemd Restart=on-failure, no OnFailure=, and critically, no
# gnome-session/gnome-session-manager@.service at all.
#
# Why this exists (see plan-desktop-resilience.md and
# docs/adr/adr-0004-*.md for the full reasoning): three real, confirmed
# crash tests against wl-res-labwc (which uses this exact architecture)
# all recovered fully in ~1s, session never left the seat. Four real
# crash tests against wl-res-gnome-shell (which goes through
# gnome-session, itself launched via a systemd --user unit) only
# recovered in-place once, in ~28s, and fell back to a full session
# teardown the other three times -- racing gnome-session's OWN
# OnFailure= machinery, which our static drop-in override could not
# reliably defeat (see plan-desktop-resilience.md's own findings on
# that). This session tests whether the SAME direct-child architecture
# that fixed labwc also fixes gnome-shell, by removing gnome-session
# from the picture entirely.
#
# Deliberately NOT the long-term answer -- see the ADR. gnome-session
# also orchestrates other session components (settings daemon pieces,
# portals, keyring, indicator services) that gnome-shell itself does not
# start; this session will be missing some or all of that polish.
# wl-res-gnome-shell (the gnome-session-based one) stays installed
# alongside this, specifically so gnome-session's own crash-handling
# behavior can keep being investigated for a real long-term fix that
# doesn't require bypassing it.
#
# Unlike labwc, no socket-discovery/symlink dance is needed here --
# gnome-shell supports --wayland-display=<name> directly, so its private
# socket name can be pinned upfront, same as the gnome-session-based
# wl-res-gnome-shell session already does. No LIBSEAT_BACKEND=logind
# either -- mutter talks to logind directly via libsystemd, never via
# libseat, and this was never actually broken for gnome-shell (it got
# DRM access fine even via the old systemd-unit approach; the problem
# there was GDM's activation handoff and gnome-session's OnFailure=
# racing, not seat/DRM access).
#
# Logs to $XDG_RUNTIME_DIR/wl-res-gnome-shell-direct-wrapper.log, same
# reasoning as the labwc wrapper's own header comment.
set -u

RUNTIME_DIR="${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR must be set}"
LOG="$RUNTIME_DIR/wl-res-gnome-shell-direct-wrapper.log"
exec >>"$LOG" 2>&1

PRIVATE_DISPLAY=wl-res-gnome-shell-direct-0

echo "$(date -Iseconds) wrapper starting"

# Periodically re-assert WAYLAND_DISPLAY/XDG_SESSION_DESKTOP in the
# background, for this whole script's lifetime -- same defensive pattern
# as the labwc wrapper (found there that Xwayland's own lazy, delayed
# activation-environment export can clobber a one-shot export well after
# the fact). Untested whether gnome-shell itself does anything
# equivalent when run this way (it might, given it has its own D-Bus
# session-registration code) -- kept as a cheap, harmless safeguard
# either way rather than assuming it's unnecessary.
(
    while :; do
        sleep 3
        dbus-update-activation-environment --systemd WAYLAND_DISPLAY=wayland-0 XDG_SESSION_DESKTOP=wl-res-gnome-shell-direct 2>/dev/null
    done
) &

while :; do
    gnome-shell --mode=ubuntu --wayland-display="$PRIVATE_DISPLAY" &
    SHELL_PID=$!
    echo "$(date -Iseconds) gnome-shell started, pid=$SHELL_PID"

    dbus-update-activation-environment --systemd WAYLAND_DISPLAY=wayland-0 XDG_SESSION_DESKTOP=wl-res-gnome-shell-direct
    systemctl --user start wayland-proxy-gnome-shell-direct.service

    wait "$SHELL_PID"
    echo "$(date -Iseconds) gnome-shell exited (pid=$SHELL_PID) -- restarting in 1s"
    sleep 1
done
