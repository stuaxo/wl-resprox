//! Diagnostic-only tool, not part of the proxy itself: connects as a
//! minimal real Wayland client DIRECTLY to a compositor (no proxy
//! involved at all) and replicates the exact "destroy old buffer/pool,
//! immediately create a differently-sized new pool" sequence a real
//! `scripts/gtk/basic_shm.py` client sent on resize, right after a
//! crash-recovery cycle -- and which the real compositor rejected with
//! `invalid arguments for wl_shm#N.create_pool` (see ADR-0006's "Open
//! issue found live 2026-08-04").
//!
//! Built specifically to settle the one open question that ADR left:
//! is this a proxy bug, or a pre-existing GTK4/mutter interaction
//! limitation that nothing before the proxy's own crash-recovery fixes
//! ever got far enough, fast enough, to trigger? Connecting straight to
//! the compositor -- no proxy, no crash, no reconnect, just the bare
//! destroy+create_pool burst at the exact same sizes the live failure
//! used -- isolates that question directly: if THIS reproduces the same
//! error, it's not our relay logic; if it doesn't, the proxy (or the
//! specific timing/ordering it introduces) is implicated after all.
//!
//! Usage: WAYLAND_DISPLAY=wl-res-gnome-shell-direct-host-0 cargo run
//! --example probe_create_pool_resize
//! (point WAYLAND_DISPLAY at the compositor's own private socket name,
//! NOT the proxy's public one -- that's the whole point.)

use std::env;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;

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

fn main() -> ExitCode {
    let runtime_dir = env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR must be set");
    let display = env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".to_string());
    let socket_path = PathBuf::from(&runtime_dir).join(&display);
    println!("connecting directly to {socket_path:?} (bypassing any proxy)...");

    let mut stream =
        UnixStream::connect(&socket_path).unwrap_or_else(|e| panic!("connecting to {socket_path:?}: {e}"));
    let raw_fd = stream.as_raw_fd();

    // get_registry(2), sync(3).
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
    let mut buf = Vec::new();
    let mut tmp = [0u8; 16 * 1024];
    'collect: loop {
        let n = stream.read(&mut tmp).expect("reading globals");
        if n == 0 {
            eprintln!("FAIL: connection closed while collecting globals");
            return ExitCode::FAILURE;
        }
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
    let wl_shm_name = wl_shm_name.expect("compositor never advertised wl_shm");
    let wl_shm_version = wl_shm_version.expect("wl_shm version");
    println!("found wl_shm: name={wl_shm_name} version={wl_shm_version}");

    let tmp_dir = env::temp_dir().join(format!("probe-create-pool-resize-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("create temp dir");

    // bind wl_shm -> guest 4.
    let mut p = Vec::new();
    put_u32(&mut p, wl_shm_name);
    put_str(&mut p, "wl_shm");
    put_u32(&mut p, wl_shm_version);
    put_u32(&mut p, 4);
    stream.write_all(&wire::build_message(2, 0, &p)).expect("sending bind");

    // Pool A: an ordinary "startup" size, matching the smaller pool a
    // real basic_shm.py client had before ever resizing.
    println!("create_pool A (size=584640) + create_buffer A (320x240)...");
    let file_a = backing_file(&tmp_dir, "pool_a", 584640);
    let mut p = Vec::new();
    put_u32(&mut p, 5); // new_id -- wl_shm_pool A
    put_u32(&mut p, 584640); // size
    let msg = wire::build_message(4, 0, &p); // wl_shm(4).create_pool
    wayland_proxy::fdsocket::send_with_fds(raw_fd, &msg, &[file_a.as_raw_fd()]).expect("send create_pool A");

    let mut p = Vec::new();
    put_u32(&mut p, 6); // new_id -- wl_buffer A
    put_u32(&mut p, 0); // offset
    put_u32(&mut p, 320); // width
    put_u32(&mut p, 240); // height
    put_u32(&mut p, 1280); // stride
    put_u32(&mut p, 0); // format (ARGB8888)
    stream.write_all(&wire::build_message(5, 0, &p)).expect("sending create_buffer A"); // wl_shm_pool(5).create_buffer

    // Destroy buffer A then pool A, then IMMEDIATELY (no delay, no
    // waiting for any response) create pool B at a DIFFERENT size --
    // exactly the burst a real client's resize-driven reallocation sent
    // live, right down to pool B / buffer B's exact dimensions.
    println!("destroy buffer A + pool A, then IMMEDIATELY create_pool B (size=1056096) + create_buffer B (579x456)...");
    stream.write_all(&wire::build_message(6, 0, &[])).expect("sending wl_buffer(6).destroy");
    stream.write_all(&wire::build_message(5, 1, &[])).expect("sending wl_shm_pool(5).destroy");

    let file_b = backing_file(&tmp_dir, "pool_b", 1_056_096);
    let mut p = Vec::new();
    put_u32(&mut p, 7); // new_id -- wl_shm_pool B
    put_u32(&mut p, 1_056_096); // size
    let msg = wire::build_message(4, 0, &p); // wl_shm(4).create_pool
    wayland_proxy::fdsocket::send_with_fds(raw_fd, &msg, &[file_b.as_raw_fd()]).expect("send create_pool B");

    let mut p = Vec::new();
    put_u32(&mut p, 8); // new_id -- wl_buffer B
    put_u32(&mut p, 0); // offset
    put_u32(&mut p, 579); // width
    put_u32(&mut p, 456); // height
    put_u32(&mut p, 2316); // stride
    put_u32(&mut p, 0); // format
    stream.write_all(&wire::build_message(7, 0, &p)).expect("sending create_buffer B"); // wl_shm_pool(7).create_buffer

    let mut p = Vec::new();
    put_u32(&mut p, 9); // new_id -- sync callback
    stream.write_all(&wire::build_message(1, 0, &p)).expect("sending sync");

    buf.clear();
    loop {
        let n = match stream.read(&mut tmp) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("FAIL: read error waiting for sync: {e}");
                return ExitCode::FAILURE;
            }
        };
        if n == 0 {
            eprintln!("FAIL: connection closed before sync completed -- compositor likely killed it");
            return ExitCode::FAILURE;
        }
        buf.extend_from_slice(&tmp[..n]);
        while let Some((msg, consumed)) = wire::take_message(&buf) {
            let header = wire::MessageHeader::parse(msg).unwrap();
            let payload = &msg[wire::HEADER_LEN..];
            match (header.sender_id, header.opcode) {
                (1, 0) => {
                    let (bad_object, code, msg_str) = decode_error(payload);
                    eprintln!(
                        "FAIL: wl_display.error: object={bad_object} code={code} message={msg_str:?} -- \
                         REPRODUCED WITHOUT THE PROXY (not a proxy bug)"
                    );
                    return ExitCode::FAILURE;
                }
                (9, 0) => {
                    println!("PASS: pool B / buffer B created and sync completed cleanly -- did NOT reproduce without the proxy");
                    return ExitCode::SUCCESS;
                }
                _ => {}
            }
            let consumed_len = consumed;
            buf.drain(..consumed_len);
        }
    }
}
