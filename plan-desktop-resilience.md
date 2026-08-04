# Plan: Crash-Resilient Real Desktop Session (GNOME/Ubuntu, Wayland)

Status: **design in progress, not yet implemented**. Originally written on
a different machine from the target (a framework desktop used
occasionally for AI work, confirmed to run the same Ubuntu version as
the target laptop -- findings below were checked live there and should
transfer directly). Intended to be picked up by a fresh Claude Code
instance running ON the actual laptop, with intermittent access ("I
will only be in front of it from time to time" -- expect this to span
multiple sessions). Read this whole file before doing anything; it
captures a real investigation, not guesswork -- don't re-derive facts
already confirmed below.

**Updated 2026-08-03, live on the actual laptop** (Ubuntu 26.04, GNOME
Shell 50.1): the open decision below is now resolved (isolated new
session), and the unit-name assumption in the first "Confirmed facts"
bullet was wrong in a way that matters -- both corrected in place below
rather than rewritten from scratch, per this file's own "don't let it go
stale" instruction. The isolated session (`wl-res-gnome-shell`) is built,
installed, and working for normal use -- login, apps render through the
proxy, `WAYLAND_DISPLAY` export is now correctly tied to gnome-shell's
own restart event (see the fix below), and diagnostic logging (hex bytes
+ remembered interface names for unresolvable binds) shipped in
`src/`.

**Real crash-recovery reliability is not there yet, and this is now
backed by 4 real `pkill -9 gnome-shell` runs, not guesswork**: 3 of 4
resulted in a full session teardown (back to GDM), each losing the
`OnFailure=`-vs-`Restart=on-failure` race against a *different* piece of
session infrastructure being mid-teardown (GPU/logind session grant once,
session D-Bus once, and once so fast the restart attempt never logged
anything at all) -- not a single consistent culprit to fix, a genuinely
racy window. The one run that recovered in-place did so *before* the
`WAYLAND_DISPLAY` export bug (see below) was fixed, so it doesn't count
as a confirmed end-to-end client-survives-a-crash win either -- as of
this update, that has not yet been demonstrated on real gnome-shell. See
the "Recommended design" section's point 1 for the detailed log evidence
and the still-open question of what to do about it.

Unattended iteration tooling is now in place (`sudo systemctl
restart/stop/start gdm` and switching the AccountsService default
session, via scoped NOPASSWD sudoers rules -- see the
`project-gdm-autologin-temporary` memory, **temporary, revert when this
debugging push ends**), so further crash-test rounds no longer need a
human physically re-entering a password each time.

## labwc comparison session (`wl-res-labwc`) -- in progress, real findings so far

Built to isolate whether the proxy/session-recovery mechanism itself
works, decoupled from GNOME's own session-teardown complexity (see the
"Recommended design" OnFailure= findings above). Genuinely different
architecture needed, discovered live, in order:

1. **labwc ships its own `/usr/share/wayland-sessions/labwc.desktop`**
   with a bare `Exec=labwc` -- no `gnome-session`-style wrapper, no
   systemd `--user` unit involved in launch at all.
2. **No `--wayland-display` equivalent, and no `WAYLAND_DISPLAY` env
   hint honored either** (confirmed live, isolated in a scratch
   `XDG_RUNTIME_DIR` to rule out risk to the real session) -- labwc
   always auto-picks via `wl_display_add_socket_auto()`, starting from
   `wayland-0`. A symlink (`wl-res-labwc-0`, re-pointed on every
   restart) gives the proxy a stable target despite this.
