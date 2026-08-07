//! Diagnostic-only tool, not part of the proxy itself: connects as a
//! minimal real Wayland client with NO real input history (never binds
//! wl_pointer/wl_keyboard, never receives a real input serial), then calls
//! wl_data_device.set_selection with a fabricated serial anyway.
//!
//! Answers one question before building anything: can a passive client
//! (e.g. a proxy-internal "clipboard holder" with no user ever clicking on
//! it) become the compositor's selection owner at all? If Mutter rejects
//! or silently ignores a synthetic serial, a persistent-holder clipboard
//! design needs a different source for the serial; if it's accepted AND a
//! real `wl-paste` can read our offered data back, the naive approach
//! works.
//!
//! Point WAYLAND_DISPLAY at the REAL compositor socket, not the proxy --
//! this probes Mutter's own behavior, independent of anything wl-resprox
//! does or doesn't relay correctly.
//!
//! Usage: WAYLAND_DISPLAY=wayland-0 cargo run --example probe_clipboard_serial -- [serial] [wait_secs]
//! While it's running (default 20s), from another terminal in the same
//! session: `wl-paste --list-types` (is our offer visible at all?) and
//! `wl-paste` (does it get real bytes back?).

use std::collections::VecDeque;
use std::env;
use std::io::Write as _;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use wayland_proxy::fdsocket::{recv_with_fds, send_with_fds};
use wayland_proxy::wire::{self, put_str, put_u32, read_str, read_u32};

const MIME_TYPE: &str = "text/plain";
const CLIPBOARD_MARKER: &[u8] = b"held by wl-resprox probe_clipboard_serial";

struct Global {
    name: u32,
    interface: String,
    version: u32,
}

/// Blocking read of one `recvmsg` call's worth of bytes+fds into `buf`/`fds`.
fn pump_once(stream: &UnixStream, buf: &mut Vec<u8>, fds: &mut VecDeque<OwnedFd>) -> bool {
    let mut tmp = [0u8; 16 * 1024];
    match recv_with_fds(stream.as_raw_fd(), &mut tmp) {
        Ok((0, _)) => false,
        Ok((n, new_fds)) => {
            buf.extend_from_slice(&tmp[..n]);
            fds.extend(new_fds);
            true
        }
        Err(e) => {
            eprintln!("read error: {e}");
            false
        }
    }
}

fn decode_error(payload: &[u8]) -> (u32, u32, String) {
    let bad_object = read_u32(payload, 0).unwrap_or(0);
    let code = read_u32(payload, 4).unwrap_or(0);
    let msg_str = read_str(payload, 8).map(|(s, _)| s).unwrap_or_default();
    (bad_object, code, msg_str)
}

