#!/usr/bin/env bash
# Starts a headless labwc compositor on the HOST and serves it over VNC.
# Run this directly, e.g. `./scripts/start-host.sh` — do NOT `source` it.
set -euo pipefail

VNC_PORT="${1:-5900}"

echo "Starting headless host compositor..."
WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 labwc &

# Poll for the socket rather than assuming it exists immediately.
sock=""
for _ in $(seq 1 10); do
  sock=$(find "$XDG_RUNTIME_DIR" -maxdepth 1 -name 'wayland-*[0-9]' 2>/dev/null | head -n1) || true
  [[ -n "$sock" ]] && break
  sleep 0.5
done

if [[ -z "$sock" ]]; then
  echo "ERROR: labwc never created a Wayland socket — check the errors above." >&2
  exit 1
fi

display_name="$(basename "$sock")"
echo "Host compositor socket: $display_name"
echo "Pass this to scripts/start-guest.sh, e.g.:"
echo "  ./scripts/start-guest.sh $display_name"
echo ""
echo "Starting wayvnc on 127.0.0.1:${VNC_PORT} (tunnel with: ssh -L ${VNC_PORT}:localhost:${VNC_PORT} <host>)"
WAYLAND_DISPLAY="$display_name" wayvnc 127.0.0.1 "$VNC_PORT"
