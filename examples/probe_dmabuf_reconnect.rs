//! Diagnostic-only tool, not part of the proxy itself: isolates ADR-0008's
//! dmabuf client-wedge bug from the real desktop entirely.
//!
//! Live testing (`scripts/gtk/dmabuf_gl.py` via `scripts/live-crash-test.sh`,
//! see docs/adr/adr-0008-live-validate-dmabuf-recreation.md) confirmed a real
//! GTK4/GL client's process permanently wedges (stuck in `poll()`, its own
//! independent GLib stall timer never firing) after a crash/recovery cycle
//! that recreates a dmabuf-backed `wl_buffer` -- while an equivalent
//! wl_shm-backed client recovers cleanly through the exact same proxy-side
//! surface/toplevel recreation code. That live investigation couldn't
//! distinguish "the proxy's dmabuf recreation replays something mutter
//! doesn't like" from "GTK/Mesa/EGL's own client-side handling of a
//! recreated dmabuf fd blocks for reasons unrelated to the protocol" --
//! GTK, Mesa, and EGL are all black boxes from the live desktop's own
//! logs.
//!
//! This tool removes all three: it's a hand-rolled Wayland client (same
//! raw-wire style as `probe_reconnect_resize_with_surface.rs`, which this
//! is adapted from) that allocates a REAL dmabuf via GBM directly against
//! `/dev/dri/renderD128` (no EGL, no Mesa GL context, no GTK), creates a
//! `wl_buffer` from it via `zwp_linux_dmabuf_v1`, maps a toplevel, crashes
//! gnome-shell, waits for this project's own recovery, and then -- the
//! actual test -- re-attaches the RECREATED buffer, requests a new
//! `wl_surface.frame()`, and waits for `wl_callback.done`. If that never
//! arrives, the wedge reproduces at the pure protocol level, with no GTK/
//! Mesa/EGL involved at all, which would mean the bug is in mutter's own
//! handling of a recreated dmabuf-backed surface (or this proxy's replay
//! of it) -- not in GTK's or Mesa's client-side libraries. If it DOES
//! arrive cleanly, the wedge is something GTK/Mesa/EGL-specific that this
//! minimal client doesn't do, ruling out the protocol/proxy layer.
//!
//! Same live-crash-testing caveat as `probe_reconnect_resize_with_surface`:
//! this tool CRASHES gnome-shell itself as part of its own run. Needs real
//! GPU access (`/dev/dri/renderD128`) -- run on the same machine/session as
//! the live tests, not in a headless-only container.
//!
//! Usage: WAYLAND_DISPLAY=wayland-0 cargo run --example probe_dmabuf_reconnect

use std::env;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use gbm::{BufferObjectFlags, Device, Format, Modifier};

use wayland_proxy::wire::{self, put_str, put_u32, read_str, read_u32};

fn decode_error(payload: &[u8]) -> (u32, u32, String) {
    let bad_object = read_u32(payload, 0).unwrap_or(0);
    let code = read_u32(payload, 4).unwrap_or(0);
    let msg_str = read_str(payload, 8).map(|(s, _)| s).unwrap_or_default();
    (bad_object, code, msg_str)
}

enum WaitResult {
    Matched,
    Error(String),
    Closed,
    TimedOut,
}

/// Waits for a specific (sender, opcode) message, same semantics as
/// `probe_reconnect_resize_with_surface`'s copy of this -- kept
/// per-probe rather than shared, matching this project's existing
/// examples/ convention (each probe is self-contained).
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