fn main() {
    let mut args = env::args().skip(1);
    let serial: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let wait_secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(20);

    let runtime_dir = env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR must be set");
    let display = env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".to_string());
    let socket_path = PathBuf::from(&runtime_dir).join(&display);
    println!("connecting to {socket_path:?}, will call set_selection with serial={serial}");

    let mut stream = UnixStream::connect(&socket_path)
        .unwrap_or_else(|e| panic!("connecting to {socket_path:?}: {e}"));

    let mut buf = Vec::new();
    let mut fds: VecDeque<OwnedFd> = VecDeque::new();

    // wl_display(1).get_registry(2), then sync(3) to mark "globals done".
    let mut out = Vec::new();
    let mut p = Vec::new();
    put_u32(&mut p, 2);
    out.extend(wire::build_message(1, 1, &p));
    p.clear();
    put_u32(&mut p, 3);
    out.extend(wire::build_message(1, 0, &p));
    stream.write_all(&out).expect("sending get_registry+sync");

    let mut globals = Vec::new();
    'collect: loop {
        if !pump_once(&stream, &mut buf, &mut fds) {
            panic!("connection closed while collecting globals");
        }
        while let Some((msg, consumed)) = wire::take_message(&buf) {
            let header = wire::MessageHeader::parse(msg).unwrap();
            let payload = &msg[wire::HEADER_LEN..];
            match (header.sender_id, header.opcode) {
                (2, 0) => {
                    if let Some(name) = read_u32(payload, 0) {
                        if let Some((interface, next)) = read_str(payload, 4) {
                            if let Some(version) = read_u32(payload, next) {
                                globals.push(Global { name, interface, version });
                            }
                        }
                    }
                }
                (1, 0) => {
                    let (obj, code, m) = decode_error(payload);
                    panic!("wl_display.error during global collection: object={obj} code={code} message={m:?}");
                }
                (3, 0) => {
                    let n = consumed;
                    buf.drain(..n);
                    break 'collect;
                }
                _ => {}
            }
            let n = consumed;
            buf.drain(..n);
        }
    }

    let seat = globals.iter().find(|g| g.interface == "wl_seat").expect("compositor has no wl_seat");
    let ddm = globals
        .iter()
        .find(|g| g.interface == "wl_data_device_manager")
        .expect("compositor has no wl_data_device_manager");
    println!(
        "found wl_seat(name={}, v{}), wl_data_device_manager(name={}, v{})",
        seat.name, seat.version, ddm.name, ddm.version
    );

    // Bind both (ids 4, 5), then sync(6) to confirm no error binding them.
    const SEAT_ID: u32 = 4;
    const DDM_ID: u32 = 5;
    let mut out = Vec::new();
    let mut p = Vec::new();
    put_u32(&mut p, seat.name);
    put_str(&mut p, "wl_seat");
    put_u32(&mut p, seat.version);
    put_u32(&mut p, SEAT_ID);
    out.extend(wire::build_message(2, 0, &p));
    p.clear();
    put_u32(&mut p, ddm.name);
    put_str(&mut p, "wl_data_device_manager");
    put_u32(&mut p, ddm.version);
    put_u32(&mut p, DDM_ID);
    out.extend(wire::build_message(2, 0, &p));
    p.clear();
    put_u32(&mut p, 6);
    out.extend(wire::build_message(1, 0, &p));
    stream.write_all(&out).expect("sending binds+sync");

    'bindsync: loop {
        if !pump_once(&stream, &mut buf, &mut fds) {
            panic!("connection closed while binding");
        }
        while let Some((msg, consumed)) = wire::take_message(&buf) {
            let header = wire::MessageHeader::parse(msg).unwrap();
            let payload = &msg[wire::HEADER_LEN..];
            match (header.sender_id, header.opcode) {
                (1, 0) => {
                    let (obj, code, m) = decode_error(payload);
                    panic!("wl_display.error binding seat/data_device_manager: object={obj} code={code} message={m:?}");
                }
                (6, 0) => {
                    let n = consumed;
                    buf.drain(..n);
                    break 'bindsync;
                }
                _ => {}
            }
            let n = consumed;
            buf.drain(..n);
        }
    }
    println!("bound wl_seat({SEAT_ID}) and wl_data_device_manager({DDM_ID}) cleanly");

    // create_data_source(7), offer("text/plain"), get_data_device(8, seat),
    // set_selection(source=7, serial), sync(9) -- all in one write, no real
    // input event was ever processed on this connection before this point.
    const SOURCE_ID: u32 = 7;
    const DEVICE_ID: u32 = 8;
    let mut out = Vec::new();
    let mut p = Vec::new();
    put_u32(&mut p, SOURCE_ID);
    out.extend(wire::build_message(DDM_ID, 0, &p)); // create_data_source
    p.clear();
    put_str(&mut p, MIME_TYPE);
    out.extend(wire::build_message(SOURCE_ID, 0, &p)); // wl_data_source.offer
    p.clear();
    put_u32(&mut p, DEVICE_ID);
    put_u32(&mut p, SEAT_ID);
    out.extend(wire::build_message(DDM_ID, 1, &p)); // get_data_device
    p.clear();
    put_u32(&mut p, SOURCE_ID);
    put_u32(&mut p, serial);
    out.extend(wire::build_message(DEVICE_ID, 1, &p)); // wl_data_device.set_selection
    p.clear();
    put_u32(&mut p, 9);
    out.extend(wire::build_message(1, 0, &p)); // sync
    stream.write_all(&out).expect("sending create_data_source+offer+get_data_device+set_selection+sync");

    'setsel: loop {
        if !pump_once(&stream, &mut buf, &mut fds) {
            panic!("connection closed after set_selection -- treat as REJECTED");
        }
        while let Some((msg, consumed)) = wire::take_message(&buf) {
            let header = wire::MessageHeader::parse(msg).unwrap();
            let payload = &msg[wire::HEADER_LEN..];
            match (header.sender_id, header.opcode) {
                (1, 0) => {
                    let (obj, code, m) = decode_error(payload);
                    println!(
                        "RESULT: set_selection(serial={serial}) REJECTED -- wl_display.error object={obj} code={code} message={m:?}"
                    );
                    return;
                }
                (9, 0) => {
                    let n = consumed;
                    buf.drain(..n);
                    break 'setsel;
                }
                (SOURCE_ID, opcode) => println!("  (event) wl_data_source#{SOURCE_ID} opcode={opcode} during initial sync"),
                _ => {}
            }
            let n = consumed;
            buf.drain(..n);
        }
    }
    println!(
        "RESULT: set_selection(serial={serial}) did not trigger a protocol error -- \
         now waiting {wait_secs}s for a real paste attempt (try `wl-paste --list-types` \
         and `wl-paste` from another terminal in this session)"
    );

    // Answer loop: respond to any wl_data_source.send by writing the
    // marker and closing the fd -- proves round-trip data delivery, not
    // just protocol-level acceptance.
    let deadline = Instant::now() + Duration::from_secs(wait_secs);
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        stream
            .set_read_timeout(Some(remaining.min(Duration::from_millis(500))))
            .expect("set_read_timeout");
        let mut tmp = [0u8; 16 * 1024];
        match recv_with_fds(stream.as_raw_fd(), &mut tmp) {
            Ok((0, _)) => {
                println!("connection closed during wait");
                break;
            }
            Ok((n, new_fds)) => {
                buf.extend_from_slice(&tmp[..n]);
                fds.extend(new_fds);
            }
            Err(e) if e == nix::errno::Errno::EAGAIN || e == nix::errno::Errno::EWOULDBLOCK => continue,
            Err(e) => {
                eprintln!("read error while waiting: {e}");
                break;
            }
        }
        while let Some((msg, consumed)) = wire::take_message(&buf) {
            let header = wire::MessageHeader::parse(msg).unwrap();
            let payload = &msg[wire::HEADER_LEN..];
            match (header.sender_id, header.opcode) {
                (1, 0) => {
                    let (obj, code, m) = decode_error(payload);
                    println!("wl_display.error while waiting: object={obj} code={code} message={m:?}");
                }
                (SOURCE_ID, 1) => {
                    // wl_data_source.send(mime_type, fd)
                    let (mime, _next) = read_str(payload, 0).unwrap_or_default();
                    match fds.pop_front() {
                        Some(fd) => {
                            println!("EVENT: wl_data_source.send(mime_type={mime:?}) -- writing marker bytes");
                            let raw = fd.as_raw_fd();
                            let _ = send_with_fds(raw, CLIPBOARD_MARKER, &[]);
                            drop(fd); // closes our end once write completes
                        }
                        None => println!("EVENT: wl_data_source.send(mime_type={mime:?}) -- no fd arrived!"),
                    }
                }
                (SOURCE_ID, opcode) => println!("EVENT: wl_data_source#{SOURCE_ID} opcode={opcode}"),
                (DEVICE_ID, opcode) => println!("EVENT: wl_data_device#{DEVICE_ID} opcode={opcode}"),
                (sender, opcode) => println!("EVENT: object#{sender} opcode={opcode} (len={consumed})"),
            }
            let n = consumed;
            buf.drain(..n);
        }
    }
    println!("done -- check above for whether a real wl-paste actually received the marker bytes");
}
