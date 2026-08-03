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
frames (common for GPU-accelerated rendering), which is far more likely
once a client has been open for a while rather than just-launched. A
dropped message referencing a stale buffer id can leave the *real*
compositor's own object-graph view inconsistent with the client's,
which Wayland's strict protocol-violation handling can turn into an
outright disconnect. Not a simple "recreate buffers too" fix -- a
dmabuf-backed buffer's actual pixel data lives in the old compositor
process's GPU import and is genuinely gone after a real crash regardless
(unlike `wl_shm` buffers, where the memory itself isn't tied to the old
process). The achievable fix is likely making the *drop* graceful (e.g.
synthesizing the `delete_id` the client would otherwise wait for
forever) rather than resurrecting buffer content. Deferred, not
forgotten -- flagged by the user explicitly as real future work.

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
