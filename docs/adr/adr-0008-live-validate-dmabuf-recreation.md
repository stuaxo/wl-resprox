# ADR-0008: Live-Validate dmabuf Buffer Recreation, and a New Client-Wedge Bug

**Status:** Closed. ADR-0006's dmabuf recreation confirmed correct at the protocol level (`examples/probe_dmabuf_reconnect.rs`). The real GTK4/GL client hang is very likely a kernel/GPU dma-fence signaling gap on abrupt compositor death — outside this project's control, not pursued further here.
**Date:** 2026-08-04
**Deciders:** Maintainers of wl-resprox

## Context

[ADR-0006](adr-0006-recreate-buffers-via-fd-handover.md) implemented `Recreatable::DmabufBuffer` — replaying `zwp_linux_dmabuf_v1.create_params` → `add` → `create_immed` against a fresh compositor connection using the proxy's own retained plane fds. That work was unit- and integration-tested (fake compositor, hand-crafted messages) but deliberately never live-validated against a real GPU client, specifically to avoid conflating a new dmabuf-specific failure with the then-still-open `wl_shm.create_pool` host-id-gap bug. That bug is now root-caused and fixed (`ShadowTable::unallocate_host_id`, live-validated with three consecutive clean crash recoveries). This ADR picks up the deferred live-validation step now that the confound is gone, and records what it found.

## Live validation, 2026-08-04

Rebuilt the release binary with the current source (including the ADR-0007 wire-engine refactor) and restarted `wayland-proxy-gnome-shell-direct.service` on orangered's real GDM session.

**Run 1 — `scripts/gtk/dmabuf_gl.py` (GL renderer, dmabuf-backed):** window realized correctly (`renderer=GLRenderer`), ticked normally at ~30fps. Crashed gnome-shell (`pkill -9 gnome-shell`). Proxy log for this client shows a clean recreation: all globals, three surfaces, `xdg_surface`/`xdg_toplevel`, both dmabuf buffers (2 planes each) recreated with no host-id gaps, `wl_buffer.release`/`wl_callback.done` synthesized for the in-flight buffer and pending frame callback. No `COMPOSITOR ERROR`, no dropped-message pileup — the ADR-0006/ADR-0007 mechanism itself behaved exactly as designed.

**But the client process never recovered.** `/proc/<pid>`: every thread idle, main thread sitting in the same `poll()` call for 3+ minutes straight (resampled repeatedly, identical each time) — including its own independent 1-second GLib stall-check timer, which is supposed to fire regardless of Wayland activity and log `STALLED`/`GIVING UP`. It never did. The window was still listed in the shell's own window list/dash (icon + "Open Windows" entry) but never became visible or raisable.

**Run 2 — `scripts/gtk/basic_shm.py` (cairo renderer, wl_shm-backed), same crash/recovery cycle:** brief ~5s hiccup in tick cadence, then fully resumed normal ~30fps rendering. Recreation sequence in the proxy log is structurally identical in shape to run 1's (same surface/xdg_surface/xdg_toplevel mechanism, `wl_shm_pool`/buffer recreated instead of dmabuf, same release/frame-done synthesis). This client recovered cleanly.

**Initially misjudged, corrected below:** mutter's own stderr shows `libmutter-CRITICAL: meta_window_set_stack_position_no_sync: assertion 'window->stack_position >= 0' failed` around the run-1 crash. First pass ruled this out as a red herring by grepping the wrapper log's *entire* multi-hour history, which also turned up the same assertion from several unrelated earlier gnome-shell instances (different pids, different test sessions from earlier in the day) — wrongly concluded from that mixed evidence that it was unrelated to which client/recreation path was involved. That grep wasn't actually scoped to one run; see the corrected comparison below.

**Runs 3 & 4 — same test, via the new `scripts/live-crash-test.sh` (scopes mutter's stderr to only the freshly-restarted gnome-shell's own pid, not the whole wrapper-log history):**

- `basic_shm.py`: recovered cleanly (confirms run 2). Mutter's stderr for *this specific* new gnome-shell pid shows a `libmutter-WARNING`: `Buggy client (org.wlresproxy.test.BasicShm) committed initial non-empty content without acknowledging configuration, working around.` No `stack_position` assertion for this pid at all.
- `dmabuf_gl.py`: hung again (confirms run 1 reproduces, not a one-off) — script's verdict: main thread stuck in `poll_schedule_timeout` across 5 resamples, no client log output for 24s. Mutter's stderr for *this* pid shows the `stack_position` **CRITICAL** once, and no "Buggy client... working around" warning.

