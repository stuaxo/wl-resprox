# Plan: Test Harness Packaging & Multi-WM Verification

Follows `docs/plan/plan-0001-proxy-core-and-crash-recovery.md` (Phases
1-5, done: proxy core + Shadow Table + crash recovery, live-verified
against one labwc instance). Separate file: different kind of work
(infra/packaging, not proxy features).

Goal: install-test the proxy against a matrix of predefined per-WM
containers, instead of one hand-built setup. Long-term target: real
`gnome-shell` restart + reconnect (not started, out of scope here).

## Testing levels (used by Phase 9/10 pass criteria)

- **L0** process survives crash (`test-crash.sh` today). No protocol check.
- **L1** protocol-correct, unattended (logs/`strace`, no visuals). What
  Phase 5 proved live.
- **L2** watchable: nested compositor inside host labwc+`wayvnc`, so the
  viewer survives the crash target dying.
- **L3** real desktop, real windows. Not started.

Matrix default: L1. L2 on demand per-container.

## Phase 6: Proxy/harness boundary

- [ ] Define split: proxy = `wayland-proxy` binary/crate, versioned
      independently. Harness = containers, WM installers, crash-inducer,
      verification scripts, VNC wiring (currently `scripts/*` +
      `Containerfile`).
- [ ] **Open Q:** harness own repo now, or stay here behind a clean
      boundary until packaging's proven? Lean: stay here for now.
- [ ] Harness must only ever consume a built proxy artifact, never
      `cargo build` the proxy inside a WM container.

## Phase 7: Package the proxy

- [ ] **Open Q:** `.deb` (via `cargo-deb`) vs bare prebuilt binary.
- [ ] If `.deb`: confirm installs on the Ubuntu base every Phase 9
      container uses.
- [ ] **Open Q:** systemd unit vs harness-managed direct invocation.
      Lean: direct invocation (matches all tests so far); revisit at L3.

## Phase 8: Package the harness

- [ ] Enumerate artifacts: container defs, crash-inducer, verification
      logic, VNC wiring. Container defs may ship as data files, not
      baked into the package.
- [ ] Real `debian/` control file, replacing ad-hoc `scripts/*.sh`.
- [ ] Verify clean install on a fresh container, no dependency on this
      repo's working tree.

## Phase 9: Per-WM containers

One `Containerfile` per WM, proxy package installed, added in risk order:

1. **sway** — same wlroots family as labwc, least new `interfaces.rs`
   coverage expected. Proves the matrix mechanism cheaply.
2. **kwin/plasma (headless)** — different protocol family (already
   partly covered, see `docs/architecture-notes.md`). Check KDE's own
   headless-CI setup before inventing one.
3. **mutter/gnome-shell (headless)** — the real target, but higher risk:
   needs more session stack (systemd user session, D-Bus, logind) than
   labwc/sway. Don't assume drop-in; budget investigation time first.

- [ ] Per container: run L1 check, record pass/fail + any new
      `interfaces.rs` gaps (expected when a new compositor family shows
      up, not a bug — see architecture-notes.md Gap 1/Gap 2).

## Phase 10: Automated matrix runner

- [ ] One entry point: build harness once, spin each Phase 9 container,
      install proxy, run L1, collect pass/fail + logs per WM.
- [ ] Pass criteria: L1 minimum. L0-only doesn't count as verified.
- [ ] Surface results durably (e.g. generated markdown table).

## Out of scope here

- L3 / real GNOME Shell integration — Phase 9's mutter entry is
  groundwork only.
- CI wiring for Phase 10's runner — after the matrix exists, not before.
