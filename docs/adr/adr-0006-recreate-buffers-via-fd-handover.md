ADR 0006: Recreate wl_buffer Objects on Reconnect via FD Handover

Status

Proposed -- design only, not implemented. Written up immediately after
the live investigation that motivated it, per the user's explicit request
to capture it while fresh, before picking it back up in a future session.

Context

`wl_buffer` has been deliberately excluded from the recreation graph
since `recreation.rs` was first written ("Nothing else -- not wl_seat,
not wl_buffer, not input devices -- gets a recipe: GTK re-obtains those
naturally by reacting to the freshly-fetched registry"). That's correct
for a client that allocates a fresh buffer every frame, but real
GPU-accelerated clients commonly keep reusing a small pool of buffers
across frames instead. Live testing 2026-08-03 (see
plan-desktop-resilience.md's running log) confirmed the resulting gap
twice, independently, days apart, with the exact same signature:

```
wl_surface.attach references untranslatable object N (ClientToHost) -- dropping
COMPOSITOR ERROR: object=1 code=1 message="invalid arguments for wl_surface#8.frame"
```

The client's `attach()` (correctly) gets dropped, since the buffer it
names predates the last reconnect and was never recreated. But a surface
that's never received a valid `attach()` and then calls `frame()`
anyway is something the real compositor's own protocol-state validation
treats as fatal -- it doesn't just ignore the request, it kills the whole
connection. This session's earlier fixes (the `xdg_toplevel.configure`
force-resize synthesis, and tonight's `wl_callback.done` synthesis for a
`frame()` dropped mid-recovery) both narrow *when* a client is likely to
hit this, but neither closes the gap itself for a client that, once
unstuck, goes on to resubmit the exact same stale buffer it already had --
confirmed live tonight: the `done` synthesis correctly unstuck a
gtk4-demo's frame clock, and it immediately used that to attempt another
`attach()` referencing its own pre-crash GPU buffer, hitting the same
fatal error one step later.

The premise this ADR challenges: an earlier assumption recorded in this
project's own history claimed dmabuf-backed buffer content is
"genuinely gone" after a crash. On reflection during tonight's session
that was flagged as an unverified overstatement, and reasoning it through
now confirms it doesn't hold up. A dmabuf is a kernel/GPU-level object
(a GEM/DRM buffer, reference-counted independent of any one process
holding it), not something owned by the compositor. The *client* process
never crashes -- only the compositor does -- so the client still holds
its own open fd to that GPU memory the entire time, with whatever pixels
it last rendered still sitting there. What's actually gone is the
`wl_buffer` *protocol object*: a handle that lived inside the old,
now-dead compositor's own bookkeeping, which the new compositor was
simply never told about. The same reasoning applies to `wl_shm`-backed
buffers too, if anything more straightforwardly (anonymous shared memory,
no multi-plane/format-modifier bookkeeping to replay).

Decision

Extend the recreation graph to cover `wl_buffer` creation, for both
paths client code actually uses:

1. **`wl_shm_pool.create_buffer(new_id, offset, width, height, stride, format)`**
   -- the simple case, a single request, no fds of its own beyond the
   pool's own backing memfd (already relayed once at `wl_shm.create_pool`
   time).
2. **`zwp_linux_dmabuf_v1`'s params dance** -- `create_params` ->
   one or more `zwp_linux_buffer_params_v1.add(fd, plane_idx, offset,
   stride, modifier_hi, modifier_lo)` calls (one per plane) ->
   `create()`/`create_immed()`. Already relayed correctly today by the
   existing generic wire-level machinery (`zwp_linux_buffer_params_v1` is
   a *statically* declared child interface, not a `bind`-style dynamic
   one, so it needs no new entry in `interfaces.rs`) -- what's missing is
   only that none of it gets *recorded* as a recipe today.