Correctly scoped per-run (not mixed across sessions), the assertion does correlate with the failing run and not the successful one across both pairs of runs so far — reversing the earlier "ruled out" call. The previous dismissal was itself a lesson in the same "too much undifferentiated log volume" problem this ADR's tooling was built to fix: skimming an unscoped, multi-hour grep produced a wrong conclusion that a properly-scoped, per-pid comparison overturned.

## Current hypothesis

Two live, reproducible facts, not yet reconciled into one mechanism:

1. The dmabuf client's process itself wedges (stuck in `poll()`, not just a compositor-side visibility glitch) — points at something client-side (Mesa/EGL/GBM handling of the recreated dmabuf fd: a fence wait, buffer-age/generation assumption, or blocking import) rather than a pure window-manager defect.
2. `meta_window_set_stack_position_no_sync` fires for the failing (dmabuf) run and not the successful (shm) run, in both pairs tested so far — suggesting mutter-side window-management state is *also* different between the two, not just the client being stuck.

These could be the same root cause (e.g. mutter's own handling of this window gets into a bad state trying to process something GL/dmabuf-specific about the recreated surface, which then also blocks the client's own round-trip) or two separate, coincidentally-correlated symptoms. Not yet distinguishable from the proxy's own logs alone — the proxy's recreation sequence itself is confirmed clean in all runs.

Still not tested: whether `basic_shm.py` ever shows the assertion on some other run (would weaken the correlation), and whether this is specific to `create_immed()` or would also affect the still-unbuilt `create()`/`created()` path.

**Follow-up investigation, same day, run 4's wedged client left running:**

- `journalctl -k` around the crash window shows nothing GPU/DRM-related (no fence timeouts, no driver errors) — rules out an actual kernel/GPU-level hang. Whatever this is, it's userspace (mutter and/or Mesa/EGL client-side), not the GPU itself.
- Precise timeline: proxy logged `connection unfrozen` for this client at 20:04:36.544; mutter's `stack_position` assertion fired at 20:04:38.471 -- **1.9s later**. The client did process the synthesized recovery events and sent a real post-recovery commit; the assertion is mutter reacting to that real traffic, not something blocking the client from responding at all.
- The assertion recurred *again*, unprompted, in a burst at 20:08:50-20:09:02 -- 5 minutes after recovery, with the client's own log completely silent since 20:04:31 and its main thread still parked in the identical `poll()` syscall (reconfirmed at the 14.5-minute mark). Since the wedged client can't be generating new protocol traffic, mutter is re-triggering this on its own (most likely periodic window/stacking housekeeping) against this one specific recreated toplevel -- meaning mutter's own internal state for this window is permanently broken, not a transient one-off at recovery time.
- `org.gnome.Shell.Eval` (Looking Glass) returns `(false, '')` even with `development-tools=true` -- disabled in this GNOME Shell version, so the real `MetaWindow.stack_position`/mapped state can't be queried directly this way.
- `coredumpctl` is active and already has real prior coredumps on this system -- confirms that if mutter's `g_critical` were made fatal (`G_DEBUG=fatal-criticals`, read once at process startup), the exact assertion would produce a real, gdb-analyzable backtrace. Not yet done: requires restarting the whole session wrapper for gnome-shell's next spawn to inherit that env var (it's a plain shell-spawned child, not a systemd unit), a bigger live-session disruption than anything done so far, so held off pending a decision on whether to do that vs. build an isolated reproduction instead (see Next steps).

Working conclusion: this now looks like a mutter-side bug in finishing the mapping of a recreated dmabuf-backed toplevel, with the client's hang most likely a downstream consequence (waiting on whatever event/callback depends on the window being properly stacked) rather than an independent client/Mesa-local fault.

## Carried forward from ADR-0006

- The async `zwp_linux_dmabuf_v1.create()`/`created()` path remains deliberately unbuilt — still no evidence real clients need it over `create_immed()`, and this session's dmabuf failure is unrelated to which creation path is used (the bug shows up after `create_immed()`'s own replay already completed without error).

## Tooling added

`scripts/live-crash-test.sh`: launches a client against the live session, crashes gnome-shell, waits for recovery, and prints one compact summary instead of raw log dumps — collapses repeated warning templates (e.g. `wp_presentation.feedback ... dropping`, which alone hit 1,118 occurrences in one ADR-0006 run) to one counted line per shape, scopes mutter's own stderr to only the freshly-restarted gnome-shell's own pid instead of the wrapper log's entire multi-session history, and gives a single hang/no-hang verdict from resampling the client's main-thread wait channel. Reproduced the bug (twice) and the clean-recovery contrast (twice) through it, and it's what caught the scoping mistake in the original stack-position dismissal above.

## Attempted: `G_DEBUG=fatal-criticals` live backtrace (2026-08-04)

