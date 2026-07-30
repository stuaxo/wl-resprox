#!/usr/bin/env bash
# Reports labwc/Wayland/wayvnc diagnostic state on both host and guest.
#
# Detects context via $CONTAINER_ID (set by the Containerfile inside the
# container, unset on the host) and runs the relevant checks. When run on
# the host, it also re-invokes itself inside the container to pull
# guest-side diagnostics into the same output.
#
# Usage (run on the host): ./scripts/diagnose.sh [container-name]
set +e  # diagnostics should keep going even if individual checks fail

CONTAINER_NAME="${1:-wayland-proxy-dev}"
SCRIPT_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"

is_inside_container() {
  [[ -n "${CONTAINER_ID:-}" ]]
}

section() {
  echo ""
  echo "=== $1 ==="
}

print_env() {
  local label="$1"
  section "${label}: environment"
  echo "WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-<unset>}"
  echo "XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-<unset>}"
  is_inside_container && echo "CONTAINER_ID=${CONTAINER_ID}"
}

print_wayland_sockets() {
  local label="$1"
  section "${label}: wayland sockets"
  ls -la "$XDG_RUNTIME_DIR"/wayland-* 2>/dev/null || echo "(none found)"
}

print_labwc_processes() {
  local label="$1"
  section "${label}: labwc processes"
  if ! pgrep -fa labwc; then
    echo "(no labwc process running)"
    return
  fi
  for pid in $(pgrep -f labwc); do
    echo "--- PID $pid ---"
    echo "State: $(grep '^State:' /proc/"$pid"/status 2>/dev/null)"
    echo "Open file descriptors:"
    ls -la /proc/"$pid"/fd 2>/dev/null | sed 's/^/  /'
  done
}

print_socket_ownership() {
  section "GUEST: socket ownership"
  if command -v fuser >/dev/null; then
    for sock in "$XDG_RUNTIME_DIR"/wayland-*[0-9]; do
      [[ -e "$sock" ]] || continue
      echo "--- $sock ---"
      sudo fuser -v "$sock" 2>&1 | sed 's/^/  /'
    done
  elif command -v lsof >/dev/null; then
    sudo lsof -U 2>/dev/null | grep wayland
  else
    echo "(neither fuser nor lsof available)"
  fi
}

print_x11_unix() {
  section "GUEST: /tmp/.X11-unix"
  ls -la /tmp/.X11-unix 2>&1
}

print_wayvnc_processes() {
  section "HOST: wayvnc processes"
  pgrep -fa wayvnc || echo "(no wayvnc process running)"
}

print_wayvnc_port() {
  section "HOST: wayvnc listening port"
  if command -v ss >/dev/null; then
    ss -tlnp 2>/dev/null | grep -E '5900|wayvnc' || echo "(nothing listening on 5900)"
  elif command -v netstat >/dev/null; then
    netstat -tlnp 2>/dev/null | grep -E '5900|wayvnc' || echo "(nothing listening on 5900)"
  else
    echo "(neither ss nor netstat available)"
  fi
}

print_containers() {
  section "HOST: podman containers"
  sudo podman ps -a --filter "name=${CONTAINER_NAME}" 2>/dev/null
}

run_guest_checks() {
  print_env "GUEST"
  print_wayland_sockets "GUEST"
  print_labwc_processes "GUEST"
  print_socket_ownership
  print_x11_unix
}

run_host_checks() {
  print_env "HOST"
  print_wayland_sockets "HOST"
  print_labwc_processes "HOST"
  print_wayvnc_processes
  print_wayvnc_port
  print_containers
}

pull_guest_diagnostics() {
  echo ""
  echo "############################################"
  echo "# Pulling diagnostics from inside '${CONTAINER_NAME}'..."
  echo "############################################"
  sudo podman exec --user stu "$CONTAINER_NAME" bash "$SCRIPT_PATH"
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
