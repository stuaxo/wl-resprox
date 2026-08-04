# ADR-0007: Strict Signature-Driven Encoding for State Replay

**Status:** Implemented
**Date:** 2026-08-04
**Deciders:** Maintainers of wl-resprox

## Context

wl-resprox must recreate Wayland state when reconnecting to a restarted compositor. Previously, `recreation.rs`'s replay path (`recover_state_after_reconnect` in `src/lib.rs`) rebuilt these replay payloads manually, as a bare sequence of wire writes (e.g. `wire::put_u32`, `wire::put_str`) per `Recreatable` variant:

```rust
let mut payload = Vec::new();
wire::put_u32(&mut payload, name);
wire::put_str(&mut payload, interface.name);
wire::put_u32(&mut payload, version);
wire::put_u32(&mut payload, host_id);
```

This manual byte-packing was the primary source of LLM and human contributor diff churn: nothing checked that a given sequence of `put_*` calls actually matched the request's real wire signature. A missed field, a wrong order, or an incorrect type silently malformed the message rather than failing loudly.

The decode path (`walk_signature` in `src/lib.rs`) already handles incoming messages this way — generically, driven by each interface's real `&[ArgumentType]` signature — and was never a source of this bug class. The encode path for state replay had no equivalent.

Storing replay payloads as raw byte blobs (`Vec<u8>`, captured verbatim at creation time) was considered and rejected as a fix: several fields (`host_id`, `surface_host_id`, and others) are stale object ids that must be re-translated against the *new* host connection on every reconnect, which a captured byte blob cannot express. This is why `recreation.rs` already keeps replay state as the explicitly typed `Recreatable` enum rather than raw bytes, and this ADR did not change that.

## Decision

Eliminate manual byte-packing in state replay by introducing a strict, signature-validated generic encoder, while retaining the existing explicitly typed `Recreatable` variants unchanged.

### 1. Strict Signature-Driven Encoder (`src/wire.rs`)

```rust
pub enum WaylandValue<'a> {
    Int(i32),
    Uint(u32),
    Fixed(i32),
    String(String),
    Object(u32),
    NewId(u32),
    Array(Vec<u8>),
    Fd(std::os::fd::BorrowedFd<'a>),
}

pub fn encode_arguments(
    signature: &[ArgumentType],
    values: Vec<WaylandValue<'_>>,
) -> anyhow::Result<(Vec<u8>, Vec<std::os::fd::RawFd>)> { ... }
```

- Asserts `values.len() == signature.len()` and that each `WaylandValue` variant matches its corresponding `ArgumentType` slot exactly. Any mismatch returns `Err` rather than silently producing a malformed message — this is a runtime helper on a live connection's task, not a test, so it does not `panic!`.
- Returns a `(Vec<u8>, Vec<RawFd>)` pair, not just bytes: FD-typed arguments carry no wire-format bytes at all (they travel out-of-band via `SCM_RIGHTS`), so `WaylandValue::Fd` values are pulled out of the byte encoding and returned separately, for the caller to pass to `write_message`'s existing `fds` parameter.
- **`Fd` borrows, it does not own.** The as-proposed draft of this ADR had `WaylandValue::Fd(OwnedFd)`, taking ownership. That was wrong: `Recreatable::ShmPool.fd` and `DmabufPlane.fd` are retained in the `RecreationGraph` for as long as the recipe lives, since a session may reconnect more than once — moving them into the encoder would close the recipe's own copy after the first replay, breaking any *later* reconnect. `WaylandValue::Fd` holds a `BorrowedFd<'a>` instead (built via `fd.as_fd()`), and the encoder converts it straight to a `RawFd` for the caller — no ownership ever changes hands.
- **No special case turned out to be needed for `wl_registry.bind`.** The original draft assumed `bind`'s static signature (`[Uint, NewId]`, matching its bare XML declaration: `name: uint`, `id: new_id`) was two entries short of the four values actually on the wire, and planned a dedicated `encode_bind_arguments` wrapper to bypass the strict length check. Checking the actual generated code disproved this: wayland-scanner's own codegen (`build_messagedesc_list` in the `wayland-scanner` crate) already expands any interface-less `new_id` argument into `[Str(No), Uint, NewId]` directly in the static `signature` array, for every protocol, not just this one message. `bind`'s real, generated signature is `[Uint, Str(No), Uint, NewId]` — already a 1:1 match for the four wire values. `encode_arguments` handles it with no wrapper and no bypass; `tests/coverage.rs` asserts this shape stays true so a future wayland-scanner change that shrank it back would be caught here.

### 2. Retain Typed `Recreatable` Variants (`src/recreation.rs`)

No new module and no new enum. The existing `Recreatable` variants (`Global`, `Surface`, `XdgSurface`, `XdgToplevel`, `ShmPool`, `ShmBuffer`, `DmabufBuffer`, `SeatDevice`, `Viewport`, `FractionalScale`) are unchanged. `recover_state_after_reconnect` builds a `Vec<WaylandValue>` from each recipe's already-typed, already-id-translated fields and passes it through `encode_arguments`, in place of the previous hand-rolled `put_*` sequences — including the synthesized `xdg_toplevel.configure`/`xdg_surface.configure` events and the `set_title`/`set_app_id`/`set_destination` follow-up replays inside the same match arms.

### 3. Targeted Signature Validation Tests (`tests/coverage.rs`)

Not a mandatory full-protocol enumeration. For every request/event `recover_state_after_reconnect` builds a `WaylandValue` list for, a scoped test asserts that list's shape (argument count and type, in order) matches the real signature read from the actual generated `Interface` tables (`wayland-client`/`wayland-protocols`), via the crate's existing public `interfaces::lookup_interface`, rather than the `FAKE_INTERFACE` stub the unit tests in `recreation.rs` use. Two child interfaces that are never bind targets (`wp_viewport`, `zwp_linux_buffer_params_v1`) resolve through their creating request's `child_interface` instead, same as `resolve_child_interface` does at runtime. One test additionally round-trips a full `bind` message through `encode_arguments` and checks the resulting bytes against `wire::build_message`'s hand-assembled equivalent, byte-for-byte.

## Consequences

### Positive

- **Eliminates encode churn:** a recipe with a missing or misordered field now fails loudly (an `Err` from `encode_arguments`, or a failing `tests/coverage.rs` case) instead of silently sending a malformed message.
- **Preserves id translation:** `Recreatable` stays typed, so stale guest/host ids are still resolved through the Shadow Table before encoding, per reconnect.
- **Low refactor burden:** the decode path (`walk_signature`) is untouched; testing is scoped to the real replay call sites, not the entire imported protocol surface.
- **Simpler than proposed:** implementation removed a planned special case (`bind`) rather than adding one, once the real generated signature was checked instead of assumed.

### Negative / Trade-Offs

- Requires defining `WaylandValue` and its FD-splitting/borrowing behavior for the encode path.

## Implementation

- `src/wire.rs`: `WaylandValue`, `encode_arguments`, `put_array`.
- `src/lib.rs`: all `Recreatable` match arms in `recover_state_after_reconnect` converted, including the two synthesized configure events and the title/app_id/destination follow-up replays.
- `tests/coverage.rs`: 18 tests — one signature-shape assertion per replayed request/event, plus the `bind` round-trip encode test.
- All 104 existing tests pass unchanged; no behavior change observed in the existing end-to-end reconnect/replay integration tests.
