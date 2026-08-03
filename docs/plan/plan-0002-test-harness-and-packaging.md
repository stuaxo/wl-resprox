# Plan: Test Harness Packaging & Multi-WM Verification

**Done.** Phases 6-10 complete; moved here from the repo root
(`plan-test-harness.md`) once finished, per this project's plan-file
convention. Two items stayed deferred rather than blocking completion:
`postinst`/`postrm` for the proxy's systemd unit (tied to future L3
desktop-integration work) and a repo split for proxy vs. harness (a
standing decision to revisit, not a scheduled task -- see
`docs/adr/adr-0003-proxy-harness-boundary.md`). See
`docs/debugging-notes.md` for everything found live while doing this.

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
- [x] Harness must only ever consume a built proxy artifact, never
      `cargo build` the proxy inside a WM container. Done (Phase 8):
      `setup-env.sh` builds the `.deb` once and `dpkg -i`'s it into the
      container; `test-crash.sh`/`test-crash-swap.sh` consume the
      installed/built binary instead of compiling it themselves. See
      ADR-0003's now-resolved "Negative consequences" entry.

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

## Phase 8: Package the harness — done

Picked back up now that Phase 9/10 have landed and the harness has
stopped changing shape every session. Doing this iteratively rather than
as one big cutover -- each slice verified live (full test-matrix.sh
pass) before moving to the next.

**Repositioned (2026-08-02)**: the harness (headless per-WM containers,
`diagnose.sh`, the run-registry/compositor-launch/socket-wait machinery)
has value to the wider Wayland community for reproducing client/
compositor protocol issues, independent of `wayland-proxy` specifically
-- confirmed with the user. `wayland-proxy` becomes one optional thing
to test with the harness, not its reason for existing. Confirmed
dual-mode: today's in-place git-checkout dev workflow keeps working
unchanged; packaging is additive. See `docs/debugging-notes.md`'s
2026-08-02 entry for the full audit/design story.

- [x] Close the ADR-0003 debt: harness no longer ever invokes `cargo
      build`/`cargo deb` per-container-per-run. `setup-env.sh` builds
      the `.deb` once (host-side) and `dpkg -i`'s it into the container
      as part of provisioning; `test-crash.sh` uses the installed
      `wayland-proxy` from `PATH`, `test-crash-swap.sh` uses
      `target/release/wayland-proxy` (the build byproduct) for its
      host-side proxy. Verified: full 4-WM `test-matrix.sh` pass with
      zero `cargo build` inside any container.
- [x] Remove now-dead `rustc`/`cargo` from all four Containerfiles --
      nothing inside any container has invoked either since the item
      above. Independent of (not gated on) the shared-base-image idea
      below: one's a removal, the other's a DRY refactor of whatever's
      left. Verified: full 4-WM `test-matrix.sh` pass after rebuilding
      all four images from scratch.
- [x] Shared base image for the four Containerfiles' common subset --
      `scripts/containers/base/Containerfile` (sudo, GTK4, wayland
      tools, dbus, wev, psmisc, iproute2, and removing Ubuntu's default
      `ubuntu` user), each per-WM Containerfile now `FROM
      wayland-proxy-dev-base:latest` instead of `FROM ubuntu:26.04`.
      Group/user creation deliberately stayed per-WM, unchanged (kwin's
      groupmod-or-groupadd fallback and mutter's generalized
      GID-collision handling are genuinely different code paths, not
      just cosmetic duplication -- see base/Containerfile's own comment
      for why splitting further wasn't worth it). `env setup` builds the
      base image before the per-WM one, same always-build-relies-on-cache
      pattern as everything else. Real payoff beyond fewer lines: each
      WM's apt-get line used to embed a different compositor package
      name, so Podman's cache never hit across WM builds even on
      identical content -- confirmed live building all four from
      scratch that the base's `apt-get upgrade` runs once, then every
      other WM's build hits cache on it (a full sway build after labwc
      took 8.5s total). Found (and fixed) a real bug along the way, not
      just a refactor: splitting the base packages into their own
      transaction ahead of kwin-wayland's changed apt/dpkg's dynamic
      system-GID allocation enough that kwin's group/user creation
      started hitting the exact GID-collision class mutter's
      Containerfile already handled generically but kwin's own simpler
      groupmod-or-groupadd fallback didn't -- gave kwin mutter's
      generalized "move whatever's occupying the target GID out of the
      way first" logic too. Verified: full 4-WM `test-matrix` pass
      (4/4, including kwin) after rebuilding every image from scratch,
      confirmed both before and after the kwin fix so the failure and
      the fix are both directly observed, not assumed.
