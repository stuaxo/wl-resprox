# Temporary system state -- cleanup checklist

Everything below was added to **this specific laptop** on 2026-08-03 to
support live desktop-resilience debugging (see `plan-desktop-resilience.md`
and `docs/adr/adr-0004-gnome-session-bypass.md`). None of it is tracked by
git (it lives outside the repo, in system/user config), and none of it is
needed for the *repo* itself to build or be understood -- this file exists
purely so a future session (or you) can cleanly revert the machine, or
confirm what's still live. Also tracked in the `project-gdm-autologin-temporary`
memory.

Nothing here is destructive to revert -- each item's own "how to revert" is
safe to run independently, in any order.

## 1. GDM auto-login -- **highest priority to revert if this laptop leaves your control**

`/etc/gdm3/custom.conf`, under `[daemon]`:
```
AutomaticLoginEnable = true
AutomaticLogin = stu
```
Anyone with physical/console access gets an instant, already-authenticated
desktop, no password -- this was accepted deliberately for unattended crash
testing, not as a standing choice.

**Revert**: re-comment those two lines, then `sudo systemctl restart gdm`.

## 2. Sudoers grants (`/etc/sudoers.d/wayland-proxy-testing`)

Full file content (added incrementally over the session -- this is the
final state):
```
stu ALL=(root) NOPASSWD: /usr/bin/systemctl restart gdm
stu ALL=(root) NOPASSWD: /usr/bin/dbus-send --system --print-reply --dest=org.freedesktop.Accounts /org/freedesktop/Accounts/User1000 org.freedesktop.Accounts.User.SetSession string:wl-res-labwc
stu ALL=(root) NOPASSWD: /usr/bin/dbus-send --system --print-reply --dest=org.freedesktop.Accounts /org/freedesktop/Accounts/User1000 org.freedesktop.Accounts.User.SetSession string:wl-res-gnome-shell
stu ALL=(root) NOPASSWD: /usr/bin/systemctl start gdm
stu ALL=(root) NOPASSWD: /usr/bin/systemctl stop gdm
stu ALL=(root) NOPASSWD: /usr/bin/dbus-send --system --print-reply --dest=org.freedesktop.Accounts /org/freedesktop/Accounts/User1000 org.freedesktop.Accounts.User.SetSession string:wl-res-gnome-shell-direct
```
Lets Claude (or anyone with your `stu` shell access) restart/stop/start
`gdm` and switch your default session without a password prompt, for
unattended iteration.

**Revert**: `sudo rm /etc/sudoers.d/wayland-proxy-testing`.

## 3. AccountsService default session

Set via `SetSession`/`SetSessionType` D-Bus calls (see item 2's sudoers
lines) rather than a file you'd hand-edit. Currently set to
`wl-res-gnome-shell-direct` -- whichever of the three test sessions was
last selected controls what GDM (and autologin, per item 1) boots into.

**Revert to the normal session**:
```bash
sudo dbus-send --system --print-reply --dest=org.freedesktop.Accounts \
  /org/freedesktop/Accounts/User$(id -u) \
  org.freedesktop.Accounts.User.SetSession string:ubuntu
```

## 4. New GDM session entries (`/usr/share/wayland-sessions/`)

```
wl-res-gnome-shell.desktop
wl-res-labwc.desktop
wl-res-gnome-shell-direct.desktop
```
These are real deliverables (the selectable sessions this whole debugging
push built), not incidental cleanup debris -- keep them unless you
specifically want the machine back to a completely stock session list.

**Revert (removes the sessions entirely)**:
```bash
sudo rm /usr/share/wayland-sessions/wl-res-gnome-shell.desktop \
        /usr/share/wayland-sessions/wl-res-labwc.desktop \
        /usr/share/wayland-sessions/wl-res-gnome-shell-direct.desktop
```

## 5. Per-user systemd units (`~/.config/systemd/user/`, no sudo involved)

```
org.gnome.Shell@wl-res-gnome-shell.service.d/override.conf
gnome-session@wl-res-gnome-shell.target.d/override.conf
wayland-proxy-gnome-shell.service          (enabled -- symlinked into graphical-session.target.wants/)
wayland-proxy-labwc.service                (enabled -- symlinked into graphical-session.target.wants/)
wayland-proxy-gnome-shell-direct.service   (enabled -- symlinked into graphical-session.target.wants/)
```
All three proxy units are `ExecCondition=`-gated on `$XDG_SESSION_DESKTOP`
matching their own session, so they no-op harmlessly in the normal `ubuntu`
session -- safe to leave enabled even if you're not actively testing.

**Revert (disables and removes all of it)**:
```bash
systemctl --user disable wayland-proxy-gnome-shell wayland-proxy-labwc wayland-proxy-gnome-shell-direct
rm -rf ~/.config/systemd/user/org.gnome.Shell@wl-res-gnome-shell.service.d \
       ~/.config/systemd/user/gnome-session@wl-res-gnome-shell.target.d \
       ~/.config/systemd/user/wayland-proxy-gnome-shell.service \
       ~/.config/systemd/user/wayland-proxy-labwc.service \
       ~/.config/systemd/user/wayland-proxy-gnome-shell-direct.service
systemctl --user daemon-reload
```

## Quick "am I still in a testing state" check

```bash
grep -q '^AutomaticLoginEnable = true' /etc/gdm3/custom.conf && echo "autologin: ON"
sudo test -f /etc/sudoers.d/wayland-proxy-testing && echo "sudoers grant: present"
systemctl --user is-enabled wayland-proxy-gnome-shell-direct 2>&1
```
