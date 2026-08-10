//! Waits for a specific socket file to be created in a directory, freezes a
//! given process the instant it appears, renames the file to a private
//! path, then resumes the process. See
//! docs/adr/adr-0005-route-shell-launched-clients-through-the-proxy.md for
//! the full reasoning behind the socket-identity swap this is one piece of.
//!
//! Exists as a standalone native helper (not shell script polling, not a
//! shell-out to `inotifywait`) for two reasons:
//!
//! 1. A plain `while [ ! -S path ]; do sleep 0.05; done` loop can match a
//!    STALE leftover socket file from a previous gnome-shell/proxy cycle
//!    instead of the compositor's own fresh bind. `inotify`'s `IN_CREATE`
//!    only fires for a file created *after* the watch is established,
//!    which rules this out by construction (the stale file already
//!    existed before the watch was set up), not just by reacting faster.
//!    The stale file is also explicitly removed before watching starts,
//!    belt-and-suspenders.
//! 2. Freezing the target process the instant the file appears, before
//!    renaming, closes the *remaining* real race: the compositor's own
//!    subsequently-spawned children (DING, notification helpers, ...)
//!    connecting to the not-yet-renamed public name before the swap
//!    completes.
//!
//! `--freeze-method` (default `ptrace`, see `FreezeMethod`'s own doc
//! comments) controls *how* point 2 is done. Found live 2026-08-10: the
//! original `sigstop` method's own claim to freeze "the instant" the file
//! appears is an assumption, not a guarantee -- there's a real, non-zero
//! window between the kernel's `bind()` returning and this process
//! actually being scheduled, reading the inotify event, and issuing
//! `kill(SIGSTOP)`, and DING's own helper process won a race through that
//! window on a loaded system, connecting straight to gnome-shell and
//! ending up with a permanently-corrupted `stack_position` (confirmed live
//! via gdb -- see docs/KNOWN_BUGS.md) that then broke window
//! activation/focus for *other*, unrelated windows for the rest of that
//! session. `ptrace` closes this at the kernel level: PTRACE_SEIZE +
//! syscall-stop tracing on `bind()` means the target literally cannot
//! execute a single further instruction -- including spawning a child --
//! between its `bind()` returning and us resuming it, no scheduling gap
//! possible. Kept selectable via a flag (not the only option) because it's
//! genuinely a bigger, more invasive mechanism -- needs `CAP_SYS_PTRACE`
//! or `kernel.yama.ptrace_scope` relaxed for a same-user non-child
//! process -- and `sigstop` remains available as a fallback if that ever
//! causes trouble of its own on some system.

use std::ffi::OsStr;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use nix::libc;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
use nix::sys::ptrace;
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum FreezeMethod {
    /// Kernel-guaranteed atomic: ptrace-seize the target and catch its
    /// bind() syscall at the exact syscall-exit boundary, so the rename
    /// happens before the target can run any further code of its own.
    /// Default -- see this file's own module doc comment for why.
    Ptrace,
    /// The original approach: watch for the socket file via inotify,
    /// then SIGSTOP as fast as userspace scheduling allows. Has a real,
    /// non-zero race window (see the module doc comment) -- kept only
    /// as a fallback in case `ptrace` ever causes trouble of its own
    /// (missing permissions, `kernel.yama.ptrace_scope`, ...).
    Sigstop,
}

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

    /// Process to freeze the instant the file appears, and release once
    /// the rename is done. Normally the compositor that's about to bind
    /// `watch_name`.
    #[arg(long)]
    freeze_pid: i32,

    /// Give up (non-zero exit) if the file never appears within this many
    /// seconds -- e.g. the target process crashed before ever binding it.
    #[arg(long, default_value_t = 30)]
    timeout_secs: u64,

    /// See `FreezeMethod`'s own doc comments.
    #[arg(long, value_enum, default_value_t = FreezeMethod::Ptrace)]
    freeze_method: FreezeMethod,
}