- [x] One shared container-mount-point constant: `scripts/harness-paths.sh`
      (`HARNESS_CONTAINER_ROOT`), replacing three independently-hardcoded
      `/workspace` literals (`setup-env.sh`, `start-guest.sh`,
      `diagnose.sh`). Mechanical, zero behavior change. Verified: full
      4-WM `test-matrix.sh` pass.
- [x] Make "which proxy to install" an explicit input: `setup-env.sh
      --proxy-deb=<path>` installs any `.deb`, from anywhere -- doesn't
      have to be `wayland-proxy`, doesn't have to be built from this
      repo. Falls back to auto-detecting a `wayland-proxy` checkout one
      directory up (today's dev-checkout default, unchanged); if
      neither applies, provisions the container without a proxy rather
      than failing. Replaced the old `${DEB_PATH#"$PROJECT_ROOT"/}`
      string-stripping rewrite with `podman cp` (works regardless of
      the source path's relationship to the bind mount). Verified live:
      default auto-detect, `--proxy-deb=` pointing inside the repo,
      `--proxy-deb=` pointing at `/tmp` (fully outside the checkout),
      and a simulated standalone harness copy with no checkout nearby
      at all (correctly provisions with the "no proxy" message and
      stays fully usable via `diagnose.sh`). Full 4-WM `test-matrix.sh`
      pass confirms zero regression to the default path.
- [x] Mount `scripts/` itself into containers, not its parent checkout
      (`setup-env.sh` now mounts `$SCRIPT_DIR`, not `$PROJECT_ROOT`, at
      `$HARNESS_CONTAINER_ROOT`; every container-side reference dropped
      the redundant `/scripts` suffix) -- confirmed via grep that
      nothing inside a container needed anything outside `scripts/`
      once the proxy `.deb` stopped relying on the bind mount (see the
      item above). Makes `$HARNESS_CONTAINER_ROOT` mean the same thing
      whether mounted from a checkout or an installed package. Verified:
      full 4-WM `test-matrix.sh` pass, plus `test-crash-swap.sh` and
      `entrypoint.sh`'s interactive path.
- [x] `debian/control` + `packaging/build-harness-deb.sh` +
      `packaging/harness/wayland-headless-harness` (CLI dispatcher,
      `setup`/`teardown`/`test-crash`/`matrix`/... forwarding to the
      installed scripts). Package name: `wayland-headless-harness`.
      Deliberately built via `dpkg-deb --build` against a hand-staged
      tree rather than `dpkg-buildpackage`/debhelper -- pure scripts +
      data files, no compilation, and a hand-built `DEBIAN/control` is
      an equally real, equally installable `.deb` with far less
      machinery to get wrong for a first pass. Verified live: builds,
      `dpkg -i`'s cleanly (with its declared deps: bash, psmisc,
      iproute2, podman, sudo -- `sudo` was missing from the first
      attempt, caught by testing in a truly bare scratch container),
      correct file layout (`/usr/lib/wayland-headless-harness/`,
      `/usr/bin/wayland-headless-harness`), and `-x`-traced confirmation
      that `setup --wm=sway` run from the *installed* location resolves
      every path correctly (Containerfile, `SCRIPT_DIR`, `PROJECT_ROOT`)
      before hitting an unrelated nested-podman-in-podman virtualization
      limitation of the scratch container itself (not a packaging bug --
      not chased further since proving that isn't what this item is
      about). Enumerating "remaining artifacts" turned out to already be
      complete: scripts + `containers/` (Containerfiles and WM configs,
      shipped as data files) are everything the harness has.
- [x] `docs/adr/adr-0003-proxy-harness-boundary.md`: updated the "never
      shipped to an end user" line and noted the repo-split question
      this ADR deferred is now, per its own text, ripe for revisiting
      (two independently-installable `.deb`s built cleanly from one
      repo) -- not acted on, a separate decision with its own cost.

**Phase 8 done.** `wayland-headless-harness` is a real, installable
package with no dependency on this repo's working tree for its own
logic; the in-place git-checkout dev workflow this whole project's
development has used stays fully supported, unchanged, alongside it.

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
