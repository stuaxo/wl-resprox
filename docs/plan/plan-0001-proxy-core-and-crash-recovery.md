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
      Superseded twice over, both 2026-07-30: raw pipe -> a `wayland-backend`
      `ObjectData`/`GlobalHandler` message relay -> (same day) the current
      hand-rolled wire-parser relay (see Phase 3.5). The *outcome* (gtk4-demo
      renders through the proxy) has held across all three; neither
      intermediate mechanism literally exists in the code anymore.
- [x] Test: Launch `WAYLAND_DISPLAY=wayland-2 gtk4-demo` and ensure it appears in the `labwc` window.
      Verified against all three mechanisms above.

## Phase 3.5: Raw Wire Protocol Parsing (The Architecture Pivot)

Comparing our `wayland-backend`-based relay against actual prior art
(sommelier-rs, waypipe -- see the review item below) showed neither uses a
backend/endpoint library at all; both hand-parse the wire format and
codegen interface tables, treating object IDs as plain `u32`s. See
docs/architecture-context.md section 4 for the full reasoning. Pivoting to
match.

- [x] Remove `wayland-backend`'s (and `wayland-client`/`wayland-server`/
      `wayland-protocols`) *runtime* (the `Backend`/`ObjectData`/
      `GlobalHandler` dispatch model), and rewrite the relay to use
      `src/wire.rs` instead. Done 2026-07-30: the relay was extracted from
      `main.rs` into `src/lib.rs` (`run_connection`/`relay_ready_messages`)
      and now hand-parses every message via `src/wire.rs` + a generic
      signature walker (`walk_signature`), tracking objects in a plain
      `HashMap<u32, &'static Interface>`. The four `wayland-*` crates are
      kept as dependencies but strictly as a static protocol-signature
      dictionary now (`src/interfaces.rs`'s `lookup_interface`) -- no
      `Backend`/`ObjectData`/`GlobalHandler` types appear anywhere in the
      relay code anymore.
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
- [x] Build the Shadow Table (using `bimap`) to track Object IDs as plain `u32` integers.
      Built 2026-07-30 on the `shadow-table-1to1` branch (not yet merged to
      `main`) -- `src/shadow_table.rs`'s `ShadowTable`: a real
      `bimap::BiMap<u32, u32>` (guest id <-> host id) plus independent
      per-side id allocators (host starts at 2, guest-server-side at
      `0xff000000`, both matching Wayland's own convention), modeled on
      sommelier-rs's `map_id`/`ShadowTable`. Unit-tested with deliberately
      mismatched guest/host ids. In live/end-to-end practice the numbers
      currently still coincide (both allocators start at 2 and increment in
      lockstep since there's only ever one host connection so far) -- that's
      expected and was a deliberate scoping choice discussed up front, not
      a gap; see docs/architecture-notes.md. It's a real independent
      mechanism, proven by a dedicated test that hand-crafts ids no real
      allocator would produce next (`shadow_table_translates_new_id_and_delete_id_round_trip`
      in `tests/integration.rs`), not a coincidence.
- [x] Intercept `wl_registry` requests to map globals.
      Re-done against the hand-rolled wire parser (Phase 3.5's removal
      item landed 2026-07-30): `relay_ready_messages` inspects every
      `wl_registry.bind`/typed-new_id request directly off the wire and
      tracks the resulting object's interface. Globals are now remapped
      through the Shadow Table above, not just mirrored.
      `src/interfaces.rs` covers 46 real-world interfaces now (core +
      xdg-shell + freedesktop staging/unstable + wlroots + KDE + misc --
      see docs/architecture-notes.md), up from 7, after a real bug
      (silently dropping a request on an unrecognized interface desyncs
      the client's new_id sequence and gets an unrelated later message
      rejected by the compositor -- see docs/debugging-notes.md's
      2026-07-30 entry) made closing that gap urgent rather than optional.
- [x] Rewrite Object IDs on-the-fly for all traversing messages.
      Done 2026-07-30 on the `shadow-table-1to1` branch, alongside the
      Shadow Table above: `relay_ready_messages` rewrites the message
      header's `sender_id` (both directions), every `Object`-typed
      argument (`walk_signature`'s `object_offsets`), `new_id` allocation,
      and `delete_id`'s payload -- all through the Shadow Table. A message
      referencing an id that can't be translated is dropped outright rather
      than partially rewritten. Live-verified against real labwc:
      `wayland-info` (10/10 clean) and a full `gtk4-demo` session, both
      with zero protocol errors.

## Phase 5: Crash Recovery Mechanics

The four unchecked items below are the direct next step now that Phase
4's Shadow Table exists (`shadow-table-1to1` branch) -- they're also what
finally gives it a real workout: reconnecting to a *new* compositor
process is the first time the guest/host id spaces will actually diverge
from each other, rather than coinciding as they still do today with only
one host connection ever in play.

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
- [x] Detect new Server socket (when `labwc` is restarted).
      Done 2026-07-30 on `shadow-table-1to1`: `reconnect_with_backoff`
      retries connecting to the compositor socket path (fixed 250ms
      backoff, no attempt limit) while frozen, wired into `run_connection`
      via a `tokio::select!` branch.
- [x] Re-request `wl_registry` globals from the new Server.
      Done 2026-07-30: `recover_state_after_reconnect` acts as its own
      synthetic client against the fresh host connection (`get_registry`
      + `sync`), collecting `wl_compositor`/`xdg_wm_base`'s fresh
      name/version.
- [x] Re-create `wl_surface` and `xdg_toplevel` objects on the new Server based on tracked state.
      Done 2026-07-30: `RecreationGraph` (`src/recreation.rs`) records
      parent-before-child recipes for the recreatable chain
      (`wl_compositor`/`xdg_wm_base` -> `wl_surface` -> `xdg_surface` ->
      `xdg_toplevel`) as they're created; replayed against the new host on
      reconnect with fresh host ids, remapped through the Shadow Table.
      Went further than originally scoped here, also covering (per
      `docs/implementation-constraints.md`): grab state (`src/grab_state.rs`
      synthesizes pointer/keyboard release before resuming traffic) and
      buffer lifetimes (a `generation` counter on the Shadow Table refuses
      to translate/forward `wl_buffer.release` for a buffer that predates
      the reconnect).
- [x] Synthesize `xdg_surface.configure` to trigger GTK repaint.
      Done 2026-07-30, including a bug found only in live testing: the
      invented serial got rejected by the real compositor's own
      `ack_configure` handling ("wrong configure serial"). Fixed by
      tracking pending synthetic serials per `xdg_surface` and swallowing
      the matching client ack instead of forwarding it. See
      `docs/debugging-notes.md`'s 2026-07-30 entries for the full
      live-debugging trail (three separate real bugs, all only reproducible
      against a real compositor, none caught by fake-compositor tests
      alone).

Live-verified end to end 2026-07-30: crashed a real headless `labwc`,
started a replacement on the same socket, watched the proxy recreate the
full surface/toplevel chain and force a repaint, and confirmed `gtk4-demo`
stayed connected and rendering throughout -- not just that recreation
messages were sent. All Phase 5 work landed on the `shadow-table-1to1`
branch (not yet squashed/merged to `main` -- see docs/architecture-notes.md
for why: it's being kept 1:1 with host/guest ids deliberately until this
phase proved reconnect+remapping actually work, per an explicit up-front
decision).

Note: `scripts/test-crash.sh` automates the kill-and-check for this phase,
but a "SUCCESS" from it alone doesn't prove any of the above works -- GTK
apps often don't notice a dead socket immediately just by being idle. It's
a harness to iterate against; the live-verification above is what actually
proves it.

## References

- [GTK MR !4073](https://gitlab.gnome.org/GNOME/gtk/-/merge_requests/4073) — Draft toolkit-level reconnection support (draft, blocked on libwayland/mesa deps)
- [Waypipe](https://github.com/deepin-community/waypipe) — Wayland forwarding proxy; ID translation, buffer serialization
- [Sommelier-rs](https://github.com/google/sommelier-rs) — Rust Wayland VM proxy; Shadow Table + protocol codegen
- [stransky/wayland-proxy](https://github.com/stransky/wayland-proxy) — Firefox-motivated Wayland proxy (C++)
- [Mozilla Bugzilla #1743144](https://bugzilla.mozilla.org/show_bug.cgi?id=1743144) — motivating bug for the above
