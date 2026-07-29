# Wayland Proxy Dev Environment

Sets up an Ubuntu 26.04 Distrobox container with Rust, GTK4, and Wayland dev
tooling (including `labwc` as a nested compositor) for Wayland proxy testing.

## Project layout

```
your-project/
├── Cargo.toml
├── README.md
├── plan.md
├── docs/
│   ├── architecture-context.md
│   ├── implementation-constraints.md
│   └── debugging-notes.md
└── scripts/
    ├── setup-env.sh     # provisions the container (run once)
    ├── playbook.yml      # used by setup-env.sh — must stay alongside it
    ├── start-host.sh     # starts a headless labwc + wayvnc on the HOST
    ├── entrypoint.sh      # runs INSIDE the container — starts nested labwc
    └── start-guest.sh    # enters the container, runs entrypoint.sh, then
                          # drops you into an interactive shell for testing
```

## Host prerequisites

Everything project-specific (Rust, GTK4, Wayland libs, Ansible itself) is
installed *inside* the container by the playbook. Your host needs the
tooling to create/enter the container, plus a Wayland session for the
nested compositor to attach to:

1. **A container engine — Podman (recommended) or Docker**
   ```bash
   sudo apt install podman      # Ubuntu/Debian
   ```
   Podman should be usable rootless (default on modern Ubuntu). Verify:
   ```bash
   podman info
   ```

2. **Distrobox itself**
   ```bash
   sudo apt install distrobox   # Ubuntu/Debian
   ```
   If your distro's version is too old:
   ```bash
   curl -s https://raw.githubusercontent.com/89luca89/distrobox/main/install | sh -s -- --prefix ~/.local
   ```
   Verify with `distrobox version`.

3. **`labwc` and `wayvnc` on the HOST** (not just inside the container —
   these run the outer, headless Wayland session):
   ```bash
   sudo apt install labwc wayvnc   # Ubuntu/Debian
   sudo dnf install labwc wayvnc   # Fedora
   sudo pacman -S labwc wayvnc     # Arch
   ```
   If you're headless over SSH (no physical display), this is exactly what
   `start-host.sh` below uses: `WLR_BACKENDS=headless` runs `labwc` without
   real graphics hardware, and `wayvnc` serves it over VNC so you can watch.

No need to pre-install `sudo`, `ansible`, `rustc`, `cargo`, or any GTK/Wayland
packages on the host — the playbook handles all of that inside the container.

## Running it

### 1. Provision the container (once)

```bash
cd scripts
chmod +x *.sh
./setup-env.sh
```
Creates the `wayland-proxy-dev` Distrobox container and installs Rust,
GTK4, and Wayland tooling inside it via Ansible.

### 2. Start the host compositor + VNC (Terminal A)

```bash
./scripts/start-host.sh
```
Prints the host's Wayland socket name (e.g. `wayland-0`) and starts
`wayvnc` listening on `127.0.0.1:5900`. Leave this running in the
foreground.

To watch it, from your local machine:
```bash
ssh -L 5900:localhost:5900 <user>@<host>
```
then point any VNC client at `localhost:5900`.

### 3. Start the nested compositor and enter the container (Terminal B)

```bash
./scripts/start-guest.sh wayland-0   # match the socket start-host.sh printed
```
This runs `entrypoint.sh` inside the container (starts the nested `labwc`
and reports its new socket), then drops you into an interactive container
shell. From there:
```bash
WAYLAND_DISPLAY=<new-socket> gtk4-demo
```

## Troubleshooting

- **`distrobox: command not found`** — not on `$PATH` (common with
  `--prefix ~/.local`; add `export PATH="$HOME/.local/bin:$PATH"` to your
  shell rc).
- **Podman permission errors** — configure rootless Podman; see the
  [Podman rootless setup docs](https://github.com/containers/podman/blob/main/docs/tutorials/rootless_tutorial.md).
- **`zsh: command not found: labwc` on the host** — `labwc`/`wayvnc` need
  installing on the host separately from the container (see prerequisite 3).
- **`compinit: insecure directories` prompt on `distrobox enter`** — harmless
  zsh warning about directory permissions; `y` to continue, or run
  `compaudit` to find and fix the flagged directory permanently.
- **Nested `labwc` fails with `libseat`/`Could not open target tty` /
  `Failed to start a DRM session`** — this means `labwc` never saw the
  host's Wayland socket and tried to become a root compositor talking
  directly to DRM instead. Fix: `export WAYLAND_DISPLAY=<host-socket>`
  *before* running `distrobox enter` (this is what `start-guest.sh` does
  for you).
- **`xwayland/sockets.c: /tmp/.X11-unix not owned by root or us`,
  `cannot create xwayland server`** — XWayland (X11 compat) fails to start
  inside the container due to `/tmp` ownership mismatches with Distrobox's
  shared mount. This project doesn't use XWayland or any X11 clients, so
  it's safe to ignore — check for a new native `wayland-N` socket instead
  of a working XWayland. To silence it, add to `~/.config/labwc/rc.xml`:
  ```xml
  <labwc_config><core><xwayland>no</xwayland></core></labwc_config>
  ```
- **`ls: cannot access '#': No such file or directory` (or similar) when
  sourcing a script in zsh** — zsh errors on inline `#` comments and on
  globs that match nothing, unlike bash. Run scripts with `./script.sh` or
  `bash script.sh` rather than `source`-ing them where possible, and expect
  a short delay before a freshly-started compositor's socket file exists.
