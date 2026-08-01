# Wayland Proxy Dev Environment

Builds a `podman` container (Ubuntu 26.04) per window manager, for
testing the Wayland proxy against each one. Includes Rust, GTK4, and
Wayland tooling; the compositor itself is whichever `--wm=<name>` you
pick (default `labwc`). No Distrobox — see
`containers/labwc/Containerfile`'s header for why.

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
    ├── containers/      # one subdir per WM: Containerfile + compositor config
    │   ├── labwc/
    │   ├── sway/
    │   ├── kwin/
    │   └── mutter/
    ├── start-host.sh    # starts a headless labwc + wayvnc on the HOST
    ├── entrypoint.sh    # runs INSIDE the container — starts the nested
    │                    # compositor named by $COMPOSITOR
    ├── start-guest.sh   # enters the container, runs entrypoint.sh, then
    │                    # drops you into an interactive shell for testing
    ├── test-crash.sh    # automated crash/reconnect check (see step 4 below)
    ├── test-crash-swap.sh # cross-compositor swap check, run from the HOST
    │                    # (see plan-test-harness.md's "Cross-compositor swap")
    ├── run-registry.sh  # sourced by entrypoint.sh/test-crash.sh: tracks
    │                    # each run's pids/containers/sockets in one place
    ├── compositor-launch.sh # sourced by all three test/entrypoint scripts:
    │                    # one implementation of each WM's headless-launch quirks
    ├── socket-wait.sh   # sourced by test-crash.sh/test-crash-swap.sh:
    │                    # "did a new compositor socket appear" detection
    ├── self-test.sh     # per-WM smoke test, run from the HOST (see step 6 below)
    ├── test-matrix.sh   # loops self-test.sh over every Phase 9 WM (step 6)
    └── diagnose.sh      # dumps compositor/Wayland/wayvnc state, host + guest
```

All of `setup-env.sh`, `teardown-env.sh`, and `start-guest.sh` take a
`--wm=<name>` flag (default `labwc`) selecting which
`containers/<name>/` to build/run against.

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
./scripts/setup-env.sh              # labwc (default)
./scripts/setup-env.sh --wm=sway
```
Builds the `wayland-proxy-dev-<wm>` image and starts it as a
long-running container. Matches the container's `dev` user's UID/GID
and the host's video/render GIDs — but `dev` is a fixed, generic login
name, not tied to your host account. Only the project directory is
shared with the container, mounted at `/workspace` — not your whole
home directory.

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
./scripts/start-guest.sh wayland-0            # match the socket start-host.sh printed
./scripts/start-guest.sh --wm=sway wayland-0  # against a different WM's container
```
Runs `entrypoint.sh` inside the container: starts the nested
compositor, reports its new socket, then drops you into an interactive
shell. Run:
```bash
WAYLAND_DISPLAY=<new-socket> gtk4-demo
```

### 4. Automated crash-recovery check (optional)

Run from the shell step 3 leaves you in (already at the project root;
`$COMPOSITOR` is already set there, baked in by the image):
```bash
bash scripts/test-crash.sh       # L0: crashes the compositor, checks the client survives
bash scripts/test-crash.sh --l1  # L1: also restarts the compositor and checks
                                  # protocol-level recovery (zero unresolvable-interface
                                  # warnings, toplevel chain recreated) from the proxy's own log
```
Self-contained; doesn't use step 3's nested compositor. See
`plan-test-harness.md` for the fuller testing-levels picture and the
per-WM results recorded there.

### 5. Full matrix check (optional, replaces steps 1-4 per WM)

```bash
./scripts/self-test.sh --wm=sway   # one WM: setup -> test-crash.sh --l1 -> diagnose -> teardown
./scripts/test-matrix.sh           # every Phase 9 WM (labwc sway kwin mutter), one command
./scripts/test-matrix.sh sway kwin # a subset
```
Builds and tears down each container itself — no need to run steps 1-4
first. Writes a pass/fail table per WM to `results.md` (gitignored,
regenerated each run) and prints where the full logs landed.

### 6. Tear down

```bash
./scripts/teardown-env.sh          # remove the container
./scripts/teardown-env.sh --image  # also remove the built image
```
Not needed after step 5 -- `self-test.sh`/`test-matrix.sh` already tear
down after themselves, pass or fail.

## Troubleshooting

- **Podman permission errors** — Configure rootless Podman: see the
  [Podman rootless setup docs](https://github.com/containers/podman/blob/main/docs/tutorials/rootless_tutorial.md).
  Unlikely here — this project's scripts always run Podman via `sudo`;
  see `containers/labwc/Containerfile`'s header for why.
- **`zsh: command not found: labwc` on the host** — Install `labwc`/
  `wayvnc` on the host (prerequisite 2). They run outside the container.
  Only `start-host.sh`'s outer session needs this — the nested
  compositor tested inside the container can be any `--wm=` this repo
  covers.
- **Nested compositor fails with `libseat`/`Could not open target tty` /
  `Failed to start a DRM session`** — it didn't see the host's Wayland
  socket and tried to run as a root compositor instead. Fix: set
  `WAYLAND_DISPLAY=<host-socket>` before starting it — `start-guest.sh`
  does this for you.
- **XWayland errors** (`/tmp/.X11-unix not owned by root or us`,
  `Cannot find Xwayland binary`, or similar) on compositor startup —
  harmless: this project doesn't use XWayland, and neither WM container
  installs it. Check for a new `wayland-N` socket instead of a clean
  log. (`containers/labwc/labwc-config/rc.xml` sets `xwayland=no`, but
  don't rely on it suppressing the process entirely — see the
  2026-07-31 corrections in `docs/debugging-notes.md`.)
- **`ls: cannot access '#': No such file or directory` (or similar) when
  sourcing a script in zsh** — zsh errors on inline `#` comments and
  empty globs. Run scripts as `./script.sh` or `bash script.sh`, not
  sourced. Expect a short delay before a freshly-started socket file
  exists.
