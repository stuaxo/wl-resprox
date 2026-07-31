ADR 0003: Proxy/Harness Boundary

Status

Accepted

Context

plan-test-harness.md's next phases (packaging the proxy, testing it against a matrix of per-WM containers) need a clear line between "the proxy" and "the test harness." Today both live undistinguished in one repo: src/ and Cargo.toml (the proxy) sit alongside scripts/, Containerfile, and scripts/labwc-config/ (containers, WM installers, the crash-inducer, diagnostics, VNC wiring) with no formal boundary between them.

Without a stated boundary, packaging work (Phase 7/8) has no clear target, and per-WM testing (Phase 9) has no clear rule for how the proxy gets into each container.

Decision

Proxy = the `wayland-proxy` binary/crate: src/, Cargo.toml, Cargo.lock. Versioned and packaged independently of everything else in this repo.

Harness = everything else under scripts/: container definitions, lifecycle scripts (setup-env.sh, teardown-env.sh, start-host.sh, start-guest.sh, entrypoint.sh), the crash-inducer (test-crash.sh), diagnostics (diagnose.sh), and scripts/labwc-config/. Exists to develop and test the proxy. Never shipped to an end user.

The harness stays in this repository for now, not split into a separate one. Revisit once Phase 7/8's packaging actually proves the boundary works in practice -- splitting first risks reconciling two repos' history and tooling before knowing the split is clean.

Going forward, the harness must only ever consume a built proxy artifact (a binary, or a .deb once Phase 7 lands) -- never invoke `cargo build` against the proxy's own source from a harness script or inside a WM container.

Consequences

Positive

The proxy can be versioned and released independently of test-tooling churn.

Testing against multiple WM containers means installing the same built artifact everywhere, not rebuilding per container from source -- exercises installation itself, closer to how the proxy would actually be deployed.

Deferring the repository split avoids paying that cost before the boundary is proven to hold.

Negative

test-crash.sh currently runs `cargo build --quiet` against the proxy's own source -- a direct violation of the "never cargo build inside the harness" rule above. Acceptable for now, since no packaged artifact exists yet (Phase 7 isn't done), but not something to leave indefinitely: it needs switching to a built artifact once Phase 7 lands, not treated as settled.

Keeping proxy and harness in one repo means their git history stays entangled. A future split, if the "stay here for now" call is revisited, will need to disentangle that history if it matters -- a deferred cost, not an eliminated one.
