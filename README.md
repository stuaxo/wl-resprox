# Wayland Crash Resilience Proxy

> **Status: spike.** Built largely with an AI coding assistant.
> Live-verified against real compositors (see below), but not yet
> independently reviewed. Treat accordingly.

A crash-resilient Wayland proxy, written in Rust. Sits between a client
and a compositor, relaying the wire protocol directly. If the
compositor crashes and restarts, the proxy reconnects and rebuilds
enough server-side state that the client never notices.

## Status

Proxy core and crash recovery: done, including cross-compositor
recovery (e.g. labwc crashes, sway takes over the same socket).
Verified live against labwc, sway, kwin and gnome-shell.

Both the proxy and its test harness have a proper CLI (`clap` for the
proxy, Python/Typer for the harness). The harness is also its own
installable package, `wayland-headless-harness`, independent of this
repo — a general-purpose tool for reproducing Wayland client/compositor
issues, not just for testing this proxy. See `scripts/README.md`.

Not done: desktop integration (auto-starting under a real session,
systemd unit lifecycle management) and independent review.

## Documentation

- `docs/plan/` — build history, one file per phase of work
- `docs/architecture-context.md`, `docs/architecture-notes.md` — design
- `docs/adr/` — accepted architecture decisions
- `docs/implementation-constraints.md` — binding spec for crash recovery
- `docs/debugging-notes.md` — investigation log for bugs found live

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
