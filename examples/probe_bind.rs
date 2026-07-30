//! Diagnostic-only tool, not part of the proxy itself: connects as a
//! minimal real Wayland client, collects the compositor's advertised
//! globals, then binds to exactly ONE of them (named on the command line)
//! and checks whether that single bind succeeds or triggers a
//! `wl_display.error`.
//!
//! Built to binary-search the "invalid arguments for wl_registry#2.bind"
//! bug against real labwc (see the 2026-07-30 entries in
//! docs/debugging-notes.md): `wayland-info` binds six globals in one burst
//! and only reports that *some* bind failed, not which one. Binding a
//! single global at a time through the proxy isolates which specific
//! interface/version combination labwc actually rejects.
//!
//! Usage: WAYLAND_DISPLAY=wayland-proxy-0 cargo run --example probe_bind -- wl_shm[:version] [more...]
//! An optional `:version` suffix binds at that version instead of the
//! compositor's max advertised version -- needed to exactly replicate a
//! real client's lower-than-max bind requests.
//!
//! Multiple interface names bind all of them in a single burst write (no
//! reads in between), matching how a real client like `wayland-info` sends
//! its startup binds -- this is deliberate: binding each of a failing set's
//! members one at a time (this tool with one arg) can all individually
//! PASS while the same set bound together FAILS, which is itself a finding
//! (see docs/debugging-notes.md).

use std::env;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;

use wayland_proxy::wire::{self, put_str, put_u32, read_str, read_u32};

struct Global {
    name: u32,
    interface: String,
    version: u32,
}