3. **Running labwc as a systemd `--user` service fails outright**, in
   three escalating ways, each fixed then revealing the next:
   - `libseat` tries its built-in direct-TTY backend (`Could not open
     target tty: Permission denied`) -- needs `LIBSEAT_BACKEND=logind`
     forced explicitly.
   - Even then, `Backend 'logind' failed to open seat: No data
     available` -- `Slice=session.slice` (what `org.gnome.Shell@.service`
     uses) is NOT sufficient to correlate a systemd `--user` service to
     the actual PAM/logind session; `sd_pid_get_session()`-style lookup
     finds nothing for a service under `user@<uid>.service`'s own cgroup
     tree, regardless of slice.
   - Explicitly resolving and exporting `$XDG_SESSION_ID` gets past
     that, but then: `Timeout waiting session to become active` /
     `Failed to start a DRM session`. **Root cause**: GDM only hands
     seat/display activation over to whatever process it **directly**
     forked for the session's `Exec=`. Routing labwc through `systemctl
     --user start` (a separate process tree entirely from
     `gdm-wayland-session`'s own child) makes it invisible to that
     activation handoff, no matter what systemd unit configuration is
     used -- this isn't a permissions problem, it's a process-tree
     relationship GDM depends on.
4. **Fix**: `wl-res-labwc.desktop`'s `Exec=` points directly at
   `packaging/wl-res-labwc-session-wrapper.sh`, a plain shell script
   that IS `gdm-wayland-session`'s own child, running labwc as ITS
   direct child in a simple restart-on-exit loop -- no systemd
   `Restart=on-failure`, no `OnFailure=`, none of the
   `gnome-session-manager`-style teardown races found above. Confirmed
   live this resolves seat activation (`Class=user`, `State=active` on
   seat0, not stuck on the greeter). `wayland-proxy-labwc.service`
   stays a normal systemd `--user` unit -- the proxy itself needs no
   DRM/seat access, none of the above applies to it.
5. **Real collision risk found and fixed**: labwc's own bind() briefly
   claimed `/run/user/1000/wayland-0` directly during testing --
   *identical path* to whichever session's proxy is using as its public
   socket. Confirmed live the running `wl-res-gnome-shell` proxy's own
   socket survived unharmed (same fd, never actually destroyed), but a
   *new* client connecting during that window would have silently
   reached the wrong compositor. Fixed two ways, belt-and-braces: the
   wrapper now holds an `flock` on `wayland-0.lock` for its whole
   lifetime (the same claiming protocol `wl_display_add_socket_auto()`
   itself uses, so labwc correctly skips straight to `wayland-1`
   instead), and `wayland-proxy-gnome-shell.service` /
   `wayland-proxy-labwc.service` now `Conflicts=` each other (neither
   session's long-lived proxy unit is aware when the *other* session
   becomes active otherwise -- nothing stops the old one automatically).
   A related bug caught and fixed in the same pass: the flock fd was
   being inherited across labwc's own `exec`, so an orphaned labwc
   outliving its wrapper kept the lock held forever -- fixed by closing
   it in a subshell before exec'ing labwc.
6. **Still open, not yet explained**: Xwayland keeps starting despite
   `-C` pointing at the test harness's own `labwc-config` (which sets
   `xwayland=no` in `rc.xml`, confirmed by grepping the file itself) --
   and appears to be what's re-clobbering the `WAYLAND_DISPLAY` export
   (same "whoever exports last wins" pattern already found for
   gnome-shell, just a different source). Not yet root-caused.
7. **Also found, not yet resolved**: neither `systemctl restart gdm`
   nor `stop`+`start` reliably terminates a labwc session's process
   tree -- it can survive as a fully-orphaned process (reparented away
   from any systemd-tracked scope), unlike gnome-shell's sessions which
   were reliably cleaned up by the same restart every time in earlier
   testing. Suspect this is *because* labwc has none of gnome-shell's
   GDM-integration code to respond to whatever signal cleanly ends a
   session with, but not confirmed. Practical impact: rapid iteration
   accumulates orphaned labwc/Xwayland/proxy processes that need manual
   `kill -9` by PID, not just a session restart.
8. **`<xwayland>no</xwayland>` in labwc-config's `rc.xml` is inert** --
   confirmed live via `man labwc-config`: no such config element exists
   in this labwc version's actual schema (only `<xwaylandPersistence>`,
   which controls whether an already-running Xwayland stays alive, not
   whether one starts) -- silently ignored by the XML parser. Xwayland
   launches lazily, on demand, at an unpredictable point, and
   re-exports `WAYLAND_DISPLAY` from its own (labwc's private) inherited
   environment when it does -- same clobbering pattern already found for
   gnome-shell, just from a different source and on an unpredictable
   delay (9 minutes, in one observed run) rather than at session start.
   **Fix**: rather than try to prevent Xwayland from starting (no clean
   way to, confirmed), the wrapper now re-asserts
   `WAYLAND_DISPLAY`/`XDG_SESSION_DESKTOP` every 3s in a background loop
   for the session's whole lifetime, not just once. Worth flagging
   separately, not chased further here: the *container* test harness's
   own `labwc-config` carries the same inert setting, meaning its tests
   may have been unknowingly running Xwayland the whole time too.

### Real crash-recovery result: CONFIRMED WORKING, and fast

With all of the above fixed, a real `pkill -9 -x labwc` against a real
`gtk4-demo` client produced **full protocol-level recovery, not just
process survival**: proxy detected the EOF and froze the client
(`compositor connection lost (EOF) -- freezing, GTK client stays
connected`), the wrapper's restart loop had a new labwc process up
within 1 second, the proxy reconnected and *recreated the actual window
state* -- `recreated global wl_compositor`, both `wl_surface` objects,
`recreated global xdg_wm_base` -- not just the raw connection. **Total
recovery time: ~1.4 seconds**, EOF to fully resumed relaying. The client
process stayed alive throughout, and the session itself never touched
GDM at all -- no teardown, no login screen, `loginctl` showed
`Class=user State=active` continuously through the whole crash.

This is the first confirmed end-to-end "client survives a real crash"
result across all of today's testing (gnome-shell's best real run
needed ~28s/3 attempts and still hadn't been confirmed with a client
actually routed through the proxy at the time; 3 of 4 later real
gnome-shell runs fell back to a full session teardown instead). Strong
confirmation of the hypothesis this comparison session was built to
test: **the proxy + isolated-session + restart-loop mechanism itself
works reliably** -- gnome-shell's remaining problems (the `OnFailure=`
race, the second independent `gnome-session-manager@.service` tripwire,
the logind-session-tied-to-session-manager teardown) are specifically
GNOME's own session architecture, not a flaw in the general approach.

## `wl-res-gnome-shell-direct` -- interim fix, confirmed working

Decision (2026-08-03, discussed with the user): don't replace
gnome-shell/gnome-session as a long-term direction. Instead, run two
sessions side by side --
`wl-res-gnome-shell-direct` (new, bypasses `gnome-session` entirely,
same direct-child-of-`gdm-wayland-session` architecture that just
proved out for labwc) as the actually-resilient interim session, and
`wl-res-gnome-shell` (existing, still goes through `gnome-session`) kept
specifically so `gnome-session`'s own crash-handling behavior can keep
being investigated for a real long-term fix -- see
`docs/adr/adr-0004-gnome-session-bypass.md`.

Built as `packaging/wl-res-gnome-shell-direct-session-wrapper.sh` --
directly analogous to the labwc wrapper, but simpler: gnome-shell
supports `--wayland-display=<name>` directly (unlike labwc), so no
socket-discovery/symlink dance is needed, and no `LIBSEAT_BACKEND=logind`
either (mutter talks to logind via `libsystemd` directly, never
`libseat` -- this was never actually broken for gnome-shell; DRM access
always worked fine even via the old systemd-unit approach, since the
real problems there were GDM's activation handoff and `gnome-session`'s
`OnFailure=` racing, not seat/DRM access).

**Confirmed live, 3/3 real `pkill -9 gnome-shell` runs**: full
protocol-level recovery every time (`recreated global wl_compositor`,
both `wl_surface` objects, `recreated global xdg_wm_base`), session
never left the seat (`loginctl` showed `Class=user State=active`
throughout every crash), client process survived every time. Recovery
took ~2-4s here (slightly slower than labwc's ~1s, gnome-shell itself
being heavier to restart, not a proxy or architecture difference) --
still a different order of magnitude from gnome-shell-via-gnome-session's
best case of ~28s, let alone the 3-of-4 full teardowns.

**The expected tradeoff is real and already visible**: `portal is not
running: ... org.freedesktop.portal.IBus exited with status 1`, and the
DING (desktop icons) extension repeatedly retrying its own launch --
concrete, observed gaps from skipping `gnome-session`'s orchestration of
the settings daemon/portal/keyring layer. Not yet characterized how much
this matters in practice for daily use; worth doing before treating this
as more than an interim/testing session.

**TODO, found live 2026-08-03**: gnome-shell's own "Log Out" does nothing
in this session -- confirmed live by the user clicking it. Expected,
given it normally talks to `gnome-session` (`SessionManager.Logout` over
D-Bus) to end things cleanly, and there's no `gnome-session` here to
receive that call. Not yet checked what it actually does under the hood
(silently no-ops vs. gnome-shell exiting on its own, which the wrapper's
restart loop would just treat as a crash and immediately relaunch,
masking the attempt entirely). A real gap for daily use, separate from
the crash-recovery goal itself.

**TODO, found live 2026-08-03 during real interactive use (not yet
fixed)**: a real crash test with `tilix` and `gtk4-demo` already open
(not freshly launched right before the crash, unlike every automated
test so far) showed both clients disconnecting shortly after an
otherwise-successful reconnect, plus a new warning:
`zwp_linux_dmabuf_feedback_v1.destroy sender has no translation on the
other side -- dropping`. Root cause: `recreation.rs` deliberately excludes
`wl_buffer` (and thus `zwp_linux_dmabuf_feedback_v1`, which hangs off it)
from the recreation graph -- correct for a client that allocates fresh
buffers every frame, but not for one reusing a small buffer pool across
frames (common for GPU-accelerated rendering). A dropped message
referencing a stale buffer id can leave the *real* compositor's own
object-graph view inconsistent with the client's, which Wayland's strict
protocol-violation handling can turn into an outright disconnect. Not a
simple "recreate buffers too" fix -- a dmabuf-backed buffer's actual
pixel data lives in the old compositor process's GPU import and is
genuinely gone after a real crash regardless (unlike `wl_shm` buffers,
where the memory itself isn't tied to the old process). The achievable
fix is likely making the *drop* graceful (e.g. synthesizing the
`delete_id` the client would otherwise wait for forever) rather than
resurrecting buffer content. Deferred, not forgotten -- flagged by the
user explicitly as real future work.

**Mechanism confirmed live 2026-08-03, with the exact compositor error**
(during `wl-res-gnome-shell-direct` testing, unrelated to that session's
fixes -- same gap, clearer evidence): `wl_surface.attach references
untranslatable object 67 (ClientToHost) -- dropping` immediately followed
by `COMPOSITOR ERROR: object=1 code=1 message="invalid arguments for
wl_surface#8.frame"` -- the *real* gnome-shell instance detecting the
surface never got a valid buffer (because the attach was dropped) and
sending a fatal protocol error, exactly as theorized. Also corrects an
earlier assumption: this hit a `gtk4-demo` that had only been running
~2s, not one open "for a while" -- GPU-accelerated clients apparently
allocate/reuse buffers from their very first frame, so this is a more
commonly-hit path than initially thought, not a rare edge case.

