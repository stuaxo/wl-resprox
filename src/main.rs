use anyhow::{Context, Result};
use std::collections::HashMap;
use std::env;
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::io::unix::AsyncFd;
use tokio::net::{UnixListener, UnixStream};
use tracing::{error, info, warn};

use wayland_backend::protocol::{Argument, ArgumentType, Interface, Message};
use wayland_backend::{client, server};

mod interfaces;
use interfaces::lookup_interface;

/// Wraps a borrowed fd just so we can hand it to `tokio::io::unix::AsyncFd`,
/// which needs an owned `T: AsRawFd`. We do NOT own the underlying fd here —
/// the corresponding wayland-backend `Backend` does, and closes it when
/// dropped. This type's `Drop` is a no-op, which is exactly what we want.
struct BorrowedRawFd(RawFd);
impl AsRawFd for BorrowedRawFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

/// Per-connection bridge state shared between the client-side backend
/// (talking to the real compositor) and the server-side backend (talking to
/// the GTK client). No ID *translation* happens here yet (Phase 4) — these
/// maps exist purely so we can bridge between wayland-backend's two
/// separate `ObjectId` type spaces (client::ObjectId vs server::ObjectId),
/// which are distinct Rust types even though, for a freshly-mirrored
/// connection like this, their protocol_id numbers happen to line up.
struct Bridge {
    server_handle: server::Handle,
    client_backend: client::Backend,
    client_id: server::ClientId,
    // client-side object's protocol_id -> mirrored server-side ObjectId
    c2s: Mutex<HashMap<u32, server::ObjectId>>,
    // server-side object's protocol_id -> mirrored client-side ObjectId
    s2c: Mutex<HashMap<u32, client::ObjectId>>,
}

/// Resolves the child interface (and version) for a message that creates a
/// new object. Most messages declare their child interface statically in
/// the protocol XML (`desc.child_interface`). The one well-known exception
/// is `wl_registry.bind`, whose wire signature is
/// `(name: uint, interface: string, version: uint, id: new_id)` — the
/// interface is a runtime string sitting right next to the new_id argument,
/// not something the protocol schema can pin down ahead of time.
fn resolve_child_interface(
    desc: &wayland_backend::protocol::MessageDesc,
    args: &[Argument<u32, RawFd>],
) -> Option<(&'static Interface, u32)> {
    if let Some(iface) = desc.child_interface {
        return Some((iface, iface.version));
    }
    // bind(): [Uint(name), Str(interface), Uint(version), NewId(id)]
    let mut interface_name = None;
    let mut version = None;
    for arg in args {
        match arg {
            Argument::Str(Some(s)) if interface_name.is_none() => {
                interface_name = s.to_str().ok();
            }
            Argument::Uint(v) if interface_name.is_some() && version.is_none() => {
                version = Some(*v);
            }
            _ => {}
        }
    }
    let iface = lookup_interface(interface_name?)?;
    Some((iface, version.unwrap_or(iface.version)))
}

/// Flattens an `Argument<Id, Fd>` down to its "shape" (id's protocol_id as a
/// plain u32, fd as a plain RawFd) so we can translate object references and
/// re-target them at whichever side we're relaying to, independent of which
/// concrete `ObjectId` type is involved.
fn flatten_args<Id: Clone, ToRaw: Fn(Id) -> u32>(
    args: &[Argument<Id, OwnedFd>],
    id_to_raw: ToRaw,
) -> Vec<Argument<u32, RawFd>> {
    args.iter()
        .map(|a| match a {
            Argument::Int(v) => Argument::Int(*v),
            Argument::Uint(v) => Argument::Uint(*v),
            Argument::Fixed(v) => Argument::Fixed(*v),
            Argument::Str(s) => Argument::Str(s.clone()),
            Argument::Array(v) => Argument::Array(v.clone()),
            Argument::Object(id) => Argument::Object(id_to_raw(id.clone())),
            Argument::NewId(id) => Argument::NewId(id_to_raw(id.clone())),
            // OwnedFd -> RawFd: hands ownership of the fd number to the
            // outgoing message. The destination backend takes it from here
            // (duplicating it onto the wire via SCM_RIGHTS and closing its
            // copy) -- this is the whole reason we're using wayland-backend
            // instead of a raw byte pipe, which drops ancillary fd data
            // entirely.
            Argument::Fd(fd) => {
                // SAFETY: we immediately hand this raw fd to the destination
                // backend's send_request/send_event, which takes ownership.
                let raw = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd.as_raw_fd()) };
                let dup = raw.try_clone_to_owned().expect("dup fd for relay");
                Argument::Fd(dup.into_raw_fd())
            }
        })
        .collect()
}

