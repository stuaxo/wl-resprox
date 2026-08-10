#!/usr/bin/env bash
# Reproduces the GNOME Shell PID-collision icon bug documented in
# docs/KNOWN_BUGS.md: a window whose app_id can't be resolved to any
# installed .desktop file (default target: Tilix -- no StartupWMClass,
# app_id "tilix" doesn't match its own com.gexperts.Tilix.desktop)
# falls through to shell-window-tracker.c's PID-based match
# (get_app_from_window_pid) before it ever reaches the correct
# xdg_activation_v1 token match -- and since every wl-resprox client
# shares the proxy's own PID (SO_PEERCRED), that PID match always hits
# some other, unrelated already-running app. See KNOWN_BUGS.md for the
# full resolution-order breakdown (confirmed against GNOME Shell
# 50.1's actual source). Not proxy-specific in the sense that the
# lookup-failure/PID-fallback bug is GNOME Shell's own; IS proxy-
# specific in that only wl-resprox's shared-PID architecture makes the
# PID fallback match every single time.
#
# Sequential, not racing -- doesn't need timing closeness, just "some
# other app is already running and sharing the colliding PID."
#
# Self-verifies via Shell.WindowTracker if Looking Glass unsafe mode is
# already enabled (org.gnome.Shell Eval over D-Bus); otherwise falls
# back to asking you to look at the Activities overview/dash.
#
# Usage: repro-icon-grouping-mixup.sh [other-app.desktop] [target-app.desktop]
# Defaults match the original 2026-08-07 live report: launch Settings
# first (so it's a running app for the PID match to hit), then Tilix.
set -euo pipefail

OTHER_APP="${1:-org.gnome.Settings.desktop}"
TARGET_APP="${2:-com.gexperts.Tilix.desktop}"
TARGET_WMCLASS="${3:-tilix}"

: "${WAYLAND_DISPLAY:?WAYLAND_DISPLAY must be set -- run this from inside a live GNOME Wayland session (or with it exported to point at one)}"

if ! command -v gtk-launch >/dev/null 2>&1; then
    echo "ERROR: gtk-launch not found (usually in libgtk-3-bin/libgtk-3-0t64's dev tools)." >&2
    exit 1
fi

for app in "$OTHER_APP" "$TARGET_APP"; do
    if ! find /usr/share/applications ~/.local/share/applications -maxdepth 1 -iname "$app" 2>/dev/null | grep -q .; then
        echo "WARNING: didn't find $app under the usual applications directories -- gtk-launch may still resolve it another way, continuing." >&2
    fi
done

echo "WAYLAND_DISPLAY=$WAYLAND_DISPLAY"
echo "$(date -Iseconds) launching $OTHER_APP (a running app for the PID match to hit)..."
gtk-launch "$OTHER_APP" &
sleep 2

echo "$(date -Iseconds) launching $TARGET_APP..."
gtk-launch "$TARGET_APP" &
wait
sleep 1

EVAL_JS="JSON.stringify(global.get_window_actors().filter(a=>a.meta_window.get_wm_class()=='${TARGET_WMCLASS}').map(a=>{let w=a.meta_window;let app=Shell.WindowTracker.get_default().get_window_app(w);return{stableSeq:w.get_stable_sequence(),resolvedApp:app?app.get_id():null};}))"
RESULT="$(busctl --user call org.gnome.Shell /org/gnome/Shell org.gnome.Shell Eval s "$EVAL_JS" 2>/dev/null || true)"

if [[ "$RESULT" == bs\ true* ]]; then
    echo "$(date -Iseconds) Shell.WindowTracker says:"
    echo "$RESULT"
    echo "resolvedApp should be $TARGET_APP's own id -- anything else confirms the bug."
else
    echo "$(date -Iseconds) done -- Looking Glass unsafe mode isn't enabled (Eval unavailable), can't self-verify."
    echo "Check by hand: does the $TARGET_APP window show its OWN icon, or $OTHER_APP's, in the Activities overview/dash?"
fi

echo "If a proxy is in the loop with RUST_LOG=debug, grep its log for"
echo "'activation' around the timestamps above -- the token trace will"
echo "look correct even when the icon is wrong (see KNOWN_BUGS.md)."
