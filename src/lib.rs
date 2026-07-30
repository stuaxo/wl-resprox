//! Core relay logic for the crash-resilient Wayland proxy. Split out from
//! `main.rs` so integration tests (see `tests/`) can drive `run_connection`
//! directly against a controlled fake compositor and a real Wayland client,
//! without needing a container, GPU, or real compositor -- see the
//! 2026-07-30 entries in docs/debugging-notes.md for why: a real compositor
//! was intermittently rejecting our output with protocol errors, and manual
//! byte-level inspection of individual messages kept coming back clean, so
//! a deterministic, known-good reproduction was needed instead of more
//! manual live testing.

use anyhow::Result;
use std::collections::{HashMap, VecDeque};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use tokio::net::UnixStream;
use tracing::{error, info, warn};

use wayland_backend::protocol::{ArgumentType, Interface, MessageDesc};

pub mod fdsocket;
pub mod interfaces;
pub mod recorder;
pub mod wire;

use recorder::recorder;

use interfaces::lookup_interface;

/// Which direction a message is travelling. Determines whether we look its
/// opcode up in `Interface::requests` (client -> host) or `Interface::events`
/// (host -> client) -- Wayland reuses request/event tables independently
/// per interface, so the same opcode number means different things in each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ClientToHost,
    HostToClient,
}

/// A byte-and-fd-aware Unix socket connection: buffers whatever bytes and
/// file descriptors have arrived but not yet been consumed into a complete
/// message. See src/fdsocket.rs for why fds need explicit handling at all.
struct Conn {
    stream: UnixStream,
    read_buf: Vec<u8>,
    read_fds: VecDeque<OwnedFd>,
}

impl Conn {
    fn new(stream: UnixStream) -> Self {
        Self { stream, read_buf: Vec::new(), read_fds: VecDeque::new() }
    }

