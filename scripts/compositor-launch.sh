#!/usr/bin/env bash
# Sourced (not executed) by test-crash.sh, entrypoint.sh, and
# test-crash-swap.sh. Single source of truth for "how do I start
# compositor $1's headless instance in THIS container" -- kwin's setcap
# fix and mutter's dual-D-Bus-bus requirement used to be duplicated
# between test-crash.sh and entrypoint.sh already; a third copy for the
# cross-compositor swap tests is what finally made that worth fixing.
# Every WM this project supports gets exactly one implementation of its
# launch quirks, here, instead of three that can silently drift apart.
#
# Usage (from a script that has already sourced run-registry.sh and
# called run_dir_init):
#   launch_compositor sway [log-file]
#   echo "$COMPOSITOR_PID"   # set by launch_compositor on return
#
# [log-file] is optional -- omit it (or pass /dev/stdout) for
# entrypoint.sh's interactive use, where output should land straight on
# the terminal; test-crash.sh passes its own mktemp'd log path.
#
# [role] is optional, defaults to "compositor" -- test-crash-swap.sh
# overrides it (e.g. "compositor-sway", "compositor-kwin") so a run
# spanning two compositors in two containers keeps a distinct pid/socket
# record for each instead of the second launch overwriting the first's.
#
# Tracks the compositor pid via run-registry.sh's run_track as a side
# effect (requires $RUN_DIR to already be set) -- every caller gets this
# for free rather than remembering to call run_track itself.
launch_compositor() {
    local wm="$1" log="${2:-/dev/stdout}" role="${3:-compositor}"
    case "$wm" in
        labwc)
            WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 \
                labwc -C "$SCRIPT_DIR/containers/labwc/labwc-config" > "$log" 2>&1 &
            ;;
        sway)
            WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 \
                sway -c "$SCRIPT_DIR/containers/sway/sway-config" > "$log" 2>&1 &
            ;;
        kwin)
            # --virtual is kwin's own headless backend (confirmed live via
            # `kwin_wayland --help`) -- the WLR_BACKENDS=headless env var
            # above is wlroots-specific and doesn't apply to kwin (Qt-based,
            # not wlroots). No -c/config-file equivalent needed for a bare
            # virtual-backend instance, unlike labwc/sway.
            kwin_wayland --virtual > "$log" 2>&1 &
            ;;
        mutter)
            # gnome-shell needs BOTH a D-Bus session bus and a D-Bus system
            # bus -- unlike labwc/sway/kwin, none of which need either.
            # Missing the session bus alone was the first symptom found
            # (it's the more obviously-needed one); missing the system bus
            # is a separate, less obvious requirement discovered afterward:
            # timeLimitsManager.js's constructor reads `Gio.DBus.system` to
            # open an org.freedesktop.MalcontentTimer1 proxy, and that
            # property getter *fatally* throws if no system bus is
            # reachable at all (not merely "the service isn't running",
            # which alone would be a harmless, gracefully-handled
            # DBus.Error.ServiceUnknown, same as several other warnings
            # seen once past this point) -- confirmed live via a
            # Gjs-CRITICAL JS ERROR / `free(): invalid pointer` abort in
            # exactly that constructor when only a session bus existed.
            # Fix: start a second private bus and point
            # DBUS_SYSTEM_BUS_ADDRESS at it too -- it doesn't need to
            # behave like a *real* system bus (no policy files, no actual
            # logind/PolicyKit/GDM/GeoClue2/colord services), just needs
            # to exist, since gnome-shell already handles individual
            # service-not-found errors gracefully once the bus itself
            # opens. See the 2026-07-31 mutter entry in
            # docs/debugging-notes.md.
            #
            # Both launched directly (not via a `dbus-run-session`
            # wrapper) so `$!` below is gnome-shell's own pid, not a
            # wrapper's -- a `dbus-run-session` wrapper forks a separate
            # dbus-daemon *and* compositor child rather than exec'ing into
            # it, so `$!` would be the wrapper, not the compositor
            # (confirmed live). --no-x11: gnome-shell starts Xwayland by
            # default otherwise, unlike labwc/sway/kwin's images, none of
            # which install it.
            #
            # --fork daemonizes immediately (detaches, reparents to
            # init) -- `$!` doesn't apply to it, so track its pid
            # separately (--print-pid) via run_track; it otherwise
            # outlives this script and leaks across repeated runs (run
            # -registry.sh's gc reaps it once confirmed dead, but nothing
            # kills it proactively without a tracked pid to act on).
            # Suffix the dbus roles the same way a non-default $role
            # suffixes "compositor" -- otherwise two mutter instances in
            # two different containers (a hypothetical mutter->mutter
            # swap; not one of the pairs this project actually runs, but
            # cheap to get right) would both write dbus-session.pid /
            # dbus-system.pid into the same shared $RUN_DIR and clobber
            # each other.
            local suffix="" session_out system_out
            [[ "$role" != "compositor" ]] && suffix="${role#compositor}"
            session_out="$(dbus-daemon --session --fork --print-address --print-pid)"
            DBUS_SESSION_BUS_ADDRESS="$(head -1 <<< "$session_out")"
            export DBUS_SESSION_BUS_ADDRESS
            run_track "dbus-session${suffix}" "$(tail -1 <<< "$session_out")"
            system_out="$(dbus-daemon --session --fork --print-address --print-pid)"
            DBUS_SYSTEM_BUS_ADDRESS="$(head -1 <<< "$system_out")"
            export DBUS_SYSTEM_BUS_ADDRESS
            run_track "dbus-system${suffix}" "$(tail -1 <<< "$system_out")"
            gnome-shell --headless --no-x11 > "$log" 2>&1 &
            ;;
        *)
            echo "ERROR: no launch case for compositor '$wm' -- add one to scripts/compositor-launch.sh" >&2
            return 1
            ;;
    esac
    COMPOSITOR_PID=$!
    run_track "$role" "$COMPOSITOR_PID"
}
