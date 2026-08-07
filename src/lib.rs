//! Core relay logic for the crash-resilient Wayland proxy. Split out from
//! `main.rs` so integration tests (see `tests/`) can drive `run_connection`
//! directly against a fake compositor and a real Wayland client, without
//! needing a container, GPU, or real compositor for a deterministic,
//! known-good reproduction of a given wire exchange.

use anyhow::Result;
use std::collections::VecDeque;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};

use tokio::net::UnixStream;
use tracing::{error, info, warn};

use wayland_backend::protocol::{ArgumentType, Interface, MessageDesc};

pub mod buffer_flow;
pub mod clipboard;
pub mod fdsocket;
pub mod grab_state;
pub mod interfaces;
pub mod notify;
pub mod pending_frames;
pub mod recorder;
pub mod recreation;
pub mod shadow_table;
pub mod wire;

use recorder::recorder;

use buffer_flow::BufferFlowTracker;
use grab_state::GrabTracker;
use interfaces::lookup_interface;
use pending_frames::PendingFrameTracker;
use recreation::{DmabufPlane, Recreatable, RecreationGraph, SeatDeviceKind};
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
    /// Uses `try_io`, not a bare `readable().await` followed by our own raw
    /// `recvmsg` call -- that combination looks right but isn't:
    /// `readable()` alone doesn't clear tokio's internal readiness bit,
    /// only `try_read`/`try_io` do (when the closure reports `WouldBlock`).
    /// Without it, a `WouldBlock` from our own raw syscall leaves tokio
    /// thinking the socket is still ready, so `readable().await` keeps
    /// resolving immediately -- a silent 100%-CPU busy loop that never
    /// notices EOF.
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
    /// EWOULDBLOCK. Batching every ready message into one buffer per
    /// `relay_ready_messages` pass (to match libwayland's own
    /// `wl_display_flush()` coalescing) was tried and reverted -- it made
    /// an intermittent real-compositor rejection 100% deterministic
    /// instead of fixing it, so messages are sent one `write_message` call
    /// each. A `SOCK_STREAM` socket can do a genuine partial write under
    /// backpressure (GTK's startup burst of 20+ back-to-back
    /// `wl_registry.bind` calls can fill the socket buffer); silently
    /// treating that as a complete write desyncs the receiver's parser for
    /// everything after it on the connection. Uses `try_io` for the same
    /// reason `fill` does.
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
// Each parameter beyond `src`/`dst`/`direction` is its own narrowly-scoped
// tracker (ShadowTable, RecreationGraph, GrabTracker, BufferFlowTracker)
// or transient map -- bundling them into one "context" struct would just
// move the same fields one level down, not reduce what this function
// actually touches, and every call site already names them explicitly.
#[allow(clippy::too_many_arguments)]
async fn relay_ready_messages(
    src: &mut Conn,
    mut dst: Option<&mut Conn>,
    objects: &mut ShadowTable,
    graph: &mut RecreationGraph,
    grabs: &mut GrabTracker,
    pending_configure_acks: &mut std::collections::HashMap<u32, u32>,
    buffer_flow: &mut BufferFlowTracker,
    pending_frames: &mut PendingFrameTracker,
    pending_dmabuf_planes: &mut std::collections::HashMap<u32, (u32, Vec<DmabufPlane>)>,
    clipboard_cache: &clipboard::SharedClipboardCache,
    reclaim: &mut clipboard::ReclaimState,
    direction: Direction,
) -> Result<()> {
    'relay: loop {
        let Some((_msg, consumed)) = wire::take_message(&src.read_buf) else {
            break;
        };
        let mut msg = src.read_buf[..consumed].to_vec();
        let header = wire::MessageHeader::parse(&msg).expect("take_message already validated this");

        // required for clipboard copy: our own synthetic wl_data_source
        // (see attempt_clipboard_splice) has no guest-side counterpart at
        // all, so the normal guest-id-driven resolution just below would
        // see it as an untranslatable object and drop it -- intercept its
        // events here, before that, using our own bookkeeping instead of
        // the shadow table.
        if matches!(direction, Direction::HostToClient) && Some(header.sender_id) == reclaim.active_source_host_id {
            handle_synthetic_clipboard_event(src, &msg, header.opcode, clipboard_cache, reclaim).await;
            src.read_buf.drain(..consumed);
            continue 'relay;
        }

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
                                            || child_iface.name == "xdg_wm_base"
                                            || child_iface.name == "wl_shm"
                                            || child_iface.name == "zwp_linux_dmabuf_v1"
                                            || child_iface.name == "wl_seat"
                                            || child_iface.name == "wp_viewporter"
                                            || child_iface.name == "wp_fractional_scale_manager_v1" =>
                                    {
                                        // The version the CLIENT itself
                                        // requested, not child_iface.version
                                        // (our compiled-in static maximum)
                                        // or the new compositor's own
                                        // advertised max -- rebinding at a
                                        // higher version than the client's
                                        // own listener structs understand
                                        // can hand it an event shape it has
                                        // no slot for. For a dynamic new_id
                                        // (bind is the only such request --
                                        // see resolve_child_interface), the
                                        // wire layout is always
                                        // [..][interface_name:string]
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
                                        Some(Recreatable::XdgToplevel { parent_guest_id: guest_sender_id, title: None, app_id: None })
                                    }
                                    ("wl_shm_pool", "create_buffer") => {
                                        // offset/width/height/stride/format
                                        // are plain Int/Uint args, not
                                        // Object-typed -- not present in
                                        // original_object_values (only
                                        // object-argument offsets, see its
                                        // own comment above), so read
                                        // straight from the payload at their
                                        // fixed positions immediately after
                                        // the new_id (see ADR-0006's
                                        // implementation sketch: no fd of
                                        // its own, draws from the pool's
                                        // already-retained backing memfd).
                                        let payload = &msg[wire::HEADER_LEN..];
                                        let read_i32 = |at: usize| wire::read_u32(payload, at).map(|v| v as i32);
                                        match (read_i32(offset + 4), read_i32(offset + 8), read_i32(offset + 12), read_i32(offset + 16), read_i32(offset + 20)) {
                                            (Some(buf_offset), Some(width), Some(height), Some(stride), Some(format)) => {
                                                Some(Recreatable::ShmBuffer {
                                                    pool_guest_id: guest_sender_id,
                                                    offset: buf_offset,
                                                    width,
                                                    height,
                                                    stride,
                                                    format: format as u32,
                                                })
                                            }
                                            _ => None,
                                        }
                                    }
                                    ("zwp_linux_dmabuf_v1", "create_params") => {
                                        // Not itself a Recreatable -- the
                                        // params object is single-use by
                                        // protocol design and never
                                        // replayed as its own object (see
                                        // DmabufBuffer's own doc comment in
                                        // recreation.rs) -- only its
                                        // parent (this dmabuf global's own
                                        // guest id) needs remembering, so a
                                        // later create_immed on this same
                                        // params guest id knows which host
                                        // dmabuf global to recreate a fresh
                                        // params object from.
                                        pending_dmabuf_planes.insert(guest_new_id, (guest_sender_id, Vec::new()));
                                        None
                                    }
                                    ("zwp_linux_buffer_params_v1", "create_immed") => {
                                        // width/height/format/flags are
                                        // plain Int/Uint args immediately
                                        // after the new_id, same reasoning
                                        // as wl_shm_pool.create_buffer
                                        // above. Planes were already
                                        // accumulated by each preceding
                                        // add() call (see the fd-retention
                                        // site below) against THIS params
                                        // object's own guest id
                                        // (guest_sender_id here, since
                                        // create_immed is a request ON the
                                        // params object) -- removed
                                        // (not just read) since the params
                                        // object is documented single-use,
                                        // nothing else will ever add() to
                                        // it again.
                                        let payload = &msg[wire::HEADER_LEN..];
                                        let read_i32 = |at: usize| wire::read_u32(payload, at).map(|v| v as i32);
                                        match (
                                            read_i32(offset + 4),
                                            read_i32(offset + 8),
                                            read_i32(offset + 12),
                                            read_i32(offset + 16),
                                            pending_dmabuf_planes.remove(&guest_sender_id),
                                        ) {
                                            (Some(width), Some(height), Some(format), Some(flags), Some((dmabuf_guest_id, planes))) => {
                                                Some(Recreatable::DmabufBuffer {
                                                    dmabuf_guest_id,
                                                    width,
                                                    height,
                                                    format: format as u32,
                                                    flags: flags as u32,
                                                    planes,
                                                })
                                            }
                                            _ => None,
                                        }
                                    }
                                    // See Recreatable::SeatDevice: needs the
                                    // same "re-map onto a fresh host id"
                                    // replay every other recipe gets, or
                                    // these go silently dead after a crash.
                                    ("wl_seat", "get_pointer") => {
                                        Some(Recreatable::SeatDevice { seat_guest_id: guest_sender_id, kind: SeatDeviceKind::Pointer })
                                    }
                                    ("wl_seat", "get_keyboard") => {
                                        Some(Recreatable::SeatDevice { seat_guest_id: guest_sender_id, kind: SeatDeviceKind::Keyboard })
                                    }
                                    ("wl_seat", "get_touch") => {
                                        Some(Recreatable::SeatDevice { seat_guest_id: guest_sender_id, kind: SeatDeviceKind::Touch })
                                    }
                                    // See Recreatable::Viewport -- same
                                    // missing-root shape as SeatDevice.
                                    // `surface` is the request's only
                                    // object argument, so index 0.
                                    ("wp_viewporter", "get_viewport") => {
                                        original_object_values.first().map(|&surface_guest_id| Recreatable::Viewport {
                                            viewporter_guest_id: guest_sender_id,
                                            surface_guest_id,
                                            destination: None,
                                        })
                                    }
                                    ("wp_fractional_scale_manager_v1", "get_fractional_scale") => {
                                        original_object_values.first().map(|&surface_guest_id| Recreatable::FractionalScale {
                                            manager_guest_id: guest_sender_id,
                                            surface_guest_id,
                                        })
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
                                // Not just diagnostic: host id 3 is always
                                // recover_state_after_reconnect's own
                                // internal wl_display.sync callback
                                // (detects "all globals have arrived"),
                                // deliberately never mapped to a guest id --
                                // so the real compositor's delete_id for it
                                // lands here on every reconnect. Forwarding
                                // it untranslated would tell the client
                                // "guest id 3 is now free" for whatever
                                // unrelated, possibly-still-live object it
                                // actually allocated third, corrupting its
                                // own id table. Same "untracked, drop"
                                // contract as every other untranslatable-id
                                // case in this function.
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

                // required for clipboard copy: the first real,
                // compositor-issued input serial this connection sees
                // after a reconnect gets borrowed to re-establish the
                // proxy as clipboard owner from cached bytes -- see
                // attempt_clipboard_splice and docs/adr/adr-0009-
                // clipboard-persistence.md, which live-verified a
                // fabricated serial gets rejected but a real one, even
                // issued for something unrelated like this, is accepted.
                if reclaim.pending
                    && matches!(direction, Direction::HostToClient)
                    && matches!(
                        (interface.name, desc.name),
                        ("wl_pointer", "enter")
                            | ("wl_pointer", "leave")
                            | ("wl_pointer", "button")
                            | ("wl_keyboard", "enter")
                            | ("wl_keyboard", "leave")
                            | ("wl_keyboard", "key")
                            | ("wl_touch", "down")
                    )
                {
                    if let Some(serial) = wire::read_u32(&msg[wire::HEADER_LEN..], 0) {
                        attempt_clipboard_splice(src, objects, reclaim, clipboard_cache, serial).await;
                    }
                }

                // wl_shm_pool.resize(size) doesn't create a new object (no
                // new_id), so it never goes through the new_id recipe-
                // capture block above -- but a pool tracked by the
                // recreation graph (see recreation.rs's ShmPool variant,
                // ADR-0006) still needs its recorded `size` kept accurate,
                // or a replayed create_pool after a reconnect would recreate
                // it at its original, now-stale size. Observation only:
                // the request itself still forwards normally below, same as
                // the grab-state observation above.
                if matches!(direction, Direction::ClientToHost) && interface.name == "wl_shm_pool" && desc.name == "resize" {
                    if let Some(size) = wire::read_u32(&msg[wire::HEADER_LEN..], 0).map(|v| v as i32) {
                        graph.update_shm_pool_size(guest_sender_id, size);
                    }
                }

                // xdg_toplevel.set_title(title)/set_app_id(app_id) --
                // same reasoning as wl_shm_pool.resize just above: neither
                // creates a new object, so neither goes through the new_id
                // recipe-capture block. See
                // RecreationGraph::update_toplevel_title: a client sends
                // these once, right after get_toplevel(), so without this
                // observation a replayed toplevel has no title/app_id.
                if matches!(direction, Direction::ClientToHost) && interface.name == "xdg_toplevel" && desc.name == "set_title" {
                    if let Some((title, _)) = wire::read_str(&msg[wire::HEADER_LEN..], 0) {
                        graph.update_toplevel_title(guest_sender_id, title);
                    }
                } else if matches!(direction, Direction::ClientToHost) && interface.name == "xdg_toplevel" && desc.name == "set_app_id" {
                    if let Some((app_id, _)) = wire::read_str(&msg[wire::HEADER_LEN..], 0) {
                        graph.update_toplevel_app_id(guest_sender_id, app_id);
                    }
                }

                // wp_viewport.set_destination(width, height) -- same
                // reasoning as set_title/set_app_id just above. See
                // Recreatable::Viewport: this is the piece that fixes a
                // recovered window rendering oversized on a
                // fractionally-scaled output.
                if matches!(direction, Direction::ClientToHost) && interface.name == "wp_viewport" && desc.name == "set_destination" {
                    let payload = &msg[wire::HEADER_LEN..];
                    if let (Some(width), Some(height)) =
                        (wire::read_u32(payload, 0).map(|v| v as i32), wire::read_u32(payload, 4).map(|v| v as i32))
                    {
                        graph.update_viewport_destination(guest_sender_id, width, height);
                    }
                }

                // Buffer lifetimes across a reconnect (see ShadowTable's
                // `generation` field doc comment for the full hazard): a
                // wl_shm-backed buffer IS now part of the recreation graph
                // (see recreation.rs's ShmBuffer variant, ADR-0006), but
                // only from the point it's actually replayed on reconnect
                // onward -- a release event whose sender is still stuck in
                // an OLDER generation (this check) is either for a buffer
                // that was never recreated at all (e.g. a dmabuf one,
                // still outside the graph), or -- worse -- a stale mapping
                // that now numerically coincides with some unrelated fresh
                // object, since both compositor instances' own server-side
                // id allocators start from the same 0xff000000 baseline.
                // Either way: never forward it, drop silently,
                // implementation-constraints.md is explicit on both points.
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

                // Buffer flow-control observation (see buffer_flow.rs):
                // recreating a wl_buffer's protocol identity isn't enough
                // if the client's own buffer pool believes every buffer it
                // owns is still "busy," waiting on a release the dead
                // compositor will never send. Observation only -- neither
                // request forwards any differently because of this.
                if matches!(direction, Direction::ClientToHost) && interface.name == "wl_surface" && desc.name == "attach" {
                    let buffer_guest_id = original_object_values.first().copied().unwrap_or(0);
                    buffer_flow.on_attach(guest_sender_id, buffer_guest_id);
                } else if matches!(direction, Direction::ClientToHost) && interface.name == "wl_surface" && desc.name == "commit" {
                    buffer_flow.on_commit(guest_sender_id);
                } else if matches!(direction, Direction::HostToClient) && interface.name == "wl_buffer" && desc.name == "release" {
                    // Only reachable here for a CURRENT-generation release
                    // (the stale-generation one above already dropped and
                    // continued) -- exactly the buffers this tracker needs
                    // to hear about.
                    buffer_flow.on_release(guest_sender_id);
                }

                // Pending-callback observation (see pending_frames.rs): a
                // frame()/sync() that reaches the OLD compositor
                // successfully (forwarded normally, so the "sender has no
                // translation" synthesis just above never fires) but never
                // gets answered before it dies leaves the client blocked
                // waiting on that specific callback's `done` forever.
                // Reached for both a genuinely-forwarded request and one
                // silently dropped while frozen (still needs answering
                // eventually) -- only the "sender has no translation"
                // variant above is excluded, since that's already answered
                // immediately, right there.
                if matches!(direction, Direction::ClientToHost) && interface.name == "wl_surface" && desc.name == "frame" {
                    if let Some(callback_guest_id) = newly_mapped_guest_id {
                        pending_frames.on_frame_requested(callback_guest_id);
                    }
                } else if matches!(direction, Direction::ClientToHost) && interface.name == "wl_display" && desc.name == "sync" {
                    if let Some(callback_guest_id) = newly_mapped_guest_id {
                        pending_frames.on_sync_requested(callback_guest_id);
                    }
                } else if matches!(direction, Direction::HostToClient) && interface.name == "wl_callback" && desc.name == "done" {
                    pending_frames.on_done_received(guest_sender_id);
                }

                // Synthetic xdg_surface.configure events (see
                // recover_state_after_reconnect) invent a serial the real
                // compositor never issued, purely to force a client
                // repaint after recreation. Forwarding the client's
                // resulting ack_configure to the real compositor gets it
                // rejected ("wrong configure serial"). Swallow exactly the
                // one ack matching a pending synthetic serial; anything
                // else is a real ack for a real configure and forwards as
                // normal.
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
                    } else if matches!(direction, Direction::ClientToHost)
                        && interface.name == "wl_surface"
                        && desc.name == "frame"
                    {
                        // wl_surface.frame registers a promise the
                        // compositor must keep -- "tell me (via
                        // wl_callback.done) when it's a good time to draw
                        // again" -- which GTK's own frame clock blocks on.
                        // Unlike wl_buffer/wl_shm (permanently outside the
                        // recreation graph), wl_surface DOES come back
                        // after a reconnect; this branch fires for a
                        // frame() request that merely arrived in the
                        // narrow window between bump_generation() and the
                        // surface being remapped by
                        // recover_state_after_reconnect, not because the
                        // surface is gone for good. Dropping it without
                        // answering leaves the client waiting forever on a
                        // callback that will never fire, since the request
                        // that would have triggered it never reached the
                        // real compositor. Losing the frame's own
                        // attach/commit is fine (one frame's pixel content
                        // is acceptable loss); the callback specifically
                        // must still be answered, or the stall is
                        // permanent, not just a skipped frame.
                        if let Some(callback_guest_id) = newly_mapped_guest_id {
                            synthesize_frame_done(src, objects, callback_guest_id).await;
                            // Answered right here -- must not ALSO get
                            // answered again by the pending_frames drain in
                            // a later recover_state_after_reconnect (see
                            // pending_frames.rs); it was never inserted
                            // there in the first place since this whole
                            // branch is reached only via the "sender has no
                            // translation" path, which the pending_frames
                            // insertion hook below (an ordinary observation
                            // on a genuinely-forwarded-or-frozen-dropped
                            // frame()) never reaches.
                        }
                        warn!(
                            "wl_surface.frame sender has no translation on the other side -- \
                             synthesized done+delete_id instead of dropping silently"
                        );
                    } else {
                        warn!(
                            "{}.{} sender has no translation on the other side -- dropping",
                            interface.name, desc.name
                        );
                    }
                    // If THIS message also carried a new_id (e.g.
                    // wl_shm.create_pool on a stale, never-recreated
                    // wl_shm), the new_id handling above already mapped and
                    // allocated a host id for it, since the sender itself
                    // is only validated at the very end. Dropping the
                    // message here without undoing that leaves the shadow
                    // table believing an object exists on the host that
                    // was never actually created there -- a later request
                    // against that phantom id would get happily translated
                    // and forwarded, and the real compositor would kill
                    // the whole connection with a fatal `wl_display.error`
                    // ("invalid object"). Roll the mapping back so the id
                    // is genuinely untracked again, matching what actually
                    // happened on the host.
                    if let Some(phantom_guest_id) = newly_mapped_guest_id {
                        // See ShadowTable::unallocate_host_id for the
                        // mechanism this closes: only a ClientToHost
                        // message burns one of *our* host ids via
                        // allocate_host_id (HostToClient allocates a guest
                        // id instead, from an independent counter never
                        // subject to this) -- give it back before
                        // forgetting the mapping, or this connection's own
                        // next legitimate new_id eventually gets rejected
                        // by the host as out-of-sequence once the gap is
                        // reached.
                        if matches!(direction, Direction::ClientToHost) {
                            if let Some(phantom_host_id) = objects.host_id(phantom_guest_id) {
                                objects.unallocate_host_id(phantom_host_id);
                            }
                        }
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

                // required for clipboard copy: wl_data_source.send's fd is
                // the pipe the client writes the copied bytes into: swap
                // it for a pipe of our own and tee the bytes into a cache
                // as they go by, so a later crash or the client quitting
                // doesn't lose them (see docs/adr/adr-0009-clipboard-
                // persistence.md). Real fd still gets every byte, just
                // relayed through us instead of handed straight to the
                // client.
                if matches!(direction, Direction::HostToClient) && interface.name == "wl_data_source" && desc.name == "send" {
                    if let Some(mime_type) = wire::read_str(&msg[wire::HEADER_LEN..], 0).map(|(s, _)| s) {
                        if clipboard::is_cacheable_mime(&mime_type) {
                            if let Some(real_fd) = fds.pop() {
                                match clipboard::start_tee(real_fd, mime_type, clipboard_cache.clone()) {
                                    Some(client_facing_fd) => fds.push(client_facing_fd),
                                    None => warn!("clipboard tee setup failed -- not forwarding a substitute fd"),
                                }
                            }
                        }
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
                    // Per-message write, not batched -- see
                    // Conn::write_message's own doc comment for why.
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

                // Retain our own copy of wl_shm.create_pool's backing fd for
                // later buffer recreation (see recreation.rs's ShmPool
                // variant and ADR-0006) -- deliberately AFTER the
                // forward/drop above, not before: forwarding above already
                // sent its own copy via sendmsg (which dup's the fd onto the
                // wire), so moving this one out of `fds` here doesn't
                // disturb what the host received, or -- while frozen --
                // what would have been sent. Retained either way: our own
                // replay against a fresh compositor never depends on
                // whether the *old*, possibly-dead host ever saw this
                // message.
                if matches!(direction, Direction::ClientToHost) && interface.name == "wl_shm" && desc.name == "create_pool" {
                    if let (Some(pool_guest_id), Some(fd)) = (newly_mapped_guest_id, fds.pop()) {
                        let payload = &msg[wire::HEADER_LEN..];
                        let size = walk
                            .new_id_offset
                            .and_then(|off| wire::read_u32(payload, off + 4))
                            .map(|v| v as i32)
                            .unwrap_or(0);
                        graph.record(pool_guest_id, Recreatable::ShmPool { wl_shm_guest_id: guest_sender_id, fd, size });
                    } else {
                        warn!("wl_shm.create_pool had no fd to retain for recreation");
                    }
                }

                // Retain our own copy of zwp_linux_buffer_params_v1.add's
                // per-plane fd for later buffer recreation (see
                // recreation.rs's DmabufBuffer variant and ADR-0006) --
                // same reasoning and same AFTER-forwarding placement as
                // wl_shm.create_pool's retention above. Accumulated
                // against the params object's own guest id
                // (guest_sender_id, since add() is a request ON the
                // params object) until create_immed() drains it into an
                // actual recipe.
                if matches!(direction, Direction::ClientToHost) && interface.name == "zwp_linux_buffer_params_v1" && desc.name == "add" {
                    if let Some(fd) = fds.pop() {
                        let payload = &msg[wire::HEADER_LEN..];
                        let fields = (
                            wire::read_u32(payload, 0),  // plane_idx
                            wire::read_u32(payload, 4),  // offset
                            wire::read_u32(payload, 8),  // stride
                            wire::read_u32(payload, 12), // modifier_hi
                            wire::read_u32(payload, 16), // modifier_lo
                        );
                        if let (Some(plane_idx), Some(plane_offset), Some(stride), Some(modifier_hi), Some(modifier_lo)) = fields {
                            if let Some((_dmabuf_guest_id, planes)) = pending_dmabuf_planes.get_mut(&guest_sender_id) {
                                planes.push(DmabufPlane {
                                    fd,
                                    plane_idx,
                                    offset: plane_offset,
                                    stride,
                                    modifier: ((modifier_hi as u64) << 32) | modifier_lo as u64,
                                });
                            } else {
                                warn!(
                                    "zwp_linux_buffer_params_v1.add on an untracked params object {guest_sender_id} \
                                     -- dropping the retained fd"
                                );
                            }
                        }
                    } else {
                        warn!("zwp_linux_buffer_params_v1.add had no fd to retain for recreation");
                    }
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

/// Answers a `wl_surface.frame()` promise the real compositor never will:
/// `wl_callback.done` (no real presentation timestamp to give) followed by
/// the `delete_id` a one-shot callback object is owed before the client
/// may reuse its numeric id (`wl_proxy_destroy` parks it as a zombie
/// otherwise). Shared by two call sites that both need exactly this pair
/// of synthesized events for a callback whose `frame()` request will never
/// be answered by the (dead) compositor that would have -- see
/// `relay_ready_messages`'s frame-synthesis branch (a frame() dropped
/// because its surface was momentarily untranslatable) and
/// `recover_state_after_reconnect`'s `pending_frames` drain (a frame()
/// that reached the OLD compositor and was simply never answered before
/// it died -- see pending_frames.rs).
async fn synthesize_frame_done(dst: &mut Conn, objects: &ShadowTable, callback_guest_id: u32) {
    let Some(callback_iface) = objects.interface(callback_guest_id) else { return };
    if let Some(done_opcode) = event_opcode(callback_iface, "done") {
        let mut done_payload = Vec::new();
        wire::put_u32(&mut done_payload, 0); // callback_data: no real presentation timestamp to give
        if let Err(e) = dst.write_message(&wire::build_message(callback_guest_id, done_opcode, &done_payload), &[]).await {
            warn!("failed to synthesize wl_callback.done for callback {callback_guest_id}: {e}");
        }
    }
    if let Some(delete_id_opcode) = objects.interface(1).and_then(|wl_display| event_opcode(wl_display, "delete_id")) {
        let mut delete_id_payload = Vec::new();
        wire::put_u32(&mut delete_id_payload, callback_guest_id);
        if let Err(e) = dst.write_message(&wire::build_message(1, delete_id_opcode, &delete_id_payload), &[]).await {
            warn!("failed to synthesize delete_id for callback {callback_guest_id}: {e}");
        }
    }
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
#[allow(clippy::too_many_arguments)] // each param is its own narrowly-scoped tracker -- see relay_ready_messages' own identical allow for why bundling wouldn't help
async fn recover_state_after_reconnect(
    host: &mut Conn,
    gtk: &mut Conn,
    objects: &mut ShadowTable,
    graph: &RecreationGraph,
    pending_configure_acks: &mut std::collections::HashMap<u32, u32>,
    buffer_flow: &mut BufferFlowTracker,
    pending_frames: &mut PendingFrameTracker,
    reclaim: &mut clipboard::ReclaimState,
) -> Result<()> {
    // A stale host id from the previous compositor life could otherwise
    // coincide with an unrelated fresh object once ids restart -- see
    // ReclaimState::active_source_host_id's own doc comment.
    reclaim.active_source_host_id = None;

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
    let mut wl_shm_global: Option<(u32, u32)> = None;
    let mut zwp_linux_dmabuf_v1_global: Option<(u32, u32)> = None;
    // See Recreatable::SeatDevice for why wl_seat needs to be a real
    // recreation root, not left for the client to naturally re-obtain.
    let mut wl_seat_global: Option<(u32, u32)> = None;
    // See Recreatable::Viewport.
    let mut wp_viewporter_global: Option<(u32, u32)> = None;
    let mut wp_fractional_scale_manager_v1_global: Option<(u32, u32)> = None;
    // Not part of the recreation graph -- required for clipboard copy
    // instead: attempt_clipboard_splice needs this to bind a fresh
    // wl_data_device_manager for the reclaim attempt.
    let mut wl_data_device_manager_global: Option<(u32, u32)> = None;
    'collect: loop {
        // `fill()` returns `Ok(0)` on EOF (matching `read()`'s convention),
        // which the `?` below does NOT catch since it's not an `Err`.
        // Left unchecked, a compositor that closes the connection right
        // after rejecting `get_registry` turns this into a silent,
        // 100%-CPU busy loop: `fill()` keeps returning `Ok(0)` immediately
        // forever, never `WouldBlock`, so nothing here ever stops calling
        // it again.
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
                                "wl_shm" => wl_shm_global = Some((name, version)),
                                "zwp_linux_dmabuf_v1" => zwp_linux_dmabuf_v1_global = Some((name, version)),
                                "wl_seat" => wl_seat_global = Some((name, version)),
                                "wp_viewporter" => wp_viewporter_global = Some((name, version)),
                                "wp_fractional_scale_manager_v1" => wp_fractional_scale_manager_v1_global = Some((name, version)),
                                "wl_data_device_manager" => wl_data_device_manager_global = Some((name, version)),
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
                    "wl_shm" => wl_shm_global,
                    "zwp_linux_dmabuf_v1" => zwp_linux_dmabuf_v1_global,
                    "wl_seat" => wl_seat_global,
                    "wp_viewporter" => wp_viewporter_global,
                    "wp_fractional_scale_manager_v1" => wp_fractional_scale_manager_v1_global,
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
                // wl_registry.bind's real signature (`[Uint, Str(No), Uint,
                // NewId]`, per wayland-scanner's own codegen for any
                // interface-less new_id -- see encode_arguments's doc
                // comment) already matches these four values 1:1; no
                // special-casing needed.
                let values = vec![
                    wire::WaylandValue::Uint(name),
                    wire::WaylandValue::String(interface.name.to_string()),
                    wire::WaylandValue::Uint(version),
                    wire::WaylandValue::NewId(host_id),
                ];
                let bind_signature = registry_interface.requests[0].signature;
                let (payload, fds) = match wire::encode_arguments(bind_signature, values) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("failed to encode bind for {}: {e}", interface.name);
                        continue;
                    }
                };
                if let Err(e) = host.write_message(&wire::build_message(registry_host_id, 0, &payload), &fds).await {
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
                let signature = parent_interface.requests[opcode as usize].signature;
                let (payload, fds) = match wire::encode_arguments(signature, vec![wire::WaylandValue::NewId(host_id)]) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("failed to encode create_surface for surface {guest_id}: {e}");
                        continue;
                    }
                };
                if let Err(e) = host.write_message(&wire::build_message(parent_host_id, opcode, &payload), &fds).await {
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
                let signature = parent_interface.requests[opcode as usize].signature;
                let values = vec![wire::WaylandValue::NewId(host_id), wire::WaylandValue::Object(surface_host_id)];
                let (payload, fds) = match wire::encode_arguments(signature, values) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("failed to encode get_xdg_surface for xdg_surface {guest_id}: {e}");
                        continue;
                    }
                };
                if let Err(e) = host.write_message(&wire::build_message(parent_host_id, opcode, &payload), &fds).await {
                    warn!("failed to recreate xdg_surface {guest_id}: {e}");
                    continue;
                }
                objects.map(guest_id, host_id, child_interface);
                info!("recreated xdg_surface (guest={guest_id}, host={host_id})");
            }
            Recreatable::XdgToplevel { parent_guest_id, title, app_id } => {
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
                let signature = parent_interface.requests[opcode as usize].signature;
                let (payload, fds) = match wire::encode_arguments(signature, vec![wire::WaylandValue::NewId(host_id)]) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("failed to encode get_toplevel for xdg_toplevel {guest_id}: {e}");
                        continue;
                    }
                };
                if let Err(e) = host.write_message(&wire::build_message(parent_host_id, opcode, &payload), &fds).await {
                    warn!("failed to recreate xdg_toplevel {guest_id}: {e}");
                    continue;
                }
                objects.map(guest_id, host_id, child_interface);
                info!("recreated xdg_toplevel (guest={guest_id}, host={host_id})");

                // Replay set_title/set_app_id -- see
                // RecreationGraph::update_toplevel_title: without this,
                // the freshly recreated host-side toplevel is never told
                // its title/app_id at all. Sent before the synthesized
                // configure below, same ordering a real client uses
                // (identity established before the surface is mapped).
                if let Some(title) = title {
                    if let Some(set_title_opcode) = request_opcode(child_interface, "set_title") {
                        let sig = child_interface.requests[set_title_opcode as usize].signature;
                        match wire::encode_arguments(sig, vec![wire::WaylandValue::String(title.clone())]) {
                            Ok((set_title_payload, fds)) => {
                                if let Err(e) =
                                    host.write_message(&wire::build_message(host_id, set_title_opcode, &set_title_payload), &fds).await
                                {
                                    warn!("failed to replay set_title for xdg_toplevel {guest_id}: {e}");
                                }
                            }
                            Err(e) => warn!("failed to encode set_title for xdg_toplevel {guest_id}: {e}"),
                        }
                    }
                }
                if let Some(app_id) = app_id {
                    if let Some(set_app_id_opcode) = request_opcode(child_interface, "set_app_id") {
                        let sig = child_interface.requests[set_app_id_opcode as usize].signature;
                        match wire::encode_arguments(sig, vec![wire::WaylandValue::String(app_id.clone())]) {
                            Ok((set_app_id_payload, fds)) => {
                                if let Err(e) =
                                    host.write_message(&wire::build_message(host_id, set_app_id_opcode, &set_app_id_payload), &fds).await
                                {
                                    warn!("failed to replay set_app_id for xdg_toplevel {guest_id}: {e}");
                                }
                            }
                            Err(e) => warn!("failed to encode set_app_id for xdg_toplevel {guest_id}: {e}"),
                        }
                    }
                }

                // Synthesize xdg_toplevel.configure(0, 0, []) BEFORE the
                // xdg_surface.configure below: a bare xdg_surface.configure
                // alone doesn't reliably signal that client state is
                // invalid, so a client can ack it and keep using its
                // existing (now stale-generation) buffers, which then get
                // silently dropped on the next attach and can trigger a
                // fatal compositor protocol error that kills the
                // connection. xdg_toplevel.configure is what real
                // compositors send on an actual resize/state change,
                // which a client's own resize-handling reacts to by
                // reallocating buffers -- width=0/height=0 is the
                // protocol's "you decide the size" convention (no forced
                // resize, no visible jump), states=[] since no tracked
                // state is known to have changed. May not be sufficient
                // alone for a client already mid-frame with pooled
                // buffers -- not yet hit live, but a plausible remaining
                // race.
                if let Some(toplevel_configure_opcode) = event_opcode(child_interface, "configure") {
                    let sig = child_interface.events[toplevel_configure_opcode as usize].signature;
                    let values = vec![
                        wire::WaylandValue::Int(0),   // width: 0 = no suggested size
                        wire::WaylandValue::Int(0),   // height: 0 = no suggested size
                        wire::WaylandValue::Array(Vec::new()), // states: empty array
                    ];
                    match wire::encode_arguments(sig, values) {
                        Ok((toplevel_configure_payload, fds)) => {
                            if let Err(e) = gtk
                                .write_message(&wire::build_message(guest_id, toplevel_configure_opcode, &toplevel_configure_payload), &fds)
                                .await
                            {
                                warn!("failed to synthesize xdg_toplevel.configure for {guest_id}: {e}");
                            }
                        }
                        Err(e) => warn!("failed to encode xdg_toplevel.configure for {guest_id}: {e}"),
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
                let sig = parent_interface.events[configure_opcode as usize].signature;
                let (configure_payload, fds) = match wire::encode_arguments(sig, vec![wire::WaylandValue::Uint(next_configure_serial)]) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("failed to encode xdg_surface.configure for {parent_guest_id}: {e}");
                        continue;
                    }
                };
                // Recorded so relay_ready_messages can recognize and
                // swallow the client's resulting ack_configure instead of
                // forwarding an invented serial to the real compositor.
                pending_configure_acks.insert(*parent_guest_id, next_configure_serial);
                next_configure_serial += 1;
                if let Err(e) = gtk
                    .write_message(&wire::build_message(*parent_guest_id, configure_opcode, &configure_payload), &fds)
                    .await
                {
                    warn!("failed to synthesize xdg_surface.configure for {parent_guest_id}: {e}");
                }
            }
            Recreatable::ShmPool { wl_shm_guest_id, fd, size } => {
                let Some(parent_interface) = objects.interface(*wl_shm_guest_id) else {
                    warn!("can't recreate shm pool {guest_id}: wl_shm {wl_shm_guest_id} was never recreated");
                    continue;
                };
                let Some(parent_host_id) = objects.host_id(*wl_shm_guest_id) else {
                    warn!("can't recreate shm pool {guest_id}: wl_shm {wl_shm_guest_id} has no host id");
                    continue;
                };
                let Some(opcode) = request_opcode(parent_interface, "create_pool") else {
                    warn!("can't recreate shm pool {guest_id}: {} has no create_pool request", parent_interface.name);
                    continue;
                };
                let child_interface =
                    parent_interface.requests[opcode as usize].child_interface.expect("create_pool always has a static child interface");
                let host_id = objects.allocate_host_id();
                let signature = parent_interface.requests[opcode as usize].signature;
                // fd is borrowed, not moved -- see WaylandValue::Fd's own
                // doc comment: this recipe's OwnedFd must survive for any
                // later reconnect too, so encoding one replay message must
                // never take ownership away from it. The new compositor
                // gets its own independent copy via sendmsg, same as every
                // other fd-bearing message this proxy relays; ours stays
                // open afterward, still owned by this recipe.
                let values =
                    vec![wire::WaylandValue::NewId(host_id), wire::WaylandValue::Fd(fd.as_fd()), wire::WaylandValue::Int(*size)];
                let (payload, fds) = match wire::encode_arguments(signature, values) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("failed to encode create_pool for shm pool {guest_id}: {e}");
                        continue;
                    }
                };
                if let Err(e) = host.write_message(&wire::build_message(parent_host_id, opcode, &payload), &fds).await {
                    warn!("failed to recreate shm pool {guest_id}: {e}");
                    continue;
                }
                objects.map(guest_id, host_id, child_interface);
                info!("recreated wl_shm_pool (guest={guest_id}, host={host_id})");
            }
            Recreatable::ShmBuffer { pool_guest_id, offset, width, height, stride, format } => {
                let Some(parent_interface) = objects.interface(*pool_guest_id) else {
                    warn!("can't recreate shm buffer {guest_id}: pool {pool_guest_id} was never recreated");
                    continue;
                };
                let Some(parent_host_id) = objects.host_id(*pool_guest_id) else {
                    warn!("can't recreate shm buffer {guest_id}: pool {pool_guest_id} has no host id");
                    continue;
                };
                let Some(opcode) = request_opcode(parent_interface, "create_buffer") else {
                    warn!("can't recreate shm buffer {guest_id}: {} has no create_buffer request", parent_interface.name);
                    continue;
                };
                let child_interface =
                    parent_interface.requests[opcode as usize].child_interface.expect("create_buffer always has a static child interface");
                let host_id = objects.allocate_host_id();
                let signature = parent_interface.requests[opcode as usize].signature;
                let values = vec![
                    wire::WaylandValue::NewId(host_id),
                    wire::WaylandValue::Int(*offset),
                    wire::WaylandValue::Int(*width),
                    wire::WaylandValue::Int(*height),
                    wire::WaylandValue::Int(*stride),
                    wire::WaylandValue::Uint(*format),
                ];
                let (payload, fds) = match wire::encode_arguments(signature, values) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("failed to encode create_buffer for shm buffer {guest_id}: {e}");
                        continue;
                    }
                };
                if let Err(e) = host.write_message(&wire::build_message(parent_host_id, opcode, &payload), &fds).await {
                    warn!("failed to recreate shm buffer {guest_id}: {e}");
                    continue;
                }
                objects.map(guest_id, host_id, child_interface);
                info!("recreated wl_shm buffer (guest={guest_id}, host={host_id})");
            }
            Recreatable::DmabufBuffer { dmabuf_guest_id, width, height, format, flags, planes } => {
                let Some(dmabuf_interface) = objects.interface(*dmabuf_guest_id) else {
                    warn!("can't recreate dmabuf buffer {guest_id}: dmabuf global {dmabuf_guest_id} was never recreated");
                    continue;
                };
                let Some(dmabuf_host_id) = objects.host_id(*dmabuf_guest_id) else {
                    warn!("can't recreate dmabuf buffer {guest_id}: dmabuf global {dmabuf_guest_id} has no host id");
                    continue;
                };
                let Some(create_params_opcode) = request_opcode(dmabuf_interface, "create_params") else {
                    warn!("can't recreate dmabuf buffer {guest_id}: {} has no create_params request", dmabuf_interface.name);
                    continue;
                };
                let params_interface = dmabuf_interface.requests[create_params_opcode as usize]
                    .child_interface
                    .expect("create_params always has a static child interface");
                // Throwaway params object -- single-use by protocol
                // design, never tracked in the Shadow Table (see
                // DmabufBuffer's own doc comment in recreation.rs), just
                // a fresh host id to address the add()/create_immed()
                // sequence below to.
                let params_host_id = objects.allocate_host_id();
                let create_params_sig = dmabuf_interface.requests[create_params_opcode as usize].signature;
                let (create_params_payload, create_params_fds) =
                    match wire::encode_arguments(create_params_sig, vec![wire::WaylandValue::NewId(params_host_id)]) {
                        Ok(v) => v,
                        Err(e) => {
                            warn!("failed to encode create_params for dmabuf buffer {guest_id}: {e}");
                            continue;
                        }
                    };
                if let Err(e) = host
                    .write_message(&wire::build_message(dmabuf_host_id, create_params_opcode, &create_params_payload), &create_params_fds)
                    .await
                {
                    warn!("failed to recreate dmabuf buffer {guest_id}: create_params failed: {e}");
                    continue;
                }
                let Some(add_opcode) = request_opcode(params_interface, "add") else {
                    warn!("can't recreate dmabuf buffer {guest_id}: {} has no add request", params_interface.name);
                    continue;
                };
                let add_sig = params_interface.requests[add_opcode as usize].signature;
                let mut add_failed = false;
                for plane in planes {
                    // Wire order per zwp_linux_buffer_params_v1.add's real
                    // signature is (fd, plane_idx, offset, stride,
                    // modifier_hi, modifier_lo) -- fd occupies a values
                    // slot (so values.len() matches signature.len()) but
                    // no wire bytes, same as every other Fd argument.
                    let values = vec![
                        wire::WaylandValue::Fd(plane.fd.as_fd()),
                        wire::WaylandValue::Uint(plane.plane_idx),
                        wire::WaylandValue::Uint(plane.offset),
                        wire::WaylandValue::Uint(plane.stride),
                        wire::WaylandValue::Uint((plane.modifier >> 32) as u32),
                        wire::WaylandValue::Uint((plane.modifier & 0xFFFF_FFFF) as u32),
                    ];
                    let (add_payload, add_fds) = match wire::encode_arguments(add_sig, values) {
                        Ok(v) => v,
                        Err(e) => {
                            warn!("failed to encode add (plane {}) for dmabuf buffer {guest_id}: {e}", plane.plane_idx);
                            add_failed = true;
                            break;
                        }
                    };
                    if let Err(e) =
                        host.write_message(&wire::build_message(params_host_id, add_opcode, &add_payload), &add_fds).await
                    {
                        warn!("failed to recreate dmabuf buffer {guest_id}: add (plane {}) failed: {e}", plane.plane_idx);
                        add_failed = true;
                        break;
                    }
                }
                if add_failed {
                    continue;
                }
                let Some(create_immed_opcode) = request_opcode(params_interface, "create_immed") else {
                    warn!("can't recreate dmabuf buffer {guest_id}: {} has no create_immed request", params_interface.name);
                    continue;
                };
                let wl_buffer_interface = params_interface.requests[create_immed_opcode as usize]
                    .child_interface
                    .expect("create_immed always has a static child interface");
                let buffer_host_id = objects.allocate_host_id();
                let create_immed_sig = params_interface.requests[create_immed_opcode as usize].signature;
                let create_immed_values = vec![
                    wire::WaylandValue::NewId(buffer_host_id),
                    wire::WaylandValue::Int(*width),
                    wire::WaylandValue::Int(*height),
                    wire::WaylandValue::Uint(*format),
                    wire::WaylandValue::Uint(*flags),
                ];
                let (create_immed_payload, create_immed_fds) = match wire::encode_arguments(create_immed_sig, create_immed_values) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("failed to encode create_immed for dmabuf buffer {guest_id}: {e}");
                        continue;
                    }
                };
                if let Err(e) = host
                    .write_message(&wire::build_message(params_host_id, create_immed_opcode, &create_immed_payload), &create_immed_fds)
                    .await
                {
                    warn!("failed to recreate dmabuf buffer {guest_id}: create_immed failed: {e}");
                    continue;
                }
                objects.map(guest_id, buffer_host_id, wl_buffer_interface);
                info!("recreated dmabuf buffer (guest={guest_id}, host={buffer_host_id}, planes={})", planes.len());
            }
            Recreatable::SeatDevice { seat_guest_id, kind } => {
                let Some(seat_interface) = objects.interface(*seat_guest_id) else {
                    warn!("can't recreate {:?} {guest_id}: wl_seat {seat_guest_id} was never recreated", kind);
                    continue;
                };
                let Some(seat_host_id) = objects.host_id(*seat_guest_id) else {
                    warn!("can't recreate {:?} {guest_id}: wl_seat {seat_guest_id} has no host id", kind);
                    continue;
                };
                let Some(opcode) = request_opcode(seat_interface, kind.request_name()) else {
                    warn!("can't recreate {:?} {guest_id}: wl_seat has no {} request", kind, kind.request_name());
                    continue;
                };
                let child_interface = seat_interface.requests[opcode as usize]
                    .child_interface
                    .expect("get_pointer/get_keyboard/get_touch always have a static child interface");
                let host_id = objects.allocate_host_id();
                let signature = seat_interface.requests[opcode as usize].signature;
                let (payload, fds) = match wire::encode_arguments(signature, vec![wire::WaylandValue::NewId(host_id)]) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("failed to encode {} for {:?} {guest_id}: {e}", kind.request_name(), kind);
                        continue;
                    }
                };
                if let Err(e) = host.write_message(&wire::build_message(seat_host_id, opcode, &payload), &fds).await {
                    warn!("failed to recreate {:?} {guest_id}: {e}", kind);
                    continue;
                }
                objects.map(guest_id, host_id, child_interface);
                info!("recreated wl_seat {:?} (guest={guest_id}, host={host_id})", kind);
            }
            Recreatable::Viewport { viewporter_guest_id, surface_guest_id, destination } => {
                let Some(manager_interface) = objects.interface(*viewporter_guest_id) else {
                    warn!("can't recreate viewport {guest_id}: wp_viewporter {viewporter_guest_id} was never recreated");
                    continue;
                };
                let (Some(manager_host_id), Some(surface_host_id)) =
                    (objects.host_id(*viewporter_guest_id), objects.host_id(*surface_guest_id))
                else {
                    warn!("can't recreate viewport {guest_id}: manager or surface has no host id");
                    continue;
                };
                let Some(opcode) = request_opcode(manager_interface, "get_viewport") else {
                    warn!("can't recreate viewport {guest_id}: wp_viewporter has no get_viewport request");
                    continue;
                };
                let child_interface =
                    manager_interface.requests[opcode as usize].child_interface.expect("get_viewport always has a static child interface");
                let host_id = objects.allocate_host_id();
                let signature = manager_interface.requests[opcode as usize].signature;
                let values = vec![wire::WaylandValue::NewId(host_id), wire::WaylandValue::Object(surface_host_id)];
                let (payload, fds) = match wire::encode_arguments(signature, values) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("failed to encode get_viewport for viewport {guest_id}: {e}");
                        continue;
                    }
                };
                if let Err(e) = host.write_message(&wire::build_message(manager_host_id, opcode, &payload), &fds).await {
                    warn!("failed to recreate viewport {guest_id}: {e}");
                    continue;
                }
                objects.map(guest_id, host_id, child_interface);
                info!("recreated wp_viewport (guest={guest_id}, host={host_id})");

                if let Some((width, height)) = destination {
                    if let Some(set_destination_opcode) = request_opcode(child_interface, "set_destination") {
                        let sig = child_interface.requests[set_destination_opcode as usize].signature;
                        let values = vec![wire::WaylandValue::Int(*width), wire::WaylandValue::Int(*height)];
                        match wire::encode_arguments(sig, values) {
                            Ok((set_destination_payload, fds)) => {
                                if let Err(e) = host
                                    .write_message(&wire::build_message(host_id, set_destination_opcode, &set_destination_payload), &fds)
                                    .await
                                {
                                    warn!("failed to replay set_destination for viewport {guest_id}: {e}");
                                }
                            }
                            Err(e) => warn!("failed to encode set_destination for viewport {guest_id}: {e}"),
                        }
                    }
                }
            }
            Recreatable::FractionalScale { manager_guest_id, surface_guest_id } => {
                let Some(manager_interface) = objects.interface(*manager_guest_id) else {
                    warn!("can't recreate fractional scale {guest_id}: wp_fractional_scale_manager_v1 {manager_guest_id} was never recreated");
                    continue;
                };
                let (Some(manager_host_id), Some(surface_host_id)) =
                    (objects.host_id(*manager_guest_id), objects.host_id(*surface_guest_id))
                else {
                    warn!("can't recreate fractional scale {guest_id}: manager or surface has no host id");
                    continue;
                };
                let Some(opcode) = request_opcode(manager_interface, "get_fractional_scale") else {
                    warn!("can't recreate fractional scale {guest_id}: manager has no get_fractional_scale request");
                    continue;
                };
                let child_interface = manager_interface.requests[opcode as usize]
                    .child_interface
                    .expect("get_fractional_scale always has a static child interface");
                let host_id = objects.allocate_host_id();
                let signature = manager_interface.requests[opcode as usize].signature;
                let values = vec![wire::WaylandValue::NewId(host_id), wire::WaylandValue::Object(surface_host_id)];
                let (payload, fds) = match wire::encode_arguments(signature, values) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("failed to encode get_fractional_scale for fractional scale {guest_id}: {e}");
                        continue;
                    }
                };
                if let Err(e) = host.write_message(&wire::build_message(manager_host_id, opcode, &payload), &fds).await {
                    warn!("failed to recreate fractional scale {guest_id}: {e}");
                    continue;
                }
                objects.map(guest_id, host_id, child_interface);
                info!("recreated wp_fractional_scale_v1 (guest={guest_id}, host={host_id})");
            }
        }
    }

    // Buffer flow-control recovery (see buffer_flow.rs): recreating a
    // buffer's protocol identity above isn't enough on its own for a
    // buffer that was attached+committed right before the crash, since
    // the client is still waiting on a wl_buffer.release the old,
    // now-dead compositor never got to send. Must run AFTER the loop
    // above (needs each buffer's freshly-mapped host id to confirm it
    // actually got recreated, not just guessed at).
    for buffer_guest_id in buffer_flow.drain_in_flight() {
        let (Some(interface), Some(_host_id)) = (objects.interface(buffer_guest_id), objects.host_id(buffer_guest_id))
        else {
            // Never recreated (no recipe, a failed replay, or destroyed
            // before the crash) -- nothing live to tell the client about.
            continue;
        };
        let Some(release_opcode) = event_opcode(interface, "release") else { continue };
        if let Err(e) = gtk.write_message(&wire::build_message(buffer_guest_id, release_opcode, &[]), &[]).await {
            warn!("failed to synthesize wl_buffer.release for in-flight buffer {buffer_guest_id}: {e}");
        } else {
            info!("synthesized wl_buffer.release for in-flight buffer {buffer_guest_id} left over from the crash");
        }
    }

    // Pending-frame-callback recovery (see pending_frames.rs) -- a
    // frame() that reached the OLD compositor and was simply never
    // answered before it died. Unlike the buffer recovery above, a
    // wl_callback has no host-side identity to confirm (never part of
    // the recreation graph -- ephemeral, existing purely as a
    // ShadowTable mapping until delete_id) -- only need to confirm we
    // still know it at all (i.e. it wasn't already legitimately
    // destroyed via a real done+delete_id before the crash).
    for callback_guest_id in pending_frames.drain_awaiting_done() {
        if objects.interface(callback_guest_id).is_none() {
            continue;
        }
        synthesize_frame_done(gtk, objects, callback_guest_id).await;
        info!("synthesized wl_callback.done for frame callback {callback_guest_id} left over from the crash");
    }

    // required for clipboard copy: enables attempt_clipboard_splice on the
    // next real input serial this connection sees. Unconditional -- the
    // splice itself no-ops cheaply if the cache turns out to be empty.
    reclaim.data_device_manager_global = wl_data_device_manager_global;
    reclaim.pending = true;

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