/// Waits for TWO specific (sender, opcode) messages to both arrive within
/// one overall deadline -- used for the post-recovery frame()+sync() pair,
/// where either one alone arriving isn't proof the other will too (a real
/// client waits on both independently).
fn read_until_both(
    stream: &mut UnixStream,
    buf: &mut Vec<u8>,
    first: (u32, u16),
    second: (u32, u16),
    deadline: Instant,
) -> WaitResult {
    let mut tmp = [0u8; 16 * 1024];
    let (mut first_seen, mut second_seen) = (false, false);
    loop {
        while let Some((msg, consumed)) = wire::take_message(buf) {
            let header = wire::MessageHeader::parse(msg).unwrap();
            let payload = &msg[wire::HEADER_LEN..];
            if header.sender_id == 1 && header.opcode == 0 {
                let (bad_object, code, msg_str) = decode_error(payload);
                buf.drain(..consumed);
                return WaitResult::Error(format!("wl_display.error: object={bad_object} code={code} message={msg_str:?}"));
            }
            if header.sender_id == first.0 && header.opcode == first.1 {
                first_seen = true;
            }
            if header.sender_id == second.0 && header.opcode == second.1 {
                second_seen = true;
            }
            buf.drain(..consumed);
            if first_seen && second_seen {
                return WaitResult::Matched;
            }
        }
        if Instant::now() >= deadline {
            return WaitResult::TimedOut;
        }
        match stream.read(&mut tmp) {
            Ok(0) => return WaitResult::Closed,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                if Instant::now() >= deadline {
                    return WaitResult::TimedOut;
                }
            }
            Err(e) => return WaitResult::Error(format!("read error: {e}")),
        }
    }
}

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
    let mut linux_dmabuf = None;
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
                                    "zwp_linux_dmabuf_v1" => linux_dmabuf = Some((name, version)),
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
    let (dmabuf_name, dmabuf_version) = linux_dmabuf.expect("proxy never advertised zwp_linux_dmabuf_v1");
    println!("found wl_compositor={wl_compositor_name} xdg_wm_base={xdg_wm_base_name} zwp_linux_dmabuf_v1={dmabuf_name}");

    // bind wl_compositor->4, xdg_wm_base->5, zwp_linux_dmabuf_v1->6;
    // create_surface->7, get_xdg_surface->8, get_toplevel->9.
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
    put_u32(&mut p, dmabuf_name);
    put_str(&mut p, "zwp_linux_dmabuf_v1");
    put_u32(&mut p, dmabuf_version);
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

    println!("allocating a real dmabuf via GBM (/dev/dri/renderD128)...");
    let width = 320u32;
    let height = 240u32;
    let render_node = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/dri/renderD128")
        .expect("opening /dev/dri/renderD128 -- needs real GPU access, run this on the live session, not a headless container");
    let gbm_device = Device::new(render_node).expect("creating GBM device");
    let bo = gbm_device
        .create_buffer_object_with_modifiers::<()>(width, height, Format::Argb8888, [Modifier::Linear].into_iter())
        .unwrap_or_else(|e| {
            // Fall back to implicit-modifier allocation -- some drivers
            // don't support the explicit-modifiers path for every format.
            eprintln!("create_buffer_object_with_modifiers failed ({e}), falling back to implicit modifiers");
            gbm_device
                .create_buffer_object::<()>(width, height, Format::Argb8888, BufferObjectFlags::RENDERING | BufferObjectFlags::LINEAR)
                .expect("allocating dmabuf buffer (fallback path)")
        });
    let stride = bo.stride();
    let modifier: u64 = bo.modifier().into();
    let dmabuf_fd = bo.fd().expect("exporting dmabuf fd");
    println!("dmabuf buffer: {width}x{height} stride={stride} modifier={modifier:#x} fd={}", dmabuf_fd.as_raw_fd());

    // zwp_linux_dmabuf_v1(6).create_params -> params(10)
    let mut p = Vec::new();
    put_u32(&mut p, 10);
    stream.write_all(&wire::build_message(6, 1, &p)).expect("sending create_params");

    // zwp_linux_buffer_params_v1(10).add(fd, plane_idx=0, offset=0, stride, modifier_hi, modifier_lo)
    let mut p = Vec::new();
    put_u32(&mut p, 0); // plane_idx
    put_u32(&mut p, 0); // offset
    put_u32(&mut p, stride);
    put_u32(&mut p, (modifier >> 32) as u32);
    put_u32(&mut p, (modifier & 0xFFFF_FFFF) as u32);
    let msg = wire::build_message(10, 1, &p);
    wayland_proxy::fdsocket::send_with_fds(raw_fd, &msg, &[dmabuf_fd.as_raw_fd()]).expect("send add(plane 0)");

    // zwp_linux_buffer_params_v1(10).create_immed(new_id=11, w, h, format, flags=0) -> wl_buffer(11)
    let mut p = Vec::new();
    put_u32(&mut p, 11);
    put_u32(&mut p, width);
    put_u32(&mut p, height);
    put_u32(&mut p, Format::Argb8888 as u32);
    put_u32(&mut p, 0); // flags
    stream.write_all(&wire::build_message(10, 3, &p)).expect("sending create_immed");

    let mut out = Vec::new();
    let mut p = Vec::new();
    put_u32(&mut p, 11); // buffer
    put_u32(&mut p, 0);
    put_u32(&mut p, 0);
    out.extend(wire::build_message(7, 1, &p)); // surface(7).attach
    let mut p = Vec::new();
    put_u32(&mut p, 0);
    put_u32(&mut p, 0);
    put_u32(&mut p, width);
    put_u32(&mut p, height);
    out.extend(wire::build_message(7, 9, &p)); // surface(7).damage_buffer
    out.extend(wire::build_message(7, 6, &[])); // surface(7).commit
    stream.write_all(&out).expect("sending attach+damage+commit");

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
    println!("dmabuf-backed surface chain confirmed live. Crashing gnome-shell (pkill -9 gnome-shell)...");

    let status = std::process::Command::new("pkill").args(["-9", "gnome-shell"]).status();
    match status {
        Ok(s) if s.success() => {}
        other => {
            eprintln!("FAIL: pkill -9 gnome-shell didn't report success: {other:?}");
            return ExitCode::FAILURE;
        }
    }

    let watchdog = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(30));
        eprintln!("FAIL: still waiting after 30s -- giving up");
        std::process::exit(1);
    });

    // Same retry-sync loop as probe_reconnect_resize_with_surface -- a
    // sync sent during the frozen window is silently dropped, not queued.
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
    println!("reconnect + recovery confirmed complete (dmabuf buffer should now be recreated on the new host connection).");

    println!("draining and acking any post-reconnect configure(s)...");
    drain_and_ack_configures(&mut stream, &mut buf, 8, Duration::from_millis(1500));
    stream.set_read_timeout(None).expect("clear read timeout");

    // THE ACTUAL TEST: re-attach the (now-recreated) dmabuf buffer, request
    // a fresh frame callback, and sync -- exactly what a real client's
    // frame clock does every repaint, and exactly what dmabuf_gl.py's own
    // frame clock was blocked waiting on live. sync_id/frame_id chosen
    // past every id used above so there's no ambiguity in read_until_both.
    println!("re-attaching recreated buffer(11), requesting frame()+sync()...");
    let frame_id = sync_id + 1;
    let final_sync_id = sync_id + 2;
    let mut out = Vec::new();
    let mut p = Vec::new();
    put_u32(&mut p, 11);
    put_u32(&mut p, 0);
    put_u32(&mut p, 0);
    out.extend(wire::build_message(7, 1, &p)); // surface(7).attach(buffer=11)
    let mut p = Vec::new();
    put_u32(&mut p, 0);
    put_u32(&mut p, 0);
    put_u32(&mut p, width);
    put_u32(&mut p, height);
    out.extend(wire::build_message(7, 9, &p)); // surface(7).damage_buffer
    let mut p = Vec::new();
    put_u32(&mut p, frame_id);
    out.extend(wire::build_message(7, 3, &p)); // surface(7).frame -> callback(frame_id)
    out.extend(wire::build_message(7, 6, &[])); // surface(7).commit
    let mut p = Vec::new();
    put_u32(&mut p, final_sync_id);
    out.extend(wire::build_message(1, 0, &p)); // wl_display.sync -> callback(final_sync_id)
    stream.write_all(&out).expect("sending post-recovery attach+frame+commit+sync");

    let deadline = Instant::now() + Duration::from_secs(10);
    let result = read_until_both(&mut stream, &mut buf, (frame_id, 0), (final_sync_id, 0), deadline);
    std::mem::forget(watchdog);
    match result {
        WaitResult::Matched => {
            println!("PASS: frame().done AND sync().done both arrived after re-attaching the recreated dmabuf buffer -- did NOT reproduce the wedge at the protocol level.");
            ExitCode::SUCCESS
        }
        WaitResult::TimedOut => {
            eprintln!(
                "FAIL: frame()/sync() did not both complete within 10s of re-attaching the recreated dmabuf buffer -- REPRODUCED at the protocol level (no GTK/Mesa/EGL involved)."
            );
            ExitCode::FAILURE
        }
        WaitResult::Error(e) => {
            eprintln!("FAIL: {e} -- REPRODUCED (as a protocol error rather than a silent hang)");
            ExitCode::FAILURE
        }
        WaitResult::Closed => {
            eprintln!("FAIL: connection closed before frame()/sync() completed -- REPRODUCED (as a disconnect rather than a silent hang)");
            ExitCode::FAILURE
        }
    }
}
