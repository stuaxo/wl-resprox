# Wayland Headless Test Harness

Builds a `podman` container (Ubuntu 26.04) per window manager, for
reproducing Wayland client/compositor protocol issues against real
compositor implementations -- headless, no full desktop session needed
on the host. Includes GTK4 and Wayland tooling; the compositor itself is
whichever `--wm=<name>` you pick (`labwc`, `sway`, `kwin`, or `mutter`;
default `labwc`). No Distrobox — see `containers/labwc/Containerfile`'s
header for why.

A proxy under test (e.g. `wayland-proxy`, the project this harness
started life alongside) is entirely optional -- the container matrix
and diagnostics work standalone, with no proxy involved, for
straightforward compositor/client debugging. When you do want one
installed, `setup-env.sh` handles it as a `.deb`; containers don't need
a Rust toolchain at all (see ADR-0003). By default it auto-detects and
builds `wayland-proxy` (`cargo deb`) if this is checked out alongside
`wayland-proxy`'s own `Cargo.toml`; pass `--proxy-deb=<path>` to install
a specific `.deb` instead (doesn't have to be `wayland-proxy`, or built
from any particular checkout).

**Two ways to use this:**
- **In place, from a git checkout** (this document's own instructions
  below) -- `./harness/wayland-headless-harness <command>`, run directly.
- **Installed as a package** (`packaging/build-harness-deb.sh` builds
  `wayland-headless-harness_<version>_all.deb`; `sudo dpkg -i` it) --
  gives a `wayland-headless-harness <command>` CLI on `$PATH` instead of
  a relative path into the checkout. Identical command surface either
  way (Python/Typer); see `wayland-headless-harness --help` for the
  full command list, or any subcommand's own `--help`.

## Project layout

```
.
├── plan-test-harness.md  # harness packaging + multi-WM plan
├── docs/
│   ├── plan/             # phase-by-phase build history, one file per plan
│   └── ...                # design notes, ADRs, debugging log
├── harness/              # the CLI + all host-side orchestration (Python/Typer)
│   ├── wayland-headless-harness   # entry point -- run directly from a
│   │                     # checkout, or staged to /usr/bin/ when installed
│   └── wayland_headless_harness/
│       ├── cli.py        # top-level Typer app (env/session/test/diagnose)
│       ├── common.py     # shared: WM validation, podman wrappers, path resolution
│       └── commands/     # env.py, session.py, testing.py, diagnose.py
└── scripts/              # container-side only -- stays Bash, runs INSIDE
    │                      # the disposable WM containers; none of the four
    │                      # Containerfiles install python3
    ├── containers/      # one subdir per WM: Containerfile + compositor config
    │   ├── labwc/
    │   ├── sway/
    │   ├── kwin/
    │   └── mutter/
    ├── entrypoint.sh    # runs INSIDE the container — starts the nested
    │                    # compositor named by $COMPOSITOR
    ├── test-crash.sh    # automated crash/reconnect check (see step 4 below)
    ├── run-registry.sh  # sourced by entrypoint.sh/test-crash.sh: tracks
    │                    # each run's pids/containers/sockets in one place
    ├── compositor-launch.sh # sourced by test-crash.sh/entrypoint.sh/`test swap`:
    │                    # one implementation of each WM's headless-launch quirks
    ├── socket-wait.sh   # sourced by test-crash.sh/`test swap`:
    │                    # "did a new compositor socket appear" detection
    ├── harness-paths.sh # sourced by diagnose.sh: the shared container-mount-point
    │                    # constant (mirrored as a Python constant in common.py)
    └── diagnose.sh      # dumps compositor/Wayland/wayvnc state, host + guest --
                         # unmodified; `diagnose` in harness/ is a thin wrapper
                         # around this exact script

debian/control              # package metadata for wayland-headless-harness
packaging/
├── build-harness-deb.sh    # builds the .deb from harness/ + the surviving scripts/*.sh
└── wayland-proxy.service   # systemd --user unit, packaged with wayland-proxy itself
```

