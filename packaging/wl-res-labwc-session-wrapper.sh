#!/bin/sh
# Exec= target for wl-res-labwc.desktop -- runs directly as
# gdm-wayland-session's child, NOT via `systemctl --user start`.
#
# Found live 2026-08-03, the hard way, after two other fixes
# (LIBSEAT_BACKEND=logind, then explicitly resolving XDG_SESSION_ID)
# each got further but still failed with "Timeout waiting session to
# become active": GDM only hands seat/display activation over to
# whatever process it directly forked for Exec=. gnome-shell has its
# own private "Registering session with GDM" handshake (visible in its
# journal), but the more fundamental issue is the process-tree
# relationship itself -- routing labwc through a systemd --user unit
# (a completely separate process tree from gdm-wayland-session's own
# child) makes it invisible to GDM's activation tracking regardless of
# any handshake protocol. The session gets created, but logind never
# marks it active on the seat, so labwc's own DRM backend times out
# waiting for it.
#
# Fix: labwc runs as THIS script's own direct child (this script IS
# gdm-wayland-session's child), restarted in a plain loop on crash --
# no systemd Restart=on-failure, no OnFailure=, none of the
# gnome-session-manager-style teardown races found earlier for
# wl-res-gnome-shell. wayland-proxy-labwc.service stays a normal
# systemd --user unit (started explicitly below) since the proxy itself
# needs no DRM/seat access at all -- none of the above applies to it.
#
# Logs to $XDG_RUNTIME_DIR/wl-res-labwc-wrapper.log since this script's
# own stdout/stderr otherwise land wherever GDM's Exec= capture goes,
# not somewhere obviously `journalctl`-able.
set -u

RUNTIME_DIR="${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR must be set}"
LOG="$RUNTIME_DIR/wl-res-labwc-wrapper.log"
exec >>"$LOG" 2>&1

LINK="$RUNTIME_DIR/wl-res-labwc-0"
export LIBSEAT_BACKEND=logind

echo "$(date -Iseconds) wrapper starting"

# Reserve wayland-0 exclusively for the PROXY's public socket -- never
# let labwc's own auto-naming (wl_display_add_socket_auto(), which
# claims names via flock() on a matching .lock file -- the same
# protocol this reservation uses) land on it. Found live 2026-08-03 the
# hard way: without this, labwc happily bound "wayland-0" directly,
# silently shadowing the wl-res-gnome-shell session's live proxy socket
# at the exact same path -- any NEW client connecting to
# WAYLAND_DISPLAY=wayland-0 during that window would have silently
# reached the wrong compositor. Held for this whole script's lifetime
# (opened once, outside the loop below) so it survives every labwc
# restart, not just the first.
exec 8>"$RUNTIME_DIR/wayland-0.lock"
if ! flock -n 8; then
    echo "$(date -Iseconds) WARNING: could not lock wayland-0.lock -- something else already holds it, collision risk"
fi

LABWC_CONFIG=/home/stu/projects/mine/wayland-resilliance-proxy/scripts/containers/labwc/labwc-config

# Periodically re-assert WAYLAND_DISPLAY/XDG_SESSION_DESKTOP in the
# background, for this whole script's lifetime. Found live 2026-08-03:
# there is NO actual "disable Xwayland" config option in this labwc
# version at all (confirmed via `man labwc-config`) -- only
# <xwaylandPersistence>, which controls whether an ALREADY-running
# Xwayland stays alive, not whether one starts. The harness's own
# `xwayland=no` rc.xml setting is inert, silently ignored by the XML
# parser (worth flagging separately: the *container* harness tests may
# have been unknowingly running Xwayland this whole time too). Xwayland
# launches lazily, on-demand, at an unpredictable point well after this
# session's own one-shot export already ran -- and re-exports
# WAYLAND_DISPLAY from its own (labwc's private) inherited environment
# when it does, same "whoever exports last wins" pattern already found
# for gnome-shell. A single export isn't robust against that; this
# re-wins the race every few seconds instead of trying to prevent the
# clobber from happening at all.
(
    while :; do
        sleep 3
        dbus-update-activation-environment --systemd WAYLAND_DISPLAY=wayland-0 XDG_SESSION_DESKTOP=wl-res-labwc 2>/dev/null
    done
) &

while :; do
    before="$(ls "$RUNTIME_DIR"/wayland-*[0-9] 2>/dev/null || true)"
    # -C reuses the test harness's own labwc config -- NOT for its
    # `xwayland=no` setting (confirmed live 2026-08-03 this element
    # doesn't exist in labwc's actual schema and is silently ignored;
    # see the periodic re-export loop above for how the resulting
    # WAYLAND_DISPLAY clobbering is actually handled), just for the
    # rest of its settings (keybinds etc.) as a reasonable default.
    # Subshell closes fd 8 (the wayland-0.lock flock) before exec'ing --
    # found live 2026-08-03: a plain `labwc &` inherits it, and if labwc
    # then outlives this wrapper (e.g. reparented as an orphan after a
    # `gdm restart` that doesn't cleanly signal a session with no
    # GDM-integration code to respond to), it keeps holding OUR lock
    # forever, blocking every future wrapper instance from ever
    # reserving wayland-0 again.
    ( exec 8>&- 2>/dev/null; exec labwc -C "$LABWC_CONFIG" ) &
    LABWC_PID=$!
    echo "$(date -Iseconds) labwc started, pid=$LABWC_PID"

    found=""
    i=0
    while [ "$i" -lt 40 ]; do
        after="$(ls "$RUNTIME_DIR"/wayland-*[0-9] 2>/dev/null || true)"
        for sock in $after; do
            name="$(basename "$sock")"
            [ "$name" = "wayland-0" ] && continue
            case "$before" in
                *"$sock"*) continue ;;
            esac
            found="$sock"
        done
        [ -n "$found" ] && break
        kill -0 "$LABWC_PID" 2>/dev/null || break
        i=$((i + 1))
        sleep 0.25
    done

    if [ -n "$found" ]; then
        ln -sf "$found" "$LINK"
        echo "$(date -Iseconds) linked $found -> $LINK"
        dbus-update-activation-environment --systemd WAYLAND_DISPLAY=wayland-0 XDG_SESSION_DESKTOP=wl-res-labwc
        systemctl --user start wayland-proxy-labwc.service
    else
        echo "$(date -Iseconds) never found labwc's socket -- proxy not (re)started this cycle"
    fi

    wait "$LABWC_PID"
    echo "$(date -Iseconds) labwc exited (pid=$LABWC_PID) -- restarting in 1s"
    sleep 1
done
