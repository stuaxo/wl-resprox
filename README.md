# Wayland Crash Resilience Proxy

A crash-resilient Wayland proxy (Rust): relays a client to a compositor
via direct wire-protocol translation, and survives the compositor
crashing and restarting without the client noticing.

## Status

Phases 1-5 done: proxy core, Shadow Table (guest/host id translation),
full crash recovery, live-verified against real `labwc`. See
`docs/plan/plan-0001-proxy-core-and-crash-recovery.md` for the build
history and `plan-test-harness.md` for what's next (multi-WM
verification, packaging).

## Documentation

- `docs/plan/` — phase-by-phase build history, one file per plan
- `plan-test-harness.md` — next phase: harness packaging + multi-WM matrix
- `docs/architecture-context.md`, `docs/architecture-notes.md` — design
- `docs/adr/` — accepted architecture decisions
- `docs/implementation-constraints.md` — binding spec for crash recovery
- `docs/debugging-notes.md` — investigation log for bugs found live

## Development environment

See `scripts/README.md`: building/running the dev container, testing
against a nested compositor, watching over VNC.