Every subcommand that targets one compositor takes a `--wm=<name>` flag
(default `labwc`, also settable via `WAYLAND_HARNESS_WM`) selecting
which `containers/<name>/` to build/run against.

## Host prerequisites

The container image installs GTK4 and Wayland libs. Your host needs:

1. **A container engine — Podman (recommended) or Docker**, a Rust
   toolchain (`cargo`, `cargo-deb`) to build the proxy `.deb` `env setup`
   installs into each container, and Python 3 with Typer for the
   harness's own CLI.
   ```bash
   sudo apt install podman python3-typer      # Ubuntu/Debian
   ```
   Podman should be usable rootless (default on modern Ubuntu). Verify:
   ```bash
   podman info
   ```
   `python3-typer` is declared in `debian/control`'s `Depends:`, but that
   only matters for `apt install ./pkg.deb` -- this project's own
   convention is plain `sudo dpkg -i`, which doesn't resolve
   dependencies at all, so install it explicitly either way.

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
./harness/wayland-headless-harness env setup              # labwc (default)
./harness/wayland-headless-harness env setup --wm=sway
```
Builds the `wayland-proxy-dev-<wm>` image and starts it as a
long-running container. Matches the container's `dev` user's UID/GID
and the host's video/render GIDs — but `dev` is a fixed, generic login
name, not tied to your host account. Only `scripts/` is shared with the
container, mounted at `/workspace` — not your whole home directory, and
not the rest of this checkout either.

### 2. Start the host compositor + VNC (Terminal A)

```bash
./harness/wayland-headless-harness session start-host
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
`test crash` (step 4) and the proxy logs don't need VNC at all.

### 3. Start the nested compositor and enter the container (Terminal B)

```bash
./harness/wayland-headless-harness session start-guest --host-socket=wayland-0            # match what start-host printed
./harness/wayland-headless-harness session start-guest --wm=sway --host-socket=wayland-0  # against a different WM's container
```
Runs `entrypoint.sh` inside the container: starts the nested
compositor, reports its new socket, then drops you into an interactive
shell. Run:
```bash
WAYLAND_DISPLAY=<new-socket> gtk4-demo
```

### 4. Automated crash-recovery check (optional)

Run from the shell step 3 leaves you in (already at `/workspace`;
`$COMPOSITOR` is already set there, baked in by the image):
```bash
bash test-crash.sh       # L0: crashes the compositor, checks the client survives
bash test-crash.sh --l1  # L1: also restarts the compositor and checks
                          # protocol-level recovery (zero unresolvable-interface
                          # warnings, toplevel chain recreated) from the proxy's own log
```
Or from the HOST, without entering the container at all:
```bash
./harness/wayland-headless-harness test crash --wm=sway --verify-recovery
```
Self-contained; doesn't use step 3's nested compositor. See
`plan-test-harness.md` for the fuller testing-levels picture and the
per-WM results recorded there.

### 5. Full matrix check (optional, replaces steps 1-4 per WM)

```bash
./harness/wayland-headless-harness test smoke --wm=sway          # one WM: setup -> crash --verify-recovery -> diagnose -> teardown
./harness/wayland-headless-harness test matrix                   # every Phase 9 WM (labwc sway kwin mutter), one command
./harness/wayland-headless-harness test matrix --wm=sway --wm=kwin  # a subset
```
Builds and tears down each container itself — no need to run steps 1-4
first. Writes a pass/fail table per WM to `results.md` (gitignored,
regenerated each run) and prints where the full logs landed.

### 6. Tear down

```bash
./harness/wayland-headless-harness env teardown          # remove the container
./harness/wayland-headless-harness env teardown --image  # also remove the built image
```
Not needed after step 5 -- `test smoke`/`test matrix` already tear down
after themselves, pass or fail.

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
