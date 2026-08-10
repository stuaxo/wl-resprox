# ADR-0009: Clipboard Persistence

**Status:** Built. `src/clipboard.rs` tees `wl_data_source.send` into a process-wide cache; `attempt_clipboard_splice`/`handle_synthetic_clipboard_event` (src/lib.rs) reclaim it on the first real input serial a connection sees after a reconnect. End-to-end tested (`tests/integration.rs`): a full crash/reconnect cycle, cached content surviving it, and a fake compositor's own eager-fetch afterward getting back the exact bytes cached before the crash.
**Date:** 2026-08-07
**Deciders:** Maintainers of wl-resprox

## Context

Goal: survive a client copying data and then either the compositor crashing or the client itself quitting, in both cases losing clipboard content. A third-party proposal ("Gemini") suggested the proxy tee `wl_data_source.send(mime_type, fd)` — the event carrying the write end of the pipe the copying client writes clipboard bytes into — caching the bytes while forwarding them unchanged, then re-establish the proxy as clipboard owner after a crash using the cached bytes.

`wl_data_device_manager`/`wl_data_source`/`wl_data_offer` are today relayed generically (pass-through, id translation only) — not in the `Recreatable` set, flagged as an open item in [ADR-0008](adr-0008-live-validate-dmabuf-recreation.md)'s follow-up section.

## What the tee mechanism itself looks like

Confirmed against the real relay code (`src/lib.rs`'s `relay_ready_messages`): `wl_data_source.send` already flows through the generic `HostToClient` event path. `wl_shm.create_pool`/`zwp_linux_buffer_params_v1.add` already retain a copy of an incoming fd after forwarding (for buffer recreation) — same interception point, but those are static resource fds; a clipboard tee needs an actual async copy pump between two pipes, not just "keep a second reference." `tokio::net::unix::pipe` covers the non-blocking I/O needed; no hand-rolled epoll state machine required (this codebase is Tokio throughout). Implemented in `src/clipboard.rs`'s `start_tee`/`pump`, MIME-filtered and size-capped (5MB, OOM fallback) per the original proposal's constraints.

## The real question: can anything re-establish ownership?

A per-connection cache (tied to one `run_connection` task) doesn't survive the client quitting — the stated headline problem — so any design that matters needs the cache to outlive a single client connection, and needs *something* to re-offer it afterward. The candidate for "something" was a persistent Wayland client living inside the proxy process, independent of any app's connection.

`wl_data_device.set_selection` requires a `serial`. A background holder client never processes real pointer/keyboard input, so it never receives one legitimately.

## Live verification, part 1: fabricated serial

`examples/probe_clipboard_serial.rs`: a minimal client connected directly to the real compositor socket (bypassing the proxy), binding only `wl_seat` and `wl_data_device_manager` — no `get_pointer`/`get_keyboard`, no input ever processed — then calling `set_selection` with a fabricated serial.

Result, run twice (`serial=0` and `serial=999999`): both times `wl_data_source.cancelled` fired immediately, before any real client attempted to paste. No `wl_display.error` — accepted at the wire level, then silently rejected. `wl-paste` during the window confirmed the real, pre-existing clipboard owner was untouched; our offer was never visible. The serial's specific value made no difference.

## Live verification, part 2: real serial, borrowed for an unrelated purpose

`examples/probe_clipboard_real_serial.rs`: maps a real, visible `xdg_toplevel` (so it can actually receive input), waits for the first serial-bearing event Mutter sends it unprompted (GNOME auto-focuses newly mapped windows, so a `wl_keyboard.enter` arrived within milliseconds with no interaction needed), then immediately calls `set_selection` using *that* serial — issued for a keyboard-focus notification, not for a copy action.

Result: accepted, no `cancelled`, no error. `wl-paste --list-types` showed our offered `text/plain` replacing the real prior selection. Once `wl-paste` triggered a real `wl_data_source.send`, the probe wrote its marker bytes into the (plain pipe, not a socket — `sendmsg`/`SCM_RIGHTS` doesn't apply there, `write()` does) fd. `wl-paste` then returned that marker — **after the probe process had already exited**. Mutter's own eager-fetch pulled the bytes into its own persistent cache while the probe was still connected, and kept serving them long after the probe was gone.

## Implication

The constraint isn't "a client needs to be the original copier" or "a client needs its own dedicated input history" — it's "a client needs *some* real, compositor-issued serial, even one meant for something else." That changes the design entirely: rather than a standalone always-on holder client (which never has any legitimate serial to use, at any time), the mechanism is a *lazy, reactive* one — the proxy already observes real input events with serials on every proxied client connection (this is the same traffic `grab_state.rs` already watches). The first such event to arrive after a reconnect can carry a spliced-in `wl_data_device.set_selection`, on that same client's own connection, using cached bytes from a prior tee. Mutter's own eager-fetch then takes over persistence from there — confirmed live, it survives the splicing client disconnecting entirely, so this also covers the original "app copies then quits" case without needing separate machinery for it.

The caveat about needing `wl_data_device_manager`/`wl_data_source`/`wl_seat` objects resolved itself during implementation: the objects the reclaim needs are entirely synthetic and host-side only (`attempt_clipboard_splice` binds its own `wl_data_device_manager`, creates its own `wl_data_source`/`wl_data_device`), never tied to whatever the real client itself has or hasn't bound. No dependency on the target client's own object graph at all.

## Decision

Built: `src/clipboard.rs` (cache, tee) plus `attempt_clipboard_splice`/`handle_synthetic_clipboard_event` (`src/lib.rs`). One reclaim attempt per reconnect (`ReclaimState::pending`, cleared whether the attempt succeeds or not); `active_source_host_id` reset on every reconnect so a stale id from the previous compositor life can't collide with an unrelated fresh object. End-to-end tested in `tests/integration.rs`.