/// required for clipboard copy: fires at most once per reconnect, on the
/// first real input serial this connection observes afterward (see the
/// call site in relay_ready_messages). Re-establishes the proxy as
/// clipboard owner using cached bytes from before the crash, by borrowing
/// that serial for `set_selection` -- confirmed live-viable in ADR-0009's
/// investigation (a fabricated serial gets cancelled outright; a real one,
/// even issued for something else, is accepted). The objects created here
/// (`wl_data_device_manager`/`wl_data_source`/`wl_data_device`) are purely
/// host-side: allocated via the same host-id counter as everything else
/// to avoid colliding with real traffic, but deliberately never mapped
/// into the shadow table -- the real client has no matching objects and
/// never should.
async fn attempt_clipboard_splice(
    host: &mut Conn,
    objects: &mut ShadowTable,
    reclaim: &mut clipboard::ReclaimState,
    cache: &clipboard::SharedClipboardCache,
    serial: u32,
) {
    reclaim.pending = false; // one attempt per reconnect, succeed or not

    let mime_types = cache.cached_mime_types();
    if mime_types.is_empty() {
        return; // nothing cached worth reclaiming
    }
    let Some((ddm_name, ddm_version)) = reclaim.data_device_manager_global else {
        warn!("clipboard splice: compositor never advertised wl_data_device_manager -- can't reclaim");
        return;
    };
    let (Some(registry_guest_id), Some(seat_guest_id)) = (
        objects.find_guest_id_by_interface_name("wl_registry"),
        objects.find_guest_id_by_interface_name("wl_seat"),
    ) else {
        warn!("clipboard splice: no wl_registry/wl_seat tracked -- can't reclaim");
        return;
    };
    let (Some(registry_host_id), Some(seat_host_id)) =
        (objects.host_id(registry_guest_id), objects.host_id(seat_guest_id))
    else {
        return;
    };

    let ddm_host_id = objects.allocate_host_id();
    let source_host_id = objects.allocate_host_id();
    let device_host_id = objects.allocate_host_id();

    let mut p = Vec::new();
    wire::put_u32(&mut p, ddm_name);
    wire::put_str(&mut p, "wl_data_device_manager");
    wire::put_u32(&mut p, ddm_version);
    wire::put_u32(&mut p, ddm_host_id);
    if let Err(e) = host.write_message(&wire::build_message(registry_host_id, 0, &p), &[]).await {
        warn!("clipboard splice: failed to bind wl_data_device_manager: {e}");
        return;
    }

    p.clear();
    wire::put_u32(&mut p, source_host_id);
    if let Err(e) = host.write_message(&wire::build_message(ddm_host_id, 0, &p), &[]).await {
        warn!("clipboard splice: failed to create_data_source: {e}");
        return;
    }

    for mime_type in &mime_types {
        p.clear();
        wire::put_str(&mut p, mime_type);
        if let Err(e) = host.write_message(&wire::build_message(source_host_id, 0, &p), &[]).await {
            warn!("clipboard splice: failed to offer {mime_type}: {e}");
            return;
        }
    }

    p.clear();
    wire::put_u32(&mut p, device_host_id);
    wire::put_u32(&mut p, seat_host_id);
    if let Err(e) = host.write_message(&wire::build_message(ddm_host_id, 1, &p), &[]).await {
        warn!("clipboard splice: failed to get_data_device: {e}");
        return;
    }

    p.clear();
    wire::put_u32(&mut p, source_host_id);
    wire::put_u32(&mut p, serial);
    if let Err(e) = host.write_message(&wire::build_message(device_host_id, 1, &p), &[]).await {
        warn!("clipboard splice: failed to set_selection: {e}");
        return;
    }

    info!("clipboard: attempting to reclaim {} cached mime type(s) using serial {serial}", mime_types.len());
    reclaim.active_source_host_id = Some(source_host_id);
}

