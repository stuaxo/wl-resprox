//! Diagnostic-only tool, not part of the proxy itself: reproduces the
//! LIVE "invalid arguments for wl_shm#N.create_pool" failure
//! (ADR-0006's "Open issue found live 2026-08-04") with a minimal raw
//! client instead of a full GTK4 app, to isolate whether GTK's own
//! interleaving matters or whether the bare proxy+compositor interaction
//! is enough on its own.
//!
//! `probe_create_pool_resize` already proved the bare destroy+create_pool
//! burst is fine both directly against the compositor AND through the
//! proxy with no crash involved -- 3/3 clean in each case. So the
//! remaining, narrower question this tool exists to answer is whether
//! the failure specifically needs a `wl_shm_pool`/`wl_buffer` that
//! SURVIVED an actual crash+reconnect (i.e. was replayed via
//! recreation.rs's ShmPool/ShmBuffer, using the proxy's OWN retained fd,
//! not the client's original one) before the destroy+create_pool burst
//! that follows.
//!
//! This tool CRASHES gnome-shell itself (`pkill -9 gnome-shell`) as part
//! of its own run -- only run it against a session you're deliberately
//! crash-testing, same as this whole project's live-testing practice.
//!
//! Usage: WAYLAND_DISPLAY=wayland-0 cargo run --example probe_reconnect_resize
//! (must point at the PROXY's public socket, not the compositor's
//! private one -- the whole point is exercising the reconnect path.)

use std::env;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use wayland_proxy::wire::{self, put_str, put_u32, read_str, read_u32};

fn decode_error(payload: &[u8]) -> (u32, u32, String) {
    let bad_object = read_u32(payload, 0).unwrap_or(0);
    let code = read_u32(payload, 4).unwrap_or(0);
    let msg_str = read_str(payload, 8).map(|(s, _)| s).unwrap_or_default();
    (bad_object, code, msg_str)
}

fn backing_file(dir: &std::path::Path, name: &str, size: u64) -> std::fs::File {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(dir.join(name))
        .unwrap_or_else(|e| panic!("creating backing file {name}: {e}"));
    f.set_len(size).unwrap_or_else(|e| panic!("sizing backing file {name}: {e}"));
    f
}

enum WaitResult {
    Matched,
    Error(String),
    Closed,
    TimedOut,
}

/// Reads until a message from `wait_sender`/`wait_opcode` arrives,
/// draining (and mostly ignoring) everything else in between -- used both
/// for the initial global-collection sync and for confirming the
/// connection is live again post-reconnect. `TimedOut` (distinct from a
/// real error) relies on the stream's own read timeout
/// (`set_read_timeout`) -- callers that can legitimately retry (a sync
/// sent too early, landing in the frozen window where it's silently
/// dropped rather than delayed -- see this tool's own retry loop below)
/// check for it specifically.
fn read_until(stream: &mut UnixStream, buf: &mut Vec<u8>, wait_sender: u32, wait_opcode: u16) -> WaitResult {
    let mut tmp = [0u8; 16 * 1024];
    loop {
        while let Some((msg, consumed)) = wire::take_message(buf) {
            let header = wire::MessageHeader::parse(msg).unwrap();
            let payload = &msg[wire::HEADER_LEN..];
            if header.sender_id == 1 && header.opcode == 0 {
                let (bad_object, code, msg_str) = decode_error(payload);
                buf.drain(..consumed);
                return WaitResult::Error(format!("wl_display.error: object={bad_object} code={code} message={msg_str:?}"));
            }
            let matched = header.sender_id == wait_sender && header.opcode == wait_opcode;
            buf.drain(..consumed);
            if matched {
                return WaitResult::Matched;
            }
        }
        match stream.read(&mut tmp) {
            Ok(0) => return WaitResult::Closed,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                return WaitResult::TimedOut;
            }
            Err(e) => return WaitResult::Error(format!("read error: {e}")),
        }
    }
}