/// Rebuilds a translated argument list against the *destination* side's real
/// `ObjectId`s, given a lookup function from (flattened u32) -> concrete Id.
/// `new_id_slot` is filled with `Id::null()` as a placeholder when present —
/// per wayland-backend's contract, the caller (client::Backend::send_request)
/// allocates the real id and hands it back; server::Handle::create_object is
/// called *before* this to get a real id there instead. See relay_* below.
fn retarget_args<Id, Lookup: Fn(u32) -> Option<Id>>(
    flat: Vec<Argument<u32, RawFd>>,
    lookup: Lookup,
    null_id: Id,
) -> Option<Vec<Argument<Id, RawFd>>>
where
    Id: Clone,
{
    let mut out = Vec::with_capacity(flat.len());
    let mut null_id = Some(null_id);
    for a in flat {
        out.push(match a {
            Argument::Int(v) => Argument::Int(v),
            Argument::Uint(v) => Argument::Uint(v),
            Argument::Fixed(v) => Argument::Fixed(v),
            Argument::Str(s) => Argument::Str(s),
            Argument::Array(v) => Argument::Array(v),
            Argument::Fd(fd) => Argument::Fd(fd),
            Argument::Object(raw) => Argument::Object(lookup(raw)?),
            // Placeholder -- the real id gets allocated by the destination
            // side's own creation call, not by us.
            Argument::NewId(_) => Argument::NewId(null_id.take()?),
        });
    }
    Some(out)
}

// ---------------------------------------------------------------------
// Client side: objects living on the real-compositor connection. Events
// arriving here get relayed to the GTK-facing server side.
// ---------------------------------------------------------------------

struct ClientObjectProxy {
    bridge: Arc<Bridge>,
}

