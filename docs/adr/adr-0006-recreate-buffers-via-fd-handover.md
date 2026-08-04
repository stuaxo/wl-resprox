ADR 0006: Recreate wl_buffer Objects on Reconnect via FD Handover

Status

wl_shm half implemented, tested, and live-validated 2026-08-04 (see the
"wl_shm implementation, tested and live-validated" section near the end).
dmabuf's `create_immed()` path also implemented and unit/integration
tested 2026-08-04 (see "dmabuf create_immed() implementation" below) --
NOT yet live-validated against `scripts/gtk/dmabuf_gl.py` (the wl_shm
open issue below needs settling first; live-testing dmabuf on top of an
already-open wl_shm question would conflate two unknowns). `create()`
(the async, server-replies-later variant) remains deliberately deferred,
per the sketch's own reasoning -- still no evidence either path is what
real clients actually use. A new, separate, NOT-YET-ROOT-CAUSED bug was
found during wl_shm's live validation -- see "Open issue found live
2026-08-04" below -- real Wayland traffic now gets further than ever
before, then hits a genuine `wl_shm.create_pool` rejection from the real
compositor on an ordinary (non-recovery) resize, unrelated to anything
this ADR's recreation logic touches.

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

Concrete implementation sketch (2026-08-04, fleshed out before starting --
the paragraph above stated the shape of the fix, this is what it actually
touches):

**New `Recreatable` variants** (`recreation.rs`), each owning any fd it
needs rather than tracking fds separately -- see the fd-lifecycle point
below for why:

```rust
ShmPool { fd: OwnedFd, size: i32 },
ShmBuffer { pool_guest_id: u32, offset: i32, width: i32, height: i32, stride: i32, format: u32 },
DmabufBuffer { width: i32, height: i32, format: u32, flags: u32, planes: Vec<DmabufPlane> },
```
```rust
struct DmabufPlane { fd: OwnedFd, plane_idx: u32, offset: u32, stride: u32, modifier: u64 }
```

**wl_shm path** (do this one first -- single-request creation, no
multi-step accumulation, closest to every existing `Recreatable`
variant): record `ShmPool` at `wl_shm.create_pool(new_id, fd, size)`,
record `ShmBuffer` at `wl_shm_pool.create_buffer(new_id, offset, w, h,
stride, format)`. `wl_shm_pool.resize(size)` needs handling too --
update the pool's already-recorded `size` in place rather than adding a
second recipe for the same guest id.

**dmabuf path, phased, do `create_immed()` first**: `zwp_linux_dmabuf_v1
.create_params(new_id)` starts a `zwp_linux_buffer_params_v1`; one or
more `.add(fd, plane_idx, offset, stride, modifier_hi, modifier_lo)`
calls accumulate per-plane data against *that* object's own guest id
(new transient state, not yet a `Recreatable` -- the buffer doesn't
exist until creation finishes); `.create_immed(new_id, w, h, format,
flags)` finalizes it in one synchronous message, matching the "client
picks the new_id, everything needed is in the one request" shape every
other `Recreatable` already assumes.

The other creation path, `.create(w, h, format, flags)` (no new_id in
the request) followed later by a server-initiated `created(new_id
buffer)` *event* (or a `failed()` event) on the params object, is
*not* being ruled out -- worth noting the existing generic new_id
machinery in `relay_ready_messages` already handles server-initiated
new_ids generically (`Direction::HostToClient => (objects
.allocate_guest_server_id(), original_new_id)`, already used for things
like `wl_data_device.data_offer`), so no new wire-relay mechanism is
needed for it, just correlating the later event back to the pending
per-params-object state by the event's own sender_id (the params object
itself). Deliberately deferred rather than built for now: which path
real clients (GTK4's GL/NGL renderer specifically) actually use is
answerable empirically with `scripts/gtk/dmabuf_gl.py` plus a look at
the wire traffic, not something to guess at and build for speculatively
-- same "verify, don't design for the unconfirmed case" discipline as
the two items already listed below.

**New transient per-connection state**: a `HashMap<u32, Vec<DmabufPlane>>`
keyed by the params object's own guest id, threaded through
`relay_ready_messages` the same way `pending_configure_acks` already is
(see that parameter for the existing pattern to follow, not a new one to
invent). Cleared once `create_immed()` (or, once built, `created()`)
finalizes -- the params object is documented as single-use, the protocol
itself doesn't expect further `add()` calls against it afterward.

**FD retention mechanics**: `relay_ready_messages` already receives any
message's fds into a local `fds: Vec<OwnedFd>` (`src.read_fds.pop_front()`
per declared FD argument) before converting to `RawFd` for the outgoing
`write_message` call -- today that local `Vec` simply drops (closing
every fd) once forwarding completes. For `wl_shm.create_pool` and
`zwp_linux_buffer_params_v1.add` specifically, move the relevant `OwnedFd`
out of that vec into the pending-params map or the new `Recreatable`
variant instead of letting it drop -- not a `dup()`/`try_clone()`, an
actual ownership transfer, since the generic forwarding code already has
its own independent copy to send onward (SCM_RIGHTS hands each receiving
end -- including this proxy -- its own fd; keeping one doesn't affect the
one already sent to the host).

