# Debugging Scratchpad

## Crucial Environment Variables

When running apps or the proxy, these will be our best friends:

- `WAYLAND_DEBUG=1` - Dumps all Wayland protocol messages (Requests and Events) to stderr.
- `WAYLAND_DISPLAY=wayland-X` - Forces the client/proxy to connect to a specific socket.
- `RUST_LOG=debug` - Standard Rust tracing for our proxy logic.

## Tools

- `wayland-info` - Good for verifying which globals our proxy is successfully passing through.
- `wev` - Wayland event viewer. Good for testing if keyboard/mouse inputs are surviving the proxy translation.

## Known Edge Cases to Watch Out For

- (To be filled: Track specific ID mismatches, memory leaks, or missing configure events here as we test)
- Buffer lifetimes: GTK might try to release a `wl_buffer` that the new compositor instance doesn't know about yet.

## Ad-hoc Observations & Experiments

- Host `gnome-shell` restart behavior: Running `pkill gnome-shell` on the host machine caused the login screen to temporarily disappear, followed by a momentary flash of existing windows before the shell fully recovered.
  - Hypothesis: The display server (Mutter) might have held onto the framebuffers briefly, or Xwayland apps survived. We need to verify if native Wayland GTK sockets actually dropped during this event.

- 2026-07-29: Chased down the `/dev/dri/renderD128: Permission denied` blocker for `gtk4-demo` inside the nested labwc. Root cause turned out to be two separate, stacked problems — device permissions were never actually the whole story:

  1. **`sudo` inside the rootful container required a real TTY, blocking all non-interactive automation.** The container ships `sudo-rs` (Ubuntu 26.04's Rust reimplementation of sudo), which enforces `Defaults use_pty` unconditionally — even with a `NOPASSWD` rule in `/etc/sudoers.d/`, `sudo` still refused with "a terminal is required to authenticate" when run from a session with no controlling tty. Commenting out `Defaults use_pty` in `/etc/sudoers` (not just adding NOPASSWD) was required to unblock non-interactive `sudo`. A `NOPASSWD` drop-in file also has to sort *after* the existing `/etc/sudoers.d/sudoers` file alphabetically (e.g. `zz-*`, not `90-*`) — sudoers uses the last matching rule, and the stock file's `%sudo ALL=(ALL:ALL) ALL` (no NOPASSWD tag) comes after a numerically-prefixed drop-in and silently re-requires a password.

  2. **`podman exec --user=stu` (what `distrobox enter --root` does under the hood) does not apply supplementary/secondary groups from `/etc/group` at all** — it only sets the primary GID from `/etc/passwd`. So `usermod -aG video,render stu` run after container creation is silently ignored by every subsequent exec session: `id stu` (an NSS lookup) correctly listed `video`/`render`, but the *actual running process's* `id` (no argument) did not, and `/dev/dri/renderD128` stayed `Permission denied` even though the device's own ownership (`root:render`, mode `0660`) and `stu`'s `/etc/group` membership were both correct. Explicitly overriding the group at exec time — `podman exec --user stu:render ...` — works reliably.
     - Tried the "proper" fix: recreating the container with `--group-add 991 --group-add 44` baked in at `distrobox create` time (now in `scripts/setup-env.sh`). Verified in isolation on a **bare** `podman run --group-add ... --privileged` container that this does make `exec --user=<name>` (no override) pick up full supplementary groups correctly, including newly-`usermod`'d ones.
     - It does **not** work the same way inside a distrobox-managed container, though — confirmed with a completely fresh test user (`useradd` + `usermod -aG render`, no distrobox-specific setup) created *inside* `wayland-proxy-dev` after the group-add fix: still only got `groups=<primary>,0(root)` on exec, identical to `stu`. `/etc/group`, `/etc/gshadow`, and `/etc/passwd` all looked structurally normal (checked with `cat -A`, no encoding/formatting issues), and a container restart didn't change anything. This looks like a genuine, unresolved podman/distrobox interaction bug rather than a config mistake on our end — not root-caused further since the explicit-override workaround is reliable. Worth revisiting if this becomes annoying enough to chase properly.
     - **Practical upshot: `distrobox enter --root wayland-proxy-dev` alone still does NOT get GPU access.** Anything needing `/dev/dri` (nested labwc, GTK apps rendering via EGL) must be invoked with the explicit group override, e.g.:
       ```
       sudo podman exec --user stu:render wayland-proxy-dev <command>
       ```
       (`playbook.yml` still sets up the `video`/`render` groups properly via Ansible — that part is correct and necessary, just not sufficient on its own for `distrobox enter` sessions specifically.)

  - Once both were worked around, `labwc -C /dev/null` started cleanly (new `wayland-N` socket, no errors) and `gtk4-demo` ran against it with no permission/EGL errors — only benign `Portal operation not allowed` warnings from the missing `xdg-desktop-portal` in this minimal headless setup.

- 2026-07-29 (follow-up): Root-caused *why* `sudo` as `stu` never has a working password in the first place — this was the thing actually catching manual attempts to run `scripts/setup-env.sh` interactively ("caught by the password thing again"). It's not a "set a password on first entry" step at all (the old comment in `setup-env.sh` claiming that was wrong/never verified) — there is no such interactive step in distrobox's tooling.

  What actually happens: distrobox's own container-init script (`/usr/bin/entrypoint`, baked into the image, not our code) unconditionally clears `stu`'s password on first container boot (`chpasswd -e` with an empty encrypted field — shows as `passwd -S stu` → `NP`, "no password"). For rootful containers it then just locks `root` (`usermod -L root`) and stops there — it never leaves `stu` with a working credential *or* a `NOPASSWD` sudoers rule. Net effect: the very first `sudo` call as `stu` (e.g. `setup-env.sh`'s own `apt-get`/`ansible-playbook` step, or any manual `sudo` in an interactive `distrobox enter --root` shell) hits a real `[sudo] password for stu:` prompt that **no password can satisfy**, because none was ever validly set. Typing anything — including guesses, blank enter, the host's own password — fails identically, which is exactly the "maybe I'm setting it to something unknown" symptom.

  Fix, in the standard "disposable dev container" style (same pattern most devcontainer base images use for their default user):
  - `scripts/setup-env.sh` no longer goes through `distrobox enter --root` (as `stu`) for provisioning at all — it now runs `apt-get`/`ansible-playbook` directly via `sudo podman exec <container>` (container root, no `--user`), sidestepping `stu`'s broken sudo entirely for automation. (Also had to add an explicit `sudo podman start` before this — `distrobox create` only creates the container, doesn't start it, and a short poll loop waiting on `podman exec ... true` before the container's own init/apt-get finishes.)
  - `playbook.yml` has a new task, `Allow stu passwordless sudo`, that writes `/etc/sudoers.d/zz-stu-nopasswd` (`stu ALL=(ALL) NOPASSWD: ALL`, validated via `visudo -cf` — confirmed sudo-rs supports `-cf` on a specific file, not just the main `/etc/sudoers`). Filename deliberately sorts after the stock `sudoers` drop-in for the same last-matching-rule reason noted above. This is what actually fixes things for *your own later interactive* `sudo` use inside `distrobox enter --root` — confirmed working (`sudo true` succeeds, no prompt, even though `passwd -S stu` still correctly shows `NP`).
  - Verified the full fresh-container flow non-interactively end to end after this fix: `setup-env.sh` (9/9 ansible tasks OK) → `start-host.sh` → `start-guest.sh` (nested `labwc` starts clean, no permission errors) → `gtk4-demo` renders with only the same benign portal warnings as before.

- 2026-07-30: Root-caused the intermittent `wl_display.error: invalid arguments for wl_registry#2.bind` bug that had survived the partial-write fix, the busy-loop fix, and disproved both the message-batching and message-pacing theories (see earlier same-day entries above this one in the raw log -- summarized here since this is the entry that actually closes it out).

  **Root cause**: not a wire-encoding bug at all. labwc/wlroots (via libwayland-server) requires a client's `new_id`-allocating requests to arrive without gaps. Our relay silently *drops* any request on an object whose interface isn't in `interfaces.rs`'s lookup table (deliberately, to avoid misattributing fds on a message we can't parse -- see that file's doc comment). Real clients routinely send such a request immediately after binding a global we don't recognize (e.g. `zwp_linux_dmabuf_v1.get_default_feedback` right after binding `zwp_linux_dmabuf_v1`) -- and if that dropped request carried a `new_id`, the compositor never learns about that id, leaving a gap. The *next* new_id-bearing message we forward (another bind, a `sync`, whatever) then gets rejected as "invalid arguments," even though it's byte-perfect on its own.

  Confirmed with a small throwaway diagnostic client (`examples/probe_bind.rs` -- connects as a real Wayland client, binds a chosen set of globals in one burst, checks for `wl_display.error`) run against real labwc:
  - Binding several recognized-and-unrecognized globals together, no interleaved follow-up requests: always passes.
  - Same binds + an interleaved follow-up on an unrecognized interface (which we drop): fails.
  - Same binds + an interleaved follow-up on a *recognized* interface (which we forward): passes.
  - A bare client-side new_id **skip** with zero proxy involvement at all (`SKIP` token in the probe, burns an id, sends nothing): fails identically. This isolated it conclusively -- it's the gap itself, not our drop logic specifically, that labwc objects to.

  **Fix**: expanded `interfaces.rs` coverage so far fewer real-world globals hit the drop path at all. Added three more crates from the same smithay/wayland-rs family -- `wayland-protocols-wlr` (wlroots' own extensions: layer-shell, screencopy, gamma-control, etc. -- never went through freedesktop's official process, hence a separate crate from `wayland-protocols`), `wayland-protocols-plasma` (KDE's `org_kde_kwin_*`), and `wayland-protocols-misc` (orphaned-but-widely-used ones that fit neither bucket: `zwp_input_method_manager_v2`, `zwp_virtual_keyboard_manager_v1`) -- plus enabled `wayland-protocols`' own `staging`/`unstable` features, which were off. Went from 7 resolvable interfaces (core wayland.xml + xdg-shell only) to 46. Only top-level bindable globals need a table entry; child objects created by typed requests (e.g. `get_default_feedback` -> `zwp_linux_dmabuf_feedback_v1`) resolve automatically through that request's static `MessageDesc::child_interface`, never through this table.

  Added `interfaces::tests::resolves_every_global_observed_from_real_labwc`: a hardcoded list of every global a real run against labwc 0.9.3/wlroots 0.19.2 actually advertised, asserting each resolves. This is a plain `cargo test`, no live compositor needed -- it's meant to fail loudly the next time a real desktop's global list outpaces this table, instead of the silent per-message drop that caused this whole bug.

  Verified: all 18 tests pass; the exact probe scenario that reproduced the bug now passes; 15/15 clean `wayland-info` runs against real labwc with zero warnings/errors in the proxy log (previously deterministic 15/15 *failure* right before this fix).

  For the standing design of `interfaces.rs` itself -- what it covers, the gap-1/gap-2 framework, what's still open, and what improvements were considered but deferred -- see `docs/architecture-notes.md` rather than duplicating that here; this entry is the investigation log, that file is the reference that outlives it.
