#!/usr/bin/env bash
# Sourced (not executed) by test-crash.sh and friends. Tracks which pid,
# container, and socket belong to which role in a single run, in one
# place -- so cleanup can kill by literal pid (no pattern matching, no
# self-matching `pkill -f` mistakes -- see the 2026-07-31 mutter entry in
# docs/debugging-notes.md) and a run in progress (or one that got stuck)
# can be inspected from outside the script that started it, instead of
# re-derived from `ps`/`fuser` archaeology every time. Motivated by
# exactly that archaeology recurring across this project's history
# (stale-vs-live socket confusion, zombie processes, and manually
# shuttling DBUS_SESSION_BUS_ADDRESS between separate `podman exec` calls
# during the mutter investigation, since env vars don't survive across
# them).
#
# Lives under $XDG_RUNTIME_DIR (not /tmp) deliberately: that directory is
# already bind-mounted at the same path into every container (see
# setup-env.sh's HOST_RUNTIME_DIR mount), so a run started from the host
# or from inside any container is visible everywhere -- required for the
# cross-compositor swap tests, which span two separate containers by
# design. One consequence worth remembering: every container's registry
# is the *same* host directory, not a private one per container.
#
# Usage (from a script that has already `set -uo pipefail`):
#   source "$SCRIPT_DIR/run-registry.sh"
#   run_dir_init                              # creates $RUN_DIR, once per script
#   run_track proxy "$PROXY_PID"              # container defaults to $CONTAINER_ID or HOST
#   run_track compositor "$pid" wayland-proxy-dev-sway  # explicit, for cross-container tracking
#   run_link_socket compositor "$XDG_RUNTIME_DIR/wayland-1"
#   run_cleanup                               # kills every tracked pid (does NOT remove $RUN_DIR --
#                                              # that's a policy decision for the caller, e.g.
#                                              # test-crash.sh only rm -rf's it after a clean pass,
#                                              # keeping it around on failure for inspection)
#
# On-disk layout (one directory per run):
#   $XDG_RUNTIME_DIR/wayland-proxy-runs/<run-id>/
#     chain              human-readable append-only log, one line per
#                        run_track/run_link_socket call -- "the chain of
#                        compositors, pids, and sockets" for this run.
#     <role>.pid         three lines: container (or literal HOST), pid,
#                        and a process-identity string (see below)
#     <role>.sock        symlink to the actual socket path, if tracked
#   $XDG_RUNTIME_DIR/wayland-proxy-runs/current -> <run-id>  (latest run)
#
# Two things a naive pid-file scheme gets wrong, both found live while
# building this (not theoretical):
#
#   1. A pid only means something within its own pid namespace, so every
#      dispatch (kill, liveness check) first asks "is this role's
#      tracked container *my own* current context ($CONTAINER_ID, or
#      HOST on the bare host)?" -- if so, signal it directly. If not,
#      only the host can reach it at all (via `podman exec`; containers
#      don't have podman installed to reach each other or the host). A
#      container asked to signal a pid tagged with some *other*
#      container can't verify anything either way, so it fails closed
#      (treated as unreachable, not as dead) -- see run_gc_stale_runs,
#      where guessing wrong would delete a live run's evidence.
#
#   2. A pid number alone isn't a stable process identity: confirmed
#      live that a killed compositor's pid got reused by an unrelated
#      process inside the same busy container within about a second,
#      which made a naive `kill -0 <pid>` liveness check falsely report
#      "still alive" and blocked garbage collection entirely. Every
#      tracked pid is therefore paired with a `ps -o lstart=` timestamp
#      captured at track time; a liveness check only counts if the pid
#      still exists *and* its current lstart still matches.
#
#   3. A zombie still "exists" and still reports its original lstart --
#      confirmed live that dbus-daemon's `--fork` leaves exactly this
#      behind, since every WM container's pid 1 is a bare `sleep
#      infinity` with nothing reaping children (the same already-known,
#      already-accepted "harmless unreaped zombie" behaviour
#      diagnose.sh's own count_zombie_labwc separately documents). Left
#      unhandled, that's a run directory gc can never actually collect,
#      since its "confirmed alive" check would never stop matching.
#      Liveness therefore also requires the process not be in state Z.

