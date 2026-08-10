# Wayland Crash Resilience Proxy

> **Status: spike.** Built largely with an AI coding assistant. Verified
> live against real compositors (see below). Not independently reviewed.

A crash-resilient Wayland proxy, written in Rust. Sits between a client
and a compositor, relaying the wire protocol between them.

If the compositor crashes and restarts, the proxy reconnects and rebuilds
enough server-side state that the client never notices.

Note: this is not the intended long-term approach — the original PRs
already established that. It was built partly as a learning exercise,
and to have something working in the meantime.

## Status

### GTK4 recovery

Proxy core and crash recovery: done, including cross-compositor
recovery (e.g. labwc crashes, sway takes over the same socket) and
both GTK4 renderers (cairo/`wl_shm` and GL/dmabuf).

Wayland extensions the proxy actively recreates/tracks state for, vs.
relaying generically with no reconnect-time recovery:

| Extension | Purpose | Status |
|---|---|---|
| `wl_compositor` | Surface creation | Done |
| `xdg_wm_base` (xdg-shell) | Window/toplevel management | Done |
| `wl_shm` | Shared-memory buffers | Done |
| `zwp_linux_dmabuf_v1` | GPU (dmabuf) buffers | Done |
| `wl_seat` | Pointer/keyboard/touch | Done |
| `wp_viewporter` | Fractional-scale geometry | Done |
| `wp_fractional_scale_manager_v1` | DPI scale info | Done |
| `wp_presentation` | Frame timing feedback | Pass-through |
| `wp_linux_drm_syncobj_v1` | Explicit GPU sync | Not bound (unused by tested clients) |
| `wl_data_device_manager` | Clipboard, drag-and-drop | Unverified |
| `gtk_shell1` | GNOME/GTK startup notification | Unsupported |

Everything else a compositor might advertise is relayed generically
(pass-through, id translation only), not part of the recreation graph —
see `docs/architecture-notes.md` for full coverage details.

### GNOME Shell

> **Caveat:** doesn't work with `gnome-session` — a gnome-shell crash was
> taking `gnome-session` down with it, dumping you back to the login
> screen. Instead a separate session (`wl-res-gnome-shell-direct`) launches
> gnome-shell itself as a direct child, without `gnome-session` in the
> loop.

Verified by running `pkill -9 gnome-shell` under the supplied session and
confirming test GTK programs still run afterwards.

### labwc / sway / kwin

Protocol coverage and the cross-compositor case above are verified in
the container test harness (`scripts/containers/`) for all three.
labwc additionally ships a real installable session
(`wl-res-labwc-session-wrapper.sh`, same direct-child architecture as
gnome-shell's) and has been run that way, not just in a container. sway
and kwin currently have no installable session — container-only.

### Other

Both the proxy and its test harness have a CLI (`clap` for the
proxy, Python/Typer for the harness). The harness is also its own
installable package, `wayland-headless-harness`, independent of this
repo — a general-purpose tool for reproducing Wayland client/compositor
issues, not just for testing this proxy. See `scripts/README.md`.

Not done: systemd unit lifecycle management for the non-GNOME sessions,
and independent review.

## Usage

```bash
cargo build --release
```
Builds `target/release/wayland-proxy`. Run it pointed at an existing
compositor socket:
```bash
wayland-proxy --display wayland-0
```
It listens on `wayland-proxy-0` by default. Point a client at it:
```bash
WAYLAND_DISPLAY=wayland-proxy-0 gtk4-demo
```
`--display`/`--listen` fall back to `WAYLAND_DISPLAY`/`WAYLAND_PROXY_LISTEN`
if not given; `--log-level` to `RUST_LOG`. See `wayland-proxy --help`
for the rest (`--record` for wire-traffic capture).

For a persistent, restart-on-crash instance under your own session, use
the systemd `--user` unit: `packaging/wayland-proxy.service` (see its
own header comment for enabling it — not yet wired into the `.deb`'s
install/remove hooks, so that's a manual step for now).

## Documentation

- `docs/plan/` — build history, one file per phase of work
- `docs/architecture-context.md`, `docs/architecture-notes.md` — design
- `docs/adr/` — accepted architecture decisions
- `docs/implementation-constraints.md` — binding spec for crash recovery
- `docs/debugging-notes.md` — investigation log for bugs found live
- `docs/KNOWN_BUGS.md` — real bugs found live that turned out not to be
  this project's (GNOME Shell/Mutter/GDM); reproductions in `known_bugs/scripts/`

## Development

See `scripts/README.md`: building the test containers, running against
a nested compositor, watching over VNC.

Enable the pre-commit hook once per clone:
```bash
git config core.hooksPath .githooks
```
Runs `cargo test`, `shellcheck` on `scripts/*.sh`/`packaging/*.sh`, a
`py_compile` check on `harness/`, and an environment sanity check —
silent on success, see `.githooks/pre-commit` for details. Needs
`shellcheck` on the host:
```bash
sudo apt install shellcheck   # Ubuntu/Debian
```
