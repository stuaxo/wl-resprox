//! Waits for a specific socket file to be created in a directory, freezes a
//! given process the instant it appears, renames the file to a private
//! path, then resumes the process. See
//! docs/adr/adr-0005-route-shell-launched-clients-through-the-proxy.md for
//! the full reasoning behind the socket-identity swap this is one piece of.
//!
//! Exists as a standalone native helper (not shell script polling, not a
//! shell-out to `inotifywait`) for two reasons found live 2026-08-03:
//!
//! 1. A plain `while [ ! -S path ]; do sleep 0.05; done` loop in the
//!    session wrapper script matched a STALE leftover socket file from a
//!    previous gnome-shell/proxy cycle instead of the compositor's own
//!    fresh bind -- confirmed by comparing the renamed file's birth time
//!    against gnome-shell's own reported bind timestamp, which were over a
//!    minute apart. `inotify`'s `IN_CREATE` only fires for a file created
//!    *after* the watch is established, which rules this out by
//!    construction (the stale file already existed before the watch was
//!    set up), not just by reacting faster. The stale file is also
//!    explicitly removed before watching starts, belt-and-suspenders.
//! 2. Freezing the target process (`SIGSTOP`) the instant the file appears,
//!    before renaming, closes the *remaining* real race: the compositor's
//!    own subsequently-spawned children (DING, notification helpers, ...)
//!    connecting to the not-yet-renamed public name before the swap
//!    completes. `SIGSTOP` stops every thread in the process at the kernel
//!    scheduler level -- no userspace code runs again until `SIGCONT`,
//!    including whatever code would otherwise go on to spawn those
//!    children. It does not affect the already-bound-and-listening kernel
//!    socket itself, so nothing already connected is disturbed.

use std::ffi::OsStr;
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Parser;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

#[derive(Parser)]
struct Cli {
    /// Directory to watch (normally $XDG_RUNTIME_DIR).
    #[arg(long)]
    dir: PathBuf,

    /// Filename to wait for within `dir`.
    #[arg(long)]
    watch_name: String,

    /// Filename to rename `watch_name` to once it appears.
    #[arg(long)]
    rename_to: String,

    /// Process to SIGSTOP the instant the file appears, and SIGCONT once
    /// the rename is done. Normally the compositor that's about to bind
    /// `watch_name`.
    #[arg(long)]
    freeze_pid: i32,

    /// Give up (non-zero exit) if the file never appears within this many
    /// seconds -- e.g. the target process crashed before ever binding it.
    #[arg(long, default_value_t = 30)]
    timeout_secs: u64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let target_path = cli.dir.join(&cli.watch_name);
    let watch_name = OsStr::new(&cli.watch_name);

    // Belt-and-suspenders alongside IN_CREATE's own semantics (see the
    // module doc comment): removing any stale file before watching starts
    // means the ONLY way an event can fire for this exact name is a
    // genuinely fresh bind(), not a leftover from a previous cycle.
    if target_path.exists() {
        std::fs::remove_file(&target_path)
            .with_context(|| format!("removing stale {}", target_path.display()))?;
    }

    let inotify = Inotify::init(InitFlags::IN_NONBLOCK).context("inotify_init1")?;
    inotify
        .add_watch(&cli.dir, AddWatchFlags::IN_CREATE | AddWatchFlags::IN_MOVED_TO)
        .with_context(|| format!("watching {}", cli.dir.display()))?;

    let pid = Pid::from_raw(cli.freeze_pid);
    let deadline = Instant::now() + Duration::from_secs(cli.timeout_secs);
    let poll_timeout = PollTimeout::try_from(Duration::from_millis(200)).expect("200ms fits");

    loop {
        if Instant::now() >= deadline {
            bail!("timed out after {}s waiting for {}", cli.timeout_secs, target_path.display());
        }
        // Signal 0 (None): never actually signals the process, just checks
        // it still exists -- so a crashed target doesn't leave us blocked
        // here until the timeout for no reason.
        if kill(pid, None).is_err() {
            bail!("pid {pid} exited before creating {}", target_path.display());
        }

        let mut fds = [PollFd::new(inotify.as_fd(), PollFlags::POLLIN)];
        if poll(&mut fds, poll_timeout).context("poll on inotify fd")? == 0 {
            continue; // nothing yet this round -- back to the liveness/deadline check
        }

        for event in inotify.read_events().context("reading inotify events")? {
            if event.name.as_deref() != Some(watch_name) {
                continue;
            }
            // Freeze IMMEDIATELY -- the kernel already has the listening
            // socket up (bind() already returned, which is what generated
            // this event), but the target's OWN subsequent code hasn't run
            // yet. See point 2 in the module doc comment for why this is
            // the actual race that matters here.
            kill(pid, Signal::SIGSTOP).context("SIGSTOP")?;
            let result = std::fs::rename(&target_path, cli.dir.join(&cli.rename_to))
                .with_context(|| format!("renaming {} -> {}", cli.watch_name, cli.rename_to));
            // Always resume, even if the rename failed -- a permanently
            // frozen compositor is worse than a failed handoff.
            let _ = kill(pid, Signal::SIGCONT);
            result?;
            return Ok(());
        }
    }
}
