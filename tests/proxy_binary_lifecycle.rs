//! Formalizes the ad-hoc verification done live 2026-08-03 (see
//! docs/adr/adr-0005-route-shell-launched-clients-through-the-proxy.md)
//! of the proxy's `SIGUSR1` listener-rebind handler into real `cargo test`
//! coverage. Spawns the actual compiled `wayland-proxy` binary
//! (`env!("CARGO_BIN_EXE_wayland-proxy")`) rather than testing
//! `run_connection` as a library function the way `tests/integration.rs`
//! does -- the accept loop and signal handling under test here live in
//! `src/main.rs`, not the library, so there's no function to call
//! directly; the real, only observable contract is "run the actual
//! binary as a process and see what happens to real sockets."
//!
//! Deliberately exercises SEVERAL rebind cycles, not just one -- this is
//! specifically the "check our assumptions through the lifecycle" gap
//! flagged after tonight's session-wrapper bug (a decision that was only
//! ever exercised once in ad-hoc testing looked fine, and was still
//! wrong the second time a *different* code path -- a fresh login -- hit
//! it).

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

fn proxy_bin() -> &'static str {
    env!("CARGO_BIN_EXE_wayland-proxy")
}

/// See socket_handoff_integration.rs's own comment on this -- AF_UNIX
/// sun_path is capped around 108 bytes, too short for this project's own
/// scratchpad path.
fn short_test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wl-proxy-lifecycle-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

/// A connection that's still readable/writable (not reset, not EOF)
/// after however-long-it's-been -- the actual property that matters for
/// "did SIGUSR1 disturb already-accepted clients," proven the same way
/// production behavior was checked live: a zero-byte non-blocking read
/// returning WouldBlock (no data, but also no error/EOF) means the
/// connection is alive and just idle, which is exactly the expected
/// state for a client sitting on a frozen/relaying connection with
/// nothing in flight.
fn assert_connection_still_alive(stream: &mut UnixStream, label: &str) {
    stream.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 1];
    match stream.read(&mut buf) {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {} // alive, just idle -- expected
        Ok(0) => panic!("{label}: connection was closed (EOF)"),
        Ok(_) => {} // some data arrived, also fine -- still means the connection is alive
        Err(e) => panic!("{label}: connection errored: {e}"),
    }
    stream.set_nonblocking(false).unwrap();
}

fn kill_and_reap(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn sigusr1_rebinds_the_listener_across_several_cycles_without_dropping_existing_clients() {
    let dir = short_test_dir("multi-cycle");

    // A trivial fake "compositor" -- this test is about the PUBLIC
    // listener surviving SIGUSR1, not wire-protocol correctness (already
    // covered by tests/integration.rs), so it only needs to accept and
    // hold connections.
    let host_listener = UnixListener::bind(dir.join("fake-host-0")).expect("bind fake host");
    std::thread::spawn(move || {
        for stream in host_listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut buf = [0u8; 4096];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                }
            });
        }
    });

    let proxy = Command::new(proxy_bin())
        .env("XDG_RUNTIME_DIR", &dir)
        .args(["--display=fake-host-0", "--listen=test-wayland-0"])
        .spawn()
        .expect("spawn wayland-proxy");
    let proxy_pid = proxy.id();

    wait_for_socket(&dir.join("test-wayland-0"));

    let mut long_lived_clients = Vec::new();
    for cycle in 0..3 {
        // A client that connects once, at the start, and must survive
        // every subsequent rebind in this loop -- the actual property
        // ADR-0005's SIGUSR1 handler exists for.
        if cycle == 0 {
            let client = UnixStream::connect(dir.join("test-wayland-0")).expect("connect first client");
            long_lived_clients.push(client);
        }

        send_sigusr1(proxy_pid);
        // Give the async signal handler a moment to run the rebind.
        std::thread::sleep(Duration::from_millis(150));

        for (i, client) in long_lived_clients.iter_mut().enumerate() {
            assert_connection_still_alive(client, &format!("cycle {cycle}, long-lived client {i}"));
        }

        // A fresh connection immediately after each rebind must succeed --
        // proves the listener is actually back up, not just that the old
        // one didn't crash.
        let mut fresh = UnixStream::connect(dir.join("test-wayland-0"))
            .unwrap_or_else(|e| panic!("cycle {cycle}: fresh connection after rebind failed: {e}"));
        fresh.write_all(b"ping").expect("write to fresh connection");
        long_lived_clients.push(fresh);
    }

    kill_and_reap(proxy);
    let _ = std::fs::remove_dir_all(&dir);
}

fn wait_for_socket(path: &std::path::Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !path.exists() {
        if std::time::Instant::now() >= deadline {
            panic!("timed out waiting for {} to appear", path.display());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn send_sigusr1(pid: u32) {
    let status = Command::new("kill")
        .args(["-USR1", &pid.to_string()])
        .status()
        .expect("run kill(1)");
    assert!(status.success(), "kill -USR1 {pid} failed");
}
