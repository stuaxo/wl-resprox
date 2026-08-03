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
# No LIBSEAT_BACKEND=logind -- mutter talks to logind directly via
# libsystemd, never via libseat, and this was never actually broken for
# gnome-shell (it got DRM access fine even via the old systemd-unit
# approach; the problem there was GDM's activation handoff and
# gnome-session's OnFailure= racing, not seat/DRM access).
#
# Socket naming (see docs/adr/adr-0005-route-shell-launched-clients-through-the-proxy.md
# for the full reasoning, confirmed against mutter/libwayland's own
# source, not guessed): gnome-shell is started with
# --wayland-display=$PUBLIC_DISPLAY -- the proxy's own public name, NOT a
# private one. This is deliberate and load-bearing: mutter's
# set_gnome_env("WAYLAND_DISPLAY", ...) does a one-time setenv() on its
# own process using exactly that string, right after creating the
# socket, which every app it directly spawns (Super key search, dock,
# Activities -- NOT covered by the dbus-update-activation-environment
# calls below, which only reach D-Bus-activated services) then inherits
# normally. Immediately after gnome-shell's socket appears, socket-handoff
# (src/bin/socket-handoff.rs, see its own module doc comment) renames it
# out from under that name to $HOST_DISPLAY (the proxy's own --display=
# target, unchanged across every restart, so the proxy's existing
# reconnect_with_backoff logic needs no awareness of any of this) and
# this script then tells the proxy to reclaim the now-vacant public name
# -- `systemctl ... start` the first time, `... kill --signal=SIGUSR1`
# every restart after that, since the proxy stays running across
# gnome-shell crashes and only needs its LISTENER rebound in place, not
# a full process restart (which would drop every already-connected
# client -- see main.rs's SIGUSR1 handler doc comment). This must be
# redone on every single restart, not just the first: mutter's own
# socket-claiming (wl_socket_lock) has no liveness check beyond a lock
# file, so a freshly-restarted gnome-shell will always successfully
# steal $PUBLIC_DISPLAY back, including from the proxy.
#
# A plain shell polling loop (`while [ ! -S path ]; ... sleep 0.05`) was
# tried here first and found live 2026-08-03 to have a real bug, not just
# a speed problem: it matched a STALE leftover socket file from a
# PREVIOUS cycle (the proxy doesn't unlink its own socket on shutdown)
# instead of gnome-shell's own fresh bind, confirmed by comparing the
# renamed file's birth time against gnome-shell's own reported bind
# timestamp -- over a minute apart. That let every one of gnome-shell's
# own startup helpers (DING, notification daemon, ...) connect directly
# to it, completely unprotected, for the rest of the session.
# socket-handoff fixes this at the root (inotify's IN_CREATE only fires
# for a file created *after* the watch starts, plus it removes any stale
# file before watching as a second line of defense) and additionally
# SIGSTOPs gnome-shell the instant its socket appears, before the rename,
# closing the *remaining* race (gnome-shell's own children connecting
# before the swap completes) -- see that binary's own doc comment for
# the full detail on both fixes.
#
# Logs to $XDG_RUNTIME_DIR/wl-res-gnome-shell-direct-wrapper.log, same
# reasoning as the labwc wrapper's own header comment.
set -u

RUNTIME_DIR="${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR must be set}"
LOG="$RUNTIME_DIR/wl-res-gnome-shell-direct-wrapper.log"
exec >>"$LOG" 2>&1

PUBLIC_DISPLAY=wayland-0
HOST_DISPLAY=wl-res-gnome-shell-direct-host-0
PROXY_UNIT=wayland-proxy-gnome-shell-direct.service

echo "$(date -Iseconds) wrapper starting"

