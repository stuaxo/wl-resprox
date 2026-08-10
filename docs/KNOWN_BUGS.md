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
Functional impact confirmed live via gdb, 2026-08-10 -- this is not
just a log warning, it's why a recreated window sometimes won't
raise/focus when activated.

**Symptom:** `libmutter-CRITICAL`, `window->stack_position >= 0`. Also
manifests as a window that never comes to front when activated (panel
click, `Meta.Window.activate()`) -- focus falls through to something
else (observed: the desktop-icons layer) instead.

**Mechanism, confirmed against Mutter 50.1's actual source**
(`src/core/stack.c`): `meta_stack_remove()` sets `window->stack_position
= -1` permanently when a window is torn down. The assertion
(`g_return_if_fail (window->stack_position >= 0)` inside
`meta_window_set_stack_position_no_sync`) fires when something later
tries to reposition a window already in that removed state -- a stale
`MetaWindow*` still referenced somewhere, touched again during a full
stack re-layout pass (which *any* window activation triggers, over
every tracked window, not just the one being activated).

**Live-identified the actual stale window**, without needing Mutter's
own (fully stripped, no debug symbols available) binary: breakpointed
`g_return_if_fail_warning` (always-exported core GLib, receives the
failing function name and expression as plain string args -- no Mutter
symbols needed), inspected callee-saved registers at that frame for
plausible GObject-shaped pointers, then called the real, exported
`meta_window_get_description()` on each via gdb directly. Result: the
same stack re-layout pass touches both `"W0 (Mozilla Firefox)"` (the
window actually being activated) and `"W4 (Desktop Icons 1)"` (DING's
own window) together. Working theory: a stale, orphaned DING window
object from an earlier gnome-shell restart earlier in the same
session, never cleaned out of some internal Mutter list, gets touched
by every subsequent activation's re-layout pass -- not necessarily
specific to Firefox, Settings, or any one app.

**Reproduction:** needs `sudo sysctl kernel.yama.ptrace_scope=0`
(revert to `1` after) to attach gdb to a same-user, non-child process.
No script yet -- was done as an interactive gdb session:

```
break g_return_if_fail_warning
commands
silent
printf "func=%s expr=%s\n", (char*)$rsi, (char*)$rdx
continue
end
continue
```

Trigger with any `Meta.Window.activate()` call (Looking Glass `Eval`,
unsafe mode: `window.activate(global.get_current_time())`) on a window
that's survived at least one prior gnome-shell restart in the session.
On a hit, callee-saved registers (`rbx`/`r12`-`r15`) are candidates for
the `window` argument (caller-saved registers like `rdi` are already
overwritten by the time execution reaches `g_return_if_fail_warning`);
`x/2xg <candidate>` to check for a GObject-shaped header before calling
anything, then `call (const char*) meta_window_get_description((void*)<candidate>)`
to identify it by name.

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
