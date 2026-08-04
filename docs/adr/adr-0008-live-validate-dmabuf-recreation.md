# ADR-0008: Live-Validate dmabuf Buffer Recreation

**Status:** Fixed. ADR-0006's dmabuf recreation is confirmed correct at the protocol level. The real GTK4/GL client hang was a genuine proxy bug — an unanswered `wl_display.sync()` left over from the crash — now fixed and live-verified (2 clean recoveries in a row, previously 0/6).
**Date:** 2026-08-04
**Deciders:** Maintainers of wl-resprox

## Context

[ADR-0006](adr-0006-recreate-buffers-via-fd-handover.md) implemented `Recreatable::DmabufBuffer` but was never live-validated against a real GPU client, to avoid conflating a new failure with the then-open `wl_shm.create_pool` host-id-gap bug. That bug is now fixed. This ADR is the deferred live validation.

## Confirmed working (unrelated to the bug below)

- **Proxy recreation itself**: every run shows a clean sequence (globals, surfaces, `xdg_surface`/`xdg_toplevel`, both dmabuf buffers, `wl_buffer.release`/`wl_callback.done` synthesized) — no protocol errors, no host-id gaps.
- **Protocol-level isolation** (`examples/probe_dmabuf_reconnect.rs`): a hand-rolled client — real GBM-allocated dmabuf, no GTK/Mesa/EGL — passes a full crash/recreate/re-attach/`frame()` cycle cleanly. Confirms both the proxy's recreation mechanism and mutter's handling of it were already correct.

## The bug

A real GTK4 client (`GSK_RENDERER=gl`, dmabuf-backed, `scripts/gtk/dmabuf_gl.py`) permanently wedged after recovery — main thread stuck in a plain `ppoll()` (confirmed via `/proc/<pid>/stack`), no GTK-visible error. An equivalent `wl_shm`/cairo client (`basic_shm.py`) recovered cleanly through the identical proxy-side recreation code.

## Root cause

`pending_frames.rs`'s tracker only covered `wl_surface.frame()` callbacks (built for an earlier, similar bug). It had no equivalent for `wl_display.sync()`. Mesa's GL renderer sends a `sync()` after *every* `wl_surface.commit()` as its own private commit-confirmation roundtrip, and blocks uninterruptibly until that specific callback's `done` arrives. The one in flight at crash time reached the old compositor the same way a `frame()` can — forwarded fine, never answered — and nothing synthesized a `done` for it on reconnect, so the client waited forever for a promise the proxy never knew existed.

Confirmed directly against the `WAYLAND_DEBUG=1` trace: the client's last action before every prior crash was `wl_display.sync(new id wl_callback#62)`, sent on Mesa's private queue, immediately after `commit()` — a pattern that repeats after every commit throughout the whole trace, always answered within ~30ms — except the final, pre-crash one, which never got a `done` or `delete_id` anywhere in the rest of the log.

`wl_shm`/cairo doesn't hit this because it apparently doesn't rely on the same per-commit `sync()` roundtrip (or hits the race far less reliably) — this was never really a dmabuf-specific bug, just one the GL renderer's own throttling made almost certain to trigger.

## Fix

`pending_frames.rs`: added `on_sync_requested`, tracked in the same `awaiting_done` set as `on_frame_requested` (the done-received/drain/synthesis side was already generic — it never cared *why* a callback existed). `relay_ready_messages` now calls it for `wl_display.sync()` the same way it already did for `wl_surface.frame()`. Reconnect recovery synthesizes `done` for any still-pending sync callback exactly like it already did for frame callbacks.

Live-verified: 2 consecutive clean recoveries with `scripts/gtk/dmabuf_gl.py` post-fix (frame ticks climbing continuously through and past the crash, no stall) — the first clean recoveries in this entire investigation.

## Ruled out along the way, with evidence

- **Missing `wl_buffer.release`**: no — confirmed synthesized by the proxy and received/processed by Mesa's own queue.
- **Explicit sync** (`wp_linux_drm_syncobj_v1`): no — advertised by the compositor, never bound by the client.
- **Mutter's `meta_window_set_stack_position_no_sync` assertion**: coincidental — reproduces on ordinary gnome-shell startup with zero proxy/dmabuf involvement (`Meta.Window.raise()` from GNOME Shell's own JS, via a `G_DEBUG=fatal-criticals` backtrace).
- **Kernel/GPU dma-fence wait**: no — `/sys/kernel/debug/dma_buf/bufinfo` at the hang showed every fence on every buffer this process held already `signalled`; the stack trace showed a plain `poll()`, not a DRM wait ioctl; `dmesg` was clean.
- **Async `zwp_linux_dmabuf_v1.create()`/`created()` path**: not in play — `WAYLAND_DEBUG=1` shows this client only ever uses `create_immed()`.

## Rejected: forced-resize workaround

Considered having the proxy synthesize a fake resize during recovery to make Mesa drop and reallocate its buffer. Rejected before the real fix was found: as originally proposed (triggered after the client is already wedged) it couldn't work at all — the trace showed the client wasn't dispatching anything once stuck, so it wouldn't have seen a synthetic event either. Moot now that the actual cause is fixed.

## Tooling added

- `scripts/live-crash-test.sh` — crash/recover/summarize in one report: collapses repeated warnings to counted lines, scopes mutter's stderr to the current gnome-shell pid, gives a hang/no-hang verdict. Used to confirm both the bug and the fix.
- `examples/probe_dmabuf_reconnect.rs` — isolated raw-protocol dmabuf client, for this class of bug generally.

## Follow-up not yet done

A broader "symmetry audit" of request/response pairs the proxy needs to keep honest across a reconnect turned up one more real candidate, not yet verified: `wl_data_source`/`wl_data_offer` (clipboard, drag-and-drop) — a client can block in a kernel `read()` on a fd the compositor was supposed to write/close, and neither object is in the `Recreatable` set today. Unverified — no test has exercised clipboard/DnD this session. Would need its own live reproduction (a clipboard test client) before building anything, the same way this bug was confirmed before being fixed.
