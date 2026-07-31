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

## Phase 6: Proxy/harness boundary — done, see ADR-0003

- [x] Define split: proxy = `wayland-proxy` binary/crate, versioned
      independently. Harness = containers, WM installers, crash-inducer,
      verification scripts, VNC wiring (currently `scripts/*` +
      `Containerfile`).
- [x] **Open Q resolved:** harness stays in this repo for now. Revisit
      once Phase 7/8 packaging proves the boundary holds.
- [ ] Harness must only ever consume a built proxy artifact, never
      `cargo build` the proxy inside a WM container. Known, accepted
      exception right now: `test-crash.sh` still builds from source --
      fine until Phase 7 exists, not to be left indefinitely.

## Phase 7: Package the proxy

- [x] **Open Q resolved:** `.deb`, via `cargo-deb`.
- [x] Confirm it installs on the Ubuntu base every Phase 9 container uses.
      `cargo deb` builds cleanly (MIT license, matching prior art:
      sommelier-rs is Apache-2.0, waypipe's Rust rewrite is
      GPL-3.0-or-later, neither obligates this project since no code was
      copied from either -- reference/ is gitignored, never committed).
      `apt-get install` of the built .deb verified live on the actual
      Phase 9 base image: binary lands at /usr/bin/wayland-proxy, unit at
      /usr/lib/systemd/user/, `$auto` dependency detection resolved to
      just `libc6`. `[package.metadata.deb]` deliberately skips
      cargo-deb's systemd-units automation (system-unit-oriented, wrong
      for our --user unit) in favour of shipping it as a plain asset.
- [x] **Open Q resolved:** ship a systemd --user unit
      (`packaging/wayland-proxy.service`) alongside the `.deb`, don't wait
      for L3. Not a jump ahead -- a packaged background service
      conventionally ships one regardless, and building it against a
      plain binary first (done, see below) is faster to iterate on than
      inside `.deb` machinery. Test harness (`test-crash.sh` etc.) still
      manages the proxy process directly, unaffected -- different concern
      per ADR-0003.
- [x] Unit validated on the host (real systemd --user, unlike the dev
      container which has no init system at all): start, `Restart=on-failure`
      after a `kill -9` (confirmed new PID within RestartSec), clean stop,
      `EnvironmentFile=` override for `WAYLAND_DISPLAY` all confirmed live.
      No proxy-side changes needed -- confirmed no signal handling exists
      today, and none is needed: default SIGTERM-terminates is correct
      for a proxy with no state to flush on shutdown.
- [ ] Wire the validated unit into the `.deb` (`cargo-deb` asset +
      `postinst`/`postrm` for enable/disable) once that packaging work
      starts.

## Phase 8: Package the harness — deferred

Skipping for now: the harness's shape is still changing (Phase 9 adds
per-WM containers, Phase 10 an automated runner), so packaging it before
that settles would mean repackaging repeatedly. Revisit once Phase 9/10
land and the harness stops changing shape every session. Original scope,
unchanged, picked back up then: enumerate artifacts (container defs,
crash-inducer, verification logic, VNC wiring -- container defs may ship
as data files, not baked in), a real `debian/` control file replacing
ad-hoc `scripts/*.sh`, verify clean install with no dependency on this
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

### Cross-compositor swap (limited pairs, not the full matrix)

Same-compositor restart (above) and swapping to a *different* compositor
are separate claims — recreation only touches standardized xdg-shell
interfaces, so it should generalize, but every live test so far has been
labwc-to-labwc. Untested risk: protocol-strictness differences between
implementations (the class of bug that hit `ack_configure` serials on
labwc — a different compositor could differ in ways we have no data on).

Not every pairing — just enough to sample same-family vs. cross-family,
chained onto Phase 9's build order so each test only needs containers
that already exist:

- [ ] labwc → sway (same family, cheapest smoke test of the swap itself)
- [ ] sway → kwin (cross-family — the actual risk case)
- [ ] kwin → mutter (cross-family, closest to the real target)

Same L1 pass criteria as above. Add more pairs only if one of these
surfaces something, not preemptively.

## Phase 10: Automated matrix runner

Seed already exists: pre-commit runs `diagnose.sh --errors-only --host-only`
(env sanity, no podman) on every commit. Next step, not done yet: a
`scripts/self-test.sh` smoke test -- setup-env.sh -> start-guest.sh ->
test-crash.sh -> diagnose.sh --errors-only -> teardown-env.sh, asserting
clean at each step. Run by hand for now; becomes the per-WM unit this
phase loops over once Phase 9's containers exist.

- [ ] One entry point: build harness once, spin each Phase 9 container,
      install proxy, run L1, collect pass/fail + logs per WM.
- [ ] Pass criteria: L1 minimum. L0-only doesn't count as verified.
- [ ] Surface results durably (e.g. generated markdown table).

## Out of scope here

- L3 / real GNOME Shell integration — Phase 9's mutter entry is
  groundwork only.
- CI wiring for Phase 10's runner — after the matrix exists, not before.

## Harness tooling follow-ups (noted, not scoped into a phase yet)

Repeated ad-hoc operations from Phase 9 work worth turning into real
scripts eventually:

- `diagnose.sh`: show each socket's owning process cmdline inline, not
  just its PID (`wayland-3: pid 12345 (labwc -C /workspace/...)`).
  Motivated live: mistook this session's own long-running `start-host.sh`
  labwc for an unrelated host session, purely from not having cmdline and
  socket in the same view.
- A cleanup script for stray/zombie compositor processes and stale
  sockets — done ad hoc many times this session (manual `pgrep`+`fuser`+
  `kill`, including a self-matching `pkill` mistake more than once).
  Folding into `diagnose.sh` as an action mode, or a separate
  `scripts/cleanup.sh`, are both reasonable; not decided.
- Detecting which compositors/WMs are installed and which are currently
  running, on host and per-container — relevant once Phase 9's matrix has
  more than two entries and Phase 10 needs to reason about it
  automatically rather than a human eyeballing `ps`.