**Direction chosen with the user 2026-08-03**: instead of resurrecting
buffer content (not achievable -- see above), tell clients the display
went away and came back, the same way an unplugged/replugged monitor
would, and lean on each client's own normal resize-handling code (which
typically reallocates buffers) to recover on its own. Two pieces, both
implemented and unit/integration-tested (not yet validated against a real
crash -- see below):

1. **Force a repaint through the real resize path, not just an ack.**
   `recover_state_after_reconnect`'s `xdg_toplevel` recreation now
   synthesizes `xdg_toplevel.configure(width=0, height=0, states=[])`
   *before* the existing `xdg_surface.configure`, so a client goes through
   its normal configure-driven resize/repaint code (which usually
   reallocates buffers) instead of just ack'ing a bare `xdg_surface`
   configure and potentially reusing a stale buffer regardless. Covered by
   `full_reconnect_recreates_surface_chain_and_synthesizes_configure` and
   the new `stale_wl_buffer_attach_is_dropped_not_forwarded_after_reconnect`
   in `tests/integration.rs`.
2. **Close the loop on requests the new configure doesn't prevent.** The
   configure above narrows the race window but doesn't close it (a client
   mid-frame with a buffer already attached/committed doesn't necessarily
   wait for it). When a client sends a request *on* a stale (pre-reconnect,
   never-recreated) object -- most commonly `wl_buffer.destroy()` from
   ordinary buffer-pool cleanup, but the same code path covers any
   destructor request on any object outside the narrow recreation graph,
   e.g. the `zwp_linux_dmabuf_feedback_v1.destroy` warning seen above --
   the host side genuinely has nothing to forward it to, but the client is
   still waiting on `wl_display.delete_id` before it can reuse that numeric
   id (`wl_proxy_destroy` in libwayland-client parks it as a zombie
   otherwise). `relay_ready_messages`'s "sender has no translation on the
   other side" branch (`src/lib.rs`) now synthesizes that `delete_id` and
   cleans up the shadow table's own tracking of the id, for any
   `ClientToHost` destructor request hitting this path, rather than just
   dropping it silently. Covered by the new
   `stale_wl_buffer_destroy_synthesizes_delete_id_after_reconnect` in
   `tests/integration.rs`.

**Validated live 2026-08-03, 3/3 clean runs**: repeated the original
"already-open client, `pkill -9 gnome-shell`" scenario against
`wl-res-gnome-shell-direct` with both `gtk4-demo` and a real `tilix`
launched directly (not via GDM's Shell-spawn path, which turned out to
bypass the proxy entirely -- see the session note on that below), each run
letting them sit a few seconds first so they'd allocated their normal
buffer pools. Both survived all 3 runs, full ~20s observation window each
-- the first fully-confirmed *multi-client* crash survival this project
has produced. Getting there required finding and fixing three more real
bugs beyond the two pieces above, found by chasing tilix's repeated deaths
through `WAYLAND_DEBUG=1`, `strace -f`, and `coredumpctl`'s crash
backtrace (not guesswork):

3. **Version mismatch causing a fatal client-side `wl_abort`.**
   `recover_state_after_reconnect` re-bound `wl_compositor`/`xdg_wm_base`
   at whatever version the *new* compositor's registry advertised, not the
   version the client itself originally requested (and whose compiled
   listener structs it's actually prepared to handle) --
   `recreation.rs`'s `Recreatable::Global` didn't even record the
   client's requested version at all. A real tilix hit this every single
   time it crashed today (8 coredumps, `SIGABRT`, confirmed via
   `coredumpctl info`'s backtrace: `wl_abort` inside
   `wl_closure_invoke`/`dispatch_event`, called from GDK's Wayland
   dispatch) while processing an ordinary `wl_surface.preferred_buffer_scale`
   event -- added in `wl_surface` v6, which tilix's own (older-negotiated)
   listener had no slot for once the recreated surface's parent
   `wl_compositor` got rebound at a higher version than originally
   negotiated. Fixed: the recipe now records the version from the
   client's own original bind request (read directly off the wire, not
   `interface.version`, our compiled-in static maximum) and replays at
   `min(originally_requested, new_compositor's_current_max)`. Covered by
   the new `reconnect_rebinds_globals_at_the_clients_originally_requested_version`.