impl client::ObjectData for ClientObjectProxy {
    fn event(
        self: Arc<Self>,
        _backend: &client::Backend,
        msg: Message<client::ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn client::ObjectData>> {
        let bridge = &self.bridge;
        let interface = msg.sender_id.interface();
        let Some(desc) = interface.events.get(msg.opcode as usize) else {
            warn!(
                "unknown event opcode {} on {} -- dropping",
                msg.opcode, interface.name
            );
            return None;
        };

        let src_protocol_id = msg.sender_id.protocol_id();
        let flat = flatten_args(&msg.args, |id| id.protocol_id());

        let has_new_id = desc.signature.contains(&ArgumentType::NewId);
        let child = if has_new_id {
            resolve_child_interface(desc, &flat)
        } else {
            None
        };

        // Find the mirrored server-side object this event is FROM.
        let server_sender = {
            let map = bridge.c2s.lock().unwrap();
            match map.get(&src_protocol_id).cloned() {
                Some(id) => id,
                None => {
                    warn!(
                        "event on client object {} ({}) with no server-side mirror -- dropping",
                        src_protocol_id, interface.name
                    );
                    return None;
                }
            }
        };

        // If this event creates a new object, create its mirror on the
        // server side FIRST, so we have a real ObjectId to put in the
        // outgoing event (server-side creation requires this up front,
        // unlike client-side send_request which allocates for us).
        let new_server_obj = if let Some((iface, version)) = child {
            let data: Arc<dyn server::ObjectData<()>> =
                Arc::new(ServerObjectProxy { bridge: bridge.clone() });
            match bridge
                .server_handle
                .create_object::<()>(bridge.client_id.clone(), iface, version, data)
            {
                Ok(id) => Some(id),
                Err(e) => {
                    error!("failed to mirror new object on server side: {e}");
                    return None;
                }
            }
        } else {
            None
        };

        let args = {
            let map = bridge.c2s.lock().unwrap();
            retarget_args(
                flat,
                |raw| map.get(&raw).cloned(),
                server::ObjectId::null(),
            )
        };
        let Some(mut args) = args else {
            warn!("event referenced an object with no server-side mirror -- dropping");
            return None;
        };
        if let Some(ref new_id) = new_server_obj {
            for a in args.iter_mut() {
                if matches!(a, Argument::NewId(id) if id.is_null()) {
                    *a = Argument::NewId(new_id.clone());
                }
            }
        }

        let out_msg = Message {
            sender_id: server_sender,
            opcode: msg.opcode,
            args: args.into(),
        };
        if let Err(e) = bridge.server_handle.send_event(out_msg) {
            error!("failed to relay event to GTK client: {e}");
        }
        if let Some(new_id) = new_server_obj {
            bridge.s2c.lock().unwrap().insert(new_id.protocol_id(), {
                // The matching client-side object was already registered by
                // the backend when it decoded this event's NewId argument --
                // find it among msg.args.
                msg.args
                    .iter()
                    .find_map(|a| match a {
                        Argument::NewId(id) => Some(id.clone()),
                        _ => None,
                    })
                    .expect("event with NewId must carry one")
            });
            bridge
                .c2s
                .lock()
                .unwrap()
                .insert(src_protocol_id_of_new(&msg), new_id.clone());
        }

        // We only ever hand out fresh ObjectData for objects we just
        // mirrored server-side; wayland-backend wants *client*-side object
        // data returned here for the newly-created *client* object.
        if child.is_some() {
            Some(Arc::new(ClientObjectProxy { bridge: bridge.clone() }))
        } else {
            None
        }
    }

    fn destroyed(&self, object_id: client::ObjectId) {
        let protocol_id = object_id.protocol_id();
        if let Some(server_id) = self.bridge.c2s.lock().unwrap().remove(&protocol_id) {
            let _ = self
                .bridge
                .server_handle
                .destroy_object::<()>(&server_id);
        }
    }
}

fn src_protocol_id_of_new(msg: &Message<client::ObjectId, OwnedFd>) -> u32 {
    msg.args
        .iter()
        .find_map(|a| match a {
            Argument::NewId(id) => Some(id.protocol_id()),
            _ => None,
        })
        .expect("event with NewId must carry one")
}

// ---------------------------------------------------------------------
// Server side: objects living on the GTK-facing connection. Requests
// arriving here get relayed out to the real compositor via the client side.
// ---------------------------------------------------------------------

struct ServerObjectProxy {
    bridge: Arc<Bridge>,
}

impl server::ObjectData<()> for ServerObjectProxy {
    fn request(
        self: Arc<Self>,
        _handle: &server::Handle,
        _data: &mut (),
        _client_id: server::ClientId,
        msg: Message<server::ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn server::ObjectData<()>>> {
        let bridge = &self.bridge;
        let interface = msg.sender_id.interface();
        let Some(desc) = interface.requests.get(msg.opcode as usize) else {
            warn!(
                "unknown request opcode {} on {} -- dropping",
                msg.opcode, interface.name
            );
            return None;
        };

        let src_protocol_id = msg.sender_id.protocol_id();
        let flat = flatten_args(&msg.args, |id| id.protocol_id());

        let has_new_id = desc.signature.contains(&ArgumentType::NewId);
        let child = if has_new_id {
            resolve_child_interface(desc, &flat)
        } else {
            None
        };

        let client_sender = {
            let map = bridge.s2c.lock().unwrap();
            match map.get(&src_protocol_id).cloned() {
                Some(id) => id,
                None => {
                    warn!(
                        "request on server object {} ({}) with no client-side mirror -- dropping",
                        src_protocol_id, interface.name
                    );
                    return None;
                }
            }
        };

        let args = {
            let map = bridge.s2c.lock().unwrap();
            retarget_args(flat, |raw| map.get(&raw).cloned(), client::ObjectId::null())
        };
        let Some(args) = args else {
            warn!("request referenced an object with no client-side mirror -- dropping");
            return None;
        };

        let out_msg = Message {
            sender_id: client_sender,
            opcode: msg.opcode,
            args: args.into(),
        };

        let new_client_id = if let Some((iface, version)) = child {
            let data: Arc<dyn client::ObjectData> =
                Arc::new(ClientObjectProxy { bridge: bridge.clone() });
            match bridge
                .client_backend
                .send_request(out_msg, Some(data), Some((iface, version)))
            {
                Ok(id) => Some(id),
                Err(e) => {
                    error!("failed to relay request creating new object: {e}");
                    return None;
                }
            }
        } else {
            if let Err(e) = bridge.client_backend.send_request(out_msg, None, None) {
                error!("failed to relay request to compositor: {e}");
            }
            None
        };

        if let Some(new_client_id) = new_client_id {
            // The matching server-side object id was already allocated by
            // the backend (it's in msg.args, since GTK picked it on the
            // wire); find it and record the mirror both ways.
            let new_server_id = msg
                .args
                .iter()
                .find_map(|a| match a {
                    Argument::NewId(id) => Some(id.clone()),
                    _ => None,
                })
                .expect("request with NewId must carry one");
            bridge
                .s2c
                .lock()
                .unwrap()
                .insert(new_server_id.protocol_id(), new_client_id.clone());
            bridge
                .c2s
                .lock()
                .unwrap()
                .insert(new_client_id.protocol_id(), new_server_id);
            return Some(Arc::new(ServerObjectProxy { bridge: bridge.clone() }));
        }

        None
    }

    fn destroyed(
        self: Arc<Self>,
        _handle: &server::Handle,
        _data: &mut (),
        _client_id: server::ClientId,
        object_id: server::ObjectId,
    ) {
        let protocol_id = object_id.protocol_id();
        if let Some(client_id) = self.bridge.s2c.lock().unwrap().remove(&protocol_id) {
            let _ = self.bridge.client_backend.destroy_object(&client_id);
        }
    }
}

/// Handles events on the client-side `wl_registry` we obtain from the real
/// compositor -- specifically `global`, which we mirror onto the GTK-facing
/// server side via `create_global`. Every other object type goes through
/// the generic `ClientObjectProxy` above; the registry needs its own
/// handling because "advertise a global" isn't a normal object-to-object
/// relay, it's wayland-backend's own bootstrap mechanism.
struct RegistryObjectProxy {
    bridge: Arc<Bridge>,
}

impl client::ObjectData for RegistryObjectProxy {
    fn event(
        self: Arc<Self>,
        _backend: &client::Backend,
        msg: Message<client::ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn client::ObjectData>> {
        match msg.opcode {
            0 => {
                // global(name: uint, interface: string, version: uint)
                let (Some(Argument::Uint(name)), Some(Argument::Str(Some(iface_name))), Some(Argument::Uint(version))) =
                    (msg.args.first(), msg.args.get(1), msg.args.get(2))
                else {
                    warn!("malformed wl_registry.global event -- ignoring");
                    return None;
                };
                let Some(iface_str) = iface_name.to_str().ok() else {
                    return None;
                };
                let Some(iface) = lookup_interface(iface_str) else {
                    // Not (yet) in our interface table -- this global simply
                    // won't be visible to GTK. See src/interfaces.rs.
                    warn!("global '{iface_str}' has no known interface table -- not advertising it");
                    return None;
                };
                let handler: Arc<dyn server::GlobalHandler<()>> = Arc::new(GlobalHandlerRelay {
                    bridge: self.bridge.clone(),
                    registry_id: msg.sender_id.clone(),
                    name: *name,
                    interface: iface,
                });
                self.bridge
                    .server_handle
                    .create_global::<()>(iface, (*version).min(iface.version), handler);
            }
            1 => {
                // global_remove(name: uint) -- not handled yet (boilerplate
                // scope); a global that disappears from the real compositor
                // will just linger as far as GTK is concerned for now.
            }
            _ => {}
        }
        None
    }

    fn destroyed(&self, _object_id: client::ObjectId) {}
}

/// Bridges a GTK `bind` request for one specific global back to the real
/// compositor's registry, mirroring the resulting object on both sides.
struct GlobalHandlerRelay {
    bridge: Arc<Bridge>,
    registry_id: client::ObjectId,
    name: u32,
    interface: &'static Interface,
}

impl server::GlobalHandler<()> for GlobalHandlerRelay {
    fn bind(
        self: Arc<Self>,
        _handle: &server::Handle,
        _data: &mut (),
        _client_id: server::ClientId,
        _global_id: server::GlobalId,
        object_id: server::ObjectId,
    ) -> Arc<dyn server::ObjectData<()>> {
        let bind_msg = Message {
            sender_id: self.registry_id.clone(),
            opcode: 0, // wl_registry.bind
            args: vec![
                Argument::Uint(self.name),
                Argument::Str(Some(Box::new(
                    std::ffi::CString::new(self.interface.name).expect("interface name has no NUL"),
                ))),
                Argument::Uint(self.interface.version),
                Argument::NewId(client::ObjectId::null()),
            ]
            .into(),
        };
        let client_obj: Arc<dyn client::ObjectData> =
            Arc::new(ClientObjectProxy { bridge: self.bridge.clone() });
        match self.bridge.client_backend.send_request(
            bind_msg,
            Some(client_obj),
            Some((self.interface, self.interface.version)),
        ) {
            Ok(new_client_id) => {
                self.bridge
                    .s2c
                    .lock()
                    .unwrap()
                    .insert(object_id.protocol_id(), new_client_id.clone());
                self.bridge
                    .c2s
                    .lock()
                    .unwrap()
                    .insert(new_client_id.protocol_id(), object_id);
            }
            Err(e) => error!("failed to relay bind() for {}: {e}", self.interface.name),
        }
        Arc::new(ServerObjectProxy { bridge: self.bridge.clone() })
    }
}

struct DumbClientData;
impl server::ClientData for DumbClientData {}

/// Drives one proxied connection to completion: relays messages in both
/// directions until either side disconnects. No ID *translation* and no
/// crash-recovery logic live here yet -- this is purely the wayland-backend
/// based plumbing that Phase 4/5 will build on.
async fn run_connection(gtk_stream: UnixStream, compositor_stream: UnixStream) -> Result<()> {
    let gtk_std: StdUnixStream = gtk_stream.into_std()?;
    gtk_std.set_nonblocking(true)?;
    let compositor_std: StdUnixStream = compositor_stream.into_std()?;
    compositor_std.set_nonblocking(true)?;

    let client_backend =
        client::Backend::connect(compositor_std).context("connecting client backend")?;

    let mut server_backend = server::Backend::<()>::new().context("creating server backend")?;
    let mut server_handle = server_backend.handle();
    let client_id = server_handle
        .insert_client(gtk_std, Arc::new(DumbClientData))
        .context("registering GTK client")?;

    let bridge = Arc::new(Bridge {
        server_handle: server_handle.clone(),
        client_backend: client_backend.clone(),
        client_id: client_id.clone(),
        c2s: Mutex::new(HashMap::new()),
        s2c: Mutex::new(HashMap::new()),
    });

    // Bootstrap. Note there's deliberately no manual handling of `wl_display`
    // here: the sys backend manages the implicit display object (id 1)
    // internally on both sides -- `sync`/`get_registry` aren't retrievable
    // via the normal object APIs (object_for_protocol_id on id 1 returns
    // InvalidId; that was tried and confirmed empirically). Instead, we
    // bootstrap by acting as a real client: send our own `get_registry`
    // to the compositor, and mirror each `wl_registry.global` we learn
    // about as a `create_global` on the server side. wayland-backend's
    // GlobalHandler mechanism then handles the GTK-facing side of
    // registry/bind semantics for us.
    use wayland_client::protocol::__interfaces::WL_REGISTRY_INTERFACE;
    let get_registry_msg = Message {
        sender_id: client_backend.display_id(),
        opcode: 1, // wl_display.get_registry
        args: vec![Argument::NewId(client::ObjectId::null())].into(),
    };
    client_backend
        .send_request(
            get_registry_msg,
            Some(Arc::new(RegistryObjectProxy { bridge: bridge.clone() }) as Arc<dyn client::ObjectData>),
            Some((&WL_REGISTRY_INTERFACE, 1)),
        )
        .context("sending get_registry to compositor")?;
    // Without this, get_registry sits in wayland-backend's internal write
    // buffer indefinitely: nothing else in the bootstrap path touches the
    // client backend's fd, so the request never actually reaches the
    // compositor and no globals ever arrive (confirmed empirically -- GTK
    // saw zero interfaces before this was added).
    client_backend.flush().context("flushing get_registry")?;

    let server_async_fd = AsyncFd::new(BorrowedRawFd(server_backend.poll_fd().as_raw_fd()))?;
    let client_async_fd = AsyncFd::new(BorrowedRawFd(client_backend.poll_fd().as_raw_fd()))?;

    info!("Proxy session established: relaying GTK client <-> compositor");

    loop {
        tokio::select! {
            guard = server_async_fd.readable() => {
                let mut guard = guard?;
                match server_backend.dispatch_all_clients(&mut ()) {
                    Ok(_) => { guard.clear_ready(); }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => { guard.clear_ready(); }
                    Err(e) => {
                        info!("GTK client disconnected: {e}");
                        return Ok(());
                    }
                }
                if let Err(e) = server_handle.flush(None) {
                    warn!("flush to GTK client failed: {e}");
                }
            }
            guard = client_async_fd.readable() => {
                let mut guard = guard?;
                let dispatched = (|| -> Result<usize, wayland_backend::client::WaylandError> {
                    match client_backend.prepare_read() {
                        Some(read_guard) => read_guard.read(),
                        None => client_backend.dispatch_inner_queue(),
                    }
                })();
                match dispatched {
                    Ok(_) => { guard.clear_ready(); }
                    Err(wayland_backend::client::WaylandError::Io(ref e))
                        if e.kind() == std::io::ErrorKind::WouldBlock =>
                    {
                        guard.clear_ready();
                    }
                    Err(e) => {
                        info!("compositor connection lost: {e}");
                        return Ok(());
                    }
                }
                if let Err(e) = client_backend.flush() {
                    warn!("flush to compositor failed: {e}");
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    info!("Starting Wayland proxy (wayland-backend message relay)...");

    let runtime_dir =
        env::var("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR environment variable is not set")?;

    let target_display = env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-1".to_string());
    let target_socket_path = PathBuf::from(&runtime_dir).join(&target_display);

    let proxy_display = "wayland-proxy-0";
    let proxy_socket_path = PathBuf::from(&runtime_dir).join(proxy_display);

    if proxy_socket_path.exists() {
        std::fs::remove_file(&proxy_socket_path).context("removing stale proxy socket")?;
    }

    let listener = UnixListener::bind(&proxy_socket_path)
        .with_context(|| format!("binding to {}", proxy_socket_path.display()))?;

    info!("Proxy listening on: {}", proxy_socket_path.display());
    info!("Forwarding connections to: {}", target_socket_path.display());
    info!("To test, run: WAYLAND_DISPLAY={} gtk4-demo", proxy_display);

    loop {
        match listener.accept().await {
            Ok((gtk_stream, _addr)) => {
                info!("New Wayland client connected!");
                let target_path = target_socket_path.clone();
                tokio::spawn(async move {
                    match UnixStream::connect(&target_path).await {
                        Ok(compositor_stream) => {
                            if let Err(e) = run_connection(gtk_stream, compositor_stream).await {
                                error!("proxy session ended with error: {e:?}");
                            }
                        }
                        Err(e) => error!("failed to connect to compositor socket {target_path:?}: {e}"),
                    }
                });
            }
            Err(e) => error!("failed to accept incoming client connection: {e}"),
        }
    }
}