/// required for clipboard copy: handles an event addressed to our own
/// synthetic clipboard data source (see attempt_clipboard_splice) --
/// intercepted before the normal guest-id-driven relay even runs, since
/// this object has no guest-side counterpart to resolve against.
async fn handle_synthetic_clipboard_event(
    src: &mut Conn,
    msg: &[u8],
    opcode: u16,
    cache: &clipboard::SharedClipboardCache,
    reclaim: &mut clipboard::ReclaimState,
) {
    use tokio::io::AsyncWriteExt;

    match opcode {
        1 => {
            // wl_data_source.send(mime_type, fd) -- Mutter's own
            // eager-fetch (or a real paste elsewhere) asking for the
            // content we claimed to offer.
            let payload = &msg[wire::HEADER_LEN..];
            let mime_type = wire::read_str(payload, 0).map(|(s, _)| s);
            let Some(fd) = src.read_fds.pop_front() else {
                warn!("clipboard splice: send() with no fd -- dropping");
                return;
            };
            let Some(mime_type) = mime_type else { return };
            let Some(bytes) = cache.get(&mime_type) else {
                warn!("clipboard splice: send() for uncached mime {mime_type:?}");
                return;
            };
            match tokio::net::unix::pipe::Sender::from_owned_fd(fd) {
                Ok(mut sender) => {
                    // Detached: writing the cached bytes has no bearing on
                    // whether relaying real traffic is safe to continue.
                    tokio::spawn(async move {
                        if let Err(e) = sender.write_all(&bytes).await {
                            warn!("clipboard splice: failed writing cached {mime_type} bytes: {e}");
                        }
                    });
                }
                Err(e) => warn!("clipboard splice: send()'s fd wasn't a writable pipe: {e}"),
            }
        }
        2 => {
            // cancelled -- superseded by a real copy elsewhere, expected
            // and not an error; nothing further will arrive for this id.
            info!("clipboard splice: reclaimed selection was superseded");
            reclaim.active_source_host_id = None;
        }
        _ => {} // target/dnd_drop_performed/dnd_finished/action: DnD-only, irrelevant here
    }
}