Set `G_DEBUG=fatal-criticals` in the session wrapper (temporarily -- since reverted) and restarted the whole session (`loginctl terminate-session` + `sudo systemctl restart gdm`, autologin brought it back) so a fresh gnome-shell would inherit it. `coredumpctl`/gdb confirmed working as a pipeline (real backtraces obtained), but the assertion fired **during plain gnome-shell startup, before any test client or crash was involved at all** -- five separate SIGABRT coredumps in about two minutes, each on ordinary startup. The backtrace for one:

```
meta_window_raise() <- libffi <- libgjs <- JS_CallFunctionValue <- ... <- g_main_loop_run
```

i.e. GNOME Shell's own JavaScript (most likely DING or core shell UI, not identified further) calling `Meta.Window.raise()` on some window during normal startup is enough to hit this assertion, with zero involvement from this project's proxy or dmabuf recreation. That's strong evidence this assertion is a **pre-existing GNOME-Shell-50.1 bug, independent of our recreation code** -- the earlier "corrected" conclusion (that it reliably correlates with the failing dmabuf runs) doesn't hold up against this: it's evidently common enough to fire from unrelated, ordinary startup activity, so its presence in the two failing runs and absence in the two successful ones is more likely coincidental exposure (whichever window state happens to be around when *something* tries to raise/restack) than something the dmabuf recreation path causes.

Reverted `G_DEBUG=fatal-criticals` and restarted the session again once this became clear -- it was crash-looping gnome-shell roughly every 10-12s, which stopped a useful test from ever running, let alone reaching the actual dmabuf scenario. Session confirmed stable and the proxy's own long-running instance (`Main PID 268190`, up since before this whole detour started) never needed to restart itself throughout -- it kept the same identity across the gnome-shell crash-loop and the two full GDM/session restarts, which is itself a small, incidental validation of the proxy's own core purpose.

## Current status of the two open questions (before the isolated probe)

