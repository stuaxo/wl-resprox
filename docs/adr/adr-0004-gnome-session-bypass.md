ADR 0004: Interim GNOME Session Bypass for Crash Resilience, Long-Term Fix Deferred

Status

Accepted

Context

plan-desktop-resilience.md's live testing (2026-08-03) found that a real
gnome-shell crash inside the `wl-res-gnome-shell` session (gnome-shell run
the normal way, via `gnome-session --session=wl-res-gnome-shell`) does not
reliably recover in place. Across four real `pkill -9 gnome-shell` runs: one
recovered in-place after ~28s and 3 restart attempts (and even that run
predated a `WAYLAND_DISPLAY` export fix, so it doesn't count as a confirmed
client-survives-a-crash win either); the other three fell back to a full
session teardown, back to the GDM login screen.

The root cause is `gnome-session`'s own systemd wiring, not the proxy. A
static drop-in clearing `OnFailure=` on `org.gnome.Shell@wl-res-gnome-shell.service`
was tried and confirmed *not* to take effect (`systemctl --user show ...
-p OnFailure` still reports the original value even with the unit fully
loaded and running, despite `systemctl cat` showing the merged config
correctly and `systemd-analyze verify` finding no syntax issue -- not yet
explained). Worse, that unit turns out to be only one of **eight** separate
systemd units, each independently wired with
`OnFailure=gnome-session-shutdown.target`: `gnome-session@.target`,
`gnome-session-initialized.target`, `gnome-session-manager@.service`,
`org.gnome.Shell@.service`, `gnome-session-basic-services.target`,
`gnome-session-services.target`, `gnome-session.target`,
`gnome-session-pre.target`. This is deliberate defense-in-depth on GNOME's
part -- every layer of the session's dependency chain independently tears
the whole session down if it fails -- and confirmed (via `man gnome-session`:
"Session definitions don't do anything on their own... they need to be
accompanied by systemd configuration") to be the *entire* mechanism, not
just one layer of a deeper, hidden application-level policy. That's good
news for eventually fixing this properly (it's systemd configuration, not
opaque C code to patch or work around), but it means a real fix means
correctly addressing some or all of eight tripwires, not one setting.

Separately, the same day's testing built `wl-res-labwc` -- a comparison
session using labwc instead of gnome-shell, specifically to isolate whether
the proxy + isolated-session + restart-loop mechanism itself was sound,
decoupled from GNOME's session-management complexity. It required a
different architecture than first attempted (a systemd `--user` unit):
GDM only hands seat/display activation over to whatever process it
**directly** forked for the session's `Exec=`; routing the compositor
through `systemctl --user start` (a separate process tree entirely) makes
it invisible to that activation handoff regardless of unit configuration.
Once labwc ran as a plain shell wrapper's direct child (the wrapper being
`gdm-wayland-session`'s own child, restarting labwc in a bare loop on
crash -- no systemd `Restart=on-failure`, no `OnFailure=`, no
`gnome-session-manager`-equivalent at all), three real `pkill -9 -x labwc`
runs against a real `gtk4-demo` client all recovered fully in ~1 second:
full protocol-level recovery (registry re-fetched, `wl_compositor`/
`wl_surface`/`xdg_wm_base` recreated), client never noticed, session never
left the seat.

Discussed with the user (Stu): gnome-shell/gnome-session should not be
replaced as a long-term direction -- this project's goal is a
crash-resilient *GNOME* session, not a different desktop. But the
labwc-proven architecture (direct child of `gdm-wayland-session`, bare
restart loop, no `gnome-session`) can be applied to gnome-shell directly
too, as a genuinely useful interim session, while the real long-term fix
(making the existing `gnome-session`-based session itself crash-resilient)
gets investigated separately without blocking on it.

Decision

Ship two sessions side by side, not a replacement:

1. **`wl-res-gnome-shell-direct`** (new): the interim, actually-resilient
   session. `packaging/wl-res-gnome-shell-direct-session-wrapper.sh` runs
   `gnome-shell --mode=ubuntu --wayland-display=wl-res-gnome-shell-direct-0`
   directly as `gdm-wayland-session`'s child, in a bare restart-on-exit
   loop -- the same architecture proven for labwc, simplified slightly
   since gnome-shell (unlike labwc) supports `--wayland-display=<name>`
   directly, so no socket-discovery/symlink indirection is needed.
   `wayland-proxy-gnome-shell-direct.service` stays a normal systemd
   `--user` unit (the proxy needs no DRM/seat access, so none of the
   GDM-activation-handoff reasoning above applies to it).

   Confirmed live, 3/3 real `pkill -9 gnome-shell` runs: full
   protocol-level recovery every time, session never left the seat,
   client survived every time, ~2-4s recovery (gnome-shell being heavier
   to restart than labwc, not an architecture difference) -- a different
   order of magnitude from the `gnome-session`-based session's best case
   of ~28s, let alone its 3-of-4 full teardowns.

2. **`wl-res-gnome-shell`** (existing, unchanged): kept specifically as
   the investigation vehicle for the long-term fix. Since bypassing
   `gnome-session` is *not* the intended end state, this session stays
   installed and available so the real fix -- making the
   `gnome-session`-based path itself crash-resilient -- can keep being
   worked on without the interim session's existence implying that work
   is abandoned.