**FD cleanup falls out for free**: once retained *inside* a
`Recreatable` variant (not tracked in a side table), `OwnedFd`'s own
`Drop` impl closes the fd automatically whenever the recipe itself is
dropped -- which already happens today, unconditionally, in
`RecreationGraph::remove()` (called on every `delete_id`/destructor
path). No new explicit close()-on-destroy bookkeeping needed; this is
Rust's own ownership model already doing exactly what the "retained fds
must be closed when their guest object is destroyed" item below asks
for, essentially by construction rather than by remembering to add it.

**Replay ordering**: no change needed to `RecreationGraph`'s existing
parent-before-child guarantee (insertion order) -- a client can only
ever create a `wl_shm_pool` before any buffer drawn from it, or finish a
`zwp_linux_buffer_params_v1` before any surface attaches the resulting
buffer, so the existing ordering contract already covers these new
variants without modification.

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
single request like everything recreated so far -- see the implementation
sketch above for the transient per-params-object state this needs, which
nothing else in `recreation.rs` currently has a shape for), plus fd
lifecycle management the proxy hasn't needed before. The *closing* half
of that lifecycle turned out to fall out of Rust's own ownership model
for free once fds are held inside the `Recreatable` variant itself
(`OwnedFd`'s `Drop` impl, triggered by the existing `RecreationGraph
::remove()` call already made on every destroy path) -- but the *opening*
half (moving a fd out of the generic per-message fd vec at exactly two
specific message types, `wl_shm.create_pool` and
`zwp_linux_buffer_params_v1.add`, without disturbing the existing
generic-forwarding code path for every other message) still means
touching the shared relay function's hot path with a few new special
cases, the same category of surgery the destructor/frame-callback
synthesis fixes already needed there tonight.

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

`wl_shm`'s own memfd lifetime is now addressed concretely, not just
assumed -- the implementation sketch's `ShmPool` variant owns the pool's
fd directly, the same retain/replay/close-for-free treatment as every
dmabuf plane fd gets, not a separate mechanism needing its own
verification.

Not yet implemented or tested -- this is a design record, not a
completed change. Suggested implementation order, per the sketch above:
`ShmPool`/`ShmBuffer` first (simplest, closest to the existing pattern,
and validate against `scripts/gtk/basic_shm.py` -- which shouldn't need
any of this today, so it's really a test of the mechanism itself before
trusting it on the harder path); then the dmabuf `create_immed()` path
against `dmabuf_gl.py`; only then decide, from what that reveals about
which creation path real clients actually use, whether `create()`/
`created()` support is worth building at all.

Checked against other prior art for this class of problem (2026-08-03):
PipeWire's own buffer/stream renegotiation, CRIU's documented GPU/DRM
fd-restore limitations, VFIO/QEMU device-state migration, and sommelier
(this project's own `ShadowTable` is already modeled on sommelier-rs).
Two items came up worth naming here, deliberately as "verify empirically
if it turns out to matter" rather than designed for now -- neither has
any evidence behind it yet, this project's own retained-context reasoning
should carry more weight than an external, project-blind opinion once
either is actually tested:

- ~~Retained fds must be closed when their guest object is legitimately
  destroyed~~ -- resolved by the implementation sketch above, not just an
  item to watch for: holding each fd *inside* its `Recreatable` variant
  means `OwnedFd`'s own `Drop` impl closes it automatically via the
  existing `RecreationGraph::remove()` call, already made on every
  destroy path today. No separate lifecycle mechanism needed, so nothing
  left here to verify empirically.
- **GPU fence state on the recreated buffer** -- whether a dma_fence
  attached to a buffer the old (crashed) compositor was mid-operation on
  could ever fail to signal, blocking the new compositor's own GPU work
  against it. The kernel's dma-fence framework is specifically designed
  to guarantee signaling even on context teardown (a driver that doesn't
  is considered buggy), and this project's crash scenario has the
  *compositor* dying, not the buffer-owning client -- mutter's own GPU
  context teardown should trigger that guarantee. Nothing observed live
  so far points at a GPU-level hang (every failure found this session
  was protocol-level: a dropped `attach`, a dropped `frame`), so treat
  this as a "watch for it, don't build for it yet" item.

wl_shm implementation, tested and live-validated (2026-08-04)

`Recreatable::ShmPool`/`ShmBuffer` (`recreation.rs`), recipe capture and
fd retention at `wl_shm.create_pool`/`wl_shm_pool.create_buffer`
(`lib.rs`), `wl_shm_pool.resize` updating the recorded size in place, and
replay in `recover_state_after_reconnect` (`wl_shm` itself is now also a
`Recreatable::Global`, needed so a recreated pool has a fresh `wl_shm`
host id to attach to) -- all per the sketch above, covered by new unit
and integration tests (including one sending a REAL fd via SCM_RIGHTS
end to end, the first test in this project to do so).

That integration test caught a real bug in the *test harness itself*,
not the proxy: `tests/integration.rs`'s fake-compositor helper used a
plain tokio `read()`, which -- once a message carries an fd -- stops
exactly at that message's boundary and never wakes for data sent
afterward (a genuine AF_UNIX `SOCK_STREAM` kernel behavior, not a test
bug). Fixed by switching the harness to the same `recv_with_fds`/`try_io`
pattern `Conn::fill()` already uses and documents for exactly this
reason.

Live validation against `scripts/gtk/basic_shm.py` on the real laptop
surfaced two further, real gaps this ADR's design didn't anticipate --
both are the same "unanswered promise" class as the `wl_surface.frame` ->
`wl_callback.done` fix from the previous session, just for two different
promises:

- **`wl_buffer.release` never sent for an in-flight buffer.** Recreating
  a buffer's protocol *identity* isn't enough if the client's own
  userspace buffer pool still believes every buffer it owns is "busy" --
  a `wl_buffer.release` the OLD, now-dead compositor was going to send
  for a pre-crash `attach`+`commit`, and never will. Fixed via
  `buffer_flow.rs`'s `BufferFlowTracker`: tracks which buffer is
  currently attached+committed per surface, and on a successful
  reconnect, synthesizes `release` for any buffer still "in flight"
  (and that actually got recreated -- nothing to synthesize for one that
  didn't). Confirmed this alone was necessary but NOT sufficient live --
  see the next item, found immediately after fixing this one.
- **`wl_callback.done` never sent for a `frame()` that reached the OLD
  compositor.** The existing `wl_surface.frame` synthesis (previous
  session) only covers a frame() *dropped* because its surface was
  momentarily untranslatable during the narrow post-`bump_generation()`
  window. A frame() that was forwarded normally, with the old compositor
  simply dying before ever answering it, hit neither that branch nor any
  other -- the client's frame clock stayed blocked on a callback nothing
  would ever complete. Fixed via `pending_frames.rs`'s
  `PendingFrameTracker`: tracks every callback awaiting `done`, and
  synthesizes it (reusing the same `done`+`delete_id` pair, now factored
  into a shared `synthesize_frame_done` helper) for any still pending
  after a reconnect.

With both fixes live, a real `basic_shm.py` client (crash mid-render,
buffer attached+committed, frame() in flight) resumed rendering after
reconnect for the first time this project has achieved -- through a
*real* compositor-driven resize/reconfigure cycle, not just the proxy's
own synthesized one. See the open issue immediately below for where it
broke next.

dmabuf create_immed() implementation (2026-08-04)

`Recreatable::DmabufBuffer`/`DmabufPlane` (`recreation.rs`), matching the
sketch above with one addition the sketch didn't spell out:
`dmabuf_guest_id` (`zwp_linux_dmabuf_v1`'s own guest id, needed on replay
to find its freshly recreated host id) -- `zwp_linux_dmabuf_v1` is now
also a `Recreatable::Global`, same treatment `wl_shm` got. Verified the
exact wire signatures against the actual compiled-in `Interface` tables
before writing any relay code, not from memory or the XML spec read
cold -- see `examples/probe_dmabuf.rs` (a small, permanent diagnostic
tool, kept for the next time this question comes up for some other
interface).

Recipe capture: `create_params()` is NOT itself a `Recreatable` -- it
only records `(dmabuf_guest_id, [])` in a new transient
`pending_dmabuf_planes` map (keyed by the params object's own guest id,
threaded through `relay_ready_messages` the same way
`pending_configure_acks` already is), exactly as the sketch anticipated.
Each `add()` call retains its fd (same "move out of the generic
per-message fd vec, after forwarding" mechanic as `wl_shm.create_pool`)
and pushes a `DmabufPlane` onto that pending entry. `create_immed()` is
where the ONE `Recreatable::DmabufBuffer` recipe actually gets recorded,
draining the accumulated planes out of the pending map in the process.

Replay reconstructs the whole dance against the fresh compositor: a
throwaway host id for a brand-new params object (never tracked in the
Shadow Table -- disposable, single-use by protocol design, exactly like
the sketch said), one `add()` per retained plane (with the proxy's own
retained fd), then `create_immed()` with the recorded
width/height/format/flags, mapping the result onto the buffer's
original, unchanged guest id. `create()`/`created()` (the async variant)
remains unbuilt, per the sketch's own deferral.

Covered by a new unit test (`records_and_retrieves_a_dmabuf_buffer_recipe_with_its_planes`)
and a new integration test
(`dmabuf_buffer_recipe_replays_correctly_after_reconnect`, sending a real
fd via SCM_RIGHTS end to end, mirroring the wl_shm recipe-replay test) --
both passed on the first run once the wire signatures were confirmed via
`probe_dmabuf.rs`. NOT yet exercised against a real client
(`scripts/gtk/dmabuf_gl.py`) or a real compositor -- deliberately held
back until the still-open `wl_shm.create_pool` issue below is settled,
so a NEW live failure here doesn't get conflated with that one.

Open issue found live 2026-08-04, not yet root-caused

Once the two fixes above let a real client get far enough to hit a
genuine post-recovery resize, its resulting *ordinary* (non-recovery,
non-recreation) `wl_shm.create_pool` -- destroying its old pool/buffer
and creating a correctly-sized new one, exactly what a healthy client is
supposed to do on resize -- gets rejected by the real compositor:

```
wl_display.error(object=1, code=1, message="invalid arguments for wl_shm#5.create_pool")
```

immediately followed by the compositor closing the connection (a real
GTK4 client then hard-crashes: `Gdk-Message: Error 22 (Invalid argument)
dispatching to Wayland display`). Reproduced 3/3 consecutive live runs
against `basic_shm.py`, always at this same point (client resize
immediately following a crash-recovery cycle), never during the
recovery-time recreation replay itself.

Ruled out, with evidence: a claimed-size-vs-actual-fd-size mismatch --
temporarily instrumented the exact `create_pool` call site with an
`fstat` on the fd about to be sent; claimed size and `fstat`-reported
size matched exactly (`1056096` both) immediately before the (still
successful, at that point) forward. The message's own byte layout
(new_id, size) was independently confirmed correct against the wire
dump. So whatever's wrong isn't in the *content* this ADR's code
constructs.

Not yet tested: whether this is a proxy bug at all, versus a
pre-existing GTK4/mutter interaction limitation that nothing before this
session's fixes ever got far enough, fast enough, to trigger (the
failing sequence is a destroy+create+resize burst all happening within
milliseconds of a compositor reconnect, a pace normal user-driven
resizing never produces). The clean way to settle this: a minimal
reproduction connecting *directly* to gnome-shell's own private socket
(bypassing the proxy) that fires the same rapid destroy/create_pool
burst, independent of any crash/reconnect. Not attempted yet -- this is
the natural next step before writing any more proxy-side code for it,
per this project's own "verify, don't guess" discipline.