/// Drives one proxied connection to completion: relays messages in both
/// directions until the GTK client disconnects. If the compositor
/// connection drops instead, the connection freezes: the GTK-facing side
/// stays open and its requests are silently dropped (see
/// docs/implementation-constraints.md's "on server disconnect" rules)
/// rather than the whole session tearing down. While frozen, this
/// retries connecting to `compositor_socket_path` in the background; once
/// that succeeds, `recover_state_after_reconnect` re-fetches globals and
/// recreates tracked surfaces/toplevels, then `synthesize_grab_releases`
/// clears any stuck pointer/keyboard grab, before the connection
/// unfreezes and relaying resumes.
pub async fn run_connection(
    gtk_stream: UnixStream,
    compositor_stream: UnixStream,
    compositor_socket_path: std::path::PathBuf,
    client_pid: Option<i32>,
    // Process-wide, shared across every client's own run_connection --
    // required for clipboard copy to survive the copying client quitting,
    // not just a compositor crash while it's still running (see
    // docs/adr/adr-0009-clipboard-persistence.md). A per-connection cache
    // would die with this task, defeating that.
    clipboard_cache: clipboard::SharedClipboardCache,
) -> Result<()> {
    let mut gtk = Conn::new(gtk_stream);
    let mut host = Conn::new(compositor_stream);

    // wl_display (id 1 on both sides, by protocol convention) is
    // pre-seeded by ShadowTable::new() -- no discovery needed.
    let mut objects = ShadowTable::new();
    let mut graph = RecreationGraph::new();
    let mut grabs = GrabTracker::new();
    let mut pending_configure_acks: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    let mut buffer_flow = BufferFlowTracker::new();
    let mut pending_frames = PendingFrameTracker::new();
    let mut pending_dmabuf_planes: std::collections::HashMap<u32, (u32, Vec<DmabufPlane>)> = std::collections::HashMap::new();
    let mut reclaim = clipboard::ReclaimState::default();

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
                        relay_ready_messages(&mut gtk, dst, &mut objects, &mut graph, &mut grabs, &mut pending_configure_acks, &mut buffer_flow, &mut pending_frames, &mut pending_dmabuf_planes, &clipboard_cache, &mut reclaim, Direction::ClientToHost).await?;
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
                        relay_ready_messages(&mut host, Some(&mut gtk), &mut objects, &mut graph, &mut grabs, &mut pending_configure_acks, &mut buffer_flow, &mut pending_frames, &mut pending_dmabuf_planes, &clipboard_cache, &mut reclaim, Direction::HostToClient).await?;
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
                match recover_state_after_reconnect(&mut host, &mut gtk, &mut objects, &graph, &mut pending_configure_acks, &mut buffer_flow, &mut pending_frames, &mut reclaim).await {
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
                        // Detached: a slow/absent notification daemon must
                        // not delay relaying, which has already resumed.
                        tokio::spawn(notify::notify_recovered(client_pid));
                    }
                    Err(e) => {
                        // Every error this function can return -- a failed
                        // write, a failed read, or the compositor closing
                        // the connection outright while fetching its
                        // registry -- means the *connection itself* is
                        // dead, not just that recovery came up short (see
                        // that function's own doc comment on individual
                        // steps degrading gracefully instead of returning
                        // Err). Stay frozen rather than unfreezing here:
                        // resuming relaying on a connection already known
                        // to be broken lets real client traffic race into
                        // a brief unfrozen window and get silently
                        // dropped/misrouted, instead of safely buffered,
                        // before the next poll sees another EOF and
                        // re-freezes. The select loop's own
                        // `reconnect_with_backoff, if frozen` arm retries
                        // next iteration. The short sleep matches
                        // `reconnect_with_backoff`'s own 250ms between
                        // failed `connect()` attempts, avoiding a hot loop
                        // against a fresh compositor's socket file before
                        // it's finished unlinking/rebinding.
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
