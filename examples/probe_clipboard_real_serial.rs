//! Diagnostic-only tool, not part of the proxy itself: follow-up to
//! `probe_clipboard_serial.rs`, which showed Mutter cancels a data source
//! set as selection with a FABRICATED serial from a client with no input
//! history, regardless of the number used. This tool tests the other half
//! of the question: does a REAL, compositor-issued serial work if it was
//! issued for something else (a keyboard focus/press event), not
//! specifically for this set_selection call? That's the mechanism a
//! "splice a synthetic set_selection onto a real client's own connection,
//! right when a real input serial arrives" design would depend on.
//!
//! Maps a real, visible, focusable window (so a real wl_keyboard.enter/key
//! carries a real serial), waits for the first serial-bearing input event,
//! then immediately calls set_selection with THAT serial -- borrowed for a
//! purpose other than what generated it, same as a splice design would do.
//!
//! Point WAYLAND_DISPLAY at the REAL compositor socket, not the proxy.
//!
//! Usage: WAYLAND_DISPLAY=wayland-0 cargo run --example probe_clipboard_real_serial -- [wait_secs]
//! While it's running, click on / focus the "clipboard-probe" window if it
//! doesn't get focus automatically (GNOME normally auto-focuses newly
//! mapped windows, so this may need no interaction at all).

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
const CLIPBOARD_MARKER: &[u8] = b"held by wl-resprox probe_clipboard_real_serial";

struct Global {
    name: u32,
    interface: String,
    version: u32,
}

fn pump_once(stream: &UnixStream, buf: &mut Vec<u8>, fds: &mut VecDeque<OwnedFd>) -> std::io::Result<usize> {
    let mut tmp = [0u8; 16 * 1024];
    match recv_with_fds(stream.as_raw_fd(), &mut tmp) {
        Ok((0, _)) => Ok(0),
        Ok((n, new_fds)) => {
            buf.extend_from_slice(&tmp[..n]);
            fds.extend(new_fds);
            Ok(n)
        }
        Err(nix::errno::Errno::EAGAIN) => Err(std::io::ErrorKind::WouldBlock.into()),
        Err(e) => Err(std::io::Error::from(e)),
    }
}

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