/// Whether `pid` is still meaningfully alive -- exited (not just zombie)
/// counts as dead. `kill(pid, None)` (signal 0) alone isn't enough: a
/// zombie process (exited, not yet reaped by its parent) still holds its
/// PID table entry, so signal-0 existence checks keep succeeding against
/// it. Found via this binary's own integration test, not live: the
/// session wrapper calls socket-handoff *before* its own `wait
/// "$SHELL_PID"`, so a gnome-shell that crashes during exactly this
/// window would sit as an unreaped zombie for the same reason, fooling a
/// signal-0-only check in real use too, not just in a test harness that
/// forgot to reap its own dummy target.
fn target_is_alive(pid: Pid) -> bool {
    if kill(pid, None).is_err() {
        return false;
    }
    match std::fs::read_to_string(format!("/proc/{pid}/status")) {
        Ok(status) => !status.lines().any(|line| line.starts_with("State:") && line.contains('Z')),
        // Can't read /proc for it at all -- treat as gone rather than
        // risking an infinite wait on something we can no longer see.
        Err(_) => false,
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let target_path = cli.dir.join(&cli.watch_name);
    let rename_path = cli.dir.join(&cli.rename_to);

    // Belt-and-suspenders alongside IN_CREATE's own semantics (see the
    // module doc comment): removing any stale file before watching starts
    // means the ONLY way an event can fire for this exact name is a
    // genuinely fresh bind(), not a leftover from a previous cycle.
    if target_path.exists() {
        std::fs::remove_file(&target_path)
            .with_context(|| format!("removing stale {}", target_path.display()))?;
    }

    match cli.freeze_method {
        FreezeMethod::Sigstop => run_sigstop(&cli, &target_path, &rename_path),
        FreezeMethod::Ptrace => run_ptrace(&cli, &target_path, &rename_path),
    }
}

/// The original inotify-watch-then-SIGSTOP implementation, unchanged in
/// behavior from before `--freeze-method` existed.
fn run_sigstop(cli: &Cli, target_path: &Path, rename_path: &Path) -> Result<()> {
    let watch_name = OsStr::new(&cli.watch_name);

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
        if !target_is_alive(pid) {
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
            // yet, *usually* -- see the module doc comment for why this
            // isn't actually guaranteed, unlike the ptrace method.
            kill(pid, Signal::SIGSTOP).context("SIGSTOP")?;
            let result = std::fs::rename(target_path, rename_path)
                .with_context(|| format!("renaming {} -> {}", cli.watch_name, cli.rename_to));
            // Always resume, even if the rename failed -- a permanently
            // frozen compositor is worse than a failed handoff.
            let _ = kill(pid, Signal::SIGCONT);
            result?;
            return Ok(());
        }
    }
}

/// Releases a ptrace-seized tracee on drop unless explicitly disarmed --
/// the safety net that keeps every exit path out of `run_ptrace` (normal
/// return, `?`-propagated error, even a panic-unwind) from ever leaving
/// the target permanently ptrace-stopped, which would otherwise mean an
/// unbootable session. Best-effort: if the detach call itself fails
/// there's nothing more a `Drop` impl can safely do about it, and the
/// kernel's own documented fallback (an untraced tracee whose tracer
/// exits gets auto-detached and resumed, unless PTRACE_O_EXITKILL was
/// set, which this file never sets) still applies even if this guard's
/// own explicit detach call errors out.
struct PtraceGuard {
    pid: Pid,
    armed: bool,
}

impl PtraceGuard {
    fn new(pid: Pid) -> Self {
        Self { pid, armed: true }
    }

    /// Call once the tracee has already been detached (successfully or
    /// not) or is confirmed gone -- prevents a redundant double-detach
    /// attempt on drop.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PtraceGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = ptrace::detach(self.pid, None);
        }
    }
}