fn main() -> ExitCode {
    let target_interfaces: Vec<String> = env::args().skip(1).collect();
    if target_interfaces.is_empty() {
        eprintln!("usage: probe_bind <interface-name> [more-interfaces...]");
        return ExitCode::FAILURE;
    }

    let runtime_dir = env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR must be set");
    let display = env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-proxy-0".to_string());
    let socket_path = PathBuf::from(&runtime_dir).join(&display);

    let mut stream = UnixStream::connect(&socket_path)
        .unwrap_or_else(|e| panic!("connecting to {socket_path:?}: {e}"));

    // wl_display(1).get_registry(new_id=2), then wl_display(1).sync(new_id=3)
    // immediately after -- the sync's callback.done is only sent once every
    // earlier request (including get_registry's resulting global events)
    // has been processed, so it marks "all globals have arrived" without
    // needing a fixed sleep.
    let mut out = Vec::new();
    let mut get_registry_payload = Vec::new();
    put_u32(&mut get_registry_payload, 2);
    out.extend(wire::build_message(1, 1, &get_registry_payload));
    let mut sync1_payload = Vec::new();
    put_u32(&mut sync1_payload, 3);
    out.extend(wire::build_message(1, 0, &sync1_payload));
    stream.write_all(&out).expect("sending get_registry+sync");

    let mut globals = Vec::new();
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
            match (header.sender_id, header.opcode) {
                (2, 0) => {
                    // wl_registry.global(name: uint, interface: string, version: uint)
                    if let Some(name) = read_u32(payload, 0) {
                        if let Some((interface, next)) = read_str(payload, 4) {
                            if let Some(version) = read_u32(payload, next) {
                                globals.push(Global { name, interface, version });
                            }
                        }
                    }
                }
                (1, 0) => {
                    let (bad_object, code, msg_str) = decode_error(payload);
                    eprintln!(
                        "FAIL: wl_display.error during global collection: object={bad_object} code={code} message={msg_str:?}"
                    );
                    return ExitCode::FAILURE;
                }
                (3, 0) => {
                    let consumed_len = consumed;
                    buf.drain(..consumed_len);
                    break 'collect;
                }
                _ => {}
            }
            let consumed_len = consumed;
            buf.drain(..consumed_len);
        }
    }

    // A literal "SKIP" entry burns one new_id without sending anything for
    // it -- isolates whether a bare GAP in the client's new_id sequence
    // (with nothing dropped by the proxy at all, purely the client's own
    // choice) is enough to trigger labwc's error on its own.
    let mut targets = Vec::new();
    for spec in &target_interfaces {
        if spec == "SKIP" {
            targets.push(None);
            continue;
        }
        // Optional "!opcode" suffix: immediately after this bind, send one
        // more request on the newly-bound object at that opcode, payload =
        // a single fresh new_id -- replicates a real client's interleaved
        // "bind, then immediately call a request that itself returns a
        // new object" pattern (e.g. zwp_linux_dmabuf_v1.get_default_feedback
        // right after binding dmabuf), which this tool's plain binds don't
        // otherwise exercise at all.
        let (spec, followup_opcode) = match spec.split_once('!') {
            Some((s, op)) => (s, Some(op.parse::<u16>().expect("opcode must be a number"))),
            None => (spec.as_str(), None),
        };
        let (iface, version_override) = match spec.split_once(':') {
            Some((iface, v)) => (iface, Some(v.parse::<u32>().expect("version must be a number"))),
            None => (spec, None),
        };
        let Some(target) = globals.iter().find(|g| g.interface == iface) else {
            eprintln!("FAIL: compositor never advertised interface {iface:?}");
            eprintln!("available interfaces: {:?}", globals.iter().map(|g| &g.interface).collect::<Vec<_>>());
            return ExitCode::FAILURE;
        };
        let version = version_override.unwrap_or(target.version);
        targets.push(Some((target.name, target.interface.clone(), version, followup_opcode)));
    }

    // wl_registry(2).bind(name: uint, interface: string, version: uint, id: new_id)
    // for every requested interface, all in ONE write (no reads in
    // between) -- matching how a real client sends its startup burst --
    // then wl_display(1).sync as the final message in the same write to
    // force a round-trip and surface any resulting protocol error. new_ids
    // start at 4 (1-3 are display/registry/sync1) and increment per bind,
    // same allocation order a real client's sequential id counter would use.
    let mut out2 = Vec::new();
    let mut next_id = 4u32;
    for target in &targets {
        let Some((name, interface, version, followup_opcode)) = target else {
            println!("SKIP: burning id={next_id} without sending anything for it...");
            next_id += 1;
            continue;
        };
        println!("binding {interface} (name={name}, version={version}) id={next_id}...");
        let mut bind_payload = Vec::new();
        put_u32(&mut bind_payload, *name);
        put_str(&mut bind_payload, interface);
        put_u32(&mut bind_payload, *version);
        put_u32(&mut bind_payload, next_id);
        out2.extend(wire::build_message(2, 0, &bind_payload));
        let bound_id = next_id;
        next_id += 1;

        if let Some(opcode) = followup_opcode {
            println!("  -> immediately sending {interface}#{bound_id}.<opcode {opcode}>(new_id={next_id})");
            let mut followup_payload = Vec::new();
            put_u32(&mut followup_payload, next_id);
            out2.extend(wire::build_message(bound_id, *opcode, &followup_payload));
            next_id += 1;
        }
    }
    let sync_id = next_id;
    let mut sync2_payload = Vec::new();
    put_u32(&mut sync2_payload, sync_id);
    out2.extend(wire::build_message(1, 0, &sync2_payload));
    stream.write_all(&out2).expect("sending binds+sync");

    buf.clear();
    loop {
        let n = match stream.read(&mut tmp) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("FAIL: read error after binding {target_interfaces:?}: {e}");
                return ExitCode::FAILURE;
            }
        };
        if n == 0 {
            eprintln!("FAIL: connection closed after binding {target_interfaces:?}");
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
                        "FAIL: wl_display.error after binding {target_interfaces:?}: object={bad_object} code={code} message={msg_str:?}"
                    );
                    return ExitCode::FAILURE;
                }
                (id, 0) if id == sync_id => {
                    println!("PASS: binding {target_interfaces:?} together succeeded");
                    return ExitCode::SUCCESS;
                }
                _ => {}
            }
            let consumed_len = consumed;
            buf.drain(..consumed_len);
        }
    }
}

/// Decodes a wl_display.error payload: (object_id: object, code: uint, message: string).
fn decode_error(payload: &[u8]) -> (u32, u32, String) {
    let bad_object = read_u32(payload, 0).unwrap_or(0);
    let code = read_u32(payload, 4).unwrap_or(0);
    let msg_str = read_str(payload, 8).map(|(s, _)| s).unwrap_or_default();
    (bad_object, code, msg_str)
}
