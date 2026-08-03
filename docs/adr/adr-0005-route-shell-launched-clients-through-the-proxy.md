ADR 0005: Route GNOME Shell's Own Spawned Children Through the Proxy via a Socket-Identity Swap

Status

Proposed -- design only, not yet implemented. Written up per the user's
explicit request to document the reasoning before writing any code, since
the design has enough moving parts (and enough plausible-but-wrong
alternatives) to be worth getting reviewed on paper first.

Context

Live testing 2026-08-03 (see plan-desktop-resilience.md) found that apps
launched via GNOME Shell's own UI -- the Super key search, the dock,
Activities -- never go through `wayland-proxy` at all, regardless of any
fix made to the proxy itself. Confirmed via `ss -xp` cross-referencing
socket inodes: a `gtk4-demo` launched this way connects directly to
gnome-shell's own private compositor socket
(`wl-res-gnome-shell-direct-0`), not the proxy's public `wayland-0`. Apps
launched from a shell with `WAYLAND_DISPLAY=wayland-0` set explicitly (or
via D-Bus-activated services, e.g. DING, the portals, which the session
wrapper's `dbus-update-activation-environment` calls do reach) go through
the proxy correctly. This makes Super key / dock launches -- almost
certainly the most common way a real user actually opens an app day to
day -- the single largest remaining gap in this project's crash-resilience
goal, bigger in practical terms than the buffer-reuse/reconnect bugs fixed
earlier the same day.

Root cause, confirmed by reading the actual upstream source (not
inferred from behavior -- see below for exactly what was read and why):
mutter's Wayland compositor startup code
(`src/wayland/meta-wayland.c`, `gnome-50` branch,
`meta_wayland_compositor_start`) does two things once its own compositor
socket exists:

```c
if (_display_name_override)
  {
    compositor->display_name = g_steal_pointer (&_display_name_override);
    if (wl_display_add_socket (compositor->wayland_display,
                               compositor->display_name) != 0)
      g_error ("Failed to create_socket");
  }
...
set_gnome_env ("WAYLAND_DISPLAY", meta_wayland_get_wayland_display_name (compositor));
```

`_display_name_override` is whatever `--wayland-display=NAME` mutter was
started with -- `wl-res-gnome-shell-direct-0` in this project's wrapper
script today. `set_gnome_env` (same file) does a plain `setenv()` on
mutter's own process, plus a best-effort push into the D-Bus activation
environment (`org.gnome.SessionManager.Setenv`, falling back to directly
patching the systemd `--user` activation environment when there's no
`gnome-session` to receive that call -- which is always the case in this
project's session-bypass architecture). The `setenv()` call is what
matters here: it happens **once**, using the literal `--wayland-display=`
string, and is never revisited. Any child process mutter/gnome-shell
subsequently forks -- via any mechanism, GJS `Gio.Subprocess`, GLib
spawn, whatever the Shell app-launching code actually uses internally,
it doesn't matter which -- inherits `WAYLAND_DISPLAY` through completely
ordinary environment inheritance from that point on. (A GDK Wayland
app-launch-context theory was checked and ruled out first --
`gdk/wayland/gdkapplaunchcontext-wayland.c` in GTK3 only handles startup
notification IDs, never touches environment variables; the actual
mechanism is entirely on mutter's side, one `setenv()` call.)

This explains why `/proc/<gnome-shell-pid>/environ` shows no
`WAYLAND_DISPLAY` at all (that file reflects the environment at `exec()`
time, not later in-process `setenv()` calls) while gnome-shell's own
children clearly have one -- a fact that was puzzling before this
source-level confirmation.

Two more facts, also confirmed by reading `wl_display_add_socket`'s real
implementation (`src/wayland-server.c`, `libwayland` upstream `main`,
matching the installed `1.24.0`), matter for the design below:

1. `wl_display_add_socket(display, name)` -> `wl_socket_lock(s)` ->
   opens/creates a companion `<name>.lock` file and takes a non-blocking
   `flock(LOCK_EX)` on it. If that succeeds, it `lstat()`s the actual
   socket path and -- with **no liveness check of any kind, no
   test-connect, nothing** -- unconditionally `unlink()`s any existing
   writable file found there. The *only* thing standing between "steal
   this name" and "refuse" is whether the flock is currently held by
   another live process.
2. If `wl_display_add_socket` fails for any reason -- including that flock
   being contended -- mutter's caller treats it as fatal:
   `g_error ("Failed to create_socket")`. `g_error` aborts the process.
   There is no fallback to a different name for an explicit
   `--wayland-display=` override (that fallback path,
   `wl_display_add_socket_auto`, only runs when no override was given at
   all).

Decision

Make mutter's own socket identity **be** the name the proxy publishes,
rather than trying to make mutter's children resolve a different name
somehow (there is no environment variable or config knob for that -- the
value is derived from mutter's own live compositor object, as shown
above). Concretely, on every gnome-shell start (first boot of the session
*and* every crash-restart, since the restart loop repeats this exact
sequence each time):

1. Start gnome-shell with `--wayland-display=wayland-0` -- the name the
   proxy currently publishes, and the name libwayland-client itself
   falls back to when a client's own `WAYLAND_DISPLAY` is unset. mutter's
   `wl_display_add_socket` will always succeed here: nothing else holds
   the `wayland-0.lock` flock (see "Rejected alternatives" below for why
   the proxy must *not* try to hold it), so mutter unconditionally steals
   and unlinks whatever's currently there -- including the proxy's own
   live socket file from the previous cycle. This is expected, not
   fought against.
2. Wrapper script polls for gnome-shell's fresh `wayland-0` socket file
   to appear (same pattern the labwc wrapper already uses for its own
   socket discovery).
3. Wrapper renames it to a **fixed** private path, e.g.
   `wl-res-gnome-shell-direct-host-0` -- the same name every cycle,
   atomically replacing whatever was there from the previous cycle. A
   bound `AF_UNIX` socket keeps working under its new path after a plain
   filesystem `rename()`: the kernel socket is tied to the inode, and
   `connect()` resolves by looking up whatever's *currently* at the given
   path at connect time. mutter never rechecks the filesystem for its own
   name after `wl_display_add_socket` returns (per the source above), so
   it never notices. **Verified live 2026-08-03** on this exact machine
   with a minimal, isolated reproduction (plain Python `AF_UNIX`
   listener, independent of gnome-shell/mutter/the proxy entirely --
   `rename_test.py`, run from the scratchpad): a connection opened
   *before* the rename kept working afterward, a brand-new connection to
   the *renamed* path reached the same listener, and a connection attempt
   to the *old* (now-vacant) path correctly failed with `ENOENT`. All
   three assertions passed. This was the one claim in this ADR not
   already confirmed by reading source; it now is.
4. Wrapper tells the running proxy "rebind now" (e.g.
   `systemctl --user kill --signal=SIGUSR1 wayland-proxy-....service`).
5. Proxy's signal handler closes its old listening fd (if any), removes
   any stale file at `wayland-0`, binds fresh, resumes accepting. Nothing
   about any already-`accept()`ed per-client connection is touched --
   each `run_connection` task owns its own fd independent of the
   listening socket, and already has its own working reconnect logic on
   the *host* side (`reconnect_with_backoff` against the fixed private
   path from step 3, unchanged by any of this).

Step 3 landing on a fixed name every cycle is what keeps the proxy's own
host-side reconnect logic untouched: it already retries a fixed path and
already tolerates "connected then immediately reset" (fixed earlier the
same day, see plan-desktop-resilience.md's reconnect-race entries) --
exactly the shape of event a mid-rename connection attempt produces.

A secondary, currently-unplanned benefit: once `compositor->display_name`
is literally `wayland-0`, mutter's own `set_gnome_env` call already pushes
`WAYLAND_DISPLAY=wayland-0` into the D-Bus activation environment as part
of its normal startup (falling back correctly in this gnome-session-less
setup, per the source above) -- the *correct* value, automatically, where
today the wrapper's own periodic `dbus-update-activation-environment`
override and mutter's automatic one actively disagree (mutter pushes its
own private name, the wrapper's loop overwrites it with the proxy's
public one every 3s). This may let the wrapper drop its own
`WAYLAND_DISPLAY` re-export entirely once this lands -- not required for
this ADR's core decision, noted for whoever implements it.

Rejected alternatives

- **Make the proxy hold the `wayland-0.lock` flock itself**, so mutter's
  own `wl_socket_lock` fails and (hopefully) falls back to a different
  name. Rejected on source evidence, not speculation: an explicit
  `--wayland-display=` override has no fallback path at all --
  `wl_display_add_socket` failing here is `g_error`, fatal, the whole
  compositor aborts. This would crash gnome-shell on every single
  restart, the opposite of the goal.
- **LD_PRELOAD or similar interception** of whatever mutter/GDK internally
  believes its display name is. Rejected as needlessly fragile compared
  to a plain `rename()` -- this project's fix would end up pinned to
  private, unstable internal symbols instead of documented POSIX
  filesystem semantics, and would need updating across GNOME versions.
- **Proxy self-monitors the socket path** (inotify or periodic stat) and
  rebinds on noticing the theft, instead of the wrapper explicitly
  signaling it. Not rejected outright -- more decoupled, more robust to
  *anything* stealing the name, not just this specific sequence -- but
  more moving parts (watch-the-parent-dir vs. watch-the-file races, missed
  fast delete-then-recreate cycles) for no benefit in this specific,
  closed three-component system where the wrapper already knows exactly
  when it caused the theft. Worth reconsidering only if the explicit
  signal proves fragile in practice.

Consequences

Positive

Closes what's very likely the single most impactful remaining gap for
real daily use of `wl-res-gnome-shell-direct` -- apps launched the way a
real user actually launches most apps (Super key, dock, Activities), not
just ones opened from a terminal with an explicit env override.

The proxy's existing, already-hardened host-side reconnect logic
(`reconnect_with_backoff`, the frozen/unfrozen state machine, the
generation-based stale-object handling) needs zero changes -- the fixed
private socket name from step 3 keeps that whole code path exactly as
tested today.

Negative

This is a real proxy code change, not a wrapper-script-only fix:
`src/main.rs`'s accept loop needs restructuring to interleave normal
`accept()` with a signal-triggered listener rebind, and needs a real
signal handler wired up (`tokio::signal::unix`). Contained in scope
(doesn't touch `src/lib.rs`'s per-connection logic at all), but not
nothing.

There's still one small, currently-unavoidable window (step 3 to step 5)
where `wayland-0` doesn't exist as a path at all -- a client connecting in
that exact instant gets a hard connection failure, not a silent bypass or
a queued retry (libwayland-client's own `connect()` is a single attempt,
no built-in retry). Worth measuring how narrow this window actually is
live before deciding whether it's negligible.

Not yet verified live: the exact wrapper-restart-loop timing needed to
keep this correct across *repeated* crash cycles against real
gnome-shell/mutter, not just a clean first startup -- the rename-after-bind
reachability assumption itself is now confirmed (see step 3 above), but
only in isolation; the full integration (real mutter restarting
repeatedly, real proxy rebinding on signal) still needs to be watched
happen end to end.
