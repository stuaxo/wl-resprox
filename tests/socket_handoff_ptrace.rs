//! Coverage for `socket-handoff --freeze-method=ptrace` specifically --
//! see socket_handoff_integration.rs for the file-watching semantics
//! shared with `--freeze-method=sigstop`, and src/bin/socket-handoff.rs's
//! own module doc comment for the full mechanism/rationale.
//!
//! Needs `kernel.yama.ptrace_scope` relaxed to 0 (or CAP_SYS_PTRACE) to
//! attach to a same-user, non-child process -- every test here checks
//! this upfront and skips (prints why, returns early) rather than
//! failing outright, so `cargo test` still passes cleanly in an
//! environment that hasn't relaxed it (e.g. a default CI container).

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

fn socket_handoff_bin() -> &'static str {
    env!("CARGO_BIN_EXE_socket-handoff")
}

fn fixture_bin() -> &'static str {
    env!("CARGO_BIN_EXE_test-fixture-bind-and-check")
}

fn short_test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wl-proxy-handoff-ptrace-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

/// True if this process can actually ptrace-seize an unrelated, same-user
/// process right now -- the real precondition every test here needs,
/// checked directly rather than just reading ptrace_scope (which doesn't
/// account for CAP_SYS_PTRACE also satisfying the requirement).
///
/// Found live 2026-08-10, the hard way: an earlier version of this spawned
/// `dummy` as a plain direct child of the test process and tried to seize
/// THAT. Yama's restricted mode (ptrace_scope=1, the normal default --
/// this machine's own value once the earlier debugging session's
/// deliberate relaxation to 0 was correctly reverted) always permits
/// tracing your own direct children regardless of scope, so that check
/// reported "available" unconditionally -- passing even when the real
/// scenario this whole file exists to test (socket-handoff, a wholly
/// unrelated process, seizing gnome-shell, a stranger's child) was
/// genuinely blocked, and the tests below failed outright with EPERM
/// instead of skipping. Fixed by spawning the dummy detached (backgrounded
/// by a shell that immediately exits), so it's reparented away to init and
/// is a true non-descendant by the time we try to seize it -- an accurate
/// stand-in for the real target relationship, not merely "any ptrace call
/// at all works".
fn ptrace_seize_available() -> bool {
    let Ok(output) = Command::new("sh")
        .args(["-c", "setsid sleep 5 </dev/null >/dev/null 2>&1 & echo $!"])
        .output()
    else {
        return false;
    };
    let Ok(dummy_pid) = String::from_utf8_lossy(&output.stdout).trim().parse::<i32>() else {
        return false;
    };
    let pid = nix::unistd::Pid::from_raw(dummy_pid);
    let available = nix::sys::ptrace::seize(pid, nix::sys::ptrace::Options::empty()).is_ok();
    if available {
        let _ = nix::sys::ptrace::detach(pid, None);
    }
    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
    available
}

macro_rules! require_ptrace_or_skip {
    () => {
        if !ptrace_seize_available() {
            eprintln!(
                "SKIPPED: can't ptrace-seize a same-user process here -- \
                 needs kernel.yama.ptrace_scope=0 or CAP_SYS_PTRACE. \
                 Run: sudo sysctl kernel.yama.ptrace_scope=0"
            );
            return;
        }
    };
}

