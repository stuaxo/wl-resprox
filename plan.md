# Wayland Crash Resilience Proxy: Project Plan

## Phase 1: Environment Setup

- [x] Spin up Distrobox container (`wayland-proxy-dev`).
- [x] Run Ansible playbook (`playbook.yml`) to provision Rust, GTK4, and Wayland tools.
- [x] Verify nested compositor (`labwc`) can be launched inside a host window.
- [x] Verify `gtk4-demo` runs successfully inside `labwc`.
- [ ] Confirm independent crash behavior (killing `labwc` kills `gtk4-demo` but leaves host intact).
      Never run as its own discrete test -- true by construction (it's the
      whole reason this project exists) but not explicitly verified in isolation.

## Phase 2: Rust Proxy Foundation

- [x] Scaffold standard `cargo` project.
- [x] Create UNIX socket listener (mocking a Wayland display, e.g., `wayland-2`).
      (Actual socket is named `wayland-proxy-0`, not `wayland-2` -- same idea.)
- [x] Implement `tokio` async loop to accept incoming client connections.
- [x] Connect out to the active compositor (e.g., `wayland-1`).

## Phase 3: The "Pass-Through" Prototype

- [x] Pipe raw bytes between the Client (GTK) and the Server (`labwc`).
      Superseded 2026-07-30 -- replaced by the `wayland-backend` message
      relay below rather than staying a raw byte pipe. The *outcome*
      (gtk4-demo renders through the proxy) still holds; the mechanism doesn't
      literally exist in the code anymore.
- [x] Test: Launch `WAYLAND_DISPLAY=wayland-2 gtk4-demo` and ensure it appears in the `labwc` window.
      Verified with both the old raw pipe and the new relay.

## Phase 4: State Tracking & ID Translation

- [ ] Review [sommelier-rs](https://github.com/google/sommelier-rs)'s Shadow Table and codegen approach, and [waypipe](https://github.com/deepin-community/waypipe)'s ID-remap handling, before implementing — both solve this exact problem.
- [x] Implement `wayland-backend` parsing to deserialize the byte stream into typed Wayland messages.
      Done 2026-07-30. Covers core wayland.xml (via wayland-client's
      generated tables) + xdg-shell (via a new wayland-protocols
      dependency); anything else is logged and skipped, not relayed.
- [ ] Build the Shadow Table (using `bimap`) to track Object IDs.
      **Not actually built yet.** What exists (`Bridge` in src/main.rs) is a
      1:1 identity bridge using plain `HashMap`s, needed only to satisfy
      Rust's type system (`client::ObjectId`/`server::ObjectId` are distinct
      types) -- not `bimap`, and it doesn't handle divergence or survive a
      reconnect. Scaffolding *for* this item, not this item.
- [x] Intercept `wl_registry` requests to map globals.
      Basic form done via wayland-backend's `GlobalHandler`/`create_global`
      mechanism -- globals are mirrored 1:1, not yet remapped.
- [ ] Rewrite Object IDs on-the-fly for all traversing messages.
      Deliberately deferred -- explicitly out of scope for the 2026-07-30 work.

## Phase 5: Crash Recovery Mechanics

- [x] Detect Server socket drop (`ECONNRESET`).
- [x] Pause proxying; keep Client socket open and suspend frame callbacks.
      Fixed 2026-07-30. `Bridge.client_backend` is now `Mutex<Option<...>>`;
      on a dropped compositor connection the proxy calls `Bridge::freeze()`
      instead of tearing the whole session down. GTK-facing requests are
      silently dropped rather than relayed or errored back. Verified live:
      killed a real headless compositor mid-session, confirmed via proxy
      logs the freeze path was taken (not "GTK client disconnected"), and
      that `gtk4-demo` and the proxy both stayed up with the proxy sitting
      idle (no busy-loop) afterward. "Suspend frame callbacks" falls out of
      this for free -- any `wl_surface.frame()` callback just never gets a
      `done` event, so GTK naturally stops rendering. No reconnect logic
      exists yet (see below), so a frozen connection currently stays frozen
      forever -- there's no way back from this state yet.
- [ ] Detect new Server socket (when `labwc` is restarted).
- [ ] Re-request `wl_registry` globals from the new Server.
- [ ] Re-create `wl_surface` and `xdg_toplevel` objects on the new Server based on tracked state.
- [ ] Synthesize `xdg_surface.configure` to trigger GTK repaint.

Note: `scripts/test-crash.sh` automates the kill-and-check for this phase,
but a "SUCCESS" from it today doesn't mean any of the above works -- GTK
apps often don't notice a dead socket immediately just by being idle. It's
a harness to iterate against, not proof of anything yet.

## References

- [GTK MR !4073](https://gitlab.gnome.org/GNOME/gtk/-/merge_requests/4073) — Draft toolkit-level reconnection support (draft, blocked on libwayland/mesa deps)
- [Waypipe](https://github.com/deepin-community/waypipe) — Wayland forwarding proxy; ID translation, buffer serialization
- [Sommelier-rs](https://github.com/google/sommelier-rs) — Rust Wayland VM proxy; Shadow Table + protocol codegen
- [stransky/wayland-proxy](https://github.com/stransky/wayland-proxy) — Firefox-motivated Wayland proxy (C++)
- [Mozilla Bugzilla #1743144](https://bugzilla.mozilla.org/show_bug.cgi?id=1743144) — motivating bug for the above