For both, the proxy already receives its own duplicate of any fds
involved as an ordinary side effect of relaying `SCM_RIGHTS`-bearing
messages (each receiving end of a unix-socket fd-passing operation gets
its own independent copy) -- today that copy is simply forwarded on to
the host and not otherwise retained. The fix: hold onto that copy (cheap
-- a single fd-table entry referencing GPU memory that already exists,
not new allocation) for the lifetime of the buffer object, alongside a
recorded recipe (which pool/params call created it, with what
format/plane/modifier/offset/stride arguments). On reconnect, replay the
buffer's own creation against the new compositor using the proxy's
retained fd copy and the recorded arguments, and map the result back onto
the client's original guest id -- the same "guest id survives the
reconnect unchanged" contract every other recreated object already
honors. The client's subsequent `attach()` on that id then succeeds for
real, using a real, valid `wl_buffer` the new compositor actually knows
about, instead of being (correctly, but fatally) dropped.

This does not resurrect *fresh* content -- the recreated buffer holds
whatever was last rendered into it before the crash, potentially one
frame stale, until the client's own next real repaint (which, thanks to
the `frame()` fix landing the same night, now reliably happens) overwrites
it. That's an acceptable, expected consequence of "tell the client the
display went away and came back" -- the design direction chosen earlier
this session -- not a shortcoming specific to this ADR.

Validation tooling: `scripts/gtk/basic_shm.py` and `dmabuf_gl.py` (added
the same night, see their own module doc comments) exist specifically to
make testing this tractable -- minimal, self-logging GTK4 clients pinned
to the SHM and GL/dmabuf renderer paths respectively via `GSK_RENDERER`,
instead of needing `strace` on a real app (gtk4-demo, tilix) to see what
buffer path is actually in play, as tonight's investigation repeatedly
did.

Consequences

Positive

Closes the last known class of fatal (connection-killing, not just
degraded) crash-recovery failure for GPU-accelerated and buffer-pool-
reusing clients -- the specific gap `wl-res-gnome-shell-direct`'s crash
tests have hit most consistently tonight, more than any other single
issue.

Reuses the project's existing recipe/replay pattern (`recreation.rs`,
`Recreatable::*`) rather than inventing a new mechanism -- `wl_buffer`
becomes another variant, not a parallel system.

Negative

Real, nontrivial scope: a genuinely new recipe shape (the dmabuf-params
protocol is a multi-request sequence with per-plane fd arguments, not a
single request like everything recreated so far), plus fd lifecycle
management the proxy hasn't needed before (holding fds open across an
indefinite period between a buffer's creation and either its destruction
or a reconnect, rather than passing them through immediately).

Assumes the new compositor instance supports the same dmabuf
format/modifier combination the old one advertised. Judged low-risk on
reflection, not just hopeful: tiling/modifier support is a property of
the GPU driver's own capabilities, queried from the kernel/DRM/GBM stack,
not something a compositor process chooses or negotiates independently --
a fresh gnome-shell restarting on the same machine (same kernel, same
driver, same hardware) will query and advertise the identical modifier
set every time. This is exactly the constraint that makes this problem
easier than the cross-host device-migration case (VFIO/QEMU live
migration) it otherwise resembles -- that one's hard specifically because
the *target* can be genuinely different hardware; this one's target is
always the same GPU. Still not verified live, but no longer an open
question about *whether* it holds, just about confirming it.

Doesn't address `wl_shm`'s own memfd lifetime the same careful way yet --
worth confirming the pool's backing memfd is *also* retained/replayed
correctly (likely already is, transitively, as a side effect of
`wl_shm.create_pool` needing its own recipe first) rather than assumed.

Not yet implemented or tested -- this is a design record, not a
completed change. Next session should start from `scripts/gtk/dmabuf_gl.py`
crash-tested against `wl-res-gnome-shell-direct` as the concrete
reproduction to fix, rather than gtk4-demo's own more complex,
harder-to-instrument behavior.
