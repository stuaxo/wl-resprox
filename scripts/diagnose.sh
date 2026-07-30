#!/usr/bin/env bash
# Reports labwc/Wayland/wayvnc diagnostic state on both host and guest.
#
# Detects context via $CONTAINER_ID (set by the Containerfile inside the
# container, unset on the host) and runs the relevant checks. When run on
# the host, it also re-invokes itself inside the container to pull
# guest-side diagnostics into the same output.
#
# Default output is a compact summary. Process/socket listings on the
# HOST are host-wide (this machine may run other, unrelated Wayland/X11
# sessions -- confirmed live on this dev host: two other labwc sessions
# and several NoMachine remote-desktop processes, nothing to do with this
# project), so those are labeled accordingly rather than presented as if
# they were all ours.
#
# Usage: ./scripts/diagnose.sh [--verbose] [container-name]
#   --verbose   also dump full file-descriptor listings per process.
#               Default omits these -- rarely the first thing needed, and
#               on a busy host they can run to hundreds of lines each.
set +e  # diagnostics should keep going even if individual checks fail

VERBOSE=false
CONTAINER_NAME="wayland-proxy-dev"
for arg in "$@"; do
  case "$arg" in
    --verbose) VERBOSE=true ;;
    *) CONTAINER_NAME="$arg" ;;
  esac
done

# Fixed, not derived from this script's own host-side location -- the
# container only ever mounts the project directory at /workspace (see
# setup-env.sh), not a host-mirrored path, so this is the one path that's
# guaranteed to be correct regardless of where the project lives on the
# host.
CONTAINER_SCRIPT_PATH="/workspace/scripts/diagnose.sh"

is_inside_container() {
  [[ -n "${CONTAINER_ID:-}" ]]
}

section() {
  echo ""
  echo "=== $1 ==="
}

have_fuser() {
  command -v fuser >/dev/null
}

socket_is_live() {
  fuser "$1" >/dev/null 2>&1
}

count_sockets() {
  if ! have_fuser; then
    local n=0
    for sock in "$XDG_RUNTIME_DIR"/wayland-*[0-9]; do [[ -e "$sock" ]] && n=$((n + 1)); done
    echo "${n} found (fuser not available -- can't tell live from stale)"
    return
  fi
  local live=0 stale=0
  for sock in "$XDG_RUNTIME_DIR"/wayland-*[0-9]; do
    [[ -e "$sock" ]] || continue
    if socket_is_live "$sock"; then live=$((live + 1)); else stale=$((stale + 1)); fi
  done
  echo "${live} live, ${stale} stale"
}

count_zombie_labwc() {
  local n=0 pid state
  for pid in $(pgrep -f labwc 2>/dev/null); do
    state="$(awk '/^State:/{print $2}' "/proc/$pid/status" 2>/dev/null)"
    [[ "$state" == "Z" ]] && n=$((n + 1))
  done
  echo "$n"
}

print_summary() {
  local label="$1"
  section "${label}: summary"
  echo "WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-<unset>}"
  echo "XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-<unset>}"
  echo "Wayland sockets: $(count_sockets)"
  echo "Zombie labwc processes: $(count_zombie_labwc) (harmless, just unreaped)"

  local proxy_sock="${XDG_RUNTIME_DIR}/wayland-proxy-0"
  if [[ ! -e "$proxy_sock" ]]; then
    echo "Proxy: not running"
  elif ! have_fuser; then
    echo "Proxy: wayland-proxy-0 exists (fuser not available -- can't confirm it's live)"
  elif socket_is_live "$proxy_sock"; then
    echo "Proxy: listening on wayland-proxy-0"
  else
    echo "Proxy: wayland-proxy-0 exists but nothing is listening (stale)"
  fi

  if [[ "$label" == "HOST" ]]; then
    if pgrep -f wayvnc >/dev/null 2>&1; then
      echo "wayvnc: running"
    else
      echo "wayvnc: not running"
    fi
    echo "Container: $(sudo podman ps --filter "name=${CONTAINER_NAME}" --format '{{.Status}}' 2>/dev/null || echo 'not found')"
  fi
}

print_wayland_sockets() {
  local label="$1"
  section "${label}: wayland sockets"
  if ! have_fuser; then
    echo "(fuser not available -- listing without live/stale status)"
  fi
  local any=false pid
  for sock in "$XDG_RUNTIME_DIR"/wayland-*[0-9]; do
    [[ -e "$sock" ]] || continue
    any=true
    if ! have_fuser; then
      echo "  $(basename "$sock")"
      continue
    fi
    pid="$(fuser "$sock" 2>/dev/null | xargs)"
    if [[ -n "$pid" ]]; then
      echo "  $(basename "$sock"): live (pid $pid)"
    else
      echo "  $(basename "$sock"): stale (nothing attached)"
    fi
  done
  $any || echo "(none found)"
}

