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
   coverage expected. Proves the matrix mechanism cheaply. Done.
2. **kwin/plasma (headless)** — different protocol family (already
   partly covered, see `docs/architecture-notes.md`). Check KDE's own
   headless-CI setup before inventing one. Done -- `--virtual` is that
   setup, no bespoke harness needed.
3. **mutter/gnome-shell (headless)** — the real target, but higher risk:
   needs more session stack (systemd user session, D-Bus, logind) than
   labwc/sway. Don't assume drop-in; budget investigation time first.
   Done -- needed two private D-Bus buses (session + system), not one;
   no real systemd/logind needed after all.

- [x] Per container: run L1 check, record pass/fail + any new
      `interfaces.rs` gaps (expected when a new compositor family shows
      up, not a bug — see architecture-notes.md Gap 1/Gap 2).
- [x] **sway: L0 pass (5/5), L1 pass.** One new `interfaces.rs` gap found
      and fixed: `zwp_keyboard_shortcuts_inhibit_manager_v1` (sway
      advertises it, labwc's snapshot never did). Uncaught, it silently
      dropped that one `wl_registry.bind`, desyncing the client's own
      `new_id` sequence -- gtk4-demo stayed alive but never got as far as
      `get_toplevel`, so its window never appeared. Same bug class as the
      original labwc-era gap (docs/debugging-notes.md, 2026-07-30). Fixed
      by adding the interface (`wayland_protocols::wp::
      keyboard_shortcuts_inhibit::zv1`) to the lookup table. Re-verified
      live: `get_toplevel` now appears, zero "unresolvable interface"
      warnings, clean reconnect/recreate of the full toplevel chain after
      killing and restarting sway, client survives throughout.
- [x] **kwin: L0 pass (5/5), L1 pass.** Headless invocation is
      `kwin_wayland --virtual` (Qt's own backend, confirmed via
      `kwin_wayland --help` live rather than guessed -- no `WLR_BACKENDS`
      equivalent, no `-c`/config-file needed for a bare instance). Two
      build-time fixes needed before it would even start, neither a
      protocol bug: (1) one of kwin-wayland's transitive deps already
      creates a `render` group in the image, so plain `groupadd` (which
      worked for labwc/sway) collided -- same groupmod-with-fallback
      pattern already used for `video` fixed it; (2) `kwin_wayland` ships
      with file capability `cap_sys_nice=ep`, which fails execve() outright
      with EPERM in this rootless-podman user namespace -- stripped via
      `setcap -r` at build time, kwin degrades to a non-fatal "failed to
      gain real time thread priority" warning and runs fine headless. Five
      new `interfaces.rs` gaps found and fixed, all Gap 1 (already
      generated by `wayland-protocols`' `staging` feature, just not wired
      in): `xdg_wm_dialog_v1`, `xdg_system_bell_v1`, `wp_color_manager_v1`,
      `wp_color_representation_manager_v1`, `wp_fifo_manager_v1` -- none
      seen from any wlroots compositor, all Plasma-6-era staging protocols.
      Re-verified live after the fix: zero unresolvable-interface warnings,
      `get_toplevel` reached, and a full crash+restart cycle recreated the
      whole toplevel chain (`wl_compositor`, two `wl_surface`s,
      `xdg_wm_base`, `xdg_surface`, `xdg_toplevel`) cleanly -- client
      survived throughout. See the 2026-07-31 kwin entry in
      `docs/debugging-notes.md` for the full story, including the
      confirmed-live fact that kwin's own socket auto-selection reuses a
      freed `wayland-N` slot the same way wlroots' does (not assumed --
      the whole reconnect test depends on it, see architecture-notes.md/
      src/main.rs's fixed-target-path design).
- [x] **mutter: L0 pass (8/8), L1 pass.** Headless invocation is
      `gnome-shell --headless --no-x11` (`--no-x11` needed -- otherwise
      it spawns Xwayland + ibus + a large D-Bus fan-out by default,
      unlike labwc/sway/kwin's images, none of which install Xwayland).
      Needed a genuinely new class of fix, not just a bigger Containerfile:
      gnome-shell requires BOTH a D-Bus session bus and a D-Bus system
      bus, and missing either one is fatal, not just a missing-service
      warning -- `timeLimitsManager.js`'s constructor accesses
      `Gio.DBus.system` (to watch GNOME's screen-time daemon), and that
      property getter throws fatally if no system bus is reachable at
      all, distinct from the graceful `ServiceUnknown` handling used for
      every actual *service* that's absent (logind, GDM, PolicyKit1,
      GeoClue2, colord -- all confirmed non-fatal once a bus exists).
      Fixed by starting a second private `dbus-daemon` and exporting it
      as `DBUS_SYSTEM_BUS_ADDRESS` -- it doesn't need real system-bus
      policy or services, gnome-shell already tolerates those being
      missing once the bus itself opens. No real systemd user session or
      logind needed after all, contrary to the plan's own risk framing
      above -- narrower blocker than expected. Two new `interfaces.rs`
      gaps: `wp_commit_timing_manager_v1` (Gap 1, fixed) and `gtk_shell1`
      (Gap 2, a GNOME-internal protocol not generated by any dependency
      crate -- left unresolved, confirmed safe to keep dropping). See the
      2026-07-31 mutter entry in `docs/debugging-notes.md` for the full
      story, including the `gresource extract`-based source dig and a
      re-encountered `pkill -f` self-matching mistake during cleanup.

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

- [x] **labwc → sway: pass** (client survives, full toplevel chain
      recreated, zero unresolvable-interface warnings). Surfaced
      dropped-buffer/syncobj warnings on reconnect that first looked
      swap-specific; a same-compositor control reproduced them
      identically, so they're a generic post-generation-bump artifact
      (in-flight buffer/syncobj objects outside the `RecreationGraph`),
      not a labwc/sway protocol difference. See the 2026-07-31 entry in
      `docs/debugging-notes.md`. Open question noted there for an L2
      pass, not blocking: whether the dropped `wl_surface.attach` stalls
      the client's next visible frame.
- [x] **sway → kwin: pass** (3/3 automated runs via `test-crash-swap.sh`,
      plus a manual `RUST_LOG=debug` pass for L1 confirmation). Socket
      reuse held across the container boundary and the WM-family switch.
      Zero unresolvable-interface warnings; full toplevel chain
      recreated. Same generic post-generation-bump buffer/syncobj
      warnings as the labwc → sway pair (already root-caused there, not
      swap-specific) -- no new interfaces.rs gap surfaced.
- [x] **kwin → mutter: pass** (3/3 automated, plus a manual L1 pass) --
      the pairing closest to the real target. Same clean result: socket
      reuse held, zero unresolvable-interface warnings, full chain
      recreated. One reassuring incidental finding: gnome-shell's own
      startup was rockier than kwin/sway's (its first accepted
      connection reset almost immediately, likely its heavier init
      sequence), and the proxy's existing reconnect-freeze logic handled
      the resulting double EOF cleanly, completing recovery on the next
      attempt -- not a bug, evidence the recovery path tolerates a
      flakier real compositor gracefully. See the 2026-07-31 swap-tests
      entry in `docs/debugging-notes.md`.

Same L1 pass criteria as above. Add more pairs only if one of these
surfaces something, not preemptively.

## Phase 10: Automated matrix runner — done

Seed already existed: pre-commit runs `diagnose.sh --errors-only --host-only`
(env sanity, no podman) on every commit.

- [x] `scripts/self-test.sh --wm=<name>`: the per-WM unit. setup-env.sh
      -> test-crash.sh --l1 -> diagnose.sh --errors-only -> teardown-env.sh,
      asserting clean at each step, always tearing down on the way out
      (pass or fail). Deviates from this section's original sketch by
      skipping `start-guest.sh`: its nested-compositor step needs a real,
      already-running HOST Wayland session, which an automated/headless
      run won't have, and every Phase 9 container has already been
      verified exclusively through `test-crash.sh`'s own self-contained
      headless path -- see the 2026-07-31 Phase 10 entry in
      `docs/debugging-notes.md`.
- [x] One entry point: `scripts/test-matrix.sh [wm...]` loops
      self-test.sh over every Phase 9 container (default: labwc sway
      kwin mutter), collecting pass/fail + a per-WM log per container.
- [x] Pass criteria: L1 minimum. L0-only doesn't count as verified --
      `test-crash.sh --l1` (new, see below) is what makes this
      automatable at all: it restarts the compositor and asserts
      protocol-level recovery from the proxy's own log (zero
      unresolvable-interface warnings, toplevel chain recreated),
      instead of relying on the manual `RUST_LOG=debug` runs every prior
      "L1 pass" claim in this file was actually based on.