fn main() {
    let wait_secs: u64 = env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(20);

    let runtime_dir = env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR must be set");
    let display = env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".to_string());
    let socket_path = PathBuf::from(&runtime_dir).join(&display);
    println!("connecting to {socket_path:?}");

    let mut stream = UnixStream::connect(&socket_path)
        .unwrap_or_else(|e| panic!("connecting to {socket_path:?}: {e}"));
    let raw_fd = stream.as_raw_fd();

    let mut buf = Vec::new();
    let mut fds: VecDeque<OwnedFd> = VecDeque::new();

    // get_registry(2) + sync(3).
    let mut out = Vec::new();
    let mut p = Vec::new();
    put_u32(&mut p, 2);
    out.extend(wire::build_message(1, 1, &p));
    p.clear();
    put_u32(&mut p, 3);
    out.extend(wire::build_message(1, 0, &p));
    stream.write_all(&out).expect("sending get_registry+sync");

    let mut wl_compositor = None;
    let mut xdg_wm_base = None;
    let mut wl_shm = None;
    let mut wl_seat = None;
    let mut ddm = None;
    'collect: loop {
        assert_ne!(pump_once(&stream, &mut buf, &mut fds).expect("read"), 0, "closed collecting globals");
        while let Some((msg, consumed)) = wire::take_message(&buf) {
            let header = wire::MessageHeader::parse(msg).unwrap();
            let payload = &msg[wire::HEADER_LEN..];
            match (header.sender_id, header.opcode) {
                (2, 0) => {
                    if let Some(name) = read_u32(payload, 0) {
                        if let Some((iface, next)) = read_str(payload, 4) {
                            if let Some(version) = read_u32(payload, next) {
                                match iface.as_str() {
                                    "wl_compositor" => wl_compositor = Some(Global { name, interface: iface, version }),
                                    "xdg_wm_base" => xdg_wm_base = Some(Global { name, interface: iface, version }),
                                    "wl_shm" => wl_shm = Some(Global { name, interface: iface, version }),
                                    "wl_seat" => wl_seat = Some(Global { name, interface: iface, version }),
                                    "wl_data_device_manager" => ddm = Some(Global { name, interface: iface, version }),
                                    _ => {}
                                }
                            }
                        }
                    }
                }
                (1, 0) => {
                    let (o, c, m) = decode_error(payload);
                    panic!("wl_display.error collecting globals: object={o} code={c} message={m:?}");
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
    let wl_compositor = wl_compositor.expect("no wl_compositor");
    let xdg_wm_base = xdg_wm_base.expect("no xdg_wm_base");
    let wl_shm = wl_shm.expect("no wl_shm");
    let wl_seat = wl_seat.expect("no wl_seat");
    let ddm = ddm.expect("no wl_data_device_manager");

    // Bind ids: compositor=4, xdg_wm_base=5, wl_shm=6, wl_seat=7, ddm=8.
    // surface=9, xdg_surface=10, xdg_toplevel=11, pointer=12, keyboard=13,
    // data_source=14, data_device=15.
    const COMPOSITOR: u32 = 4;
    const WM_BASE: u32 = 5;
    const SHM: u32 = 6;
    const SEAT: u32 = 7;
    const DDM: u32 = 8;
    const SURFACE: u32 = 9;
    const XDG_SURFACE: u32 = 10;
    const XDG_TOPLEVEL: u32 = 11;
    const POINTER: u32 = 12;
    const KEYBOARD: u32 = 13;
    const SOURCE_ID: u32 = 14;
    const DEVICE_ID: u32 = 15;

    let mut out = Vec::new();
    for (g, id) in [(&wl_compositor, COMPOSITOR), (&xdg_wm_base, WM_BASE), (&wl_shm, SHM), (&wl_seat, SEAT), (&ddm, DDM)] {
        let mut p = Vec::new();
        put_u32(&mut p, g.name);
        put_str(&mut p, &g.interface);
        put_u32(&mut p, g.version);
        put_u32(&mut p, id);
        out.extend(wire::build_message(2, 0, &p));
    }
    let mut p = Vec::new();
    put_u32(&mut p, SURFACE);
    out.extend(wire::build_message(COMPOSITOR, 0, &p)); // create_surface
    p.clear();
    put_u32(&mut p, XDG_SURFACE);
    put_u32(&mut p, SURFACE);
    out.extend(wire::build_message(WM_BASE, 2, &p)); // get_xdg_surface
    p.clear();
    put_u32(&mut p, XDG_TOPLEVEL);
    out.extend(wire::build_message(XDG_SURFACE, 1, &p)); // get_toplevel
    p.clear();
    put_str(&mut p, "clipboard-probe");
    out.extend(wire::build_message(XDG_TOPLEVEL, 2, &p)); // set_title
    p.clear();
    put_u32(&mut p, POINTER);
    out.extend(wire::build_message(SEAT, 0, &p)); // get_pointer
    p.clear();
    put_u32(&mut p, KEYBOARD);
    out.extend(wire::build_message(SEAT, 1, &p)); // get_keyboard
    p.clear();
    put_u32(&mut p, SOURCE_ID);
    out.extend(wire::build_message(DDM, 0, &p)); // create_data_source
    p.clear();
    put_str(&mut p, MIME_TYPE);
    out.extend(wire::build_message(SOURCE_ID, 0, &p)); // offer
    p.clear();
    put_u32(&mut p, DEVICE_ID);
    put_u32(&mut p, SEAT);
    out.extend(wire::build_message(DDM, 1, &p)); // get_data_device
    out.extend(wire::build_message(SURFACE, 6, &[])); // initial commit, no buffer
    stream.write_all(&out).expect("sending setup burst");

    // Wait for the initial xdg_surface.configure, ack it, then map with a
    // real (tiny) shm buffer so the window is visible/focusable.
    let tmp_dir = env::temp_dir().join(format!("probe-clipboard-real-serial-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("create temp dir");
    stream.set_read_timeout(Some(Duration::from_millis(300))).expect("set read timeout");

    let mut configured = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !configured && Instant::now() < deadline {
        match pump_once(&stream, &mut buf, &mut fds) {
            Ok(0) => panic!("closed waiting for configure"),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => panic!("read error waiting for configure: {e}"),
        }
        while let Some((msg, consumed)) = wire::take_message(&buf) {
            let header = wire::MessageHeader::parse(msg).unwrap();
            let payload = &msg[wire::HEADER_LEN..];
            // wl_keyboard.keymap(format, fd, size) fires once,
            // automatically, right after get_keyboard -- its fd must be
            // popped off our shared queue here or a later pop_front()
            // (e.g. for wl_data_source.send's fd) gets this one instead.
            if header.sender_id == KEYBOARD && header.opcode == 0 {
                fds.pop_front();
            }
            if header.sender_id == XDG_SURFACE && header.opcode == 0 {
                if let Some(serial) = read_u32(payload, 0) {
                    println!("got initial xdg_surface.configure(serial={serial}), acking");
                    let mut p = Vec::new();
                    put_u32(&mut p, serial);
                    stream.write_all(&wire::build_message(XDG_SURFACE, 4, &p)).expect("ack_configure");
                    configured = true;
                }
            }
            let n = consumed;
            buf.drain(..n);
        }
    }
    assert!(configured, "never got an initial configure");

    const W: u32 = 200;
    const H: u32 = 150;
    let stride = W * 4;
    let size = (stride * H) as u64;
    let file = backing_file(&tmp_dir, "pool", size);
    // Fill with opaque mid-grey (ARGB8888) so the window is visibly a real
    // surface, not transparent/undrawn.
    {
        use std::os::fd::AsFd;
        let mmap = unsafe {
            memmap_grey(&file, size as usize)
        };
        let _ = mmap; // best-effort; a blank buffer still maps and focuses fine
        let _ = file.as_fd();
    }

    const POOL: u32 = 16;
    const BUFFER: u32 = 17;
    let mut p = Vec::new();
    put_u32(&mut p, POOL);
    put_u32(&mut p, size as u32);
    let msg = wire::build_message(SHM, 0, &p);
    send_with_fds(raw_fd, &msg, &[file.as_raw_fd()]).expect("create_pool");
    p.clear();
    put_u32(&mut p, BUFFER);
    put_u32(&mut p, 0);
    put_u32(&mut p, W);
    put_u32(&mut p, H);
    put_u32(&mut p, stride);
    put_u32(&mut p, 0); // ARGB8888
    stream.write_all(&wire::build_message(POOL, 0, &p)).expect("create_buffer");

    let mut out = Vec::new();
    let mut p = Vec::new();
    put_u32(&mut p, BUFFER);
    put_u32(&mut p, 0);
    put_u32(&mut p, 0);
    out.extend(wire::build_message(SURFACE, 1, &p)); // attach
    p.clear();
    put_u32(&mut p, 0);
    put_u32(&mut p, 0);
    put_u32(&mut p, W);
    put_u32(&mut p, H);
    out.extend(wire::build_message(SURFACE, 9, &p)); // damage_buffer
    out.extend(wire::build_message(SURFACE, 6, &[])); // commit
    stream.write_all(&out).expect("sending attach+damage+commit");
    println!("window mapped -- waiting up to {wait_secs}s for a real serial-bearing input event...");

    // Wait for the first serial-bearing event: wl_pointer.enter(0)/
    // button(3), or wl_keyboard.enter(1)/key(3). All start with
    // serial:uint as their first argument.
    let mut captured_serial: Option<(String, u32)> = None;
    let deadline = Instant::now() + Duration::from_secs(wait_secs);
    while captured_serial.is_none() && Instant::now() < deadline {
        match pump_once(&stream, &mut buf, &mut fds) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => {
                println!("read error while waiting for input: {e}");
                break;
            }
        }
        while let Some((msg, consumed)) = wire::take_message(&buf) {
            let header = wire::MessageHeader::parse(msg).unwrap();
            let payload = &msg[wire::HEADER_LEN..];
            if header.sender_id == KEYBOARD && header.opcode == 0 {
                fds.pop_front(); // wl_keyboard.keymap's fd -- see the earlier loop's comment
            }
            let label = match (header.sender_id, header.opcode) {
                (POINTER, 0) => Some("wl_pointer.enter"),
                (POINTER, 3) => Some("wl_pointer.button"),
                (KEYBOARD, 1) => Some("wl_keyboard.enter"),
                (KEYBOARD, 3) => Some("wl_keyboard.key"),
                (1, 0) => {
                    let (o, c, m) = decode_error(payload);
                    println!("wl_display.error while waiting: object={o} code={c} message={m:?}");
                    None
                }
                _ => None,
            };
            if let Some(label) = label {
                if let Some(serial) = read_u32(payload, 0) {
                    println!("got REAL {label}(serial={serial}) -- capturing it");
                    captured_serial = Some((label.to_string(), serial));
                }
            }
            let n = consumed;
            buf.drain(..n);
        }
    }

    let Some((label, serial)) = captured_serial else {
        println!("RESULT: INCONCLUSIVE -- no serial-bearing input event arrived in {wait_secs}s (window may not have gotten focus; try clicking the 'clipboard-probe' window manually and rerun)");
        return;
    };

    // NOW splice: use this real, but off-purpose, serial for set_selection.
    let mut p = Vec::new();
    put_u32(&mut p, SOURCE_ID);
    put_u32(&mut p, serial);
    stream.write_all(&wire::build_message(DEVICE_ID, 1, &p)).expect("set_selection");
    p.clear();
    put_u32(&mut p, 18);
    stream.write_all(&wire::build_message(1, 0, &p)).expect("sync");

    stream.set_read_timeout(Some(Duration::from_millis(30))).expect("set read timeout");
    let sync_deadline = Instant::now() + Duration::from_secs(3);
    let mut rejected = false;
    'confirm: while Instant::now() < sync_deadline {
        match pump_once(&stream, &mut buf, &mut fds) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => break,
        }
        while let Some((msg, consumed)) = wire::take_message(&buf) {
            let header = wire::MessageHeader::parse(msg).unwrap();
            let payload = &msg[wire::HEADER_LEN..];
            match (header.sender_id, header.opcode) {
                (1, 0) => {
                    let (o, c, m) = decode_error(payload);
                    println!("wl_display.error after set_selection: object={o} code={c} message={m:?}");
                    rejected = true;
                }
                (SOURCE_ID, 2) => {
                    println!("wl_data_source.cancelled fired right after set_selection -- REJECTED");
                    rejected = true;
                }
                (SOURCE_ID, 1) => answer_data_source_send(payload, &mut fds),
                (18, 0) => {
                    let n = consumed;
                    buf.drain(..n);
                    break 'confirm;
                }
                _ => {}
            }
            let n = consumed;
            buf.drain(..n);
        }
    }
    println!(
        "RESULT: set_selection using real {label}'s serial={serial} (borrowed for an unrelated purpose) -> {}",
        if rejected { "REJECTED (cancelled/error)" } else { "ACCEPTED at wire level, no cancel yet" }
    );
    println!("now check from another terminal: wl-paste --list-types ; wl-paste");

    // Stay up briefly to actually answer a real paste request if one comes.
    let hold_deadline = Instant::now() + Duration::from_secs(25);
    while Instant::now() < hold_deadline {
        match pump_once(&stream, &mut buf, &mut fds) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => break,
        }
        while let Some((msg, consumed)) = wire::take_message(&buf) {
            let header = wire::MessageHeader::parse(msg).unwrap();
            let payload = &msg[wire::HEADER_LEN..];
            match (header.sender_id, header.opcode) {
                (SOURCE_ID, 1) => answer_data_source_send(payload, &mut fds),
                (SOURCE_ID, 2) => println!("EVENT: wl_data_source.cancelled (late)"),
                _ => {}
            }
            let n = consumed;
            buf.drain(..n);
        }
    }
    println!("done");
}

/// Answers a real wl_data_source.send by writing the marker bytes into
/// its fd. Called from both the post-set_selection confirm loop and the
/// later hold loop -- send() can arrive during either (Mutter's own
/// eager-fetch fires immediately, often before the confirm loop's own
/// sync completes), and missing it in the confirm loop leaves the fd
/// unanswered, which -- when routed through wl-resprox's tee -- means its
/// own pump task waits forever for a write that never comes.
fn answer_data_source_send(payload: &[u8], fds: &mut VecDeque<OwnedFd>) {
    let (mime, _) = read_str(payload, 0).unwrap_or_default();
    let Some(fd) = fds.pop_front() else {
        println!("EVENT: wl_data_source.send(mime_type={mime:?}) -- NO FD IN QUEUE");
        return;
    };
    // Plain pipe (created by the pasting client / Mutter, not our
    // Wayland socket) -- write() directly, not sendmsg/SCM_RIGHTS.
    println!("EVENT: wl_data_source.send(mime_type={mime:?}) -- answering with marker bytes, fd={}", fd.as_raw_fd());
    let mut pipe = std::fs::File::from(fd);
    match pipe.write_all(CLIPBOARD_MARKER) {
        Ok(()) => println!("  wrote {} bytes", CLIPBOARD_MARKER.len()),
        Err(e) => println!("  write FAILED: {e}"),
    }
}

/// Best-effort opaque fill so the mapped window isn't blank/transparent;
/// not load-bearing for the test itself (focus doesn't depend on pixel
/// content), just makes it easier for a human to spot and click if needed.
unsafe fn memmap_grey(file: &std::fs::File, size: usize) -> Option<()> {
    use std::io::Seek;
    let mut f = file.try_clone().ok()?;
    f.seek(std::io::SeekFrom::Start(0)).ok()?;
    let grey = [0x80u8, 0x80, 0x80, 0xff].repeat(size / 4);
    std::io::Write::write_all(&mut f, &grey).ok()?;
    Some(())
}
