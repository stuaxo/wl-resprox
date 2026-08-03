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
use std::collections::VecDeque;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use tokio::net::UnixStream;
use tracing::{error, info, warn};

use wayland_backend::protocol::{ArgumentType, Interface, MessageDesc};

pub mod fdsocket;
pub mod grab_state;
pub mod interfaces;
pub mod recorder;
pub mod recreation;
pub mod shadow_table;
pub mod wire;

use recorder::recorder;

use grab_state::GrabTracker;
use interfaces::lookup_interface;
use recreation::{Recreatable, RecreationGraph};
use shadow_table::ShadowTable;

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
/// signature: everything we need to know to track new objects, translate
/// existing ones, and correctly attribute file descriptors, without fully
/// decoding every argument into a typed structure.
struct SignatureWalk {
    /// Byte offset (into the payload, after the 8-byte header) of the
    /// message's `NewId` argument, if it has one.
    new_id_offset: Option<usize>,
    /// Byte offset of every `Object`-typed argument -- an existing object
    /// this message *refers to*, as opposed to `new_id_offset`'s "object
    /// this message creates". Both need rewriting once IDs are actually
    /// translated (the Shadow Table), not just tracked; a message can
    /// reference more than one existing object (e.g. `get_pointer` on a
    /// seat also implicitly ties to that seat, or multi-arg requests like
    /// `wl_data_device.start_drag`'s `origin`/`icon` surfaces), so this is
    /// a `Vec`, unlike `new_id_offset` -- the wire format only ever allows
    /// one `new_id` per message.
    object_offsets: Vec<usize>,
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
    let mut object_offsets = Vec::new();
    let mut fd_count = 0;
    let mut dynamic_interface_name = None;

