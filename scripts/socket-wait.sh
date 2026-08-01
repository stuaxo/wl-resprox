#!/usr/bin/env bash
# Sourced by test-crash.sh and test-crash-swap.sh. Detects "did a new
# compositor socket appear" the hard-won way -- see docs/debugging-notes.md's
# 2026-07-31 entries for the two distinct false-hang bugs this specifically
# avoids: a stale dead socket mistaken for live, and a legitimate reused
# slot (wlroots'/kwin's/gnome-shell's own auto-selection all reuse a
# freed slot rather than always picking a fresh one) mistaken for
# "nothing new." Comparing the full (name, pid) pair, not just names,
# catches both.
#
# Requires $RUNTIME_DIR and $PROXY_DISPLAY to already be set by the
# caller (the fixed proxy socket name is excluded from consideration --
# it's never the compositor's own socket).

# snapshot_live_sockets: echoes "name=pid" for every live wayland-N
# socket right now.
snapshot_live_sockets() {
    local sock name pid
    for sock in "$RUNTIME_DIR"/wayland-*[0-9]; do
        [[ -e "$sock" ]] || continue
        name="$(basename "$sock")"
        pid="$(fuser "$sock" 2>/dev/null | xargs)"
        [[ -n "$pid" ]] && echo "${name}=${pid}"
    done
}

# wait_for_new_socket <before-snapshot>: polls up to 20s (same budget
# confirmed necessary live -- see test-crash.sh's own comment history)
# for a socket that's live now and wasn't live under the same pid
# before. Echoes the new socket's name, or nothing on timeout.
wait_for_new_socket() {
    local before="$1" found="" entry name
    for _ in $(seq 1 80); do
        while IFS= read -r entry; do
            [[ -z "$entry" ]] && continue
            name="${entry%%=*}"
            [[ "$name" == "$PROXY_DISPLAY" ]] && continue
            if ! grep -qxF "$entry" <<< "$before"; then
                found="$name"
            fi
        done <<< "$(snapshot_live_sockets)"
        [[ -n "$found" ]] && break
        sleep 0.25
    done
    echo "$found"
}