    /// Reads more data (and any accompanying fds) into our buffers,
    /// blocking asynchronously until at least one byte is available.
    /// Returns `Ok(0)` on EOF, matching `read()`'s convention.
    ///
    /// Uses `try_io`, not a bare `readable().await` followed by our own
    /// raw `recvmsg` call -- that combination looks right but isn't:
    /// `readable()` alone doesn't clear tokio's internal readiness bit,
    /// only tokio's own `try_read`/`try_io` do that (by construction, when
    /// the closure reports `WouldBlock`). Without it, a `WouldBlock` from
    /// our own raw syscall left tokio thinking the socket was still ready,
    /// so `readable().await` kept resolving immediately -- a silent 100%-CPU
    /// busy loop that never actually noticed EOF. Caught this empirically:
    /// killing the compositor never logged the freeze message, and the
    /// process's utime was climbing on an otherwise-idle connection.
    async fn fill(&mut self) -> std::io::Result<usize> {
        let raw_fd = self.stream.as_raw_fd();
        loop {
            self.stream.readable().await?;
            let mut tmp = [0u8; 16 * 1024];
            let result = self.stream.try_io(tokio::io::Interest::READABLE, || {
                fdsocket::recv_with_fds(raw_fd, &mut tmp).map_err(std::io::Error::from)
            });
            match result {
                Ok((0, _fds)) => return Ok(0),
                Ok((n, fds)) => {
                    self.read_buf.extend_from_slice(&tmp[..n]);
                    self.read_fds.extend(fds);
                    return Ok(n);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Writes a single message's bytes plus its fds, retrying on
    /// EWOULDBLOCK. Tried batching every ready message into one buffer and
    /// issuing a single `write_message` call per `relay_ready_messages`
    /// pass, to match how a real libwayland client's `wl_display_flush()`
    /// coalesces writes -- verified byte-correct via `strace`, but it made
    /// the intermittent real-compositor rejection (see below) 100%
    /// deterministic instead of fixing it, so it was reverted; messages are
    /// sent one `write_message` call each (see the 2026-07-30 entries in
    /// docs/debugging-notes.md). Attaches fds only to the first `sendmsg`
    /// call: a `SOCK_STREAM` unix socket can do a *partial* write under
    /// backpressure (a real, not
    /// theoretical, concern here -- e.g. GTK's startup burst of 20+
    /// back-to-back `wl_registry.bind` calls can fill the socket buffer).
    /// Silently treating a partial write as complete truncates the buffer
    /// and desyncs the receiver's parser for everything after it on the
    /// connection -- confirmed empirically: this exact bug produced
    /// "invalid arguments for wl_registry#2.bind" (and, in a different
    /// run, "for wl_display#1.sync") from a real compositor, which then
    /// reset the connection. Uses `try_io` for the same reason `fill`
    /// does -- see its comment.
    async fn write_message(&mut self, msg: &[u8], fds: &[RawFd]) -> std::io::Result<()> {
        let raw_fd = self.stream.as_raw_fd();
        let mut sent = 0;
        while sent < msg.len() {
            self.stream.writable().await?;
            // fds ride with the first byte of a message; once any bytes of
            // this message have gone out, later partial-write continuations
            // must not attach them again.
            let fds_for_this_call: &[RawFd] = if sent == 0 { fds } else { &[] };
            let result = self.stream.try_io(tokio::io::Interest::WRITABLE, || {
                fdsocket::send_with_fds(raw_fd, &msg[sent..], fds_for_this_call)
                    .map_err(std::io::Error::from)
            });
            match result {
                Ok(n) => sent += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// Result of walking a message's payload against its `MessageDesc`
/// signature: everything we need to know to track new objects and
/// correctly attribute file descriptors, without fully decoding every
/// argument into a typed structure.
struct SignatureWalk {
    /// Byte offset (into the payload, after the 8-byte header) of the
    /// message's `NewId` argument, if it has one.
    new_id_offset: Option<usize>,
    /// How many `Fd`-typed arguments this signature declares. Fds ride as
    /// ancillary data alongside the bytes, contributing nothing to the
    /// payload itself -- this is the only way to know how many to pull off
    /// the connection's fd queue for this specific message.
    fd_count: usize,
    /// wl_registry.bind's shape is `[Uint, Str, Uint, NewId]` with no
    /// static `child_interface` -- the new object's interface is a runtime
    /// string sitting in the message itself. Captured here when present,
    /// for `resolve_child_interface` to resolve.
    dynamic_interface_name: Option<String>,
}

fn walk_signature(payload: &[u8], signature: &[ArgumentType]) -> Result<SignatureWalk, &'static str> {
    let mut offset = 0usize;
    let mut new_id_offset = None;
    let mut fd_count = 0;
    let mut dynamic_interface_name = None;

    for arg_type in signature {
        match arg_type {
            ArgumentType::Int | ArgumentType::Uint | ArgumentType::Fixed | ArgumentType::Object(_) => {
                let end = offset.checked_add(4).ok_or("offset overflow")?;
                if payload.len() < end {
                    return Err("payload shorter than its own signature declares");
                }
                offset = end;
            }
            ArgumentType::NewId => {
                let end = offset.checked_add(4).ok_or("offset overflow")?;
                if payload.len() < end {
                    return Err("payload shorter than its own signature declares");
                }
                new_id_offset = Some(offset);
                offset = end;
            }
            ArgumentType::Str(_) | ArgumentType::Array => {
                let len_end = offset.checked_add(4).ok_or("offset overflow")?;
                if payload.len() < len_end {
                    return Err("payload shorter than its own signature declares");
                }
                let len = u32::from_ne_bytes(payload[offset..len_end].try_into().unwrap()) as usize;
                let padded = len.next_multiple_of(4);
                let data_end = len_end.checked_add(padded).ok_or("offset overflow")?;
                if payload.len() < data_end {
                    return Err("payload shorter than its own signature declares");
                }
                // Str's length prefix includes a NUL terminator (unlike
                // Array); null strings are encoded as a zero length with no
                // bytes at all, matching every other Wayland implementation.
                if matches!(arg_type, ArgumentType::Str(_)) && dynamic_interface_name.is_none() && len > 0 {
                    let content = &payload[len_end..len_end + len - 1];
                    dynamic_interface_name = std::str::from_utf8(content).ok().map(str::to_owned);
                }
                offset = data_end;
            }
            ArgumentType::Fd => {
                fd_count += 1;
            }
        }
    }
    Ok(SignatureWalk { new_id_offset, fd_count, dynamic_interface_name })
}

/// Resolves the interface (and version) of a newly-created object. Most
/// messages declare their child interface statically (`desc.child_interface`).
/// `wl_registry.bind` is the well-known exception: the interface is a
/// runtime string next to the new_id, not something the protocol schema
/// can pin down ahead of time -- that's what `dynamic_interface_name` is for.
fn resolve_child_interface(
    desc: &MessageDesc,
    dynamic_interface_name: Option<&str>,
) -> Option<(&'static Interface, u32)> {
    if let Some(iface) = desc.child_interface {
        return Some((iface, iface.version));
    }
    let iface = lookup_interface(dynamic_interface_name?)?;
    Some((iface, iface.version))
}

/// Per-connection object table: which interface each live object id
/// belongs to. Plain `u32` on both sides -- no ID *translation* happens
/// yet (that's Phase 4's actual Shadow Table, a `bimap`); this milestone is
/// "simple ID reflection" (client id == host id always), so one shared map
/// covers both directions.
type ObjectTable = HashMap<u32, &'static Interface>;

/// Relays every complete message currently buffered in `src` onward to
/// `dst` (or drops it silently if `dst` is `None`, i.e. we're frozen -- see
/// `run_connection`), tracking newly-created objects in `objects` as it
/// goes. One `write_message` call per message (not batched -- see
/// `Conn::write_message`'s doc comment for why that was tried and reverted).
async fn relay_ready_messages(
    src: &mut Conn,
    mut dst: Option<&mut Conn>,
    objects: &mut ObjectTable,
    direction: Direction,
) -> Result<()> {
    loop {
        let Some((_msg, consumed)) = wire::take_message(&src.read_buf) else {
            break;
        };
        let msg = src.read_buf[..consumed].to_vec();
        let header = wire::MessageHeader::parse(&msg).expect("take_message already validated this");

        let interface = objects.get(&header.sender_id).copied();
        let desc = interface.and_then(|iface| {
            let table = match direction {
                Direction::ClientToHost => iface.requests,
                Direction::HostToClient => iface.events,
            };
            table.get(header.opcode as usize)
        });

        let (Some(interface), Some(desc)) = (interface, desc) else {
            // Unknown object or opcode: we don't have a signature, so we
            // genuinely cannot tell whether this message carries any fds
            // (Fd-typed args are invisible in the byte stream itself --
            // that's the whole reason we need signatures at all). Forwarding
            // it blindly risks desyncing fd attribution for every message
            // after it. Dropping it is the same limitation the old
            // wayland-backend version had (globals we don't recognize are
            // simply not usable), just now enforced per-message instead of
            // by never advertising the global in the first place.
            warn!(
                "unknown object {} opcode {} ({:?}) -- dropping",
                header.sender_id, header.opcode, direction
            );
            if let Some(rec) = recorder() {
                rec.record(
                    &format!("{direction:?}"),
                    "DROPPED_UNKNOWN",
                    "?",
                    "?",
                    header.sender_id,
                    header.opcode,
                    0,
                    &src.read_buf[..consumed],
                );
            }
            src.read_buf.drain(..consumed);
            continue;
        };

        match walk_signature(&msg[wire::HEADER_LEN..], desc.signature) {
            Ok(walk) => {
                if let Some(offset) = walk.new_id_offset {
                    let abs = wire::HEADER_LEN + offset;
                    let new_id = u32::from_ne_bytes(msg[abs..abs + 4].try_into().unwrap());
                    match resolve_child_interface(desc, walk.dynamic_interface_name.as_deref()) {
                        Some((child_iface, _version)) => {
                            objects.insert(new_id, child_iface);
                        }
                        None => {
                            warn!(
                                "{}.{} created object {} with unresolvable interface -- not tracking it",
                                interface.name, desc.name, new_id
                            );
                        }
                    }
                }

                if interface.name == "wl_display" && desc.name == "error" {
                    let p = &msg[wire::HEADER_LEN..];
                    if p.len() >= 12 {
                        let bad_object = u32::from_ne_bytes(p[0..4].try_into().unwrap());
                        let code = u32::from_ne_bytes(p[4..8].try_into().unwrap());
                        let strlen = u32::from_ne_bytes(p[8..12].try_into().unwrap()) as usize;
                        let msg_str = if strlen > 0 && p.len() >= 12 + strlen - 1 {
                            std::str::from_utf8(&p[12..12 + strlen - 1]).unwrap_or("<invalid utf8>")
                        } else {
                            "<empty>"
                        };
                        error!("COMPOSITOR ERROR: object={bad_object} code={code} message={msg_str:?}");
                    }
                }

                // wl_display.delete_id tells us an id is now free; drop our
                // own tracking of it so a future reused id gets re-resolved
                // fresh rather than keeping stale interface data around.
                if interface.name == "wl_display" && desc.name == "delete_id" {
                    if let Some(id_bytes) = msg.get(wire::HEADER_LEN..wire::HEADER_LEN + 4) {
                        let deleted = u32::from_ne_bytes(id_bytes.try_into().unwrap());
                        objects.remove(&deleted);
                    }
                }

                let mut fds = Vec::with_capacity(walk.fd_count);
                for _ in 0..walk.fd_count {
                    match src.read_fds.pop_front() {
                        Some(fd) => fds.push(fd),
                        None => warn!(
                            "{}.{} declares a fd argument that never arrived",
                            interface.name, desc.name
                        ),
                    }
                }

                if let Some(dst) = dst.as_deref_mut() {
                    let raw_fds: Vec<RawFd> = fds.iter().map(|fd| fd.as_raw_fd()).collect();
                    tracing::debug!(
                        "relay {:?} {}.{} sender={} opcode={} len={} fds={} bytes={:02x?}",
                        direction, interface.name, desc.name, header.sender_id, header.opcode, consumed, raw_fds.len(), msg
                    );
                    if let Some(rec) = recorder() {
                        rec.record(
                            &format!("{direction:?}"), "RELAYED", interface.name, desc.name,
                            header.sender_id, header.opcode, raw_fds.len(), &msg,
                        );
                    }
                    // Per-message write, NOT batched: batching everything
                    // into one combined write was tried and made the real
                    // compositor's intermittent rejection fully
                    // deterministic (worse), so per-message sends are
                    // deliberately kept -- see the 2026-07-30 entries in
                    // docs/debugging-notes.md for the full experiment and
                    // why the "batch like a real client's flush()" theory
                    // didn't hold up.
                    if let Err(e) = dst.write_message(&msg, &raw_fds).await {
                        error!("failed to relay {}.{}: {e}", interface.name, desc.name);
                    }
                    // fds are dup'd onto the wire by sendmsg; our copies
                    // are closed when `fds` drops at the end of this scope.
                } else if let Some(rec) = recorder() {
                    // Frozen: would have relayed this, but the compositor
                    // connection is dead, so it's silently dropped instead
                    // (see run_connection). Recorded distinctly from
                    // DROPPED_UNKNOWN since we DO understand this message.
                    // `fds` (OwnedFd) closes when it drops at the end of
                    // this scope, un-forwarded -- correct, not a leak.
                    rec.record(
                        &format!("{direction:?}"), "DROPPED_FROZEN", interface.name, desc.name,
                        header.sender_id, header.opcode, walk.fd_count, &msg,
                    );
                }
            }
            Err(e) => {
                // We know the interface/opcode but couldn't walk its own
                // declared signature -- a real bug (either in our signature
                // data or in the sender), not a normal "unknown protocol"
                // case. Drop rather than forward something we don't
                // understand the shape of.
                warn!("failed to parse {}.{}: {e} -- dropping", interface.name, desc.name);
                if let Some(rec) = recorder() {
                    rec.record(
                        &format!("{direction:?}"), "DROPPED_PARSE_ERROR", interface.name, desc.name,
                        header.sender_id, header.opcode, 0, &msg,
                    );
                }
            }
        }

        src.read_buf.drain(..consumed);
    }

    Ok(())
}

/// Drives one proxied connection to completion: relays messages in both
/// directions until the GTK client disconnects. If the compositor
/// connection drops instead, the connection freezes: the GTK-facing side
/// stays open and its requests are silently dropped (see
/// docs/implementation-constraints.md's "on server disconnect" rules)
/// rather than the whole session tearing down. There's no reconnect logic
/// yet (Phase 5 items 3-6), so a frozen connection stays frozen for good.
pub async fn run_connection(gtk_stream: UnixStream, compositor_stream: UnixStream) -> Result<()> {
    let mut gtk = Conn::new(gtk_stream);
    let mut host = Conn::new(compositor_stream);

    // Object 1 is always wl_display, by protocol convention -- no
    // discovery needed. Both sides agree because IDs are reflected 1:1 in
    // this milestone (Phase 4's real Shadow Table replaces this).
    let mut objects: ObjectTable = HashMap::new();
    objects.insert(1, &wayland_client::protocol::__interfaces::WL_DISPLAY_INTERFACE);

    let mut frozen = false;

    info!("Proxy session established: relaying GTK client <-> compositor");

    loop {
        tokio::select! {
            res = gtk.fill() => {
                match res {
                    Ok(0) => {
                        info!("GTK client disconnected");
                        return Ok(());
                    }
                    Ok(_) => {
                        let dst = if frozen { None } else { Some(&mut host) };
                        relay_ready_messages(&mut gtk, dst, &mut objects, Direction::ClientToHost).await?;
                    }
                    Err(e) => {
                        info!("GTK client disconnected: {e}");
                        return Ok(());
                    }
                }
            }
            res = host.fill(), if !frozen => {
                match res {
                    Ok(0) => {
                        info!("compositor connection lost (EOF) -- freezing, GTK client stays connected");
                        frozen = true;
                    }
                    Ok(_) => {
                        relay_ready_messages(&mut host, Some(&mut gtk), &mut objects, Direction::HostToClient).await?;
                    }
                    Err(e) => {
                        info!("compositor connection lost ({e}) -- freezing, GTK client stays connected");
                        frozen = true;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wayland_backend::protocol::AllowNull;

    static FAKE_CHILD_INTERFACE: Interface =
        Interface { name: "fake_child", version: 3, requests: &[], events: &[], c_ptr: None };

    fn push_u32(buf: &mut Vec<u8>, v: u32) {
        buf.extend_from_slice(&v.to_ne_bytes());
    }

    fn push_str(buf: &mut Vec<u8>, s: &str) {
        let len = s.len() as u32 + 1; // includes NUL terminator
        push_u32(buf, len);
        buf.extend_from_slice(s.as_bytes());
        buf.push(0);
        while buf.len() % 4 != 0 {
            buf.push(0);
        }
    }

    #[test]
    fn walk_signature_locates_new_id_and_counts_fds() {
        // get_registry-like: [NewId]
        let mut payload = Vec::new();
        push_u32(&mut payload, 42);
        let walk = walk_signature(&payload, &[ArgumentType::NewId]).unwrap();
        assert_eq!(walk.new_id_offset, Some(0));
        assert_eq!(walk.fd_count, 0);
    }

    #[test]
    fn walk_signature_finds_bind_style_dynamic_interface() {
        // bind: [Uint(name), Str(interface), Uint(version), NewId(id)]
        let mut payload = Vec::new();
        push_u32(&mut payload, 5); // name
        push_str(&mut payload, "wl_compositor");
        push_u32(&mut payload, 4); // version
        let new_id_offset = payload.len();
        push_u32(&mut payload, 0); // new_id placeholder

        let sig = [
            ArgumentType::Uint,
            ArgumentType::Str(AllowNull::No),
            ArgumentType::Uint,
            ArgumentType::NewId,
        ];
        let walk = walk_signature(&payload, &sig).unwrap();
        assert_eq!(walk.new_id_offset, Some(new_id_offset));
        assert_eq!(walk.dynamic_interface_name.as_deref(), Some("wl_compositor"));
        assert_eq!(walk.fd_count, 0);
    }

    #[test]
    fn walk_signature_counts_fd_args_without_advancing_offset() {
        // e.g. wl_shm.create_pool-shaped: [NewId, Fd, Int]
        let mut payload = Vec::new();
        push_u32(&mut payload, 0); // new_id
        push_u32(&mut payload, 4096); // size (the fd itself isn't in the byte stream)

        let sig = [ArgumentType::NewId, ArgumentType::Fd, ArgumentType::Int];
        let walk = walk_signature(&payload, &sig).unwrap();
        assert_eq!(walk.fd_count, 1);
        assert_eq!(walk.new_id_offset, Some(0));
    }

    #[test]
    fn walk_signature_rejects_truncated_payload() {
        let payload = [0u8; 2]; // claims a Uint (4 bytes) but only has 2
        assert!(walk_signature(&payload, &[ArgumentType::Uint]).is_err());
    }

    #[test]
    fn resolve_child_interface_prefers_static_declaration() {
        let desc = MessageDesc {
            name: "get_thing",
            since: 1,
            is_destructor: false,
            signature: &[ArgumentType::NewId],
            child_interface: Some(&FAKE_CHILD_INTERFACE),
            arg_interfaces: &[],
        };
        let (iface, version) = resolve_child_interface(&desc, None).expect("should resolve");
        assert_eq!(iface.name, "fake_child");
        assert_eq!(version, 3);
    }

    #[test]
    fn resolve_child_interface_falls_back_to_dynamic_name() {
        let desc = MessageDesc {
            name: "bind",
            since: 1,
            is_destructor: false,
            signature: &[ArgumentType::Uint, ArgumentType::Str(AllowNull::No), ArgumentType::Uint, ArgumentType::NewId],
            child_interface: None,
            arg_interfaces: &[],
        };
        let (iface, _) = resolve_child_interface(&desc, Some("wl_shm")).expect("should resolve wl_shm");
        assert_eq!(iface.name, "wl_shm");
    }

    #[test]
    fn resolve_child_interface_returns_none_for_unknown_dynamic_name() {
        let desc = MessageDesc {
            name: "bind",
            since: 1,
            is_destructor: false,
            signature: &[ArgumentType::Uint, ArgumentType::Str(AllowNull::No), ArgumentType::Uint, ArgumentType::NewId],
            child_interface: None,
            arg_interfaces: &[],
        };
        assert!(resolve_child_interface(&desc, Some("not_a_real_interface")).is_none());
    }
}
