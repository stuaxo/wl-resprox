# Known Bugs (Not This Project's)

Bugs found testing wl-resprox that belong to GNOME Shell/Mutter/GDM,
not this proxy. Separate from `docs/debugging-notes.md`'s append-only
log because these recur across unrelated sessions and keep costing time
being re-suspected. Check here first.

Status labels distinguish "reproduces with zero proxy involvement"
(confirmed) from "looks unrelated" (suspected).

Reproduction scripts live in `known_bugs/scripts/` (see its own
README). None self-verify fully — most need eyes on a screen or Looking
Glass access.

## GNOME Shell: window inherits an unrelated app's icon (PID collision)

**Status:** structural interaction, not a pure GNOME Shell bug in
isolation — GNOME Shell's own resolution order plus this project's
architecture. Root cause confirmed against GNOME Shell 50.1's actual
source (`shell-window-tracker.c`, package matches this system exactly).
Fixable on this project's side (see below); not yet done.

**Symptom:** a window shows a different app's icon in the dash/overview.
Observed: Tilix showing Settings' (later: Zed's) icon, persisting
indefinitely.

**`get_app_for_window()`'s resolution order** (`shell-window-tracker.c`),
first match wins:
1. `app_id` vs. installed `.desktop` files — fails for Tilix (no
   `StartupWMClass`, `app_id` `"tilix"` doesn't match
   `com.gexperts.Tilix.desktop`).
2. Sandboxed app id — n/a.
3. `gtk_shell1`-derived GApplication id — n/a, we drop `gtk_shell1`.
4. **PID match** (`get_app_from_window_pid`): first currently-running
   `ShellApp` with a window sharing this window's PID.
5. `xdg_activation_v1` startup-notification/token match.
6. X11 window-group match — n/a.
7. Fake per-window placeholder (`window:N`) — safe fallback.

Step 4 runs before step 5. Every wl-resprox client shares the proxy's
own PID (`SO_PEERCRED` reflects whoever actually `connect()`ed, i.e.
the proxy) — so step 4 always finds *some* match against whichever app
happens first in GNOME Shell's own running-apps list, and returns it
before step 5 (already proven correct — `xdg_activation_v1` relays
byte-identical both sides, full token round-trip live-verified
including a real race) ever runs.

**Fixable here, not just GNOME Shell's problem:** if each client's
host-side connection were opened by a genuinely separate OS process
(rather than one process handling every client), each would get its
own real PID, step 4 would stop false-matching, and steps 5/7 — both
already correct — would take over naturally. A real architectural
change (per-client process isolation), not a small patch; not yet
attempted.

**Also ruled out in this project's own code:** id translation and
object-arg rewriting (`src/shadow_table.rs`, generic, no off-by-one);
in-order relay, no batching (`relay_ready_messages`).

**Reproduction:** `known_bugs/scripts/repro-icon-grouping-mixup.sh` —
launches app A (resolves correctly), then app B (unresolvable
`app_id`) via `gtk-launch`. B deterministically inherits A's icon.
Ground truth: `Shell.WindowTracker` via Looking Glass `Eval` (unsafe
mode) — query in the script's own output.

## Mutter: `meta_window_set_stack_position_no_sync` assertion

**Status:** confirmed not us (reproduces with zero proxy involvement).

**Symptom:** `libmutter-CRITICAL`, `window->stack_position >= 0`, from
`Meta.Window.raise()` called by GNOME Shell's own JS.

**Evidence:** reproduces on ordinary gnome-shell startup, no proxy or
dmabuf recreation involved — see
`docs/adr/adr-0008-live-validate-dmabuf-recreation.md`'s "Ruled out"
section.

**Reproduction script:** none — original trigger found through
exploratory testing, not yet a scripted recipe.

## Mutter: fractional-scaling `GLib-GObject-CRITICAL` (`scale-x`/`scale-y` = "inf")

**Status:** confirmed not us (proxy wasn't running in the session where
this was found).

**Symptom:** a freshly-opened window renders tiny regardless of DPI
setting; log shows `value "inf" ... invalid or out of range for
property 'scale-x'/'scale-y'`.

**Evidence:** found 2026-08-07 in a session where the proxy hadn't
claimed the display yet (separate, since-fixed startup-ordering bug) —
rules the proxy out categorically for that occurrence.

**Reproduction script:** none — exact trigger (monitor/scaling
combination) not pinned down.

## GDM: greeter has no visible mouse pointer after `loginctl terminate-session`

**Status:** suspected not us, not deeply root-caused.

**Symptom:** the greeter after `loginctl terminate-session` (not a full
reboot) has no visible cursor.

**Reproduction:** `known_bugs/scripts/repro-logout-loses-mouse-pointer.sh`
— destructive, requires `--yes`, needs physical/VNC access afterward to
check.

**Not yet done:** confirming this also happens on a stock GNOME session
with no wl-resprox wrapper involved at all.
