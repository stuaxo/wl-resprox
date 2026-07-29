# Wayland Proxy Dev Environment

Sets up an Ubuntu 26.04 Distrobox container with Rust, GTK4, and Wayland dev
tooling (including `labwc` as a nested compositor) for Wayland proxy testing.

## Host prerequisites

Everything project-specific (Rust, GTK4, Wayland libs, Ansible itself) is
installed *inside* the container by the playbook. Your host only needs the
tooling to create and enter that container:

1. **A container engine — Podman (recommended) or Docker**
   Distrobox is a wrapper around one of these; it does not create containers
   itself.
   ```bash
   # Ubuntu/Debian host
   sudo apt install podman
   ```
   Podman should be usable rootless (this is the default on modern Ubuntu).
   Verify with:
   ```bash
   podman info
   ```

2. **Distrobox itself**
   ```bash
   # Ubuntu/Debian host
   sudo apt install distrobox
   ```
   If your distro's repo version is too old, use the official install script
   instead:
   ```bash
   curl -s https://raw.githubusercontent.com/89luca89/distrobox/main/install | sh -s -- --prefix ~/.local
   ```
   Verify with:
   ```bash
   distrobox version
   ```

3. **A running Wayland (or X11) session on the host**
   Distrobox forwards your host's display socket into the container — it
   doesn't create one. Since the whole point here is to run `labwc` as a
   *nested* compositor for testing, you need an outer Wayland/X11 session
   already running to nest it inside. If you're at a plain TTY with no
   graphical session, `labwc -C /dev/null` will have nothing to attach to.

No need to pre-install `sudo`, `ansible`, `rustc`, `cargo`, or any GTK/Wayland
packages on the host — the playbook handles all of that inside the container.

## Running it

Both files must be in the same working directory (the playbook is copied in
via `distrobox enter`'s working-directory sharing, and `ansible-playbook`
is run relative to the current directory):

```
your-project/
├── setup-env.sh
└── playbook.yml
```

Then:

```bash
chmod +x setup-env.sh
./setup-env.sh
```

This will:
1. Create an `ubuntu:26.04` Distrobox container named `wayland-proxy-dev`
2. Enter it and run `playbook.yml` via Ansible to install everything
3. Print instructions for re-entering the container and starting the nested
   compositor:
   ```bash
   distrobox enter wayland-proxy-dev
   labwc -C /dev/null &
   ```

## Troubleshooting

- **`distrobox: command not found`** — Distrobox isn't installed or isn't on
  your `$PATH` (common if you used `--prefix ~/.local`; add
  `export PATH="$HOME/.local/bin:$PATH"` to your shell rc file).
- **Podman permission errors** — you likely need rootless Podman configured;
  see the [Podman rootless setup docs](https://github.com/containers/podman/blob/main/docs/tutorials/rootless_tutorial.md).
- **`labwc` starts but shows nothing** — check that `$WAYLAND_DISPLAY` (or
  `$DISPLAY`) is set inside the container; this comes from the host session
  in prerequisite #3 above.