/// See `FreezeMethod::Ptrace`'s own doc comment and this file's module
/// doc comment for the full reasoning. Traces only `bind()` (via
/// PTRACE_SYSCALL syscall-entry/exit stops, filtered by syscall number --
/// not a seccomp-bpf filter, since that would need the *tracee itself* to
/// install it before the syscall we care about, which isn't ours to
/// arrange for an already-running gnome-shell); every other syscall the
/// target makes is single-stepped past with no other inspection.
fn run_ptrace(cli: &Cli, target_path: &Path, rename_path: &Path) -> Result<()> {
    let pid = Pid::from_raw(cli.freeze_pid);
    let deadline = Instant::now() + Duration::from_secs(cli.timeout_secs);

    if !target_is_alive(pid) {
        bail!("pid {pid} exited before creating {}", target_path.display());
    }

    // PTRACE_SEIZE: attach without stopping the tracee or disturbing its
    // current state (unlike PTRACE_ATTACH, which sends an implicit
    // SIGSTOP the instant it attaches). Requires kernel.yama.ptrace_scope
    // relaxed (0) for a same-user, non-child process, or CAP_SYS_PTRACE.
    ptrace::seize(pid, ptrace::Options::PTRACE_O_TRACESYSGOOD).with_context(|| {
        format!(
            "PTRACE_SEIZE on pid {pid} -- is /proc/sys/kernel/yama/ptrace_scope relaxed to 0, \
             or does this process have CAP_SYS_PTRACE? (--freeze-method=sigstop is available as a fallback)"
        )
    })?;
    let mut guard = PtraceGuard::new(pid);

    // SEIZE alone doesn't stop anything -- request an initial stop so the
    // syscall-tracing loop below has a well-defined starting point. This
    // lands as a PTRACE_EVENT_STOP, a DISTINCT wait-status shape from an
    // ordinary signal-stop (see the match arms below) -- confirmed
    // against nix 0.31's own WaitStatus::from_raw before relying on it,
    // not assumed.
    ptrace::interrupt(pid).context("PTRACE_INTERRUPT")?;

    let mut in_syscall = false; // toggles entry <-> exit on each syscall-stop pair
    loop {
        if Instant::now() >= deadline {
            bail!("timed out after {}s waiting for pid {pid} to bind() {}", cli.timeout_secs, target_path.display());
        }

        match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
            // Found live 2026-08-10: a real gnome-shell makes tens of
            // thousands of syscalls before its own bind() (dynamic
            // linking, GPU/DRM/EGL setup, ...), and this WNOHANG loop
            // pays its sleep on very nearly EVERY one of them (the
            // gap between one resume and the next syscall-stop is
            // almost always shorter than the sleep, so each stop costs
            // one full sleep, not zero). At the old 20ms this meant
            // ~25 syscalls/sec -- a real startup blew straight through
            // --timeout-secs (30s default), confirmed via instrumented
            // counters: ~1500 stops processed before timing out, vs.
            // the 30,000+ a real cycle actually needs. 100us keeps the
            // same safe single-threaded polling design (no watchdog
            // thread coordinating ptrace calls from a second thread,
            // which Linux disallows anyway -- only the attaching
            // thread may issue ptrace ops) while cutting the per-stop
            // tax by ~200x; confirmed live afterwards: same real
            // gnome-shell startup completed in ~6s instead of timing
            // out at 30s.
            Ok(WaitStatus::StillAlive) => {
                std::thread::sleep(Duration::from_micros(100));
                continue;
            }
            Ok(WaitStatus::Exited(_, code)) => {
                guard.disarm(); // nothing left to detach from
                bail!("pid {pid} exited (code {code}) before ever calling bind()");
            }
            Ok(WaitStatus::Signaled(_, sig, _)) => {
                guard.disarm();
                bail!("pid {pid} was killed by {sig:?} before ever calling bind()");
            }
            // The initial PTRACE_INTERRUPT stop (PTRACE_EVENT_STOP), and
            // any other ptrace-event stop we might see -- this
            // configuration never requests PTRACE_O_TRACEFORK/CLONE/EXEC,
            // so in practice this is only ever the interrupt itself.
            // Same response either way: start (or resume) syscall
            // tracing.
            Ok(WaitStatus::PtraceEvent(_, _, _)) => {
                ptrace::syscall(pid, None).context("PTRACE_SYSCALL (after ptrace-event stop)")?;
            }
            Ok(WaitStatus::PtraceSyscall(_)) => {
                in_syscall = !in_syscall;
                if in_syscall {
                    // Syscall entry -- its return value isn't known
                    // until the matching exit stop, nothing to check yet.
                    ptrace::syscall(pid, None).context("PTRACE_SYSCALL (entry)")?;
                    continue;
                }
                // Syscall exit -- orig_rax still names which syscall this
                // was; rax now holds its return value.
                let regs = ptrace::getregs(pid).context("PTRACE_GETREGS")?;
                let is_bind = regs.orig_rax as i64 == libc::SYS_bind;
                let succeeded = (regs.rax as i64) >= 0;
                if is_bind && succeeded && target_path.exists() {
                    // The tracee is STILL stopped at this exact point --
                    // it cannot run any of its own code (spawn a child,
                    // connect a socket, anything at all) until we resume
                    // it. Doing the rename here, before that resume, is
                    // what makes this atomic where the sigstop method
                    // isn't -- see the module doc comment.
                    let result = std::fs::rename(target_path, rename_path)
                        .with_context(|| format!("renaming {} -> {}", target_path.display(), rename_path.display()));
                    let detach_result = ptrace::detach(pid, None).context("PTRACE_DETACH");
                    guard.disarm();
                    result?;
                    detach_result?;
                    return Ok(());
                }
                ptrace::syscall(pid, None).context("PTRACE_SYSCALL (exit)")?;
            }
            Ok(WaitStatus::Stopped(_, sig)) => {
                // A genuine signal the tracee received (not one of our
                // own synthetic stops, which are the two arms above) --
                // re-inject it so the target's own signal handling still
                // works normally, then keep tracing syscalls.
                let deliver = match sig {
                    Signal::SIGTRAP | Signal::SIGSTOP => None,
                    other => Some(other),
                };
                ptrace::syscall(pid, deliver).context("PTRACE_SYSCALL (signal-stop)")?;
            }
            Ok(other) => bail!("unexpected wait status for pid {pid}: {other:?}"),
            Err(nix::errno::Errno::ECHILD) => {
                guard.disarm();
                bail!("pid {pid} is gone (ECHILD) before ever calling bind()");
            }
            Err(e) => bail!("waitpid on pid {pid} failed: {e}"),
        }
    }
}
