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

## Phase 3.5: Raw Wire Protocol Parsing (The Architecture Pivot)

Comparing our `wayland-backend`-based relay against actual prior art
(sommelier-rs, waypipe -- see the review item below) showed neither uses a
backend/endpoint library at all; both hand-parse the wire format and
codegen interface tables, treating object IDs as plain `u32`s. See
docs/architecture-context.md section 4 for the full reasoning. Pivoting to
match.

- [ ] Remove `wayland-backend` (and `wayland-client`/`wayland-server`/
      `wayland-protocols`) dependency, and rewrite `main.rs`'s relay to use
      `src/wire.rs` instead. Not done yet -- `main.rs` still relays via
      wayland-backend; doing this requires rewriting the relay itself, not
      just adding the new module, so it's deliberately a separate step
      from the two below (keeps the build green in between).
- [x] Implement 8-byte Wayland header parser (`src/wire.rs`) to read
      `[Sender ID: u32][Opcode: u16][Length: u16]`.
      Done 2026-07-30: `MessageHeader::parse`/`take_message`. Not yet
      called from anywhere in `main.rs` -- see above.
- [x] Implement basic payload mutation to rewrite `sender_id` on the fly.
      Done 2026-07-30: `wire::write_sender_id`. Same caveat -- not wired
      into the relay yet.

## Phase 4: State Tracking & ID Translation

- [x] Review [sommelier-rs](https://github.com/google/sommelier-rs)'s Shadow Table and codegen approach, and [waypipe](https://github.com/deepin-community/waypipe)'s ID-remap handling, before implementing — both solve this exact problem.
      Done 2026-07-30 (cloned into `reference/`, gitignored). Findings:
      sommelier-rs's shadow table is a literal `u32 <-> u32` bimap
      (`map_id(host_id, guest_id)`) plus per-id interface tracking -- this
      is the direct analog for our problem (two independently-numbered
      sessions needing reconciling). Waypipe's "ID-remap handling" turned
      out to not be ID remapping at all -- it's a transport optimizer
      where IDs pass through unchanged; what it actually has
      (`WpObject`/`WpExtra` in `tracking.rs`) is per-object *semantic
      state* tracking (buffer scale/transform, damage, viewport), which is
      the more relevant prior art for Phase 5's "recreate objects from
      tracked state" than for this phase.
- [x] Implement `wayland-backend` parsing to deserialize the byte stream into typed Wayland messages.
      Done 2026-07-30, then superseded the same day by the pivot above --
      see Phase 3.5. Worked and was verified live, but fighting the
      library's endpoint-oriented object model (distinct client/server
      `ObjectId` types, `wl_display` not being a normal retrievable
      object) is what motivated moving to hand-rolled wire parsing instead.
- [ ] Build the Shadow Table (using `bimap`) to track Object IDs as plain `u32` integers.
      Not built yet. Now that IDs are (going to be) plain `u32`s rather
      than two distinct wayland-backend types, this can be the real thing
      -- a `bimap::BiMap<u32, u32>` per connection, matching sommelier-rs's
      `map_id`, rather than the identity-bridge `HashMap` scaffolding this
      note used to describe.
- [x] Intercept `wl_registry` requests to map globals.
      Basic form done via wayland-backend's `GlobalHandler`/`create_global`
      mechanism -- globals are mirrored 1:1, not yet remapped. Will need
      re-doing against the hand-rolled wire parser once Phase 3.5's
      removal item lands.
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
