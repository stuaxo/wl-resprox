# Wayland Proxy Dev Environment

Builds a `podman` container (Ubuntu 26.04) for testing the Wayland
proxy. Includes Rust, GTK4, Wayland tooling, and `labwc` as a nested
compositor. No Distrobox — see `Containerfile`'s header for why.

## Project layout

```
.
├── plan-test-harness.md  # harness packaging + multi-WM plan
├── docs/
│   ├── plan/             # phase-by-phase build history, one file per plan
│   └── ...                # design notes, ADRs, debugging log
└── scripts/
    ├── setup-env.sh     # builds the image and starts the container (run once)
    ├── teardown-env.sh  # stops and removes the container (reverses setup-env.sh)
    ├── Containerfile    # used by setup-env.sh — must stay alongside it
    ├── start-host.sh    # starts a headless labwc + wayvnc on the HOST
    ├── entrypoint.sh    # runs INSIDE the container — starts nested labwc
    ├── start-guest.sh   # enters the container, runs entrypoint.sh, then
    │                    # drops you into an interactive shell for testing
    ├── test-crash.sh    # automated crash/reconnect check (see step 4 below)
    └── diagnose.sh      # dumps labwc/Wayland/wayvnc state, host + guest
```

## Host prerequisites

The container image installs Rust, GTK4, and Wayland libs. Your host
needs:

1. **A container engine — Podman (recommended) or Docker**
   ```bash
   sudo apt install podman      # Ubuntu/Debian
   ```
   Podman should be usable rootless (default on modern Ubuntu). Verify:
   ```bash
   podman info
   ```

2. **`labwc` and `wayvnc` on the HOST** — run the outer Wayland session,
   not just inside the container:
   ```bash
   sudo apt install labwc wayvnc   # Ubuntu/Debian
   sudo dnf install labwc wayvnc   # Fedora
   sudo pacman -S labwc wayvnc     # Arch
   ```
   For headless/SSH use: `start-host.sh` runs `labwc` with
   `WLR_BACKENDS=headless` and serves it over VNC via `wayvnc`.

## Running it

### 1. Build and start the container (once)

```bash
./scripts/setup-env.sh
```
Builds the `wayland-proxy-dev` image and starts it as a long-running
container. Matches the container's `dev` user's UID/GID and the host's
video/render GIDs — but `dev` is a fixed, generic login name, not tied
to your host account. Only the project directory is shared with the
container, mounted at `/workspace` — not your whole home directory.

### 2. Start the host compositor + VNC (Terminal A)

```bash
./scripts/start-host.sh
```
Prints the host's Wayland socket name (e.g. `wayland-0`) and starts
`wayvnc` listening on `127.0.0.1:5900`. Leave this running in the
foreground.

**Already SSH'd into the host (remote dev)?** Steps 1-4 all run fine
over your existing SSH session — nothing here needs a local display.
Only VNC viewing needs a second connection, and it's from your **local
machine**, not from within the session you're already in:
```bash
ssh -L 5900:localhost:5900 <user>@<host>
```
Run that from your own machine, then point a VNC client at
`localhost:5900`. Skip it entirely if you don't need to see the screen —
`test-crash.sh` (step 4) and the proxy logs don't need VNC at all.

### 3. Start the nested compositor and enter the container (Terminal B)

```bash
./scripts/start-guest.sh wayland-0   # match the socket start-host.sh printed
```
Runs `entrypoint.sh` inside the container: starts the nested `labwc`,
reports its new socket, then drops you into an interactive shell. Run:
```bash
WAYLAND_DISPLAY=<new-socket> gtk4-demo
```

### 4. Automated crash-recovery check (optional)

Run from the shell step 3 leaves you in (already at the project root):
```bash
bash scripts/test-crash.sh
```
Starts its own compositor, proxy, and `gtk4-demo`. Crashes the
compositor. Checks the client process survives — nothing more.
Self-contained; doesn't use step 3's nested `labwc`. See
`plan-test-harness.md` for the fuller testing-levels picture.

### 5. Tear down

```bash
./scripts/teardown-env.sh          # remove the container
./scripts/teardown-env.sh --image  # also remove the built image
```

## Troubleshooting

- **Podman permission errors** — Configure rootless Podman: see the
  [Podman rootless setup docs](https://github.com/containers/podman/blob/main/docs/tutorials/rootless_tutorial.md).
  Unlikely here — this project's scripts always run Podman via `sudo`;
  see `Containerfile`'s header for why.
- **`zsh: command not found: labwc` on the host** — Install `labwc`/
  `wayvnc` on the host (prerequisite 2). They run outside the container.
- **Nested `labwc` fails with `libseat`/`Could not open target tty` /
  `Failed to start a DRM session`** — `labwc` didn't see the host's
  Wayland socket and tried to run as a root compositor instead. Fix: set
  `WAYLAND_DISPLAY=<host-socket>` before starting it — `start-guest.sh`
  does this for you.
- **`xwayland/sockets.c: /tmp/.X11-unix not owned by root or us`,
  `cannot create xwayland server`** — XWayland fails inside the
  container (`/tmp` ownership mismatch with the shared mount). Harmless:
  this project doesn't use XWayland. Check for a new `wayland-N` socket
  instead. To silence it, add to `~/.config/labwc/rc.xml`:
  ```xml
  <labwc_config><core><xwayland>no</xwayland></core></labwc_config>
  ```
- **`ls: cannot access '#': No such file or directory` (or similar) when
  sourcing a script in zsh** — zsh errors on inline `#` comments and
  empty globs. Run scripts as `./script.sh` or `bash script.sh`, not
  sourced. Expect a short delay before a freshly-started socket file
  exists.