RUN_REGISTRY_ROOT="${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR must be set}/wayland-proxy-runs"

run_dir_init() {
    mkdir -p "$RUN_REGISTRY_ROOT"
    local run_id
    run_id="$(date +%Y%m%dT%H%M%S)-$$"
    RUN_DIR="$RUN_REGISTRY_ROOT/$run_id"
    mkdir -p "$RUN_DIR"
    ln -sfn "$run_id" "$RUN_REGISTRY_ROOT/current"
    echo "Run directory: $RUN_DIR"
    # Best-effort hygiene, safe by construction (run_gc_stale_runs never
    # removes a run it can't positively confirm is all-dead) -- so every
    # new run also sweeps up old ones instead of requiring a separate step.
    run_gc_stale_runs
}

# _run_can_reach <container>: can *this* shell actually signal/check a
# pid tagged with <container>? Always true for our own current context;
# true for any other container only when we're the bare host (the only
# place with podman+sudo to reach into one).
_run_can_reach() {
    local container="$1"
    [[ "$container" == "${CONTAINER_ID:-HOST}" ]] && return 0
    [[ -z "${CONTAINER_ID:-}" ]]
}

# _run_pid_identity <container> <pid>: an opaque, effectively-unique
# string identifying *this specific process instance*, empty if <pid>
# doesn't currently exist there. See the module doc's point (2) for why
# a bare pid number isn't enough on its own.
_run_pid_identity() {
    local container="$1" pid="$2"
    if [[ "$container" == "${CONTAINER_ID:-HOST}" ]]; then
        ps -o lstart= -p "$pid" 2>/dev/null
    else
        sudo podman exec "$container" ps -o lstart= -p "$pid" 2>/dev/null
    fi
}

# run_track <role> <pid> [container]
# container defaults to $CONTAINER_ID (baked into every WM image, see
# each Containerfile's own ENV) if set, else the literal string HOST --
# i.e. "whatever namespace I'm actually running in", unless the caller is
# on the host tracking a pid that lives inside some *other* container
# (the cross-compositor swap case), which must pass it explicitly.
run_track() {
    local role="$1" pid="$2" container="${3:-${CONTAINER_ID:-HOST}}" identity
    identity="$(_run_pid_identity "$container" "$pid")"
    printf '%s\n%s\n%s\n' "$container" "$pid" "$identity" > "$RUN_DIR/${role}.pid"
    echo "$(date -Iseconds) role=${role} container=${container} pid=${pid}" >> "$RUN_DIR/chain"
}

run_link_socket() {
    local role="$1" sock="$2"
    ln -sfn "$sock" "$RUN_DIR/${role}.sock"
    echo "$(date -Iseconds) role=${role} sock=${sock}" >> "$RUN_DIR/chain"
}

# _run_pid_state <container> <pid>: process state letter (S, R, Z, ...)
# via `ps -o stat=`; empty if the pid doesn't exist.
_run_pid_state() {
    local container="$1" pid="$2"
    if [[ "$container" == "${CONTAINER_ID:-HOST}" ]]; then
        ps -o stat= -p "$pid" 2>/dev/null
    else
        sudo podman exec "$container" ps -o stat= -p "$pid" 2>/dev/null
    fi
}

# _run_confirm <container> <pid> <expected-identity>: true only if <pid>
# in <container> exists right now, is still the exact process run_track
# originally saw (not some later, unrelated process reusing the same pid
# number -- module doc point 2), and isn't a zombie (module doc point 3).
_run_confirm() {
    local container="$1" pid="$2" expected="$3" actual
    actual="$(_run_pid_identity "$container" "$pid")"
    [[ -n "$actual" && "$actual" == "$expected" ]] || return 1
    [[ "$(_run_pid_state "$container" "$pid")" != Z* ]]
}

