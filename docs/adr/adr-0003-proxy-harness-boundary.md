ADR 0003: Proxy/Harness Boundary

Status

Accepted

Context

plan-test-harness.md's next phases (packaging the proxy, testing it against a matrix of per-WM containers) need a clear line between "the proxy" and "the test harness." Today both live undistinguished in one repo: src/ and Cargo.toml (the proxy) sit alongside scripts/, Containerfile, and scripts/labwc-config/ (containers, WM installers, the crash-inducer, diagnostics, VNC wiring) with no formal boundary between them.

Without a stated boundary, packaging work (Phase 7/8) has no clear target, and per-WM testing (Phase 9) has no clear rule for how the proxy gets into each container.

Decision

Proxy = the `wayland-proxy` binary/crate: src/, Cargo.toml, Cargo.lock. Versioned and packaged independently of everything else in this repo.

Harness = everything else under scripts/: container definitions, lifecycle scripts (setup-env.sh, teardown-env.sh, start-host.sh, start-guest.sh, entrypoint.sh), the crash-inducer (test-crash.sh), diagnostics (diagnose.sh), and scripts/labwc-config/. Exists to develop and test the proxy. ~~Never shipped to an end user.~~ **Superseded (2026-08-02, Phase 8)**: repositioned as a general-purpose headless multi-compositor Wayland test harness, useful to the wider Wayland developer community for reproducing client/compositor protocol issues -- independent of `wayland-proxy` specifically, which is now one *optional* thing to test with it rather than the harness's reason for existing. Packaged and distributed standalone as `wayland-headless-harness` (see `packaging/`). The proxy/harness boundary drawn by this ADR still holds -- if anything, it's what made this repositioning possible without a rewrite: the harness never depended on wayland-proxy's internals, only on consuming a built artifact of it. **Updated (2026-08-02, CLI redesign)**: the lifecycle scripts named above (setup-env.sh, teardown-env.sh, start-host.sh, start-guest.sh, and self-test.sh/test-matrix.sh/test-crash-swap.sh added since) were rewritten from Bash to Python and now live in `harness/` as `wayland-headless-harness`'s Typer-based CLI, not under scripts/. The container-side pieces named here that actually run inside a WM container -- entrypoint.sh, test-crash.sh, diagnose.sh, and the run-registry.sh/compositor-launch.sh/socket-wait.sh libraries they share -- stay exactly where this ADR put them, unmodified Bash, since none of the four Containerfiles install python3. `scripts/labwc-config/` itself moved earlier, to `scripts/containers/<wm>/labwc-config/` per WM, once sway/kwin/mutter containers existed alongside labwc.

The harness stays in this repository for now, not split into a separate one. Revisit once Phase 7/8's packaging actually proves the boundary works in practice -- splitting first risks reconciling two repos' history and tooling before knowing the split is clean.

Going forward, the harness must only ever consume a built proxy artifact (a binary, or a .deb once Phase 7 lands) -- never invoke `cargo build` against the proxy's own source from a harness script or inside a WM container.

Consequences

Positive

The proxy can be versioned and released independently of test-tooling churn.

Testing against multiple WM containers means installing the same built artifact everywhere, not rebuilding per container from source -- exercises installation itself, closer to how the proxy would actually be deployed.

Deferring the repository split avoids paying that cost before the boundary is proven to hold. **Update (2026-08-02)**: Phase 7/8 packaging is done now -- two independently-versioned, independently-installable `.deb`s (`wayland-proxy`, `wayland-headless-harness`) built cleanly from one repo, neither depending on the other's internals. This is the condition this ADR's own "revisit once proven" note was waiting for. Not acting on a repo split now -- that's a separate decision with its own cost (disentangling shared git history) -- but the boundary itself is no longer just asserted, it's demonstrated.

Negative

~~test-crash.sh currently runs `cargo build --quiet` against the proxy's own source -- a direct violation of the "never cargo build inside the harness" rule above.~~ **Resolved** (2026-08-01, Phase 8): `setup-env.sh` now builds the `.deb` once per invocation (`cargo deb`) and installs it into the container via `dpkg -i`; `test-crash.sh`/`test-crash-swap.sh` consume the resulting `wayland-proxy` binary (from `PATH` inside a container, or `target/release/wayland-proxy` on the host for the swap test's host-side proxy) and no longer invoke `cargo build`/`cargo deb` themselves. Verified live: full 4-WM matrix (`./scripts/test-matrix.sh`) passes with the containers no longer ever compiling the proxy.

Keeping proxy and harness in one repo means their git history stays entangled. A future split, if the "stay here for now" call is revisited, will need to disentangle that history if it matters -- a deferred cost, not an eliminated one.