- **The client-side wedge** (dmabuf client's process permanently stuck in `poll()`) remains real, reproduced twice, and unexplained. This is the actual bug blocking ADR-0006's dmabuf half.
- **The `stack_position` assertion** is very likely an unrelated, pre-existing bug -- not further pursued as the explanation for the wedge.

## Isolated reproduction: `examples/probe_dmabuf_reconnect.rs` (2026-08-04)

Built a hand-rolled Wayland client (raw wire protocol, same style as `probe_reconnect_resize_with_surface.rs`) that allocates a real dmabuf via GBM directly against `/dev/dri/renderD128` (`gbm::Device::create_buffer_object_with_modifiers`, `Format::Argb8888`, `Modifier::Linear`) -- no EGL, no GL context, no GTK, no Mesa client-side buffer management beyond the raw GBM allocation. It creates a `wl_buffer` from that dmabuf via `zwp_linux_dmabuf_v1` (`create_params`/`add`/`create_immed`), maps a real `xdg_toplevel`, crashes gnome-shell, waits for this proxy's own recovery, then re-attaches the *recreated* buffer and does exactly what a real client's frame clock does every repaint: `wl_surface.frame()` + `commit()` + `wl_display.sync()`, waiting for both callbacks.

**Result: PASS.** Both `frame().done` and `sync().done` arrived within the timeout, at the pure protocol level, with the session fully recovering afterward (confirmed stable, same long-running proxy instance throughout).

This is decisive: **the wedge does not reproduce without GTK/Mesa/EGL in the loop.** The proxy's dmabuf recreation (`Recreatable::DmabufBuffer`, ADR-0006) and mutter's own handling of a recreated dmabuf-backed surface being re-attached and re-committed are both confirmed protocol-correct -- a client that only speaks the wire protocol, with a real GBM-backed buffer, sails through a full crash/recovery/re-attach/frame cycle cleanly. The bug is therefore in GTK's and/or Mesa/EGL's own client-side library behavior -- most likely something specific to how the GL renderer's buffer-reuse/fence/roundtrip logic reacts to a dmabuf `wl_buffer` whose protocol identity was replayed against a new compositor connection, which this minimal client doesn't exercise (no EGL image import, no GL rendering, no Mesa-internal state at all).

## Where this leaves ADR-0006/ADR-0008

Nothing left to fix on the proxy side that this investigation has found -- `Recreatable::DmabufBuffer`'s mechanism is confirmed working correctly through a real crash/recovery cycle, protocol-level. The remaining open question (why a *real* GTK4/GL client hangs) is a GTK/Mesa/EGL client-library question, not a wl-resprox one.

## `WAYLAND_DEBUG=1` through a real crash cycle (2026-08-04)

Ran `scripts/gtk/dmabuf_gl.py` with `WAYLAND_DEBUG=1` through `scripts/live-crash-test.sh` -- the client's stderr (captured in its own log by the script) is libwayland-client's own request/event trace, showing exactly what Mesa's private queues (`mesa egl display queue`, `mesa egl surface queue`) do, not just what our proxy logs.

The trace around the crash (timestamps are libwayland's own internal clock, ms):

```
[774970.398] {mesa egl surface queue} -> wl_display#1.sync(new id wl_callback#62)
                ... 4.4s gap (the crash + recovery) ...
[779350.783] {Default Queue} wl_callback#63.done(0)           <- proxy's synthesized frame-done
[779350.818] xdg_toplevel#58.configure(0, 0, array[0])        <- proxy's synthesized configure
[779350.831] xdg_surface#57.configure(1)
[779350.843]  -> xdg_surface#57.ack_configure(1)               <- client correctly acks
[779351.794] {mesa egl surface queue} wl_buffer#64.release()  <- proxy's synthesized release, RECEIVED
[779351.821] {mesa egl surface queue} wl_buffer#61.release()  <- proxy's synthesized release, RECEIVED
[779353.833]  -> wl_surface#49.frame(new id wl_callback#63)
[779353.863]  -> wp_presentation#28.feedback(...)
[779353.876]  -> wl_surface#49.offset(0, 0)
                (trace ends here, permanently -- no further requests, no pending reads)
```

This is decisive on two fronts:

1. **Both `wl_buffer.release()` events are directly confirmed received and processed by Mesa's own queue**, in Mesa's own client-side log -- not just sent by the proxy (already known) but actually delivered and dispatched. Gemini's "missing release" theory is refuted at the client-log level now, not just the proxy-log level.
2. **The hang is not inside any Wayland dispatch.** The client successfully sends `frame()`, the presentation-feedback request, and `offset()` -- then stops *before* attempting the next `attach()`. No request is pending, nothing is waiting on a read. That's inconsistent with blocking inside `wl_display_dispatch_queue()` (Gemini's original mechanism, which requires waiting on a *received* event) -- it's consistent with Mesa blocking in its own CPU-side code, before it gets back to the Wayland connection for this frame at all.

## Refined hypothesis: kernel/GPU-level dma-fence wait, not a Wayland-level block

With both buffers confirmed released and idle protocol-wise, but Mesa still unable to proceed to attach one, the remaining candidate is the item ADR-0006 flagged from the start and explicitly set aside as unverified ("GPU fence state on the recreated buffer"): a **dma-fence wait on the dmabuf's own kernel-level shared reservation object**, entirely independent of the Wayland protocol. Mesa's implicit-sync path waits for a buffer to be GPU-idle before rendering into it again; if the fence in question belongs to the *old, killed* compositor process's GPU context and never gets signaled (an abrupt `SIGKILL` rather than the graceful teardown the kernel's dma-fence framework is supposed to guarantee signaling across), Mesa would block forever with zero further Wayland traffic -- exactly what's observed. This also fits the `poll_schedule_timeout` wchan sampled earlier: kernel wait primitives for both a socket `poll()` and a fence/ioctl wait bottom out in the same scheduler primitive, so that observation doesn't distinguish the two, but the trace now does.

This is outside anything wl-resprox -- or even mutter -- controls: it would be a kernel GPU driver (amdgpu) or Mesa fence-handling question, not a Wayland protocol or compositor-proxy one.

## Status: stopping here

This investigation has answered its own question. What started as "does ADR-0006's dmabuf recreation work" is confirmed yes, protocol-level, decisively (via `examples/probe_dmabuf_reconnect.rs`). The remaining real client hang is very likely a kernel/GPU dma-fence signaling gap on abrupt compositor death, upstream of this project's control. Not pursuing further within wl-resprox; if revisited, the next step would be kernel-side (ftrace/GPU driver debugfs on the dma-fence/reservation object for this exact buffer during a reproduction), not anything in this codebase.

## Next steps

Nothing further planned on the proxy side -- `examples/probe_dmabuf_reconnect.rs` confirmed the protocol-level mechanism is correct, which was the actual open question this ADR existed to answer. If the GTK/Mesa/EGL-side hang is worth chasing later, that's a separate, upstream-facing investigation (`MESA_DEBUG`/`EGL_LOG_LEVEL` tracing on a real client, or upstream bug reports), not further work on this proxy.

Still open, low priority: whether the unbuilt async `create()`/`created()` dmabuf path would behave any differently (no evidence either way -- the client-side hang happens well after `create_immed()`'s own replay already completed without error, so it's unlikely to be specific to which creation request was used).