# Reads a <role>.pid file into the three vars named by the caller
# (container, pid, identity) via nameref -- avoids repeating the same
# three `sed -n` calls at every call site.
_run_read_pidfile() {
    local pidfile="$1"
    local -n _container="$2" _pid="$3" _identity="$4"
    _container="$(sed -n '1p' "$pidfile")"
    _pid="$(sed -n '2p' "$pidfile")"
    _identity="$(sed -n '3p' "$pidfile")"
}

# run_is_alive <role> [run-dir] -- for external inspection (diagnose.sh)
# of a run this script didn't itself start. Returns 1 (not confirmed
# alive) both when the process is actually gone and when this vantage
# point simply can't reach it -- callers that need to tell those apart
# should check _run_can_reach themselves first, same as run_gc_stale_runs.
run_is_alive() {
    local role="$1" run_dir="${2:-$RUN_DIR}" container pid identity
    [[ -f "$run_dir/${role}.pid" ]] || return 1
    _run_read_pidfile "$run_dir/${role}.pid" container pid identity
    [[ -n "$pid" ]] || return 1
    _run_can_reach "$container" || return 1
    _run_confirm "$container" "$pid" "$identity"
}

# run_cleanup: kill every tracked pid -- but only if it's confirmed to
# still be the exact process instance that was tracked (see module doc
# point 2); a reused pid number is silently skipped rather than killed,
# since that would be killing an unrelated, unrecognized process by
# coincidence. Does NOT remove $RUN_DIR -- that's a policy decision left
# to the caller (see the module doc's usage example). Safe to call more
# than once or with nothing tracked yet.
run_cleanup() {
    [[ -d "${RUN_DIR:-}" ]] || return 0
    local pidfile container pid identity
    for pidfile in "$RUN_DIR"/*.pid; do
        [[ -e "$pidfile" ]] || continue
        _run_read_pidfile "$pidfile" container pid identity
        [[ -n "$pid" ]] || continue
        _run_confirm "$container" "$pid" "$identity" || continue
        if [[ "$container" == "${CONTAINER_ID:-HOST}" ]]; then
            kill -9 "$pid" 2>/dev/null
        else
            sudo podman exec "$container" kill -9 "$pid" 2>/dev/null
        fi
    done
}

# run_gc_stale_runs: removes every run directory under $RUN_REGISTRY_ROOT
# (other than the one `current` points at) whose tracked roles are ALL
# confirmed dead. Fails closed: a run directory is only ever deleted if
# every single tracked pid was both reachable-to-check from here and
# confirmed dead -- any pid we can't verify (wrong container, no podman
# from inside a container) keeps the whole directory, since guessing
# "probably dead" and deleting a still-running compositor's paper trail
# would defeat the entire point of this module. Called automatically by
# run_dir_init (so every new run sweeps up old ones) and by diagnose.sh's
# host checks (the vantage point with the best cross-container reach).
run_gc_stale_runs() {
    [[ -d "$RUN_REGISTRY_ROOT" ]] || return 0
    local current_target=""
    [[ -L "$RUN_REGISTRY_ROOT/current" ]] && current_target="$(readlink "$RUN_REGISTRY_ROOT/current")"
    local dir run_id keep pidfile container pid identity
    for dir in "$RUN_REGISTRY_ROOT"/*/; do
        [[ -d "$dir" ]] || continue
        run_id="$(basename "$dir")"
        # `*/ ` matches the `current` symlink too, since it points at a
        # directory -- skip it explicitly, not just via current_target
        # (which protects the *target* dir, not the symlink's own name).
        [[ "$run_id" == "current" ]] && continue
        [[ "$run_id" == "$current_target" ]] && continue
        keep=false
        for pidfile in "$dir"*.pid; do
            [[ -e "$pidfile" ]] || continue
            _run_read_pidfile "$pidfile" container pid identity
            [[ -n "$pid" ]] || continue
            if ! _run_can_reach "$container"; then
                keep=true # unknown -- don't guess, don't delete
                break
            fi
            if _run_confirm "$container" "$pid" "$identity"; then
                keep=true # confirmed alive (same process instance)
                break
            fi
        done
        [[ "$keep" == false ]] && rm -rf "$dir"
    done
}
