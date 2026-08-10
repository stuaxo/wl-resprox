//! Formalizes the ad-hoc verification done live 2026-08-03 (see
//! docs/adr/adr-0005-route-shell-launched-clients-through-the-proxy.md)
//! into real `cargo test` coverage, per the user's explicit ask for
//! lifecycle-level integration tests -- everything here was previously
//! only checked by hand with throwaway shell commands against the built
//! binary, which is exactly the kind of check that stops happening once
//! nobody's actively debugging the issue that prompted it.
//!
//! Spawns the actual compiled `socket-handoff` binary as a subprocess
//! (`env!("CARGO_BIN_EXE_socket-handoff")`, Cargo's own mechanism for
//! referencing a sibling binary target from an integration test) rather
//! than testing library functions directly -- `socket-handoff` has no
//! library surface, it's a small standalone tool, and its actual contract
//! is "run it as a process against a real directory and a real target
//! pid," which is what these tests exercise.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

fn socket_handoff_bin() -> &'static str {
    env!("CARGO_BIN_EXE_socket-handoff")
}

/// A short-lived unique directory under /tmp, not the shared scratchpad --
/// AF_UNIX socket paths are capped at ~108 bytes (sun_path), and this
/// project's own scratchpad path is already too long for that (hit live
/// while writing the very first ad-hoc version of this test, see
/// plan-desktop-resilience.md).
fn short_test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wl-proxy-handoff-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

/// The bug this proves fixed: a stale leftover socket file from a
/// previous cycle must never be mistaken for a fresh bind. Found live
/// 2026-08-03 as the root cause of DING and other gnome-shell startup
/// helpers connecting straight to gnome-shell instead of the proxy --
/// the session wrapper's original plain `while [ ! -S path ]` poll loop
/// matched a stale file immediately, before gnome-shell's own real bind
/// ever happened.
///
/// Explicitly `--freeze-method=sigstop`: this test's dummy target
/// (`sleep`) never calls `bind()` itself -- the test creates the "real"
/// socket from its own process instead, standing in for a target that's
/// merely *frozen*, not one whose own syscall we need to catch. That
/// setup only exercises the file-watching semantics this test is
/// actually about, which apply the same way regardless of freeze
/// method; see socket_handoff_ptrace.rs for coverage of the ptrace
/// method's own distinct mechanism (which needs a target that really
/// does call bind() itself).
#[test]
fn ignores_a_preexisting_stale_file_and_picks_up_the_real_one() {
    let dir = short_test_dir("stale");
    std::fs::write(dir.join("wayland-0"), b"").expect("seed stale file");

    let target = Command::new("sleep").arg("30").spawn().expect("spawn dummy target");
    let target_pid = target.id();

    let mut child = Command::new(socket_handoff_bin())
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
            "5",
            "--freeze-method",
            "sigstop",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn socket-handoff");

    // Give it time to remove the stale file and start watching -- if it
    // wrongly matched the stale file, it would have already exited by now.
    std::thread::sleep(Duration::from_millis(200));
    assert!(!dir.join("wayland-0").exists(), "stale file should have been removed immediately");
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "socket-handoff should still be waiting -- it must not have matched the stale file"
    );

    // Now create the REAL socket, as gnome-shell's own bind() would.
    let _real_socket = std::os::unix::net::UnixListener::bind(dir.join("wayland-0")).expect("bind real socket");

    let status = child.wait().expect("wait for socket-handoff");
    assert!(status.success(), "socket-handoff should succeed once the real file appears");
    assert!(!dir.join("wayland-0").exists(), "the real socket should have been renamed away");
    assert!(dir.join("host-0").exists(), "the real socket should now exist at the rename target");

    let _ = kill_and_reap(target);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The other half of the contract: if the target process dies before
/// ever creating the watched file (e.g. gnome-shell crashing during its
/// own startup, before wl_display_add_socket), socket-handoff must fail
/// promptly -- not hang until --timeout-secs, and not report success.
#[test]
fn exits_promptly_and_nonzero_if_the_target_dies_first() {
    let dir = short_test_dir("dies-first");

    // Deliberately not reaped until after socket-handoff runs -- this is
    // exactly what proves the liveness check is robust against a zombie
    // (see target_is_alive's own doc comment), not just against a fully
    // gone PID. A stray "your Child was dropped without being waited on"
    // lint would defeat the entire point of this test.
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
        ])
        .output()
        .expect("run socket-handoff");
    let elapsed = start.elapsed();

    assert!(!output.status.success(), "must report failure when the target died first");
    assert!(
        elapsed < Duration::from_secs(5),
        "must fail promptly once the target's liveness check notices it's gone, not wait out the full \
         10s --timeout-secs (took {elapsed:?})"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("exited"),
        "error message should explain the target process exited, got: {stderr:?}"
    );

    let _ = target.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

fn kill_and_reap(mut child: std::process::Child) -> std::io::Result<()> {
    let _ = child.kill();
    child.wait().map(|_| ())
}