/// The core atomicity claim, proven directly rather than inferred: the
/// fixture's own very next instruction after bind() returning can NEVER
/// see the original path still present, when socket-handoff is
/// genuinely watching via ptrace. This is the test the sigstop method
/// could never reliably pass (its whole bug was that this window,
/// while usually short, is real and non-zero) -- see
/// docs/KNOWN_BUGS.md and this file's own module doc comment.
#[test]
fn bind_is_atomically_renamed_before_the_target_can_see_it() {
    require_ptrace_or_skip!();

    let dir = short_test_dir("atomic");
    let target_path = dir.join("wayland-0");

    let fixture = Command::new(fixture_bin())
        .arg(&target_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fixture");
    let fixture_pid = fixture.id();

    let handoff_output = Command::new(socket_handoff_bin())
        .args([
            "--dir",
            dir.to_str().unwrap(),
            "--watch-name",
            "wayland-0",
            "--rename-to",
            "host-0",
            "--freeze-pid",
            &fixture_pid.to_string(),
            "--timeout-secs",
            "10",
            "--freeze-method",
            "ptrace",
        ])
        .output()
        .expect("run socket-handoff");
    assert!(
        handoff_output.status.success(),
        "socket-handoff --freeze-method=ptrace failed: {}",
        String::from_utf8_lossy(&handoff_output.stderr)
    );

    let fixture_output = fixture.wait_with_output().expect("wait for fixture");
    assert!(fixture_output.status.success(), "fixture itself failed to bind()");
    let stdout = String::from_utf8_lossy(&fixture_output.stdout);
    assert!(
        stdout.contains("still_exists=false"),
        "the fixture's own very next instruction after bind() saw the ORIGINAL path still \
         present -- the freeze wasn't actually atomic. Got: {stdout:?}"
    );

    assert!(!target_path.exists(), "original path should be gone (renamed away)");
    assert!(dir.join("host-0").exists(), "renamed socket should exist at the target name");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Same stale-file protection socket_handoff_integration.rs proves for
/// sigstop, against a target that genuinely calls bind() itself this
/// time (a real precondition for the ptrace method specifically, unlike
/// sigstop's dummy-`sleep`-based test).
#[test]
fn ptrace_method_also_ignores_a_preexisting_stale_file() {
    require_ptrace_or_skip!();

    let dir = short_test_dir("stale-ptrace");
    std::fs::write(dir.join("wayland-0"), b"").expect("seed stale file");

    let fixture = Command::new(fixture_bin())
        .arg(dir.join("wayland-0"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fixture");
    let fixture_pid = fixture.id();

    let handoff_output = Command::new(socket_handoff_bin())
        .args([
            "--dir",
            dir.to_str().unwrap(),
            "--watch-name",
            "wayland-0",
            "--rename-to",
            "host-0",
            "--freeze-pid",
            &fixture_pid.to_string(),
            "--timeout-secs",
            "10",
            "--freeze-method",
            "ptrace",
        ])
        .output()
        .expect("run socket-handoff");
    assert!(
        handoff_output.status.success(),
        "socket-handoff should succeed once the fixture's own real bind() happens: {}",
        String::from_utf8_lossy(&handoff_output.stderr)
    );

    let fixture_output = fixture.wait_with_output().expect("wait for fixture");
    assert!(fixture_output.status.success());
    assert!(dir.join("host-0").exists(), "the real socket should now exist at the rename target");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The other half of the contract, same as sigstop's own test: if the
/// target dies before ever calling bind(), socket-handoff must fail
/// promptly, not hang until --timeout-secs.
#[test]
fn ptrace_method_exits_promptly_if_the_target_dies_first() {
    require_ptrace_or_skip!();

    let dir = short_test_dir("dies-first-ptrace");

    let mut target = Command::new("sleep").arg("1").spawn().expect("spawn short-lived dummy target");
    let target_pid = target.id();

    let start = std::time::Instant::now();
    let output = Command::new(socket_handoff_bin())
        .args([
            "--dir",
            dir.to_str().unwrap(),
            "--watch-name",
            "wayland-0",
            "--rename-to",
            "host-0",
            "--freeze-pid",
            &target_pid.to_string(),
            "--timeout-secs",
            "10",
            "--freeze-method",
            "ptrace",
        ])
        .output()
        .expect("run socket-handoff");
    let elapsed = start.elapsed();

    assert!(!output.status.success(), "must report failure when the target died first");
    assert!(
        elapsed < Duration::from_secs(5),
        "must fail promptly once the target's exit is noticed, not wait out the full \
         10s --timeout-secs (took {elapsed:?})"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("exited") || stderr.contains("gone"),
        "error message should explain the target process exited, got: {stderr:?}"
    );

    let _ = target.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