- [x] Surface results durably: `results.md` (gitignored, regenerated per
      run) at the project root, a markdown table with a PASS/FAIL and
      log path per WM.

Verified live: full 4/4 pass (`./scripts/test-matrix.sh`, no args),
correct exit code and **FAIL** row on an injected failure
(`./scripts/test-matrix.sh bogus-wm`).

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
- ~~A cleanup script for stray/zombie compositor processes and stale
  sockets~~ — done: `scripts/run-registry.sh` (sourced by
  `test-crash.sh`/`entrypoint.sh`), one directory per run under
  `$XDG_RUNTIME_DIR/wayland-proxy-runs/<run-id>/` recording every tracked
  pid's container, pid, and process-identity, plus symlinks to the
  sockets in play. `run_cleanup` kills by literal pid (confirmed still
  the same process instance first -- no more `pkill -f` pattern-matching
  self-hits), `run_gc_stale_runs` reaps directories whose pids are all
  confirmed dead (fails closed on anything it can't verify), and a
  failed `test-crash.sh` run keeps its directory around for postmortem
  instead of deleting the one thing that'd help debug it. `diagnose.sh`
  surfaces the registry under a new "run registry" section. Chosen over
  a separate `scripts/cleanup.sh` since the existing scripts already had
  the right hook points (pid assignment, trap-based cleanup).
- Detecting which compositors/WMs are installed and which are currently
  running, on host and per-container — relevant once Phase 9's matrix has
  more than two entries and Phase 10 needs to reason about it
  automatically rather than a human eyeballing `ps`.