# Periodically re-assert XDG_SESSION_DESKTOP/XDG_CURRENT_DESKTOP in the
# background, for this whole script's lifetime -- same defensive pattern
# as the labwc wrapper (found there that Xwayland's own lazy, delayed
# activation-environment export can clobber a one-shot export well after
# the fact). WAYLAND_DISPLAY deliberately NOT included here anymore
# (ADR-0005): now that gnome-shell itself binds $PUBLIC_DISPLAY directly,
# mutter's own set_gnome_env already pushes the correct value into the
# activation environment as part of its normal startup -- this loop
# fighting it with the same value would be redundant, and before
# ADR-0005 landed the two were actively disagreeing (mutter pushing its
# own private name, this loop overwriting with the proxy's public one
# every 3s).
#
# XDG_CURRENT_DESKTOP found live 2026-08-03, and load-bearing: nothing
# else sets it for this session at all (gnome-session normally does),
# so the SHARED systemd --user activation environment (what
# D-Bus-activated services like xdg-desktop-portal-gnome actually see --
# confirmed via /proc/<gnome-shell-pid>/environ having the *correct*
# value while `systemctl --user show-environment` didn't) was stuck on
# whatever a PREVIOUS session (e.g. wl-res-labwc) last set it to.
# Portal/D-Bus-activated components checking "am I in a real GNOME
# session" against this variable would see stale garbage. Value matches
# what this session's own .desktop DesktopNames= would produce
# naturally for a normal login.
(
    while :; do
        sleep 3
        dbus-update-activation-environment --systemd XDG_SESSION_DESKTOP=wl-res-gnome-shell-direct XDG_CURRENT_DESKTOP=wl-res-gnome-shell-direct:ubuntu:GNOME 2>/dev/null
    done
) &

while :; do
    gnome-shell --mode=ubuntu --wayland-display="$PUBLIC_DISPLAY" &
    SHELL_PID=$!
    echo "$(date -Iseconds) gnome-shell started, pid=$SHELL_PID"

    # Blocks until gnome-shell's own socket appears at $PUBLIC_DISPLAY --
    # it always binds there (see the header comment), stealing whatever
    # was previously there (including a prior cycle's now-orphaned proxy
    # listener) unconditionally -- then SIGSTOPs gnome-shell, renames the
    # socket to $HOST_DISPLAY, and SIGCONTs it. See socket-handoff's own
    # doc comment for why this replaced a plain polling loop. Non-zero
    # exit (gnome-shell died before ever creating the socket, or a real
    # I/O error) means there's nothing to hand off this cycle.
    if /home/stu/projects/mine/wayland-resilliance-proxy/target/release/socket-handoff \
        --dir "$RUNTIME_DIR" --watch-name "$PUBLIC_DISPLAY" --rename-to "$HOST_DISPLAY" \
        --freeze-pid "$SHELL_PID"
    then
        echo "$(date -Iseconds) handed off $PUBLIC_DISPLAY -> $HOST_DISPLAY, reclaiming $PUBLIC_DISPLAY for the proxy"

        # Query the unit's ACTUAL current state, not a local variable --
        # found live 2026-08-03: a local "have I started it yet" flag
        # resets to unset every time this SCRIPT restarts (i.e. every
        # fresh login), but the proxy unit itself is a separate,
        # longer-lived systemd --user service that stays running ACROSS
        # logins (that's the whole point -- it survives gnome-shell
        # crashes without itself restarting). A flag blind to that meant
        # the first handoff after every fresh login called `start`
        # against an ALREADY-active unit -- a silent no-op -- instead of
        # the SIGUSR1 rebind it actually needed, leaving nothing
        # listening on $PUBLIC_DISPLAY at all until the next crash cycle
        # (or later, an explicit unit restart) happened to trigger a real
        # rebind. Confirmed live: gtk4-demo/tilix launched via Super key
        # in that state got ENOENT connecting to wayland-0 and silently
        # fell back to X11 -- looked like a HiDPI bug, wasn't.
        if systemctl --user is-active --quiet "$PROXY_UNIT"; then
            systemctl --user kill --signal=SIGUSR1 "$PROXY_UNIT"
        else
            systemctl --user start "$PROXY_UNIT"
        fi
    else
        echo "$(date -Iseconds) socket-handoff failed this cycle (see above) -- nothing to hand off"
    fi

    dbus-update-activation-environment --systemd XDG_SESSION_DESKTOP=wl-res-gnome-shell-direct XDG_CURRENT_DESKTOP=wl-res-gnome-shell-direct:ubuntu:GNOME

    wait "$SHELL_PID"
    echo "$(date -Iseconds) gnome-shell exited (pid=$SHELL_PID) -- restarting in 1s"
    sleep 1
done
