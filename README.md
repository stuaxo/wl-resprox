# Wayland Crash Resilience Proxy

> **Status: AI/slop-coded spike.** Built largely with an AI coding
> assistant, exploratory in nature. Live-verified against a real
> compositor (see below), but not yet independently reviewed. Treat
> accordingly.

A crash-resilient Wayland proxy (Rust): relays a client to a compositor
via direct wire-protocol translation, and survives the compositor
crashing and restarting without the client noticing.

## Status

Phases 1-5 done: proxy core, Shadow Table (guest/host id translation),
full crash recovery, live-verified against real `labwc`. See
`docs/plan/plan-0001-proxy-core-and-crash-recovery.md` for the build
history. Current plan: [`plan-test-harness.md`](plan-test-harness.md)
(multi-WM verification, packaging) — Phase 9 in progress, sway done.

## Documentation

- `docs/plan/` — phase-by-phase build history, one file per plan
- `plan-test-harness.md` — next phase: harness packaging + multi-WM matrix
- `docs/architecture-context.md`, `docs/architecture-notes.md` — design
- `docs/adr/` — accepted architecture decisions
- `docs/implementation-constraints.md` — binding spec for crash recovery
- `docs/debugging-notes.md` — investigation log for bugs found live

## Development

See `scripts/README.md`: building/running the dev container, testing
against a nested compositor, watching over VNC.

Enable the pre-commit hook once per clone:
```bash
git config core.hooksPath .githooks
```
Runs `cargo test`, `shellcheck` on `scripts/*.sh`, and an environment
sanity check on every commit — silent on success, see `.githooks/pre-commit`
for details. Needs `shellcheck` on the host:
```bash
sudo apt install shellcheck   # Ubuntu/Debian
```