fn main() -> ExitCode {
    let runtime_dir = env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR must be set");
    let display = env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".to_string());
    let socket_path = PathBuf::from(&runtime_dir).join(&display);
    println!("connecting to {socket_path:?} (should be the PROXY's public socket)...");

    let mut stream =
        UnixStream::connect(&socket_path).unwrap_or_else(|e| panic!("connecting to {socket_path:?}: {e}"));
    let raw_fd = stream.as_raw_fd();
    let mut buf = Vec::new();

    let mut out = Vec::new();
    let mut p = Vec::new();
    put_u32(&mut p, 2);
    out.extend(wire::build_message(1, 1, &p));
    let mut p = Vec::new();
    put_u32(&mut p, 3);
    out.extend(wire::build_message(1, 0, &p));
    stream.write_all(&out).expect("sending get_registry+sync");

    let mut wl_shm_name = None;
    let mut wl_shm_version = None;
    {
        let mut tmp = [0u8; 16 * 1024];
        'collect: loop {
            let n = stream.read(&mut tmp).expect("reading globals");
            assert_ne!(n, 0, "connection closed while collecting globals");
            buf.extend_from_slice(&tmp[..n]);
            while let Some((msg, consumed)) = wire::take_message(&buf) {
                let header = wire::MessageHeader::parse(msg).unwrap();
                let payload = &msg[wire::HEADER_LEN..];
                if header.sender_id == 2 && header.opcode == 0 {
                    if let Some(name) = read_u32(payload, 0) {
                        if let Some((iface, next)) = read_str(payload, 4) {
                            if let Some(version) = read_u32(payload, next) {
                                if iface == "wl_shm" {
                                    wl_shm_name = Some(name);
                                    wl_shm_version = Some(version);
                                }
                            }
                        }
                    }
                } else if header.sender_id == 3 && header.opcode == 0 {
                    buf.drain(..consumed);
                    break 'collect;
                }
                buf.drain(..consumed);
            }
        }
    }
    let wl_shm_name = wl_shm_name.expect("proxy never advertised wl_shm");
    let wl_shm_version = wl_shm_version.expect("wl_shm version");
    println!("found wl_shm: name={wl_shm_name} version={wl_shm_version}");

    let tmp_dir = env::temp_dir().join(format!("probe-reconnect-resize-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("create temp dir");

    // bind wl_shm -> guest 4, create pool A (guest 5) + buffer A (guest
    // 6) -- deliberately NOT destroyed before the crash, so they survive
    // via recreation.rs's ShmPool/ShmBuffer replay, using the PROXY's own
    // retained fd (not this client's original one) -- exactly the state
    // a real basic_shm.py client was in when it hit the live failure.
    let mut p = Vec::new();
    put_u32(&mut p, wl_shm_name);
    put_str(&mut p, "wl_shm");
    put_u32(&mut p, wl_shm_version);
    put_u32(&mut p, 4);
    stream.write_all(&wire::build_message(2, 0, &p)).expect("sending bind");

    println!("create_pool A (size=584640) + create_buffer A (320x240)...");
    let file_a = backing_file(&tmp_dir, "pool_a", 584640);
    let mut p = Vec::new();
    put_u32(&mut p, 5);
    put_u32(&mut p, 584640);
    let msg = wire::build_message(4, 0, &p);
    wayland_proxy::fdsocket::send_with_fds(raw_fd, &msg, &[file_a.as_raw_fd()]).expect("send create_pool A");

    let mut p = Vec::new();
    put_u32(&mut p, 6);
    put_u32(&mut p, 0);
    put_u32(&mut p, 320);
    put_u32(&mut p, 240);
    put_u32(&mut p, 1280);
    put_u32(&mut p, 0);
    stream.write_all(&wire::build_message(5, 0, &p)).expect("sending create_buffer A");

    // sync -- confirm pool A/buffer A are live before crashing.
    let mut p = Vec::new();
    put_u32(&mut p, 7);
    stream.write_all(&wire::build_message(1, 0, &p)).expect("sending sync");
    match read_until(&mut stream, &mut buf, 7, 0) {
        WaitResult::Matched => {}
        WaitResult::Error(e) => {
            eprintln!("FAIL: pre-crash sync failed: {e}");
            return ExitCode::FAILURE;
        }
        WaitResult::Closed | WaitResult::TimedOut => {
            eprintln!("FAIL: pre-crash sync didn't complete");
            return ExitCode::FAILURE;
        }
    }
    println!("pool A / buffer A confirmed live. Crashing gnome-shell (pkill -9 gnome-shell)...");

    let status = std::process::Command::new("pkill").args(["-9", "gnome-shell"]).status();
    match status {
        Ok(s) if s.success() => {}
        other => {
            eprintln!("FAIL: pkill -9 gnome-shell didn't report success: {other:?}");
            return ExitCode::FAILURE;
        }
    }

    // The client-facing socket stays open while the proxy reconnects, but
    // (found the hard way, on this tool's own first run) a sync sent
    // DURING the frozen window doesn't queue and wait -- it's silently
    // DROPPED (same as any other non-specially-handled request while
    // frozen -- see relay_ready_messages' DROPPED_FROZEN path), and
    // nothing ever answers it. Retry with a FRESH sync id (a late answer
    // for an abandoned one is simply drained and ignored by a later
    // read_until call looking for a different id) until one lands after
    // recovery has actually completed. Give it a generous overall
    // ceiling in case something's gone wrong -- a watchdog thread, since
    // read_until's own per-attempt timeout only bounds ONE attempt.
    let watchdog = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(20));
        eprintln!("FAIL: still waiting on reconnect after 20s -- giving up");
        std::process::exit(1);
    });
    stream
        .set_read_timeout(Some(Duration::from_millis(400)))
        .expect("set read timeout");

    let mut sync_id = 8u32;
    loop {
        let mut p = Vec::new();
        put_u32(&mut p, sync_id);
        stream.write_all(&wire::build_message(1, 0, &p)).expect("sending post-crash sync");
        match read_until(&mut stream, &mut buf, sync_id, 0) {
            WaitResult::Matched => break,
            WaitResult::TimedOut => {
                sync_id += 1;
                continue;
            }
            WaitResult::Error(e) => {
                eprintln!("FAIL: post-crash sync failed: {e}");
                return ExitCode::FAILURE;
            }
            WaitResult::Closed => {
                eprintln!("FAIL: connection closed waiting for post-crash sync");
                return ExitCode::FAILURE;
            }
        }
    }
    // Recovery confirmed complete -- back to blocking reads for the rest
    // of this tool's run (no more legitimate reason to retry).
    stream.set_read_timeout(None).expect("clear read timeout");
    println!("reconnect + recovery confirmed complete (pool A / buffer A should now be replayed).");

    // NOW the actual scenario: destroy the (recreated) buffer A / pool A,
    // immediately create pool B at a different size + buffer B, matching
    // the exact live failure.
    println!("destroy buffer A + pool A, then IMMEDIATELY create_pool B (size=1056096) + create_buffer B (579x456)...");
    stream.write_all(&wire::build_message(6, 0, &[])).expect("sending wl_buffer(6).destroy");
    stream.write_all(&wire::build_message(5, 1, &[])).expect("sending wl_shm_pool(5).destroy");

    let file_b = backing_file(&tmp_dir, "pool_b", 1_056_096);
    let mut p = Vec::new();
    put_u32(&mut p, 9);
    put_u32(&mut p, 1_056_096);
    let msg = wire::build_message(4, 0, &p);
    wayland_proxy::fdsocket::send_with_fds(raw_fd, &msg, &[file_b.as_raw_fd()]).expect("send create_pool B");

    let mut p = Vec::new();
    put_u32(&mut p, 10);
    put_u32(&mut p, 0);
    put_u32(&mut p, 579);
    put_u32(&mut p, 456);
    put_u32(&mut p, 2316);
    put_u32(&mut p, 0);
    stream.write_all(&wire::build_message(9, 0, &p)).expect("sending create_buffer B");

    let mut p = Vec::new();
    put_u32(&mut p, 11);
    stream.write_all(&wire::build_message(1, 0, &p)).expect("sending final sync");

    let result = read_until(&mut stream, &mut buf, 11, 0);
    // The watchdog thread only ever fires by calling process::exit, so
    // leaking its handle here (instead of joining) is deliberate -- we've
    // already got our answer by the time we'd join it.
    std::mem::forget(watchdog);
    match result {
        WaitResult::Matched => {
            println!("PASS: pool B / buffer B created and sync completed cleanly -- did NOT reproduce through a real reconnect");
            ExitCode::SUCCESS
        }
        WaitResult::Error(e) => {
            eprintln!("FAIL: {e} -- REPRODUCED through a real reconnect");
            ExitCode::FAILURE
        }
        WaitResult::Closed => {
            eprintln!("FAIL: connection closed before the final sync completed -- REPRODUCED through a real reconnect");
            ExitCode::FAILURE
        }
        WaitResult::TimedOut => {
            eprintln!("FAIL: no read timeout should be active here -- this shouldn't happen");
            ExitCode::FAILURE
        }
    }
}