print_labwc_processes() {
  local label="$1"
  section "${label}: labwc processes"
  local pids
  pids="$(pgrep -f labwc 2>/dev/null)"
  if [[ -z "$pids" ]]; then
    echo "(none running)"
    return
  fi
  if [[ "$label" == "HOST" ]]; then
    echo "(host-wide match -- may include sessions unrelated to this project on a shared host)"
  fi
  local zombies=0 pid state cmd
  for pid in $pids; do
    state="$(awk '/^State:/{print $2}' "/proc/$pid/status" 2>/dev/null)"
    if [[ "$state" == "Z" ]]; then
      zombies=$((zombies + 1))
      continue
    fi
    cmd="$(tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null)"
    echo "  PID $pid (${state:-?}): ${cmd:-?}"
    if [[ "$VERBOSE" == true ]]; then
      echo "    open fds:"
      ls -la "/proc/$pid/fd" 2>/dev/null | tail -n +2 | sed 's/^/      /'
    fi
  done
  [[ "$zombies" -gt 0 ]] && echo "  + ${zombies} zombie process(es) (harmless, unreaped)"
}

# Who actually holds each XWayland display-number slot -- the thing that
# actually explained an intermittent, hard-to-diagnose test-crash.sh
# failure on this host: two entirely unrelated XWayland/labwc sessions
# already held X0 and X1, forcing every compositor we start to search
# past them (confirmed live via this exact `ss -xlp` query).
print_x11_contention() {
  local label="$1"
  section "${label}: X11 display slots (XWayland contention)"
  if ! command -v ss >/dev/null; then
    echo "(ss not available)"
    return
  fi
  if [[ "$label" == "GUEST" ]]; then
    echo "(sockets are visible here via --network host, but owning"
    echo " processes usually aren't -- separate PID namespace. \"?\""
    echo " entries below are expected, not a fault.)"
  fi
  local lines
  # No sudo: only `sudo podman` is passwordless on this host (a scoped
  # sudoers rule, not blanket) -- confirmed live, plain `sudo ss` prompts
  # for a password non-interactively and just fails. Same reason
  # print_wayvnc_port below never used sudo either. Own-user socket
  # ownership shows fine without it regardless; only a different unix
  # user's sockets would show blank owner/pid, which is an honest gap,
  # not a bug.
  lines="$(ss -xlp 2>/dev/null | grep -P '(?<!@)/tmp/\.X11-unix/X[0-9]+\s')"
  if [[ -z "$lines" ]]; then
    echo "(no X11 sockets found)"
    return
  fi
  local slot owner pid
  while IFS= read -r line; do
    slot="$(grep -oP 'X11-unix/\KX[0-9]+' <<< "$line")"
    owner="$(grep -oP 'users:\(\("\K[^"]+' <<< "$line" | head -1)"
    pid="$(grep -oP 'pid=\K[0-9]+' <<< "$line" | head -1)"
    if [[ "$label" == "HOST" && -n "$pid" ]]; then
      echo "  ${slot}: ${owner:-?} (pid ${pid:-?} -- may be unrelated to this project)"
    else
      echo "  ${slot}: ${owner:-?} (pid ${pid:-?})"
    fi
  done <<< "$lines" | sort -u
}

print_wayvnc_port() {
  section "HOST: wayvnc listening port"
  if command -v ss >/dev/null; then
    ss -tlnp 2>/dev/null | grep -E '5900|wayvnc' || echo "(nothing listening on 5900)"
  else
    echo "(ss not available)"
  fi
}

run_guest_checks() {
  print_summary "GUEST"
  print_wayland_sockets "GUEST"
  print_labwc_processes "GUEST"
  print_x11_contention "GUEST"
}

run_host_checks() {
  print_summary "HOST"
  print_wayland_sockets "HOST"
  print_labwc_processes "HOST"
  print_x11_contention "HOST"
  print_wayvnc_port
}

pull_guest_diagnostics() {
  echo ""
  echo "############################################"
  echo "# Pulling diagnostics from inside '${CONTAINER_NAME}'..."
  echo "############################################"
  local verbose_flag=()
  [[ "$VERBOSE" == true ]] && verbose_flag=(--verbose)
  sudo podman exec --user dev -e XDG_RUNTIME_DIR=/run/user/1000 \
    "$CONTAINER_NAME" bash "$CONTAINER_SCRIPT_PATH" "${verbose_flag[@]}"
}

main() {
  if is_inside_container; then
    run_guest_checks
  else
    run_host_checks
    pull_guest_diagnostics
  fi
}

main
