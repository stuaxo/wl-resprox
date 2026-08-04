//! Diagnostic-only tool, not part of the proxy itself: extends
//! `probe_reconnect_resize` (which proved a bare wl_shm-only client does
//! NOT reproduce the live "invalid arguments for wl_shm#N.create_pool"
//! failure, 2/2 clean, even through a real crash+reconnect+recreation
//! cycle) by adding the one major structural piece that tool skipped: a
//! real `wl_surface`/`xdg_surface`/`xdg_toplevel` chain, mapped and
//! `commit()`-ed for real, with every `xdg_surface.configure` actually
//! acked -- exactly the sequence `WAYLAND_DEBUG=1` showed a real GTK4
//! client going through immediately before the failure (a real
//! compositor-driven resize `configure`, acked, THEN the
//! destroy+create_pool burst).
//!
//! Same live-crash-testing caveat as `probe_reconnect_resize`: this tool
//! CRASHES gnome-shell itself as part of its own run.
//!
//! Usage: WAYLAND_DISPLAY=wayland-0 cargo run --example
//! probe_reconnect_resize_with_surface

use std::env;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

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

/// Drains and acks every `xdg_surface(xdg_surface_id).configure` seen
/// within `quiet_for` of silence -- a real client always acks promptly,
/// and (per the live trace this tool is reproducing) more than one
/// configure can arrive close together (this project's own synthesized
/// one during recovery, then a later real one once traffic resumes).
/// Requires the stream to already have a read timeout set shorter than
/// `quiet_for`.
fn drain_and_ack_configures(stream: &mut UnixStream, buf: &mut Vec<u8>, xdg_surface_id: u32, quiet_for: Duration) {
    let mut tmp = [0u8; 16 * 1024];
    let mut last_activity = Instant::now();
    loop {
        while let Some((msg, consumed)) = wire::take_message(buf) {
            let header = wire::MessageHeader::parse(msg).unwrap();
            let payload = &msg[wire::HEADER_LEN..];
            if header.sender_id == xdg_surface_id && header.opcode == 0 {
                if let Some(serial) = read_u32(payload, 0) {
                    println!("  got xdg_surface.configure(serial={serial}), acking...");
                    let mut p = Vec::new();
                    put_u32(&mut p, serial);
                    stream.write_all(&wire::build_message(xdg_surface_id, 4, &p)).expect("sending ack_configure");
                }
            }
            buf.drain(..consumed);
            last_activity = Instant::now();
        }
        if last_activity.elapsed() >= quiet_for {
            return;
        }
        match stream.read(&mut tmp) {
            Ok(0) => return,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                last_activity = Instant::now();
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => return,
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

    let mut wl_compositor = None;
    let mut xdg_wm_base = None;
    let mut wl_shm = None;
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
                                match iface.as_str() {
                                    "wl_compositor" => wl_compositor = Some((name, version)),
                                    "xdg_wm_base" => xdg_wm_base = Some((name, version)),
                                    "wl_shm" => wl_shm = Some((name, version)),
                                    _ => {}
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
    let (wl_compositor_name, wl_compositor_version) = wl_compositor.expect("proxy never advertised wl_compositor");
    let (xdg_wm_base_name, xdg_wm_base_version) = xdg_wm_base.expect("proxy never advertised xdg_wm_base");
    let (wl_shm_name, wl_shm_version) = wl_shm.expect("proxy never advertised wl_shm");
    println!("found wl_compositor={wl_compositor_name} xdg_wm_base={xdg_wm_base_name} wl_shm={wl_shm_name}");

    let tmp_dir = env::temp_dir().join(format!("probe-reconnect-resize-surface-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("create temp dir");

    // bind wl_compositor->4, xdg_wm_base->5, wl_shm->6; create_surface->7,
    // get_xdg_surface->8, get_toplevel->9.
    let mut out = Vec::new();
    let mut p = Vec::new();
    put_u32(&mut p, wl_compositor_name);
    put_str(&mut p, "wl_compositor");
    put_u32(&mut p, wl_compositor_version);
    put_u32(&mut p, 4);
    out.extend(wire::build_message(2, 0, &p));
    let mut p = Vec::new();
    put_u32(&mut p, xdg_wm_base_name);
    put_str(&mut p, "xdg_wm_base");
    put_u32(&mut p, xdg_wm_base_version);
    put_u32(&mut p, 5);
    out.extend(wire::build_message(2, 0, &p));
    let mut p = Vec::new();
    put_u32(&mut p, wl_shm_name);
    put_str(&mut p, "wl_shm");
    put_u32(&mut p, wl_shm_version);
    put_u32(&mut p, 6);
    out.extend(wire::build_message(2, 0, &p));
    let mut p = Vec::new();
    put_u32(&mut p, 7);
    out.extend(wire::build_message(4, 0, &p)); // wl_compositor(4).create_surface -> surface(7)
    let mut p = Vec::new();
    put_u32(&mut p, 8);
    put_u32(&mut p, 7);
    out.extend(wire::build_message(5, 2, &p)); // xdg_wm_base(5).get_xdg_surface(surface=7) -> xdg_surface(8)
    let mut p = Vec::new();
    put_u32(&mut p, 9);
    out.extend(wire::build_message(8, 1, &p)); // xdg_surface(8).get_toplevel -> xdg_toplevel(9)
    stream.write_all(&out).expect("sending bind+chain");

    // Initial commit with no buffer -- required by xdg-shell before the
    // first real configure arrives.
    stream.write_all(&wire::build_message(7, 6, &[])).expect("sending initial commit");

    println!("waiting for the initial xdg_surface.configure...");
    stream.set_read_timeout(Some(Duration::from_millis(500))).expect("set read timeout");
    drain_and_ack_configures(&mut stream, &mut buf, 8, Duration::from_millis(800));

    println!("create_pool A (size=584640) + create_buffer A (320x240), attach+commit...");
    let file_a = backing_file(&tmp_dir, "pool_a", 584640);
    let mut p = Vec::new();
    put_u32(&mut p, 10);
    put_u32(&mut p, 584640);
    let msg = wire::build_message(6, 0, &p);
    wayland_proxy::fdsocket::send_with_fds(raw_fd, &msg, &[file_a.as_raw_fd()]).expect("send create_pool A");

    let mut p = Vec::new();
    put_u32(&mut p, 11);
    put_u32(&mut p, 0);
    put_u32(&mut p, 320);
    put_u32(&mut p, 240);
    put_u32(&mut p, 1280);
    put_u32(&mut p, 0);
    stream.write_all(&wire::build_message(10, 0, &p)).expect("sending create_buffer A");

    let mut out = Vec::new();
    let mut p = Vec::new();
    put_u32(&mut p, 11); // buffer
    put_u32(&mut p, 0);
    put_u32(&mut p, 0);
    out.extend(wire::build_message(7, 1, &p)); // surface(7).attach
    let mut p = Vec::new();
    put_u32(&mut p, 0);
    put_u32(&mut p, 0);
    put_u32(&mut p, 320);
    put_u32(&mut p, 240);
    out.extend(wire::build_message(7, 9, &p)); // surface(7).damage_buffer
    out.extend(wire::build_message(7, 6, &[])); // surface(7).commit
    stream.write_all(&out).expect("sending attach+damage+commit A");

    // sync -- confirm everything is live before crashing.
    let mut p = Vec::new();
    put_u32(&mut p, 12);
    stream.write_all(&wire::build_message(1, 0, &p)).expect("sending sync");
    match read_until(&mut stream, &mut buf, 12, 0) {
        WaitResult::Matched => {}
        other => {
            eprintln!("FAIL: pre-crash sync didn't complete cleanly ({})", matches!(other, WaitResult::Error(_)));
            return ExitCode::FAILURE;
        }
    }
    println!("surface chain + pool A / buffer A confirmed live. Crashing gnome-shell (pkill -9 gnome-shell)...");

    let status = std::process::Command::new("pkill").args(["-9", "gnome-shell"]).status();
    match status {
        Ok(s) if s.success() => {}
        other => {
            eprintln!("FAIL: pkill -9 gnome-shell didn't report success: {other:?}");
            return ExitCode::FAILURE;
        }
    }

    let watchdog = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(25));
        eprintln!("FAIL: still waiting after 25s -- giving up");
        std::process::exit(1);
    });

    // Same retry-sync loop as probe_reconnect_resize -- a sync sent
    // during the frozen window is silently dropped, not queued.
    let mut sync_id = 13u32;
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
    println!("reconnect + recovery confirmed complete.");

    // Real compositors (and this project's own synthesis) send a fresh
    // configure once the toplevel is recreated -- drain and ack whatever
    // shows up, same as a real client always does, before touching
    // buffers again.
    println!("draining and acking any post-reconnect configure(s)...");
    drain_and_ack_configures(&mut stream, &mut buf, 8, Duration::from_millis(1500));
    stream.set_read_timeout(None).expect("clear read timeout");

    // NOW the actual scenario: destroy the (recreated) buffer A / pool A,
    // immediately create pool B at a different size + buffer B, attach it,
    // damage, frame, commit -- matching the exact live failure's own
    // request order.
    println!("destroy buffer A + pool A, then IMMEDIATELY create_pool B (size=1056096) + create_buffer B (579x456) + attach+commit...");
    stream.write_all(&wire::build_message(11, 0, &[])).expect("sending wl_buffer(11).destroy");
    stream.write_all(&wire::build_message(10, 1, &[])).expect("sending wl_shm_pool(10).destroy");

    let file_b = backing_file(&tmp_dir, "pool_b", 1_056_096);
    let mut p = Vec::new();
    put_u32(&mut p, 14);
    put_u32(&mut p, 1_056_096);
    let msg = wire::build_message(6, 0, &p);
    wayland_proxy::fdsocket::send_with_fds(raw_fd, &msg, &[file_b.as_raw_fd()]).expect("send create_pool B");

    let mut p = Vec::new();
    put_u32(&mut p, 15);
    put_u32(&mut p, 0);
    put_u32(&mut p, 579);
    put_u32(&mut p, 456);
    put_u32(&mut p, 2316);
    put_u32(&mut p, 0);
    stream.write_all(&wire::build_message(14, 0, &p)).expect("sending create_buffer B");

    let mut out = Vec::new();
    let mut p = Vec::new();
    put_u32(&mut p, 15); // buffer B
    put_u32(&mut p, 0);
    put_u32(&mut p, 0);
    out.extend(wire::build_message(7, 1, &p)); // surface(7).attach
    let mut p = Vec::new();
    put_u32(&mut p, 0);
    put_u32(&mut p, 0);
    put_u32(&mut p, 579);
    put_u32(&mut p, 456);
    out.extend(wire::build_message(7, 9, &p)); // surface(7).damage_buffer
    out.extend(wire::build_message(7, 6, &[])); // surface(7).commit
    stream.write_all(&out).expect("sending attach+damage+commit B");

    let mut p = Vec::new();
    put_u32(&mut p, 16);
    stream.write_all(&wire::build_message(1, 0, &p)).expect("sending final sync");

    let result = read_until(&mut stream, &mut buf, 16, 0);
    std::mem::forget(watchdog);
    match result {
        WaitResult::Matched => {
            println!("PASS: pool B / buffer B created, attached, and sync completed cleanly -- did NOT reproduce");
            ExitCode::SUCCESS
        }
        WaitResult::Error(e) => {
            eprintln!("FAIL: {e} -- REPRODUCED");
            ExitCode::FAILURE
        }
        WaitResult::Closed => {
            eprintln!("FAIL: connection closed before the final sync completed -- REPRODUCED");
            ExitCode::FAILURE
        }
        WaitResult::TimedOut => {
            eprintln!("FAIL: no read timeout should be active here -- this shouldn't happen");
            ExitCode::FAILURE
        }
    }
}