4. **A dropped message could still leave a phantom object mapping.**
   Any request carrying a `new_id` (e.g. `wl_shm.create_pool`, called on a
   stale `wl_shm` -- outside the recreation graph, same as `wl_buffer`)
   gets its new_id mapped/allocated *before* the later "sender has no
   translation" check runs. Dropping the message there (correctly) still
   left the shadow table believing the new object existed on the host,
   which it never did (the message that would have created it was never
   forwarded). A later request against that phantom id got happily
   translated to a bogus host id and forwarded -- which is exactly what
   killed tilix immediately after the version fix above: `wl_shm.create_pool
   sender has no translation... -- dropping` followed instantly by the
   real compositor's `wl_display.error(..., "invalid object 19")`, a fatal
   protocol violation that closes the whole connection. Fixed: roll the
   phantom mapping back when the message ends up dropped. Covered by the
   new `create_pool_on_a_stale_wl_shm_does_not_leave_a_phantom_mapping`.
5. **A failed partial recovery still resumed relaying.** Found while
   chasing an early, more chaotic version of this race (a burst of several
   freeze/reconnect cycles within milliseconds): when
   `recover_state_after_reconnect` fails (every error it can return means
   the connection itself is dead -- a failed write, a failed read, or the
   compositor closing the connection outright while fetching its
   registry, never just "recovery came up short"), the old code logged a
   warning but unfroze anyway, resuming relay on a connection already
   known to be broken. Fixed: stay frozen on failure and let the existing
   `reconnect_with_backoff` retry arm handle it, with a short sleep to
   avoid hot-looping the same stale-socket race that triggers this.