    for arg_type in signature {
        match arg_type {
            ArgumentType::Int | ArgumentType::Uint | ArgumentType::Fixed => {
                let end = offset.checked_add(4).ok_or("offset overflow")?;
                if payload.len() < end {
                    return Err("payload shorter than its own signature declares");
                }
                offset = end;
            }
            ArgumentType::Object(_) => {
                let end = offset.checked_add(4).ok_or("offset overflow")?;
                if payload.len() < end {
                    return Err("payload shorter than its own signature declares");
                }
                object_offsets.push(offset);
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
    Ok(SignatureWalk { new_id_offset, object_offsets, fd_count, dynamic_interface_name })
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

/// Relays every complete message currently buffered in `src` onward to
/// `dst` (or drops it silently if `dst` is `None`, i.e. we're frozen -- see
/// `run_connection`), translating every object-id-bearing field through
/// `objects` (the Shadow Table) as it goes -- both directions of the
/// message header's own `sender_id`, every `Object`-typed argument, and
/// `NewId`/`delete_id` allocation/teardown. One `write_message` call per
/// message (not batched -- see `Conn::write_message`'s doc comment for why
/// that was tried and reverted). Also records/forgets recreation recipes
/// in `graph` for the narrow set of objects that need to survive a
/// reconnect (see recreation.rs), and observes pointer/keyboard
/// focus/button state into `grabs` (see grab_state.rs) for the same reason.
async fn relay_ready_messages(
    src: &mut Conn,
    mut dst: Option<&mut Conn>,
    objects: &mut ShadowTable,
    graph: &mut RecreationGraph,
    grabs: &mut GrabTracker,
    pending_configure_acks: &mut std::collections::HashMap<u32, u32>,
    direction: Direction,
) -> Result<()> {
    'relay: loop {
        let Some((_msg, consumed)) = wire::take_message(&src.read_buf) else {
            break;
        };
        let mut msg = src.read_buf[..consumed].to_vec();
        let header = wire::MessageHeader::parse(&msg).expect("take_message already validated this");

        // The wire's sender_id is in the *sender's own* address space:
        // already a guest id for a client request, but a host id for a
        // compositor event -- translate to guest space up front so every
        // lookup below (interface, and eventually the rewritten id we send
        // onward) has one canonical space to work from.
        let guest_sender_id = match direction {
            Direction::ClientToHost => Some(header.sender_id),
            Direction::HostToClient => objects.guest_id(header.sender_id),
        };
        let interface = guest_sender_id.and_then(|g| objects.interface(g));
        let desc = interface.and_then(|iface| {
            let table = match direction {
                Direction::ClientToHost => iface.requests,
                Direction::HostToClient => iface.events,
            };
            table.get(header.opcode as usize)
        });

        let (Some(guest_sender_id), Some(interface), Some(desc)) = (guest_sender_id, interface, desc) else {
            // Unknown/untranslatable object or opcode: we don't have a
            // signature, so we genuinely cannot tell whether this message
            // carries any fds (Fd-typed args are invisible in the byte
            // stream itself -- that's the whole reason we need signatures
            // at all) or any object-id arguments needing translation.
            // Forwarding it blindly risks desyncing fd attribution *and*
            // sending a guest-space or host-space id verbatim into the
            // wrong side's namespace. Dropping it is the same limitation
            // the old wayland-backend version had (globals we don't
            // recognize are simply not usable), just now enforced
            // per-message instead of by never advertising the global in
            // the first place.
            // sender_id is only guest-space for ClientToHost (see the
            // guest_sender_id match above) -- only look up a remembered
            // name in that direction, not against a host-space id.
            let known_interface = match direction {
                Direction::ClientToHost => objects.unresolvable_interface_name(header.sender_id),
                Direction::HostToClient => None,
            };
            let hex = wire::hex_encode(&src.read_buf[..consumed]);
            match known_interface {
                Some(name) => warn!(
                    "unknown object {} ({name}) opcode {} ({:?}) -- dropping (bytes={hex})",
                    header.sender_id, header.opcode, direction
                ),
                None => warn!(
                    "unknown object {} opcode {} ({:?}) -- dropping (bytes={hex})",
                    header.sender_id, header.opcode, direction
                ),
            }
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
                // Guest-space object-argument values, captured before
                // translation overwrites them in place -- recipe-building
                // below (e.g. get_xdg_surface's `surface` argument) needs
                // the *guest* id of an existing object, which won't be
                // recoverable from `msg` anymore once the loop just below
                // rewrites it to a host id.
                let original_object_values: Vec<u32> = walk
                    .object_offsets
                    .iter()
                    .map(|&offset| {
                        let abs = wire::HEADER_LEN + offset;
                        u32::from_ne_bytes(msg[abs..abs + 4].try_into().unwrap())
                    })
                    .collect();

                // Translate every existing-object reference in the payload
                // before touching new_id/delete_id/sender_id -- if any of
                // them can't be translated (an id we've never seen, in
                // whichever direction this message travels), the whole
                // message is unsafe to forward: partially-translated bytes
                // would misdirect the receiver just as badly as untranslated
                // ones, so this drops the entire message rather than send
                // a corrupted mix.
                for &offset in &walk.object_offsets {
                    let abs = wire::HEADER_LEN + offset;
                    let original = u32::from_ne_bytes(msg[abs..abs + 4].try_into().unwrap());
                    let translated = match direction {
                        Direction::ClientToHost => objects.host_id(original),
                        Direction::HostToClient => objects.guest_id(original),
                    };
                    match translated {
                        Some(t) => msg[abs..abs + 4].copy_from_slice(&t.to_ne_bytes()),
                        None => {
                            warn!(
                                "{}.{} references untranslatable object {original} ({:?}) -- dropping",
                                interface.name, desc.name, direction
                            );
                            if let Some(rec) = recorder() {
                                rec.record(
                                    &format!("{direction:?}"), "DROPPED_UNTRANSLATABLE", interface.name, desc.name,
                                    header.sender_id, header.opcode, 0, &msg,
                                );
                            }
                            src.read_buf.drain(..consumed);
                            continue 'relay;
                        }
                    }
                }

                let mut newly_mapped_guest_id: Option<u32> = None;
                if let Some(offset) = walk.new_id_offset {
                    let abs = wire::HEADER_LEN + offset;
                    let original_new_id = u32::from_ne_bytes(msg[abs..abs + 4].try_into().unwrap());
                    match resolve_child_interface(desc, walk.dynamic_interface_name.as_deref()) {
                        Some((child_iface, _version)) => {
                            // Each side allocates from its *own* id space --
                            // never reflects the other side's chosen number
                            // verbatim, since once ids are independently
                            // allocated (rather than mirrored), a client's
                            // literal new_id could collide with something we
                            // already allocated on the host side at that same
                            // number, and vice versa.
                            let (guest_new_id, host_new_id) = match direction {
                                Direction::ClientToHost => (original_new_id, objects.allocate_host_id()),
                                Direction::HostToClient => (objects.allocate_guest_server_id(), original_new_id),
                            };
                            objects.map(guest_new_id, host_new_id, child_iface);
                            // Remembered so the "sender has no translation"
                            // check below can roll this back if the message
                            // ends up dropped there -- see that check's own
                            // comment for why this specific ordering
                            // (new_id mapped before the sender itself is
                            // known to be forwardable) is a real hazard, not
                            // just theoretical.
                            newly_mapped_guest_id = Some(guest_new_id);

                            // Recipe capture for the deliberately narrow
                            // recreatable set (see recreation.rs's doc
                            // comment) -- all four triggering requests are
                            // always client-issued, never events.
                            if matches!(direction, Direction::ClientToHost) {
                                let recipe = match (interface.name, desc.name) {
                                    ("wl_registry", "bind")
                                        if child_iface.name == "wl_compositor"
                                            || child_iface.name == "xdg_wm_base" =>
                                    {
                                        // The version the CLIENT itself
                                        // requested, not child_iface.version
                                        // (our own compiled-in static
                                        // maximum) -- found live 2026-08-03
                                        // (see plan-desktop-resilience.md):
                                        // recovery previously re-bound at
                                        // whatever version the *new*
                                        // compositor's registry advertised,
                                        // which can exceed what the
                                        // client's own compiled listener
                                        // structs understand. A real tilix
                                        // then hit libwayland-client's own
                                        // fatal `wl_abort` (confirmed via a
                                        // coredump backtrace through
                                        // wl_closure_invoke) processing a
                                        // newer wl_surface event
                                        // (preferred_buffer_scale, added in
                                        // wl_surface v6) its older stub had
                                        // no listener slot for. For a
                                        // dynamic new_id (bind is the only
                                        // such request -- see
                                        // resolve_child_interface's doc
                                        // comment), the wire layout is
                                        // always [..][interface_name:string]
                                        // [version:uint][new_id:uint], so
                                        // the version sits exactly 4 bytes
                                        // before new_id's own offset.
                                        let requested_version = msg
                                            .get(abs.saturating_sub(4)..abs)
                                            .and_then(|b| b.try_into().ok())
                                            .map(u32::from_ne_bytes)
                                            .unwrap_or(child_iface.version);
                                        Some(Recreatable::Global { interface: child_iface, version: requested_version })
                                    }
                                    ("wl_compositor", "create_surface") => {
                                        Some(Recreatable::Surface { parent_guest_id: guest_sender_id })
                                    }
                                    ("xdg_wm_base", "get_xdg_surface") => {
                                        original_object_values.first().map(|&surface_guest_id| {
                                            Recreatable::XdgSurface {
                                                parent_guest_id: guest_sender_id,
                                                surface_guest_id,
                                            }
                                        })
                                    }
                                    ("xdg_surface", "get_toplevel") => {
                                        Some(Recreatable::XdgToplevel { parent_guest_id: guest_sender_id })
                                    }
                                    _ => None,
                                };
                                if let Some(recipe) = recipe {
                                    graph.record(guest_new_id, recipe);
                                }
                            }

                            let rewritten = match direction {
                                Direction::ClientToHost => host_new_id,
                                Direction::HostToClient => guest_new_id,
                            };
                            msg[abs..abs + 4].copy_from_slice(&rewritten.to_ne_bytes());
                        }
                        None => {
                            // Can't track this object at all (unresolvable
                            // interface) -- forwarding it anyway would hand
                            // the other side a raw, unallocated id with no
                            // way to route future messages to/from it
                            // correctly. Drop the whole message; this is
                            // strictly necessary now (not just a tracking
                            // gap) since ids are independently allocated per
                            // side -- see docs/architecture-notes.md.
                            warn!(
                                "{}.{} would create an object with unresolvable interface {:?} -- dropping",
                                interface.name, desc.name, walk.dynamic_interface_name
                            );
                            // Client-space only: `original_new_id` is the
                            // guest id here (the client chose it), so a
                            // later message referencing it as a
                            // ClientToHost sender_id can name the interface
                            // instead of just showing a bare number -- see
                            // the "unknown object" warning below. Not
                            // meaningful for HostToClient (original_new_id
                            // is a host id there, a different space).
                            if matches!(direction, Direction::ClientToHost) {
                                if let Some(name) = &walk.dynamic_interface_name {
                                    objects.remember_unresolvable_interface(original_new_id, name.clone());
                                }
                            }
                            if let Some(rec) = recorder() {
                                rec.record(
                                    &format!("{direction:?}"), "DROPPED_UNRESOLVABLE_CHILD", interface.name, desc.name,
                                    header.sender_id, header.opcode, 0, &msg,
                                );
                            }
                            src.read_buf.drain(..consumed);
                            continue 'relay;
                        }
                    }
                }

                if interface.name == "wl_display" && desc.name == "error" {
                    let p = &msg[wire::HEADER_LEN..];
                    if p.len() >= 12 {
                        // bad_object is left untranslated (host-space, as
                        // received) -- this is purely diagnostic logging,
                        // never re-transmitted, so there's no correctness
                        // reason to spend a lookup on it.
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

                // wl_display.delete_id tells the client an id is now free.
                // Its payload is host-space (delete_id is host->client
                // only), so translate it to the guest id before both
                // forgetting our own tracking of it and rewriting the bytes
                // -- the client only ever recognizes its own guest-space
                // numbering, never the host's.
                if interface.name == "wl_display" && desc.name == "delete_id" {
                    if let Some(id_bytes) = msg.get(wire::HEADER_LEN..wire::HEADER_LEN + 4) {
                        let host_deleted_id = u32::from_ne_bytes(id_bytes.try_into().unwrap());
                        match objects.guest_id(host_deleted_id) {
                            Some(guest_deleted_id) => {
                                objects.remove_guest(guest_deleted_id);
                                graph.remove(guest_deleted_id);
                                msg[wire::HEADER_LEN..wire::HEADER_LEN + 4]
                                    .copy_from_slice(&guest_deleted_id.to_ne_bytes());
                            }
                            None => {
                                // Confirmed live 2026-08-03 (see
                                // plan-desktop-resilience.md): this is NOT
                                // just diagnostic-only. host id 3 is always
                                // `recover_state_after_reconnect`'s own
                                // internal wl_display.sync callback (used
                                // solely to detect "all globals have
                                // arrived"), deliberately never mapped to a
                                // guest id -- so the real compositor's
                                // later delete_id for it always lands here,
                                // on EVERY reconnect. The old code warned
                                // but still fell through and forwarded the
                                // message with its host-space payload
                                // UNTRANSLATED -- telling the client
                                // "your own guest-space id 3 is now free",
                                // except guest id 3 is whatever unrelated,
                                // very-possibly-still-live object the
                                // client itself allocated third. Caught
                                // live via WAYLAND_DEBUG=1 against a real
                                // tilix: this landed immediately before an
                                // unexplained clean client exit with no
                                // error output, consistent with a corrupted
                                // client-side id table. Never forward it --
                                // same "untracked, drop" contract as every
                                // other untranslatable-id case in this
                                // function.
                                warn!("delete_id for untracked host id {host_deleted_id} -- dropping");
                                if let Some(rec) = recorder() {
                                    rec.record(
                                        &format!("{direction:?}"), "DROPPED_UNTRACKED_DELETE_ID", interface.name, desc.name,
                                        header.sender_id, header.opcode, 0, &msg,
                                    );
                                }
                                src.read_buf.drain(..consumed);
                                continue 'relay;
                            }
                        }
                    }
                }

                // Grab-state observation (see grab_state.rs) -- events
                // only, and by this point walk.object_offsets' translation
                // loop has already rewritten any `surface` argument to its
                // guest id in place, which is exactly what these need to
                // record (a synthetic release on reconnect is sent to the
                // client, guest-space). wl_pointer.button's `button`/`state`
                // are plain uints, not caught by that loop, so read
                // straight from the payload at their fixed protocol offsets.
                if matches!(direction, Direction::HostToClient) && interface.name == "wl_pointer" {
                    let p = &msg[wire::HEADER_LEN..];
                    match desc.name {
                        "enter" if p.len() >= 8 => {
                            let surface = u32::from_ne_bytes(p[4..8].try_into().unwrap());
                            grabs.on_pointer_enter(guest_sender_id, surface);
                        }
                        "leave" => grabs.on_pointer_leave(guest_sender_id),
                        "button" if p.len() >= 16 => {
                            let button = u32::from_ne_bytes(p[8..12].try_into().unwrap());
                            let state = u32::from_ne_bytes(p[12..16].try_into().unwrap());
                            grabs.on_pointer_button(guest_sender_id, button, state);
                        }
                        _ => {}
                    }
                } else if matches!(direction, Direction::HostToClient) && interface.name == "wl_keyboard" {
                    let p = &msg[wire::HEADER_LEN..];
                    match desc.name {
                        "enter" if p.len() >= 8 => {
                            let surface = u32::from_ne_bytes(p[4..8].try_into().unwrap());
                            grabs.on_keyboard_enter(guest_sender_id, surface);
                        }
                        "leave" => grabs.on_keyboard_leave(guest_sender_id),
                        _ => {}
                    }
                }

                // Buffer lifetimes across a reconnect (see ShadowTable's
                // `generation` field doc comment for the full hazard):
                // wl_buffer is deliberately not part of the recreation
                // graph, so a buffer's mapping never gets refreshed to the
                // new generation on its own. A release event whose sender
                // is still stuck in an older generation is either for a
                // buffer the new compositor can't possibly know about, or
                // -- worse -- a stale mapping that now numerically
                // coincides with some unrelated fresh object, since both
                // compositor instances' own server-side id allocators
                // start from the same 0xff000000 baseline. Either way:
                // never forward it, drop silently, implementation-constraints.md
                // is explicit on both points.
                if matches!(direction, Direction::HostToClient)
                    && interface.name == "wl_buffer"
                    && desc.name == "release"
                    && !objects.is_current_generation(guest_sender_id)
                {
                    if let Some(rec) = recorder() {
                        rec.record(
                            &format!("{direction:?}"), "DROPPED_STALE_GENERATION", interface.name, desc.name,
                            header.sender_id, header.opcode, 0, &msg,
                        );
                    }
                    src.read_buf.drain(..consumed);
                    continue 'relay;
                }

                // Synthetic xdg_surface.configure events (see
                // recover_state_after_reconnect) invent a serial the real
                // compositor never issued, purely to force a client
                // repaint after recreation. Forwarding the client's
                // resulting ack_configure to the real compositor gets it
                // rejected ("wrong configure serial") -- confirmed live
                // against labwc, see the 2026-07-30 entry in
                // docs/debugging-notes.md. Swallow exactly the one ack
                // matching a pending synthetic serial; anything else is a
                // real ack for a real configure and forwards as normal.
                if matches!(direction, Direction::ClientToHost) && interface.name == "xdg_surface" && desc.name == "ack_configure" {
                    if let Some(serial_bytes) = msg.get(wire::HEADER_LEN..wire::HEADER_LEN + 4) {
                        let serial = u32::from_ne_bytes(serial_bytes.try_into().unwrap());
                        if pending_configure_acks.get(&guest_sender_id) == Some(&serial) {
                            pending_configure_acks.remove(&guest_sender_id);
                            src.read_buf.drain(..consumed);
                            continue 'relay;
                        }
                    }
                }

                // Now that every argument is translated, rewrite the
                // message's own sender_id: the id this message is
                // addressed *from*, in the receiving side's namespace.
                let other_side_sender_id = match direction {
                    Direction::ClientToHost => objects.host_id(guest_sender_id),
                    Direction::HostToClient => Some(guest_sender_id),
                };
                let Some(other_side_sender_id) = other_side_sender_id else {
                    // Reachable, despite `guest_sender_id` coming from a
                    // successful `interface()` lookup: `interface()` isn't
                    // generation-checked (only `host_id`/`guest_id` are --
                    // see ShadowTable's `generation` doc comment), so this
                    // fires for a request the client sends on an object
                    // that predates the last reconnect and was never
                    // refreshed (anything outside the narrow recreation
                    // graph -- wl_buffer, wl_seat, wl_shm_pool, ...). The
                    // host side genuinely never heard of this id; there's
                    // nothing to forward the request to. For a destructor
                    // request specifically (e.g. wl_buffer.destroy), the
                    // client is still waiting on wl_display.delete_id
                    // before it can reuse this numeric id --
                    // wl_proxy_destroy parks it as a zombie otherwise --
                    // so synthesize that ourselves, mirroring what a real
                    // compositor would eventually send, rather than
                    // leaking the id out of the client's own allocator.
                    if matches!(direction, Direction::ClientToHost) && desc.is_destructor {
                        if let Some(delete_id_opcode) =
                            objects.interface(1).and_then(|wl_display| event_opcode(wl_display, "delete_id"))
                        {
                            let mut delete_id_payload = Vec::new();
                            wire::put_u32(&mut delete_id_payload, guest_sender_id);
                            if let Err(e) = src
                                .write_message(&wire::build_message(1, delete_id_opcode, &delete_id_payload), &[])
                                .await
                            {
                                warn!("failed to synthesize delete_id for stale {}: {e}", interface.name);
                            }
                        }
                        objects.remove_guest(guest_sender_id);
                        graph.remove(guest_sender_id);
                    } else {
                        warn!(
                            "{}.{} sender has no translation on the other side -- dropping",
                            interface.name, desc.name
                        );
                    }
                    // Found live 2026-08-03 (see plan-desktop-resilience.md):
                    // if THIS message also carried a new_id (e.g.
                    // wl_shm.create_pool on a stale, never-recreated
                    // wl_shm), the new_id handling above already mapped and
                    // allocated a host id for it -- before this check ever
                    // ran, since the sender itself is only validated at the
                    // very end. Dropping the message here without undoing
                    // that leaves the shadow table believing an object
                    // exists on the host that was never actually created
                    // there (the request that would have created it never
                    // got forwarded). A real tilix hit exactly this: a
                    // later request against that phantom id got happily
                    // translated and forwarded, and the real compositor
                    // killed the whole connection with a fatal
                    // `wl_display.error` ("invalid object"). Roll the
                    // mapping back so the id is genuinely untracked again,
                    // matching what actually happened on the host.
                    if let Some(phantom_guest_id) = newly_mapped_guest_id {
                        objects.remove_guest(phantom_guest_id);
                        graph.remove(phantom_guest_id);
                    }
                    src.read_buf.drain(..consumed);
                    continue 'relay;
                };
                wire::write_sender_id(&mut msg, other_side_sender_id)
                    .expect("msg is at least HEADER_LEN bytes, take_message already validated this");

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
                        "relay {:?} {}.{} sender={}->{} opcode={} len={} fds={} bytes={:02x?}",
                        direction, interface.name, desc.name, header.sender_id, other_side_sender_id,
                        header.opcode, consumed, raw_fds.len(), msg
                    );
                    if let Some(rec) = recorder() {
                        rec.record(
                            &format!("{direction:?}"), "RELAYED", interface.name, desc.name,
                            other_side_sender_id, header.opcode, raw_fds.len(), &msg,
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
                        other_side_sender_id, header.opcode, walk.fd_count, &msg,
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

/// Retries connecting to `path` with a fixed short backoff until it
/// succeeds. No attempt limit or timeout -- a frozen connection is meant
/// to wait indefinitely for the compositor to come back (that's the whole
/// point of freezing instead of tearing down); the caller decides how long
/// is too long, this helper doesn't.
async fn reconnect_with_backoff(path: &std::path::Path) -> UnixStream {
    loop {
        match UnixStream::connect(path).await {
            Ok(stream) => return stream,
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
        }
    }
}

/// Finds a request's opcode by name -- its index into `interface.requests`
/// -- rather than a hardcoded number: opcodes are otherwise entirely
/// implicit (position in a generated static array), and hardcoding them
/// for the handful of requests `recover_state_after_reconnect` needs to
/// issue itself would be exactly the kind of magic-number brittleness the
/// rest of this codebase's generic, signature-driven approach avoids.
fn request_opcode(interface: &Interface, name: &str) -> Option<u16> {
    interface.requests.iter().position(|d| d.name == name).map(|i| i as u16)
}

/// Same as `request_opcode`, for the events side of an interface.
fn event_opcode(interface: &Interface, name: &str) -> Option<u16> {
    interface.events.iter().position(|d| d.name == name).map(|i| i as u16)
}

/// Runs once after a successful reconnect, before relaying resumes:
/// implementation-constraints.md's full "On Server Reconnect" section,
/// short of grab/buffer bookkeeping (separate, orthogonal work). Acts as
/// its own synthetic Wayland client against the fresh `host` connection --
/// re-fetches its registry, re-binds `wl_compositor`/`xdg_wm_base`, then
/// replays every recipe `graph` recorded (parent-before-child, see
/// recreation.rs) to recreate each tracked `wl_surface`/`xdg_surface`/
/// `xdg_toplevel` chain, synthesizing an `xdg_surface.configure` straight
/// to `gtk` after each recreated toplevel to force a repaint.
///
/// Best-effort throughout: a global the new compositor doesn't re-advertise,
/// or a recipe whose parent failed to recreate, is logged and skipped
/// rather than aborting the rest of recovery -- a partially-recovered
/// connection (some windows redrawing, others not) is strictly better than
/// none, and matches how the rest of this codebase treats an unrecoverable
/// single message (drop and continue, not tear down the connection).
async fn recover_state_after_reconnect(
    host: &mut Conn,
    gtk: &mut Conn,
    objects: &mut ShadowTable,
    graph: &RecreationGraph,
    pending_configure_acks: &mut std::collections::HashMap<u32, u32>,
) -> Result<()> {
    let Some(registry_guest_id) = objects.find_guest_id_by_interface_name("wl_registry") else {
        info!("no wl_registry tracked yet -- nothing to recover");
        return Ok(());
    };
    let registry_interface = objects.interface(registry_guest_id).expect("just found by interface name");

    // wl_display(1).get_registry(new_id), then wl_display(1).sync(new_id)
    // immediately after -- the sync's callback.done is only sent once
    // every earlier request (including get_registry's resulting global
    // events) has been processed, so it marks "all globals have arrived"
    // without a fixed sleep. Same pattern as examples/probe_bind.rs.
    let registry_host_id = objects.allocate_host_id();
    let sync_host_id = objects.allocate_host_id();
    let mut get_registry_payload = Vec::new();
    wire::put_u32(&mut get_registry_payload, registry_host_id);
    host.write_message(&wire::build_message(1, 1, &get_registry_payload), &[]).await?;
    let mut sync_payload = Vec::new();
    wire::put_u32(&mut sync_payload, sync_host_id);
    host.write_message(&wire::build_message(1, 0, &sync_payload), &[]).await?;

    // Only the two roots every recreatable chain ultimately descends from
    // need to be found here -- not a general registry, just enough to
    // satisfy whatever Recreatable::Global entries `graph` holds.
    let mut wl_compositor_global: Option<(u32, u32)> = None; // (name, version)
    let mut xdg_wm_base_global: Option<(u32, u32)> = None;
    'collect: loop {
        // `fill()` returns `Ok(0)` on EOF (matching `read()`'s convention
        // -- see its own doc comment), which the `?` below does NOT catch
        // since it's not an `Err`. Left unchecked, a compositor that
        // closes the connection right after rejecting `get_registry`
        // (confirmed live: labwc did exactly this, "invalid arguments for
        // wl_display#1.get_registry" -- see the 2026-07-30 entry in
        // docs/debugging-notes.md) turns this into a silent, 100%-CPU
        // busy loop: `fill()` keeps returning `Ok(0)` immediately forever,
        // never `WouldBlock`, so nothing here ever stops calling it again.
        if host.fill().await? == 0 {
            return Err(anyhow::anyhow!(
                "compositor closed the connection while fetching its registry (probably rejected get_registry -- possibly a new_id it didn't expect)"
            ));
        }
        while let Some((_msg, consumed)) = wire::take_message(&host.read_buf) {
            let msg = host.read_buf[..consumed].to_vec();
            let header = wire::MessageHeader::parse(&msg).expect("take_message already validated this");
            let payload = &msg[wire::HEADER_LEN..];
            if header.sender_id == registry_host_id && header.opcode == 0 {
                if let Some(name) = wire::read_u32(payload, 0) {
                    if let Some((iface_name, next)) = wire::read_str(payload, 4) {
                        if let Some(version) = wire::read_u32(payload, next) {
                            match iface_name.as_str() {
                                "wl_compositor" => wl_compositor_global = Some((name, version)),
                                "xdg_wm_base" => xdg_wm_base_global = Some((name, version)),
                                _ => {}
                            }
                        }
                    }
                }
            } else if header.sender_id == sync_host_id && header.opcode == 0 {
                host.read_buf.drain(..consumed);
                break 'collect;
            }
            host.read_buf.drain(..consumed);
        }
    }
    objects.map(registry_guest_id, registry_host_id, registry_interface);
    info!("re-fetched registry from reconnected compositor");

    let mut next_configure_serial = 1u32;

    for (guest_id, recipe) in graph.iter() {
        match recipe {
            Recreatable::Global { interface, version: requested_version } => {
                let found = match interface.name {
                    "wl_compositor" => wl_compositor_global,
                    "xdg_wm_base" => xdg_wm_base_global,
                    other => {
                        warn!("recreation graph tracked an unexpected global {other} -- skipping");
                        None
                    }
                };
                let Some((name, fresh_max_version)) = found else {
                    warn!("compositor didn't re-advertise {} after reconnect -- can't recreate it", interface.name);
                    continue;
                };
                // Bind at whatever the client originally requested, same as
                // a real client would itself -- never our own compiled-in
                // static maximum, and never blindly the new compositor's
                // own advertised maximum either (see this recipe's own doc
                // comment in recreation.rs for the wl_abort hazard that
                // caused). Capped by the new compositor's current
                // advertised max on the off chance it's now lower than
                // what was originally negotiated (a real client bind can
                // never exceed the registry's advertised version either).
                let version = (*requested_version).min(fresh_max_version);
                let host_id = objects.allocate_host_id();
                let mut payload = Vec::new();
                wire::put_u32(&mut payload, name);
                wire::put_str(&mut payload, interface.name);
                wire::put_u32(&mut payload, version);
                wire::put_u32(&mut payload, host_id);
                if let Err(e) = host.write_message(&wire::build_message(registry_host_id, 0, &payload), &[]).await {
                    warn!("failed to re-bind {}: {e}", interface.name);
                    continue;
                }
                objects.map(guest_id, host_id, interface);
                info!("recreated global {} (guest={guest_id}, host={host_id})", interface.name);
            }
            Recreatable::Surface { parent_guest_id } => {
                let Some(parent_interface) = objects.interface(*parent_guest_id) else {
                    warn!("can't recreate surface {guest_id}: parent {parent_guest_id} was never recreated");
                    continue;
                };
                let Some(parent_host_id) = objects.host_id(*parent_guest_id) else {
                    warn!("can't recreate surface {guest_id}: parent {parent_guest_id} has no host id");
                    continue;
                };
                let Some(opcode) = request_opcode(parent_interface, "create_surface") else {
                    warn!("can't recreate surface {guest_id}: {} has no create_surface request", parent_interface.name);
                    continue;
                };
                let child_interface =
                    parent_interface.requests[opcode as usize].child_interface.expect("create_surface always has a static child interface");
                let host_id = objects.allocate_host_id();
                let mut payload = Vec::new();
                wire::put_u32(&mut payload, host_id);
                if let Err(e) = host.write_message(&wire::build_message(parent_host_id, opcode, &payload), &[]).await {
                    warn!("failed to recreate surface {guest_id}: {e}");
                    continue;
                }
                objects.map(guest_id, host_id, child_interface);
                info!("recreated wl_surface (guest={guest_id}, host={host_id})");
            }
            Recreatable::XdgSurface { parent_guest_id, surface_guest_id } => {
                let Some(parent_interface) = objects.interface(*parent_guest_id) else {
                    warn!("can't recreate xdg_surface {guest_id}: parent {parent_guest_id} was never recreated");
                    continue;
                };
                let (Some(parent_host_id), Some(surface_host_id)) =
                    (objects.host_id(*parent_guest_id), objects.host_id(*surface_guest_id))
                else {
                    warn!("can't recreate xdg_surface {guest_id}: parent or surface has no host id");
                    continue;
                };
                let Some(opcode) = request_opcode(parent_interface, "get_xdg_surface") else {
                    warn!("can't recreate xdg_surface {guest_id}: {} has no get_xdg_surface request", parent_interface.name);
                    continue;
                };
                let child_interface =
                    parent_interface.requests[opcode as usize].child_interface.expect("get_xdg_surface always has a static child interface");
                let host_id = objects.allocate_host_id();
                let mut payload = Vec::new();
                wire::put_u32(&mut payload, host_id);
                wire::put_u32(&mut payload, surface_host_id);
                if let Err(e) = host.write_message(&wire::build_message(parent_host_id, opcode, &payload), &[]).await {
                    warn!("failed to recreate xdg_surface {guest_id}: {e}");
                    continue;
                }
                objects.map(guest_id, host_id, child_interface);
                info!("recreated xdg_surface (guest={guest_id}, host={host_id})");
            }
            Recreatable::XdgToplevel { parent_guest_id } => {
                let Some(parent_interface) = objects.interface(*parent_guest_id) else {
                    warn!("can't recreate xdg_toplevel {guest_id}: parent {parent_guest_id} was never recreated");
                    continue;
                };
                let Some(parent_host_id) = objects.host_id(*parent_guest_id) else {
                    warn!("can't recreate xdg_toplevel {guest_id}: parent {parent_guest_id} has no host id");
                    continue;
                };
                let Some(opcode) = request_opcode(parent_interface, "get_toplevel") else {
                    warn!("can't recreate xdg_toplevel {guest_id}: {} has no get_toplevel request", parent_interface.name);
                    continue;
                };
                let child_interface =
                    parent_interface.requests[opcode as usize].child_interface.expect("get_toplevel always has a static child interface");
                let host_id = objects.allocate_host_id();
                let mut payload = Vec::new();
                wire::put_u32(&mut payload, host_id);
                if let Err(e) = host.write_message(&wire::build_message(parent_host_id, opcode, &payload), &[]).await {
                    warn!("failed to recreate xdg_toplevel {guest_id}: {e}");
                    continue;
                }
                objects.map(guest_id, host_id, child_interface);
                info!("recreated xdg_toplevel (guest={guest_id}, host={host_id})");

                // Synthesize xdg_toplevel.configure(0, 0, []) BEFORE the
                // xdg_surface.configure below -- found live 2026-08-03 (see
                // plan-desktop-resilience.md): a bare xdg_surface.configure
                // alone doesn't reliably say anything about client state
                // being invalid, so a client can ack it and keep using its
                // existing (now stale-generation) buffers, which then get
                // silently dropped on the next attach and can trigger a
                // real, fatal compositor protocol error ("invalid arguments
                // for wl_surface#N.frame") that kills the connection --
                // exactly the failure this proxy exists to prevent.
                // xdg_toplevel.configure is the event real compositors send
                // on an actual resize/state change, which clients' already
                // -tested resize-handling code reacts to by reallocating
                // buffers -- width=0/height=0 is the protocol's own "you
                // decide the size" convention (no forced resize, avoids any
                // visible jump), states=[] (empty array) since none of the
                // tracked states (maximized/fullscreen/etc.) are known to
                // have changed. Not yet confirmed this alone is sufficient
                // against a real client (there's likely still a race window
                // for a client already mid-frame with pooled buffers) --
                // see task #7's graceful-stale-reference handling for the
                // remaining gap this doesn't close.
                if let Some(toplevel_configure_opcode) = event_opcode(child_interface, "configure") {
                    let mut toplevel_configure_payload = Vec::new();
                    wire::put_u32(&mut toplevel_configure_payload, 0); // width: 0 = no suggested size
                    wire::put_u32(&mut toplevel_configure_payload, 0); // height: 0 = no suggested size
                    wire::put_u32(&mut toplevel_configure_payload, 0); // states: empty array (length 0)
                    if let Err(e) = gtk
                        .write_message(&wire::build_message(guest_id, toplevel_configure_opcode, &toplevel_configure_payload), &[])
                        .await
                    {
                        warn!("failed to synthesize xdg_toplevel.configure for {guest_id}: {e}");
                    }
                } else {
                    warn!("xdg_toplevel has no configure event -- can't force a buffer-reallocating repaint for {guest_id}");
                }

                // Force a repaint: synthesize xdg_surface.configure straight
                // to the client on the PARENT xdg_surface's guest id (gtk
                // already knows this id, unchanged across the reconnect --
                // implementation-constraints.md is explicit that this must
                // happen "immediately after recreation").
                let Some(configure_opcode) = event_opcode(parent_interface, "configure") else {
                    warn!("xdg_surface has no configure event -- can't force a repaint for {guest_id}");
                    continue;
                };
                let mut configure_payload = Vec::new();
                wire::put_u32(&mut configure_payload, next_configure_serial);
                // Recorded so relay_ready_messages can recognize and
                // swallow the client's resulting ack_configure instead of
                // forwarding an invented serial to the real compositor.
                pending_configure_acks.insert(*parent_guest_id, next_configure_serial);
                next_configure_serial += 1;
                if let Err(e) = gtk
                    .write_message(&wire::build_message(*parent_guest_id, configure_opcode, &configure_payload), &[])
                    .await
                {
                    warn!("failed to synthesize xdg_surface.configure for {parent_guest_id}: {e}");
                }
            }
        }
    }

    Ok(())
}

/// implementation-constraints.md's "Grab State (mid-interaction crash)"
/// rule: synthesizes `wl_pointer.leave` for any surface a pointer still
/// had focus on, a fake button-release for any button still tracked as
/// held, and `wl_keyboard.leave` for any surface a keyboard still had
/// focus on -- all sent straight to `gtk`, all using whatever focus/press
/// state `grabs` observed before the crash (see grab_state.rs). Clears
/// `grabs` once done, since a fresh compositor connection starts with no
/// focus/grabs of its own. Best-effort per grab, same reasoning as
/// `recover_state_after_reconnect`: one failure shouldn't skip releasing
/// the rest.
async fn synthesize_grab_releases(gtk: &mut Conn, objects: &ShadowTable, grabs: &mut GrabTracker) -> Result<()> {
    let mut serial = 1u32;

    for (pointer_guest_id, entered_surface, pressed_buttons) in grabs.active_pointer_grabs() {
        let Some(interface) = objects.interface(pointer_guest_id) else { continue };

        if let Some(surface_guest_id) = entered_surface {
            if let Some(opcode) = event_opcode(interface, "leave") {
                let mut payload = Vec::new();
                wire::put_u32(&mut payload, serial);
                serial += 1;
                wire::put_u32(&mut payload, surface_guest_id);
                if let Err(e) = gtk.write_message(&wire::build_message(pointer_guest_id, opcode, &payload), &[]).await {
                    warn!("failed to synthesize wl_pointer.leave for {pointer_guest_id}: {e}");
                }
            }
        }
        if !pressed_buttons.is_empty() {
            if let Some(opcode) = event_opcode(interface, "button") {
                for &button in pressed_buttons {
                    let mut payload = Vec::new();
                    wire::put_u32(&mut payload, serial);
                    serial += 1;
                    wire::put_u32(&mut payload, 0); // time: no real value available, synthetic
                    wire::put_u32(&mut payload, button);
                    wire::put_u32(&mut payload, 0); // wl_pointer_button_state::Released
                    if let Err(e) = gtk.write_message(&wire::build_message(pointer_guest_id, opcode, &payload), &[]).await {
                        warn!("failed to synthesize wl_pointer.button release for {pointer_guest_id}: {e}");
                    }
                }
            }
        }
        info!("released pointer grab on {pointer_guest_id} before resuming traffic");
    }

    for (keyboard_guest_id, surface_guest_id) in grabs.active_keyboard_grabs() {
        let Some(interface) = objects.interface(keyboard_guest_id) else { continue };
        if let Some(opcode) = event_opcode(interface, "leave") {
            let mut payload = Vec::new();
            wire::put_u32(&mut payload, serial);
            serial += 1;
            wire::put_u32(&mut payload, surface_guest_id);
            if let Err(e) = gtk.write_message(&wire::build_message(keyboard_guest_id, opcode, &payload), &[]).await {
                warn!("failed to synthesize wl_keyboard.leave for {keyboard_guest_id}: {e}");
            }
        }
        info!("released keyboard grab on {keyboard_guest_id} before resuming traffic");
    }

    grabs.clear();
    Ok(())
}

/// Drives one proxied connection to completion: relays messages in both
/// directions until the GTK client disconnects. If the compositor
/// connection drops instead, the connection freezes: the GTK-facing side
/// stays open and its requests are silently dropped (see
/// docs/implementation-constraints.md's "on server disconnect" rules)
/// rather than the whole session tearing down. While frozen, this
/// retries connecting to `compositor_socket_path` in the background; once
/// that succeeds, `recover_state_after_reconnect` re-fetches globals and
/// recreates tracked surfaces/toplevels before the connection unfreezes
/// and relaying resumes. Grab/buffer bookkeeping across a reconnect is
/// separate, not-yet-wired-in work (implementation-constraints.md's other
/// two "On Server Reconnect"-adjacent rules).
pub async fn run_connection(
    gtk_stream: UnixStream,
    compositor_stream: UnixStream,
    compositor_socket_path: std::path::PathBuf,
) -> Result<()> {
    let mut gtk = Conn::new(gtk_stream);
    let mut host = Conn::new(compositor_stream);

    // wl_display (id 1 on both sides, by protocol convention) is
    // pre-seeded by ShadowTable::new() -- no discovery needed.
    let mut objects = ShadowTable::new();
    let mut graph = RecreationGraph::new();
    let mut grabs = GrabTracker::new();
    let mut pending_configure_acks: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();

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
                        relay_ready_messages(&mut gtk, dst, &mut objects, &mut graph, &mut grabs, &mut pending_configure_acks, Direction::ClientToHost).await?;
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
                        relay_ready_messages(&mut host, Some(&mut gtk), &mut objects, &mut graph, &mut grabs, &mut pending_configure_acks, Direction::HostToClient).await?;
                    }
                    Err(e) => {
                        info!("compositor connection lost ({e}) -- freezing, GTK client stays connected");
                        frozen = true;
                    }
                }
            }
            new_host_stream = reconnect_with_backoff(&compositor_socket_path), if frozen => {
                info!("compositor reconnected -- recovering state");
                host = Conn::new(new_host_stream);
                // Before anything else: everything (re)mapped from here on
                // belongs to the new generation. Objects recover_state_after_reconnect
                // is about to replay get remapped and so become current
                // again; anything it doesn't touch (wl_buffer, deliberately
                // not part of the recreation graph -- see recreation.rs)
                // stays behind in the old generation, which is exactly what
                // marks it stale for the wl_buffer.release check below.
                objects.bump_generation();
                match recover_state_after_reconnect(&mut host, &mut gtk, &mut objects, &graph, &mut pending_configure_acks).await {
                    Ok(()) => {
                        // Must happen before traffic resumes (frozen =
                        // false, below) -- implementation-constraints.md is
                        // explicit that a stuck grab is worse than a
                        // dropped click.
                        if let Err(e) = synthesize_grab_releases(&mut gtk, &objects, &mut grabs).await {
                            warn!("failed to synthesize grab releases after reconnect: {e:?}");
                        }
                        frozen = false;
                        info!("connection unfrozen, relaying resumed");
                    }
                    Err(e) => {
                        // Every error this function can return -- a failed
                        // write, a failed read, or the compositor closing
                        // the connection outright while fetching its
                        // registry -- means the *connection itself* is
                        // dead, not just that recovery came up short (see
                        // that function's own doc comment on individual
                        // steps degrading gracefully instead of returning
                        // Err). Found live 2026-08-03 (see
                        // plan-desktop-resilience.md): the old code
                        // unfroze anyway here, resuming relaying on a
                        // connection already known to be broken -- the
                        // next poll would immediately see another EOF and
                        // re-freeze, but real client traffic could race
                        // into that brief unfrozen window and get
                        // silently dropped/misrouted instead of safely
                        // buffered. Stay frozen; the select loop's own
                        // `reconnect_with_backoff, if frozen` arm retries
                        // on the next iteration. This is also exactly the
                        // race a fresh compositor's stale-socket cleanup
                        // window can trigger: `reconnect_with_backoff`'s
                        // `connect()` can succeed against a socket file
                        // the new compositor hasn't finished
                        // unlinking/rebinding yet, so a short sleep here
                        // (matching reconnect_with_backoff's own 250ms
                        // between failed connect() attempts) avoids
                        // hot-looping through that same window.
                        warn!("state recovery after reconnect failed partway, staying frozen and retrying: {e:?}");
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
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
        assert_eq!(walk.object_offsets, Vec::<usize>::new());
    }

    #[test]
    fn walk_signature_locates_every_object_argument() {
        // wl_data_device.start_drag-shaped: [Object(source), Object(origin), Object(icon), Uint(serial)]
        let mut payload = Vec::new();
        push_u32(&mut payload, 10); // source
        push_u32(&mut payload, 11); // origin
        push_u32(&mut payload, 12); // icon
        push_u32(&mut payload, 99); // serial

        let sig = [
            ArgumentType::Object(AllowNull::Yes),
            ArgumentType::Object(AllowNull::No),
            ArgumentType::Object(AllowNull::Yes),
            ArgumentType::Uint,
        ];
        let walk = walk_signature(&payload, &sig).unwrap();
        assert_eq!(walk.object_offsets, vec![0, 4, 8]);
        assert_eq!(walk.new_id_offset, None);
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