Path forward for the long-term fix (not resolved by this ADR): correctly
override or remove `OnFailure=gnome-session-shutdown.target` across the
relevant instance-specific units in the eight-unit list above (the
`gnome-session@ubuntu.target.d/`-style per-session-name drop-in pattern
`man gnome-session` itself documents as the supported extension point is
the likely right shape), and separately re-investigate why the first,
single-unit attempt at this didn't visibly take effect -- now that the "is
there hidden app-level policy" question is answered (no), that failure
needs a cleaner explanation before concluding the approach doesn't work at
all.

**Addendum (2026-08-03, later the same day)**: live use of
`wl-res-gnome-shell-direct` (not just automated single-client crash tests)
surfaced further, concrete evidence of the orchestration gap this ADR
already anticipated -- `xdg-desktop-portal-gnome` reports "Non-compatible
display server, exposing settings only" in this session (root cause not
yet pinned down -- likely a D-Bus call to gnome-shell it expects to
succeed and doesn't), which cascades into `xdg-desktop-portal.service`
itself timing out (~90s) every time something tries to activate it,
plausibly explaining an observed multi-second delay launching ordinary
apps (GTK4 apps commonly query portal `Settings` on startup). Confirmed
live this is *not* an artifact of repeated crash-testing -- the very
first activation attempt, ~1 minute into a fresh, not-yet-crashed
session, failed the identical way, and there's no evidence of
accumulating stale process/restart debris from repeated `pkill -9`
cycles.

This sharpens what "the long-term fix" actually needs to mean, beyond
just defeating the eight `OnFailure=` tripwires: `gnome-session` merely
*not tearing down* on a gnome-shell crash is necessary but not obviously
sufficient. `gnome-session`'s job is ongoing coordination (D-Bus
activation environment, keeping portals/settings-daemon aligned with
whichever gnome-shell instance is currently live), not a one-time launch
-- surviving the crash doesn't by itself prove `gnome-session` would
correctly notice gnome-shell restarted and re-coordinate its other
components with the *new* instance. It may need something analogous to
the proxy's own explicit reconnect-and-recreate logic, not just
"stop shutting down." Two directions this could go, both genuinely
bigger refactors than anything built today, not scoped further here:
keeping the *whole* `gnome-session`-coordinated stack alive and reconciled
across a gnome-shell restart, or explicitly migrating/handing off state
from the dying session to a fresh one at the point of restart. Recorded
as the sharper open question for whoever picks the long-term fix back up,
not resolved by this ADR.

Consequences

Positive

Real crash resilience is available now, for actual use, not just as a
research result -- `wl-res-gnome-shell-direct` is a selectable GDM session
today with 3/3 confirmed real-crash recovery.

Doesn't compromise the long-term direction: gnome-shell/gnome-session isn't
being replaced or abandoned, just temporarily bypassed for one clearly-interim
session that exists alongside, not instead of, the real one.

The path forward for the long-term fix is now concrete and scoped (a
specific list of eight units), not a vague "investigate gnome-session
more" -- a direct product of today's investigation, not deferred to a
future session to rediscover from scratch.

Negative

`wl-res-gnome-shell-direct` had real, observed gaps from skipping
`gnome-session`'s orchestration of the settings daemon/portal/keyring
layer -- `xdg-desktop-portal`/`xdg-desktop-portal-gnome` failing outright,
DING (desktop icons) retrying its own launch repeatedly, File-roller's
D-Bus service timing out. **Root cause found and fixed, 2026-08-03**:
`graphical-session.target` (a standard systemd unit, not GNOME-specific)
has `RefuseManualStart=yes` and was never reached in this session at all
-- nothing pulled it in as a dependency, and several
`graphical-session.target`-gated components (`xdg-desktop-portal-gnome.service`
has `Requisite=graphical-session.target`) failed as a direct result. Fix:
`wayland-proxy-gnome-shell-direct.service` (already explicitly started by
the wrapper once gnome-shell is up) now declares
`Wants=graphical-session.target`, which correctly pulls the target in as
an allowed dependency rather than a refused direct start. Confirmed live
this resolved all three symptoms at once from one fix, not three separate
ones. A separate, real gap remains around stale pre-crash `wl_buffer`
references causing outright client disconnects on some crashes (not
`gnome-session`-bypass-specific -- see `plan-desktop-resilience.md`'s own
TODO on this), and gnome-shell's own "Log Out" does nothing in this
session (expected, no `gnome-session` to receive the `SessionManager.Logout`
D-Bus call) -- neither characterized or fixed yet.

The long-term fix is genuinely nontrivial: up to eight independent
`OnFailure=` tripwires to potentially address (more, if any of them
propagate failures to units not yet identified), plus an unexplained gap
in why the first, simplest attempt (a single static drop-in) didn't
visibly take effect.

Running two near-identical GDM sessions side by side is temporary
surface-area overhead -- two wrapper scripts / proxy units whose shared
logic (the periodic `WAYLAND_DISPLAY` re-export pattern, the
`ExecCondition=`/`Conflicts=` idiom) has to be kept in sync by hand where
it overlaps, until the long-term fix lands and the interim session can be
retired.