6. **An untracked `delete_id` was forwarded with an untranslated payload.**
   `recover_state_after_reconnect` allocates host id 3 for its own
   internal `wl_display.sync` (used only to detect "all globals have
   arrived"), deliberately never mapped to a guest id. The real
   compositor's later `delete_id(3)` for that callback hit the shadow
   table's "untracked host id" branch, which logged a warning but still
   forwarded the message with its *host-space* payload untouched --
   telling the client "your own guest-space id 3 is now free", where
   guest id 3 is whatever unrelated (and very possibly still-live) object
   the client itself happened to allocate third. Caught live via
   `WAYLAND_DEBUG=1` against a real tilix, landing immediately before an
   otherwise-unexplained clean exit. Fixed: never forward it. Covered by
   the new `delete_id_for_an_untracked_host_id_is_dropped_not_forwarded`.

**Session note on test methodology**: apps launched via GNOME Shell's own
UI (Super key search, dock, Activities) turned out to bypass the proxy
entirely -- gnome-shell (as the Wayland compositor) exports
`WAYLAND_DISPLAY` pointing at *itself* to anything it directly spawns,
independent of the systemd `--user` activation environment the session
wrapper patches (which only affects D-Bus/systemd-activated apps, e.g.
DING, portals). Only apps launched from a shell with the correct
`WAYLAND_DISPLAY=wayland-0` (confirmed via `ss -xp` cross-referencing
socket inodes against the proxy's own fds, not just the env var) actually
exercise the proxy.

Root-caused (source-confirmed, not guessed) and a design proposed:
`docs/adr/adr-0005-route-shell-launched-clients-through-the-proxy.md`.
Mutter's `set_gnome_env("WAYLAND_DISPLAY", compositor->display_name)`
(`src/wayland/meta-wayland.c`) does a one-time `setenv()` on its own
process right after creating its compositor socket, using the literal
`--wayland-display=` string -- every child gnome-shell later forks
inherits that normally, no GDK app-launch-context involved at all
(checked and ruled out). Proposed fix: start gnome-shell with
`--wayland-display=wayland-0` itself (so its own self-belief/export
becomes the name the proxy wants to own), immediately rename the
resulting socket file to a fixed private path before anything can connect
to it, and have the proxy rebind its own listener at the now-vacant
`wayland-0` -- in place, without restarting the proxy process itself,
since that would drop every already-connected client, the one thing this
whole project exists to prevent. Needs redoing on every gnome-shell
restart, not just once (confirmed via source: mutter's own socket-claim
logic has no liveness check, so a restarting gnome-shell will always
successfully steal `wayland-0` back). Status: designed, not yet
implemented -- see the ADR for the full reasoning, source citations, and
rejected alternatives.

**ADR-0005 implemented and live-verified 2026-08-03, same day.** The
rename-after-bind design, `SIGUSR1` listener rebind (`src/main.rs`), and
the native `socket-handoff` helper (`src/bin/socket-handoff.rs`, using
`inotify` via `nix` -- not a shell polling loop, not `inotifywait`) all
landed and were confirmed working live: Shell-launched apps (Super key,
dock) now correctly route through the proxy, not gnome-shell directly.
Three further bugs found chasing this live, all fixed and covered by new
tests, not just patched ad hoc:

- **Stale-file false match**: the session wrapper's original plain
  `while [ ! -S path ]` poll loop matched a stale leftover socket file
  from a previous cycle instead of gnome-shell's fresh bind (the proxy
  doesn't unlink its own socket on shutdown). `socket-handoff` fixes this
  at the root via `inotify`'s `IN_CREATE` (only fires for files created
  *after* the watch starts) plus removing any stale file before
  watching, and additionally `SIGSTOP`s gnome-shell the instant its
  socket appears -- before the rename -- closing the *remaining* race
  (gnome-shell's own startup helpers, e.g. DING, connecting before the
  swap completes).
- **Wrapper login-state bug**: the wrapper decided `systemctl start` vs.
  `kill --signal=SIGUSR1` using a local shell variable that resets on
  every fresh login, while the proxy unit itself correctly stays running
  *across* logins -- so the first handoff after every login called
  `start` against an already-active unit (silent no-op), leaving
  `wayland-0` with nothing listening on it. Looked exactly like a HiDPI
  bug (gtk4-demo/tilix launched tiny) until `strace` on gnome-shell's own
  `execve`/`connect` calls showed `ENOENT` connecting to `wayland-0`.
  Fixed by querying `systemctl --user is-active` instead of trusting
  local memory.
- **Dropped `wl_surface.frame` stalls the client forever**: a real
  gtk4-demo caught mid-render at the exact moment of a crash never
  redrew again, even though its surface/`xdg_toplevel` otherwise
  recovered fully seconds later. `gtk.fill()` in `run_connection`'s
  select loop has no `if !frozen` guard, so client requests keep getting
  processed the whole time frozen -- including the gap between a failed
  reconnect attempt and the next one succeeding, during which
  `bump_generation()` has already run but the client's objects aren't
  remapped yet. A `frame()` request landing there hit the same drop path
  as a *permanently* stale object like `wl_buffer`, except a surface
  comes back seconds later -- the client was just never told, since the
  `wl_callback.done` its frame clock was blocking on was silently
  dropped along with the request. Fixed by synthesizing `done` (+ the
  `delete_id` a one-shot callback is owed) instead of dropping silently,
  same pattern as the `wl_buffer.destroy`/`delete_id` fix.

Also added `tests/socket_handoff_integration.rs` and
`tests/proxy_binary_lifecycle.rs`, spawning the actual compiled binaries
across multiple restart/rebind cycles rather than just once -- per the
explicit ask to check lifecycle assumptions with real tests instead of
ad hoc shell commands, since the wrapper login-state bug above was
exactly that shape of bug (worked the first time a path was exercised,
silently wrong the second time a *different* path hit it).

Still to verify: a full crash test with the frame-callback fix live,
confirming a mid-render gtk4-demo actually keeps rendering after
recovery, not just that the proxy-side mechanics are correct.

**Buffer-reuse gap found live 2026-08-03, same night, immediately after
the frame-callback fix above.** The `done` synthesis correctly unstuck a
real gtk4-demo's frame clock, but it then re-`attach()`ed its own
pre-crash GPU buffer, hitting `wl_surface.attach references
untranslatable object N -- dropping` followed by a fatal `COMPOSITOR
ERROR: ... invalid arguments for wl_surface#8.frame` that killed the
connection -- `wl_buffer` was deliberately never part of the recreation
graph (see `recreation.rs`'s original doc comment). Root-caused and
designed as `docs/adr/adr-0006-recreate-buffers-via-fd-handover.md`:
the proxy already receives its own SCM_RIGHTS copy of any buffer-backing
fd as a side effect of relaying `wl_shm.create_pool`/dmabuf's params
dance, today simply forwarded and forgotten; retaining it plus a
recorded recipe lets a buffer be replayed against the fresh compositor
on reconnect, the same pattern every other recreated object already
uses.

**wl_shm half implemented and test-verified 2026-08-04** (not yet live
on the real laptop): `Recreatable::ShmPool`/`ShmBuffer` (`recreation.rs`),
recipe capture + fd retention at `wl_shm.create_pool`/
`wl_shm_pool.create_buffer` (moving the proxy's own received `OwnedFd`
out of the generic per-message fd vec, not a `dup()`), `wl_shm_pool
.resize` updating the recorded size in place, and replay in
`recover_state_after_reconnect` (`wl_shm` itself is now also a
`Recreatable::Global`, needed so a recreated pool has a fresh `wl_shm`
host id to attach to). New integration test
`wl_shm_pool_and_buffer_recipes_replay_correctly_after_reconnect` sends
a REAL fd via SCM_RIGHTS (the first test here to do so) and confirms
both `create_pool` (with the proxy's retained fd) and `create_buffer`
replay correctly against a second fake compositor life, host-id-chained
correctly. Caught and fixed a real bug surfaced only by writing this
test: `tests/integration.rs`'s fake-compositor harness used a plain
tokio `read()`, which -- once an SCM_RIGHTS-bearing message is involved --
stops exactly at that message's boundary and never wakes for data sent
afterward (a real AF_UNIX `SOCK_STREAM` kernel quirk, not a test-harness
typo); fixed by switching the harness to the same `recv_with_fds`/
`try_io` pattern `Conn::fill()` already uses and documents for exactly
this reason. Not yet exercised against real fd-cleanup-on-destroy or the
dmabuf half (`create_immed()`), both still per the ADR's own suggested
order: validate this against `scripts/gtk/basic_shm.py` live next, then
build the dmabuf path against `dmabuf_gl.py`.

**Live-validated 2026-08-04, on the real laptop, with two more real gaps
found and fixed along the way** (full detail in ADR-0006's own "wl_shm
implementation, tested and live-validated" section -- this is the short
version): the wl_shm recreation above wasn't sufficient by itself for a
real `basic_shm.py` client to actually resume rendering after a crash --
found and fixed, in order, live: (1) no synthesized `wl_buffer.release`
for a buffer that was attached+committed right when the crash hit
(`buffer_flow.rs`), (2) no synthesized `wl_callback.done` for a `frame()`
that reached the OLD compositor and was simply never answered before it
died (`pending_frames.rs` -- distinct from the *dropped*-frame() case
already fixed the previous session). With both fixes, a real client
resumed rendering after reconnect for the first time this project has
achieved. It then hit a NEW, separate, NOT-YET-ROOT-CAUSED bug on its
own following resize (`invalid arguments for wl_shm#N.create_pool` from
the real compositor, on an entirely ordinary, non-recovery `create_pool`)
-- ruled out an fd-size mismatch via `fstat`.

**Also implemented and unit/integration tested 2026-08-04, later the
same day**: ADR-0006's dmabuf `create_immed()` half
(`Recreatable::DmabufBuffer`, `examples/probe_dmabuf.rs` to verify wire
signatures first). Deliberately not yet live-validated -- held back
until the create_pool issue below is settled.

**create_pool bug investigation, 2026-08-04, same day**: built four
increasingly faithful reproductions (`examples/probe_create_pool_resize.rs`,
`probe_reconnect_resize.rs`, `probe_reconnect_resize_with_surface.rs` --
bare burst direct-to-compositor, same burst through the proxy, through a
*real* crash+reconnect+recreation cycle with a minimal client, then with
a full real surface/xdg_toplevel/configure/ack_configure chain). **All
four passed cleanly** -- none reproduced the failure, while `basic_shm.py`
itself still fails the same way on the same day. Ruled out: pre-existing
GTK4/mutter limitation (fails clean without the proxy), proxy relay/fd-
retention being broken in general (fails clean through the proxy without
a crash), and "a pool recreated via the proxy's retained fd taints later
traffic" as the sole cause (fails clean through a real reconnect too).
Remaining candidates: real whole-desktop concurrent-client load during
an actual crash (none of the synthetic repros run alongside that scale
of simultaneous reconnection), or a GTK-specific request
(`wp_viewport`/`wp_presentation` traffic seen interleaved in the real
trace) none of the repros send. Suggested next step: use the proxy's own
built-in wire recorder (`src/recorder.rs`, `WAYLAND_PROXY_RECORD=`) to
capture a full byte-for-byte trace of a live failing run for comparison,
rather than more guess-and-check reproductions. Full detail in
ADR-0006's own "Open issue" section.

## The actual problem

gnome-shell sometimes crashes while the screen is **locked**. Since
gnome-shell is the only compositor for the whole session -- lock screen
included -- there's no way back except restarting `gdm`, which ends the
session. Apps were briefly glimpsed mid-restart (proof the crash itself
doesn't destroy anything; only the lack of a way to reconnect does).
This is the same crash-and-reconnect problem `wayland-proxy` already
solves for headless test containers (see
`docs/plan/plan-0002-test-harness-and-packaging.md`, Phase 9) -- the gap
is real desktop integration ("L3", explicitly out of scope there,
"groundwork only").

**Goal, confirmed with the user**: full takeover, not a smaller
opt-in/manual milestone. A manual "point one test app at the proxy
separately" approach was considered and rejected -- it structurally
can't reach the lock screen, since that's rendered by gnome-shell too,
so it wouldn't address the actual problem at all. The whole session
needs to go through the proxy from login, with automatic crash-restart.

## Confirmed facts (live-checked, same Ubuntu version as target)

- `gnome-shell --help` includes `--wayland-display <name>` -- lets us
  pin gnome-shell to an explicit socket name instead of letting it
  auto-pick. This is *more* control than the test harness has for
  labwc/sway/kwin, where `scripts/socket-wait.sh` has to poll for
  whatever name got auto-picked.
- The unit template is `/usr/lib/systemd/user/org.gnome.Shell@.service`
  (owned by `gnome-shell-common` 50.1-0ubuntu1.1, confirmed pristine via
  `dpkg -V` -- not locally modified). **Corrected 2026-08-03, live on the
  actual laptop**: the instance actually running for the everyday session
  is `org.gnome.Shell@ubuntu.service`, not `@wayland.service` as assumed
  below -- confirmed via a real `status=9/KILL` journal entry for that
  exact unit name. The template's actual current content (differs from
  what's quoted below, which was this file's original, since-corrected
  assumption):
  ```
  [Unit]
  AssertEnvironment=XDG_SESSION_TYPE=wayland
  OnFailure=org.gnome.Shell-disable-extensions.service gnome-session-shutdown.target
  OnFailureJobMode=replace-irreversibly
  [Service]
  Type=notify
  ExecStart=/usr/bin/gnome-shell --mode=%i
  SuccessExitStatus=1
  Restart=no
  # On wayland we cannot restart
  ```
  Critically, `--mode=%i` means the **instance parameter is the
  gnome-session session name** (from `gnome-session --session=<name>`,
  itself from the selected `/usr/share/wayland-sessions/<name>.desktop`'s
  `Exec=`), not the session type -- and it doubles as the gnome-shell UI
  mode name, so it has to be a real one:
  `/usr/share/gnome-shell/modes/` only has `ubuntu.json` and
  `initial-setup.json` on this box. See the corrected "Open decision"
  section below -- this is what makes isolation easy rather than hard.
  The old `ConditionEnvironment=XDG_SESSION_TYPE=%I` / bare
  `ExecStart=/usr/bin/gnome-shell` form this file originally described
  only exists in a stale, unrelated
  `org.gnome.Shell@wayland.service.backup.NX` file left behind by an old
  NoMachine (remote-desktop software) install from 2022 -- inert cruft,
  not the live template; investigated and ruled out, don't re-open it.

  Either way, the mechanism behind the observed symptom is unchanged: a
  crash triggers `OnFailure=` which tears the whole session down via
  `gnome-session-shutdown.target`, on purpose, no attempt to recover.
- `Restart=` is **not** a systemd default (default is `no` for any
  unit) -- this unit explicitly disables it, with a comment ("we cannot
  restart") suggesting a deliberate reasoning: without something that
  preserves client connections across the restart, a bare restart is
  close to useless anyway (new socket, every client's existing
  connection is just gone, same practical outcome as a fresh login).
  **This is exactly the gap wayland-proxy fills** -- it's specifically
  what makes turning `Restart=on-failure` on here worthwhile, where it
  wasn't for GNOME's own default config.
- `ExecStart=` uses an **absolute path** (`/usr/bin/gnome-shell`) --
  ruled out an earlier PATH-shim idea (intercepting a bare `gnome-shell`
  command via `$PATH`); systemd doesn't do `$PATH` lookups for absolute
  `ExecStart=` paths, so a shim would have been silently bypassed.
  Good this was checked before building it.
- `SuccessExitStatus=1` -- a normal user-initiated logout exits 1 and
  is *not* treated as a systemd "failure" (so `OnFailure=` doesn't fire
  for a clean logout, only for a genuine crash). Relevant for reasoning
  about whether our own changes could accidentally make logout behave
  like a crash or vice versa -- needs checking, not assumed.
- ~~The unit is shared across every Wayland session type, not session
  name~~ **Corrected 2026-08-03**: wrong, see the unit-name correction
  above. The unit is instanced per gnome-session *session name*
  (`org.gnome.Shell@<session-name>.service`), which is exactly the
  granularity a new, isolated session needs -- not shared with `ubuntu`
  at all. This was the one open design fork; see "Open decision" below,
  now resolved.
- Existing proof that instance-specific systemd drop-ins are a real,
  already-used pattern on this exact system:
  `/usr/lib/systemd/user/gnome-session@ubuntu.target.d/ubuntu.session.conf`
  (filename corrected 2026-08-03 -- it's `ubuntu.session.conf` on this
  box, not `gnome.session.conf`; the original note was checked on the
  other machine and the exact filename didn't transfer, which cost real
  time live-debugging a "No such file" before the right name was found)
  overrides the `ubuntu` instance of the `gnome-session@.target`
  template specifically. **Corrected 2026-08-03**: this *does* directly
  transfer to `org.gnome.Shell@.service` after all -- both templates turn
  out to be instanced the same way, per gnome-session session name, not
  session type as originally assumed here. `ubuntu.desktop`/
  `ubuntu.session` are themselves owned by the small `ubuntu-session`
  package (not `gnome-session-bin`/`-common`), and `ubuntu.session` is
  nearly empty (`[GNOME Session]` / `Name=Ubuntu`, no
  `RequiredComponents=`) -- modern gnome-session is systemd-target-driven,
  so a new session file can be equally minimal. **This drop-in's actual
  content turned out to be load-bearing, not just illustrative**: it's
  `Requires=gnome-session-services.target` /
  `Requires=org.gnome.Shell@ubuntu.service` -- i.e. the *only* thing that
  pulls gnome-shell into the session at all. Nothing generic does it
  (confirmed live: the first `wl-res-gnome-shell` login attempt reached
  `gnome-session-initialized.target` successfully with
  `org.gnome.Shell@wl-res-gnome-shell.service` never even attempted --
  wayland-proxy came up and listened correctly, but had nothing to
  connect to). A matching drop-in on `gnome-session@wl-res-gnome-shell.target`
  is therefore **required**, not optional -- added to the design below.
- ~~Why "isolate to a brand-new session only" is hard~~ **Corrected
  2026-08-03**: it isn't, on this box -- the `Condition*=`
  OR-semantics/`XDG_SESSION_TYPE` concerns below were real risks *if* the
  unit were instanced per session type, but it's actually instanced per
  session *name* (see above), so a new session name
  (`wl-res-gnome-shell`) gets a fully distinct
  `org.gnome.Shell@wl-res-gnome-shell.service` instance for free, with no
  `Condition=` fighting needed and no two-instance race risk -- a drop-in
  scoped to that instance literally cannot affect `@ubuntu.service`. One
  wrinkle from `--mode=%i` (see above): the drop-in must still force
  `ExecStart=` to `--mode=ubuntu` explicitly, since `wl-res-gnome-shell`
  isn't a valid mode name on its own. Original hardness analysis kept
  below for the record, since it correctly describes why the
  *shared-unit* route would have been risky -- just not why isolation is:
  systemd's `Condition*=` stanzas OR together when repeated (same
  directive, multiple lines = satisfied if *any* match), not AND. A
  drop-in naively adding an exclusion condition risks making the unit
  start *more* often, not less. `XDG_SESSION_TYPE` itself can't safely be
  set to a made-up custom value either -- systemd-logind only understands
  a small fixed enum (`tty`/`x11`/`wayland`/etc.), and lying about it
  risks breaking logind's own permission/idle-detection model.
- Packaging: **drop-in overrides are conflict-free**, not a risk.
  `/usr/lib/systemd/user/org.gnome.Shell@wl-res-gnome-shell.service.d/<name>.conf`
  is a new file at a new path -- dpkg only objects to two packages
  claiming the *same* path. Survives `gnome-shell`/`ubuntu-desktop`
  package upgrades cleanly (upgrading gnome-shell only touches files
  *it* owns). `/usr/share/wayland-sessions/` is already a
  multi-package shared directory (`gnome-session`, `labwc` both drop
  files there today) -- a new `.desktop` file is equally conflict-free.
  Correct location is `/usr/lib/systemd/user/...d/` (the vendor/package
  tree), not `/etc/systemd/user/...d/` (reserved for local admin
  overrides layered on top).
- A drop-in needs `systemctl --user daemon-reload` to take effect --
  an install-time, per-user-session action. This directly reconnects to
  the previously-deferred `postinst`/`postrm` systemd-wiring item (see
  `docs/plan/plan-0002-test-harness-and-packaging.md`) -- now with a
  concrete reason to revisit it, not just a hypothetical one.
- `gnome-shell`/`gnome-session` should be a soft `Recommends:` on the
  `wayland-proxy` `.deb`, not a hard `Depends:` -- the proxy itself has
  nothing to do with GNOME when used with labwc/sway/kwin.
- No proxy code changes needed for the "where did it recover" logging
  ask -- confirmed live earlier this session that `compositor
  reconnected -- recovering state`, `recreated <type>`, etc. are
  already at `info` level by default, not buried behind `--log-level
  debug`.
- `src/main.rs`'s accept loop connects to the target socket lazily,
  per accepted client (`UnixStream::connect` inside the per-connection
  `tokio::spawn`), not at proxy startup -- the proxy does **not** need
  its target (gnome-shell's private socket) to already exist when it
  itself starts. This matters for unit ordering: the proxy's own
  systemd unit doesn't strictly need `After=org.gnome.Shell@wl-res-gnome-shell.service`
  to be correct, though starting it first is still probably the
  sensible default ordering.

## Open decision -- RESOLVED 2026-08-03, live on the actual laptop

**Scope of the change**: isolated new session, not the shared unit.
Confirmed easy per the unit-name correction above -- a new session name
gets its own systemd instance for free, so isolation carries none of the
risk (two-instance race, `Condition=` OR-semantics fights) originally
feared. The user also asked directly for a distinct, selectable
`wl-res-gnome-shell` login-screen entry, which only the isolation route
can give. The existing "Ubuntu on Wayland" session's own unit
(`org.gnome.Shell@ubuntu.service`) is not touched by anything below.

## Recommended design (isolated session -- `wl-res-gnome-shell`)

New session name, new `.desktop` entry, new gnome-session file, and a
systemd drop-in scoped to that instance only:

1. **Drop-in** at
   `/usr/lib/systemd/user/org.gnome.Shell@wl-res-gnome-shell.service.d/wayland-proxy.conf`
   (instance-specific -- cannot affect `@ubuntu.service`):
   - `Restart=on-failure`, plus sensible `RestartSec=`/
     `StartLimitIntervalSec=`/`StartLimitBurst=` so genuinely repeated
     crashes eventually give up rather than looping forever (systemd's
     own built-in protection, not something to hand-roll).
   - `ExecStart=` cleared (empty assignment) then reset to
     `/usr/bin/gnome-shell --mode=ubuntu --wayland-display=<fixed-private-name>`
     (standard systemd drop-in idiom for *replacing*, not appending to,
     a list-type directive; `--mode=ubuntu` is required explicitly since
     `--mode=%i` would otherwise try the invalid mode
     `wl-res-gnome-shell` -- see the unit-name correction above).
   - **CONFIRMED LIVE 2026-08-03** (real `pkill -9 gnome-shell` -- a
     plain `pkill`/`killall` without `-9` just exits cleanly, status 0,
     and triggers *neither* `OnFailure=` nor `Restart=`; only a genuine
     `code=killed, status=9/KILL` counts as a failure): `OnFailure=`
     **does** still fire alongside `Restart=on-failure`, and they race.
     First run observed: `Failed with result 'signal'` -> `Triggering
     OnFailure= dependencies` and `Scheduled restart job` logged within
     the same second -> the resulting restart attempt was itself stopped
     ~1s later (`OnFailure=`'s `gnome-session-shutdown.target` chain won
     that round) -> a further restart attempt ~24s after that finally
     stuck, ~28s and 3 attempts total.

     **Tried overriding `OnFailure=` to empty in this same drop-in
     (isolated instance, safe to experiment with) -- confirmed it does
     NOT take effect**: `systemctl --user show ... -p OnFailure` still
     reports the original value even with the unit fully loaded and
     running, despite `systemctl cat` showing the merged config
     correctly and `systemd-analyze verify` finding no syntax issue.
     Best guess, not confirmed: gnome-session itself may set this as a
     transient/runtime property when it starts the unit via D-Bus
     `StartUnit`, overriding the static file -- would need chasing into
     gnome-session's own startup code to confirm, not done.

     **Three further real runs (unattended, via the sudo/autologin
     tooling below) all lost the race, landing on a full teardown, each
     to a *different* piece of infrastructure**: GPU/logind session
     grant already gone (`Unknown object '/org/freedesktop/login1/session/_NNN'`,
     `Failed to setup: No GPUs found`), the session D-Bus already closed
     (`Failed to get a session bus proxy: ... The connection is closed`),
     and once the restart attempt was torn down before gnome-shell even
     logged its first startup line.

     **Net across 4 real runs: 1 in-place recovery (predates the
     `WAYLAND_DISPLAY` fix below, so doesn't count as a confirmed
     client-survives-a-crash win either), 3 full teardowns.** Not a
     reliable mechanism as currently built -- `StartLimitBurst=5` means
     it isn't an infinite crash loop at least, and being scoped to one
     isolated instance means a bad outcome here can't take the everyday
     `ubuntu` session down with it, but "usually falls back to GDM" is
     the honest current state, not "recovers seamlessly." Also worth
     noting: `gnome-session-manager@.service` (a *different* unit from
     `org.gnome.Shell@.service`) has its own, separate
     `OnFailure=gnome-session-shutdown.target` -- untouched by anything
     above, and possibly a second independent path to the same teardown
     outcome even if the shell unit's own `OnFailure=` override is
     eventually made to work.
2. **New session entry**: `/usr/share/wayland-sessions/wl-res-gnome-shell.desktop`
   (`Exec=/usr/bin/gnome-session --session=wl-res-gnome-shell`,
   `DesktopNames=wl-res-gnome-shell;ubuntu;GNOME` to keep GNOME's
   `OnlyShowIn=GNOME` autostart entries and theming working identically
   to the real Ubuntu session) plus
   `/usr/share/gnome-session/sessions/wl-res-gnome-shell.session`
   (mirrors `ubuntu.session`, which is already minimal).
3. **`gnome-session@wl-res-gnome-shell.target.d/` drop-in**, mirroring
   `gnome-session@ubuntu.target.d/ubuntu.session.conf`:
   `Requires=gnome-session-services.target` /
   `Requires=org.gnome.Shell@wl-res-gnome-shell.service`. **Confirmed
   live 2026-08-03 this is required, not optional** -- without it the
   session reaches `gnome-session-initialized.target` "successfully"
   with no compositor ever started (blank screen, cursor only), since
   nothing else requests the shell instance.
4. **wayland-proxy's own systemd unit**: a second, distinctly-named unit
   (not the generic `packaging/wayland-proxy.service`, which is
   `WantedBy=graphical-session.target` unconditionally and would
   otherwise start a stray proxy in the *regular* Ubuntu session too)
   gated with `ExecCondition=` on `$XDG_SESSION_DESKTOP` matching
   `wl-res-gnome-shell`, with explicit
   `--display=<fixed-private-name> --listen=<public-name>`, started as
   part of this same session.
5. **Exporting `WAYLAND_DISPLAY=<public-name>`** into the session
   environment so gnome-session's *other* components (settings daemon,
   portals, dock, etc.) pick up the proxy's socket, not gnome-shell's
   private one. **CONFIRMED LIVE 2026-08-03, and more load-bearing than
   expected**: gnome-shell exports its own private `WAYLAND_DISPLAY`
   itself, unconditionally, on **every one of its own restarts** within
   a session -- not just once at session start. A one-shot export on the
   *proxy's* unit (which only starts once per session) only wins the
   race the very first time; every subsequent gnome-shell restart
   (crash-recovery is, definitionally, exactly this) clobbers it straight
   back to the private socket. Caught this the hard way: a real client
   (`gnome-terminal`) launched after a crash-recovery restart connected
   straight to gnome-shell's private socket, never through the proxy at
   all, so the proxy's own freeze-and-reconnect logic never got a chance
   to help it -- looked like a proxy failure, was actually an environment
   bug one layer up. **Fix**: the re-export has to live on
   `org.gnome.Shell@wl-res-gnome-shell.service.d/`'s own `ExecStartPost=`
   (fires after *every* gnome-shell start/restart, correctly ordered
   since it's `Type=notify` -- `ExecStartPost=` waits for its `sd_notify`
   READY, not just process spawn), not on the proxy unit. Also: use
   `dbus-update-activation-environment --systemd VAR=value` (explicit
   value) -- the bare-name form (`... --systemd WAYLAND_DISPLAY`, no
   `=value`) reads the value from its own inherited environment instead
   of whatever was just staged, and silently clobbers a just-set value
   right back -- found live the same session.
6. Confirm `wayland-proxy`'s current behavviour tolerates its target
   socket not existing yet at startup (already reasoned through above
   from `src/main.rs` -- should be fine, but worth a real confirmation
   once this is running for real).

## What to actually do next (in order)

1. ~~Get an explicit answer...~~ **Done 2026-08-03**: isolated session,
   resolved above.
2. On the real laptop: investigate `OnFailure=`-vs-`Restart=`
   interaction and the `WAYLAND_DISPLAY` export timing question --
   both need live systemd behaviour, not more reading.
3. Write the drop-in + proxy unit variant + environment-export piece,
   test the *simplest* possible crash (`killall -9 gnome-shell` from a
   terminal within a real, unlocked session) before ever testing the
   actual locked-screen scenario -- cheaper to debug, same underlying
   mechanism.
4. Only once that works: test the actual locked-screen crash scenario.
5. Package properly (extend `wayland-proxy`'s existing `cargo-deb`
   assets in `Cargo.toml`; revisit the deferred `postinst`/`postrm`
   item, now genuinely needed for `daemon-reload`); write a short ADR
   documenting whichever scope/isolation decision was actually made and
   why, matching this project's existing practice
   (`docs/adr/`) -- useful for future-you or future-Claude debugging
   this again.

## Explicitly not re-litigated

- Full takeover (not manual/opt-in) is the confirmed target -- don't
  re-propose the smaller milestone, it was considered and rejected for
  a concrete reason (see "The actual problem" above).
- The PATH-shim idea is a dead end (`ExecStart=` uses an absolute
  path) -- don't reintroduce it.
- Iteration is expected. This plan is the best design achievable
  without laptop access, not a guarantee any of it works first try.
  Update this file as real findings come in, the same way
  `docs/plan/plan-0002-test-harness-and-packaging.md` was kept updated
  throughout its own life -- don't let this file go stale while the
  work is still in progress.
