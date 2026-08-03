//! Deterministic, in-process reproduction harness for the intermittent
//! "invalid arguments for wl_registry#2.bind" bug (see the 2026-07-30
//! entries in docs/debugging-notes.md).
//!
//! Both ends are "known good": a real `wayland-backend`-driven client
//! (the same crate our own proxy uses for its static protocol tables) and
//! a minimal `wayland-backend`-driven fake compositor -- neither is code
//! we're trying to validate, so if this reproduces the failure, it proves
//! the bug is in `run_connection` itself, not in labwc, GTK, or
//! wayland-info's specific behavior. No container, no GPU, no real
//! compositor needed.
//!
//! The driving client deliberately replicates the real trigger: after
//! collecting advertised globals, it binds *all* of them back-to-back with
//! no delay and an explicit `flush()` after each one, mirroring exactly
//! what `strace` showed GTK/wayland-info doing when this broke live.

use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wayland_backend::protocol::{Argument, Message};
use wayland_backend::{client, server};

/// A representative slice of interfaces a real compositor advertises --
/// enough volume to replicate a rapid-bind burst like GTK's startup.
const TEST_GLOBALS: &[&str] = &[
    "wl_compositor",
    "wl_subcompositor",
    "wl_shm",
    "wl_seat",
    "wl_output",
    "wl_data_device_manager",
    "xdg_wm_base",
    "wl_region",
    "wl_shm_pool",
    "wl_buffer",
    "wl_data_device",
    "wl_data_source",
    "wl_data_offer",
    "wl_pointer",
    "wl_keyboard",
    "wl_touch",
    "wl_subsurface",
    "wl_shell",
    "wl_shell_surface",
    "wl_fixes",
    "xdg_positioner",
    "xdg_surface",
    "xdg_toplevel",
    "xdg_popup",
]; // every interface our own interfaces.rs table knows, to match labwc's
   // real-world advertisement volume (~50 globals) as closely as we can
   // without inventing fake interface names.

#[derive(Default)]
struct ClientObserved {
    /// (name, interface, version) tuples from wl_registry.global events.
    globals: Vec<(u32, String, u32)>,
    binds_sent: usize,
    sync_done: bool,
    /// Set if wl_display.error fires -- this is what we're checking for.
    protocol_error: Option<String>,
    /// Set if the connection drops before sync completes, without an
    /// explicit wl_display.error (e.g. a hard reset).
    disconnected_early: bool,
}

// ---- fake compositor (server side) ----

struct FakeClientData;
impl server::ClientData for FakeClientData {}

struct FakeGlobalHandler;
impl server::GlobalHandler<()> for FakeGlobalHandler {
    fn bind(
        self: Arc<Self>,
        _handle: &server::Handle,
        _data: &mut (),
        _client_id: server::ClientId,
        _global_id: server::GlobalId,
        _object_id: server::ObjectId,
    ) -> Arc<dyn server::ObjectData<()>> {
        Arc::new(FakeObjectData)
    }
}

struct FakeObjectData;
impl server::ObjectData<()> for FakeObjectData {
    fn request(
        self: Arc<Self>,
        _handle: &server::Handle,
        _data: &mut (),
        _client_id: server::ClientId,
        _msg: Message<server::ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn server::ObjectData<()>>> {
        None
    }
    fn destroyed(
        self: Arc<Self>,
        _handle: &server::Handle,
        _data: &mut (),
        _client_id: server::ClientId,
        _object_id: server::ObjectId,
    ) {
    }
}

fn run_fake_compositor(stream: StdUnixStream, stop_after: Duration) {
    let mut backend = server::Backend::<()>::new().expect("fake compositor backend");
    let mut handle = backend.handle();
    handle
        .insert_client(stream, Arc::new(FakeClientData))
        .expect("insert fake client");
    for name in TEST_GLOBALS {
        let iface = wayland_proxy::interfaces::lookup_interface(name)
            .unwrap_or_else(|| panic!("test global {name} must be a known interface"));
        handle.create_global::<()>(iface, iface.version, Arc::new(FakeGlobalHandler));
    }

    let deadline = Instant::now() + stop_after;
    loop {
        match backend.dispatch_all_clients(&mut ()) {
            Ok(_) => {}
            Err(_) => break, // client (our proxy) disconnected
        }
        let _ = handle.flush(None);
        if Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

// ---- driving client ----

struct ClientRegistryData(Arc<Mutex<ClientObserved>>);
impl client::ObjectData for ClientRegistryData {
    fn event(
        self: Arc<Self>,
        _backend: &client::Backend,
        msg: Message<client::ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn client::ObjectData>> {
        if msg.opcode == 0 {
            // global(name: uint, interface: string, version: uint)
            if let [Argument::Uint(name), Argument::Str(Some(iface)), Argument::Uint(version)] = &msg.args[..] {
                let iface_str = iface.to_string_lossy().into_owned();
                self.0.lock().unwrap().globals.push((*name, iface_str, *version));
            }
        }
        None
    }
    fn destroyed(&self, _object_id: client::ObjectId) {}
}

struct ClientCallbackData(Arc<Mutex<ClientObserved>>);
impl client::ObjectData for ClientCallbackData {
    fn event(
        self: Arc<Self>,
        _backend: &client::Backend,
        _msg: Message<client::ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn client::ObjectData>> {
        self.0.lock().unwrap().sync_done = true;
        None
    }
    fn destroyed(&self, _object_id: client::ObjectId) {}
}

struct SinkObjectData;
impl client::ObjectData for SinkObjectData {
    fn event(
        self: Arc<Self>,
        _backend: &client::Backend,
        _msg: Message<client::ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn client::ObjectData>> {
        None
    }
    fn destroyed(&self, _object_id: client::ObjectId) {}
}

fn pump_client(backend: &client::Backend) {
    match backend.prepare_read() {
        Some(guard) => {
            let _ = guard.read();
        }
        None => {
            let _ = backend.dispatch_inner_queue();
        }
    }
}

/// Drives the client side to completion, mutating `observed` as events
/// arrive. Runs on its own OS thread (the low-level backend is
/// synchronous), independent of the tokio runtime driving `run_connection`.
fn run_driving_client(stream: StdUnixStream, observed: Arc<Mutex<ClientObserved>>) {
    use wayland_client::protocol::__interfaces::{WL_CALLBACK_INTERFACE, WL_REGISTRY_INTERFACE};

    let backend = match client::Backend::connect(stream) {
        Ok(b) => b,
        Err(e) => {
            observed.lock().unwrap().disconnected_early = true;
            eprintln!("client: failed to connect: {e}");
            return;
        }
    };
    // Deliberately not calling set_data on backend.display_id() here: the
    // sys backend's implicit display object (id 1) isn't retrievable via
    // the normal object-data APIs -- confirmed earlier (both here and in
    // the proxy's own now-abandoned wayland-backend implementation) that
    // this returns InvalidId. backend.last_error() below is the supported
    // way to detect a protocol-error-triggered disconnect instead.

    let registry_id = backend
        .send_request(
            Message {
                sender_id: backend.display_id(),
                opcode: 1, // wl_display.get_registry
                args: vec![Argument::NewId(client::ObjectId::null())].into(),
            },
            Some(Arc::new(ClientRegistryData(observed.clone())) as Arc<dyn client::ObjectData>),
            Some((&WL_REGISTRY_INTERFACE, 1)),
        )
        .expect("send get_registry");
    backend.flush().expect("flush get_registry");

    // Collect global advertisements for up to 500ms or until we've seen
    // them all.
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        pump_client(&backend);
        if observed.lock().unwrap().globals.len() >= TEST_GLOBALS.len() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    // The actual trigger: bind every advertised global back-to-back, no
    // delay, flushing after each one -- matching strace of the real
    // failure exactly (one sendmsg per bind, no pacing).
    let globals = observed.lock().unwrap().globals.clone();
    for (name, iface_name, version) in &globals {
        let Some(iface) = wayland_proxy::interfaces::lookup_interface(iface_name) else {
            continue;
        };
        let bind_result = backend.send_request(
            Message {
                sender_id: registry_id.clone(),
                opcode: 0, // wl_registry.bind
                args: vec![
                    Argument::Uint(*name),
                    Argument::Str(Some(Box::new(
                        std::ffi::CString::new(iface_name.as_str()).unwrap(),
                    ))),
                    Argument::Uint(*version),
                    Argument::NewId(client::ObjectId::null()),
                ]
                .into(),
            },
            Some(Arc::new(SinkObjectData) as Arc<dyn client::ObjectData>),
            Some((iface, *version)),
        );
        if bind_result.is_ok() {
            observed.lock().unwrap().binds_sent += 1;
        }
        if let Err(e) = backend.flush() {
            eprintln!("client: flush failed mid-burst: {e}");
            break;
        }
    }

    // Final sync -- if this never completes and no error/disconnect was
    // observed either, something else entirely is wrong, but the deadline
    // below still bounds the test.
    let _ = backend.send_request(
        Message {
            sender_id: backend.display_id(),
            opcode: 0, // wl_display.sync
            args: vec![Argument::NewId(client::ObjectId::null())].into(),
        },
        Some(Arc::new(ClientCallbackData(observed.clone())) as Arc<dyn client::ObjectData>),
        Some((&WL_CALLBACK_INTERFACE, 1)),
    );
    let _ = backend.flush();

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        {
            let o = observed.lock().unwrap();
            if o.sync_done || o.protocol_error.is_some() {
                return;
            }
        }
        if let Some(err) = backend.last_error() {
            let mut o = observed.lock().unwrap();
            o.disconnected_early = true;
            o.protocol_error = Some(err.to_string());
            eprintln!("client: backend reported error: {err}");
            return;
        }
        if Instant::now() > deadline {
            return;
        }
        pump_client(&backend);
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[tokio::test]
async fn proxy_survives_rapid_bind_burst_from_a_real_client() {
    // Real named Unix sockets via bind/listen/accept/connect, not an
    // anonymous socketpair() -- matching main.rs's actual production
    // plumbing exactly, in case socketpair() has different kernel-level
    // buffering behavior that masks something. TEST_GLOBALS also expanded
    // to every interface we know (23), not just 8, to better match labwc's
    // real ~50-global advertisement volume.
    let tmp_dir = std::env::temp_dir().join(format!("wayland-proxy-test-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("create temp dir");
    let proxy_socket_path = tmp_dir.join("proxy.sock");
    let host_socket_path = tmp_dir.join("host.sock");

    let _ = std::fs::remove_file(&host_socket_path); // clear any stale socket from a prior run
    let _ = std::fs::remove_file(&proxy_socket_path);

    let host_listener =
        std::os::unix::net::UnixListener::bind(&host_socket_path).expect("bind host socket");
    host_listener.set_nonblocking(true).expect("host listener nonblocking");
    std::thread::spawn(move || loop {
        match host_listener.accept() {
            Ok((stream, _)) => {
                run_fake_compositor(stream, Duration::from_secs(3));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) => panic!("host accept failed: {e}"),
        }
    });

    let proxy_listener =
        tokio::net::UnixListener::bind(&proxy_socket_path).expect("bind proxy socket");
    let host_socket_path_for_proxy = host_socket_path.clone();
    tokio::spawn(async move {
        let (gtk_stream, _) = proxy_listener.accept().await.expect("proxy accept");
        let compositor_stream = tokio::net::UnixStream::connect(&host_socket_path_for_proxy)
            .await
            .expect("proxy connect to host");
        if let Err(e) =
            wayland_proxy::run_connection(gtk_stream, compositor_stream, host_socket_path_for_proxy).await
        {
            eprintln!("run_connection ended with error: {e:?}");
        }
    });

    // Give the proxy a moment to start listening before the client connects.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client_std =
        StdUnixStream::connect(&proxy_socket_path).expect("client connect to proxy");
    client_std.set_nonblocking(true).expect("client set_nonblocking");
    let observed = Arc::new(Mutex::new(ClientObserved::default()));
    let observed_for_thread = observed.clone();
    let client_thread =
        std::thread::spawn(move || run_driving_client(client_std, observed_for_thread));

    // Bound the whole test even if the client thread hangs unexpectedly.
    for _ in 0..40 {
        if client_thread.is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let o = observed.lock().unwrap();
    assert_eq!(
        o.globals.len(),
        TEST_GLOBALS.len(),
        "client should have seen every advertised global relayed through the proxy"
    );
    assert_eq!(
        o.binds_sent,
        TEST_GLOBALS.len(),
        "client should have successfully sent a bind for every global"
    );
    assert!(
        o.protocol_error.is_none(),
        "compositor reported a protocol error during the bind burst: {:?}",
        o.protocol_error
    );
    assert!(
        !o.disconnected_early,
        "connection was lost before the final sync completed"
    );
    assert!(o.sync_done, "final sync never completed");
}

/// Builds a raw wire message by hand (see `wayland_proxy::wire`'s own doc
/// comment for the header layout) -- deliberately not using a real client
/// library here, unlike the test above, so the test can freely pick
/// object ids that a real client's own sequential allocator never would,
/// proving translation happened rather than merely coinciding.
use wayland_proxy::wire::build_message;

async fn read_one_message(stream: &mut tokio::net::UnixStream) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        if let Some((msg, _consumed)) = wayland_proxy::wire::take_message(&buf) {
            return msg.to_vec();
        }
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut tmp))
            .await
            .expect("timed out waiting for a message")
            .expect("read error");
        assert_ne!(n, 0, "connection closed before a full message arrived");
        buf.extend_from_slice(&tmp[..n]);
    }
}

/// Reads exactly `n` messages using one persistent buffer -- unlike calling
/// `read_one_message` `n` times, which starts a fresh buffer every call and
/// silently loses any message that arrived in the same underlying `read()`
/// as an earlier one (see `stale_wl_buffer_release_is_dropped_after_reconnect`'s
/// own comment on this -- the same trap, hit again writing the
/// xdg_toplevel.configure + xdg_surface.configure pair, which are written
/// back-to-back with no delay and reliably land in one read()).
async fn read_n_messages(stream: &mut tokio::net::UnixStream, n: usize) -> Vec<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        while out.len() < n {
            let Some((msg, consumed)) = wayland_proxy::wire::take_message(&buf) else { break };
            out.push(msg.to_vec());
            buf.drain(..consumed);
        }
        if out.len() >= n {
            break;
        }
        let read_n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut tmp))
            .await
            .expect("timed out waiting for a message")
            .expect("read error");
        assert_ne!(read_n, 0, "connection closed before all expected messages arrived");
        buf.extend_from_slice(&tmp[..read_n]);
    }
    out
}

/// Proves the Shadow Table's translation actually runs end-to-end through
/// `run_connection`'s real socket plumbing, not just in its own unit tests
/// (which necessarily test the lookup logic in isolation) or the bind-burst
/// test above (whose ids happen to coincide across the translation, since
/// both sides' allocators start at 2 and increment in lockstep for a real
/// client -- see the ShadowTable commit's message and
/// docs/architecture-notes.md for why that's an inherent limit of testing
/// this without reconnect logic yet). Bypassing a real client library and
/// hand-picking an object id no real allocator would produce next closes
/// that gap: any passing assertion here can only be explained by the
/// rewrite mechanism actually running, never by coincidence.
///
/// Covers new_id allocation (ClientToHost direction) and delete_id
/// (HostToClient direction) round-tripping back to the client's original
/// id. Object-typed argument rewriting (walk.object_offsets) shares the
/// exact same read-translate-write code path as new_id/delete_id here and
/// has its own dedicated unit coverage
/// (`walk_signature_locates_every_object_argument` in src/lib.rs's own
/// tests, plus the ShadowTable's `translates_in_both_directions_even_when_ids_differ`)
/// -- not re-proven with a second live-socket scenario here for that
/// reason, not because it's untested.
#[tokio::test]
async fn shadow_table_translates_new_id_and_delete_id_round_trip() {
    use tokio::io::AsyncWriteExt;

    let (gtk_proxy_side, mut gtk_test_side) = tokio::net::UnixStream::pair().expect("pair");
    let (host_proxy_side, mut host_test_side) = tokio::net::UnixStream::pair().expect("pair");

    tokio::spawn(async move {
        // Never used -- this test never triggers a freeze/reconnect --
        // but run_connection needs a path unconditionally.
        let unused_path = std::path::PathBuf::from("/nonexistent/unused-in-this-test");
        if let Err(e) = wayland_proxy::run_connection(gtk_proxy_side, host_proxy_side, unused_path).await {
            eprintln!("run_connection ended with error: {e:?}");
        }
    });

    // wl_display(1).get_registry(new_id=CLIENT_CHOSEN_ID) -- get_registry
    // specifically because its child interface (wl_registry) is resolved
    // statically by wayland-backend's own generated data, no dependency on
    // our own interfaces.rs lookup table.
    const CLIENT_CHOSEN_ID: u32 = 42;
    let get_registry = build_message(1, 1, &CLIENT_CHOSEN_ID.to_ne_bytes());
    gtk_test_side.write_all(&get_registry).await.expect("write get_registry");

    let received = read_one_message(&mut host_test_side).await;
    let header = wayland_proxy::wire::MessageHeader::parse(&received).expect("valid header");
    assert_eq!(header.sender_id, 1, "wl_display is always id 1 on both sides");
    assert_eq!(header.opcode, 1, "get_registry request opcode");
    let forwarded_new_id = u32::from_ne_bytes(received[8..12].try_into().unwrap());
    assert_eq!(
        forwarded_new_id, 2,
        "host should see its own freshly-allocated id (2, the first after wl_display's pre-seeded 1), \
         not the client's arbitrary original {CLIENT_CHOSEN_ID}"
    );

    // Host now deletes the object it knows as 2 -- the client should get
    // its own original 42 back, proving the reverse translation.
    let delete_id = build_message(1, 1, &2u32.to_ne_bytes());
    host_test_side.write_all(&delete_id).await.expect("write delete_id");

    let received = read_one_message(&mut gtk_test_side).await;
    let header = wayland_proxy::wire::MessageHeader::parse(&received).expect("valid header");
    assert_eq!(header.sender_id, 1);
    assert_eq!(header.opcode, 1, "delete_id event opcode");
    let forwarded_deleted_id = u32::from_ne_bytes(received[8..12].try_into().unwrap());
    assert_eq!(
        forwarded_deleted_id, CLIENT_CHOSEN_ID,
        "client should see its own original id back, not the host's internal 2"
    );
}

/// Found live 2026-08-03 via WAYLAND_DEBUG=1 against a real tilix (see
/// plan-desktop-resilience.md): `recover_state_after_reconnect` allocates
/// host id 3 for its own internal `wl_display.sync` (used only to detect
/// "all globals have arrived" -- never mapped to a guest id, the client
/// never learns this id exists at all). When the real compositor later
/// frees that sync callback and sends `wl_display.delete_id(3)`, the old
/// code logged "untracked host id -- ignoring" but still forwarded the
/// message with its host-space payload UNTRANSLATED -- telling the client
/// "your own guest-space id 3 is now free", except guest id 3 is whatever
/// unrelated (and very possibly still-live) object the client itself
/// happened to allocate third. This landed immediately before tilix
/// cleanly exited with no error output during a real crash test,
/// consistent with a corrupted client-side id table. This test proves the
/// general case directly (not tied to reconnect machinery): ANY
/// `wl_display.delete_id` for a host id the shadow table never mapped must
/// never reach the client, regardless of its payload value.
#[tokio::test]
async fn delete_id_for_an_untracked_host_id_is_dropped_not_forwarded() {
    use tokio::io::AsyncWriteExt;

    let (gtk_proxy_side, mut gtk_test_side) = tokio::net::UnixStream::pair().expect("pair");
    let (host_proxy_side, mut host_test_side) = tokio::net::UnixStream::pair().expect("pair");

    tokio::spawn(async move {
        let unused_path = std::path::PathBuf::from("/nonexistent/unused-in-this-test");
        if let Err(e) = wayland_proxy::run_connection(gtk_proxy_side, host_proxy_side, unused_path).await {
            eprintln!("run_connection ended with error: {e:?}");
        }
    });

    // The host frees an id the shadow table never mapped in the first
    // place (no bind/create/get_registry ever produced it) -- exactly the
    // condition `recover_state_after_reconnect`'s own internal sync
    // callback (host id 3) hits on every single reconnect.
    let delete_id = build_message(1, 1, &3u32.to_ne_bytes());
    host_test_side.write_all(&delete_id).await.expect("write delete_id");

    // Must never reach the client -- a timeout here is the expected,
    // passing outcome, same negative-assertion shape as the stale-buffer
    // tests.
    let result = tokio::time::timeout(Duration::from_millis(500), read_one_message(&mut gtk_test_side)).await;
    assert!(
        result.is_err(),
        "delete_id for a host id the shadow table never tracked must be dropped, not forwarded \
         with an untranslated (and potentially colliding) payload, but got: {result:?}"
    );
}

/// Found live 2026-08-03 via a real crash test against `wl-res-gnome-shell-direct`
/// (see plan-desktop-resilience.md): a real tilix survived the Wayland-level
/// reconnect cleanly, then a couple of seconds later hit libwayland-client's
/// own fatal `wl_abort` (confirmed via `coredumpctl`'s backtrace, through
/// `wl_closure_invoke`/`dispatch_event`) while dispatching an ordinary
/// `wl_surface.preferred_buffer_scale` event -- an event added in `wl_surface`
/// v6 that tilix's own (older) compiled listener had no slot for. Root
/// cause: `recover_state_after_reconnect` re-bound `wl_compositor` at
/// whatever version the *new* compositor's registry advertised, not the
/// version the client itself originally requested (and whose listener
/// structs it's actually prepared to handle) -- `recreation.rs`'s
/// `Recreatable::Global` didn't even record that. This test proves the fix:
/// a client that deliberately binds below the advertised maximum must be
/// re-bound at that SAME lower version after a reconnect, even though the
/// second life advertises a higher one.
#[tokio::test]
async fn reconnect_rebinds_globals_at_the_clients_originally_requested_version() {
    let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).try_init();
    use tokio::io::AsyncWriteExt;
    use wayland_proxy::wire;

    const GLOBALS: &[(&str, u32)] = &[("wl_compositor", 6)];
    const CLIENT_REQUESTED_VERSION: u32 = 3;

    let tmp_dir = std::env::temp_dir()
        .join(format!("wayland-proxy-version-mismatch-test-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("create temp dir");
    let host_socket_path = tmp_dir.join("host.sock");
    let _ = std::fs::remove_file(&host_socket_path);
    let host_listener = tokio::net::UnixListener::bind(&host_socket_path).expect("bind host");

    let (gtk_proxy_side, mut gtk_test_side) = tokio::net::UnixStream::pair().expect("pair");
    let first_host_conn = tokio::net::UnixStream::connect(&host_socket_path)
        .await
        .expect("initial host connect");
    let (first_host_accepted, _) = host_listener.accept().await.expect("accept first host conn");

    let host_socket_path_for_proxy = host_socket_path.clone();
    tokio::spawn(async move {
        if let Err(e) =
            wayland_proxy::run_connection(gtk_proxy_side, first_host_conn, host_socket_path_for_proxy)
                .await
        {
            eprintln!("run_connection ended with error: {e:?}");
        }
    });

    let (first_sink, _first_sink_rx) = tokio::sync::mpsc::unbounded_channel();
    let first_life_task = tokio::spawn(serve_fake_compositor_life(first_host_accepted, GLOBALS, 1, first_sink));

    let mut out = Vec::new();
    let mut p = Vec::new();
    wire::put_u32(&mut p, 2);
    out.extend(wire::build_message(1, 1, &p)); // get_registry
    let mut p = Vec::new();
    wire::put_u32(&mut p, 3);
    out.extend(wire::build_message(1, 0, &p)); // sync
    gtk_test_side.write_all(&out).await.expect("write get_registry+sync");

    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let mut wl_compositor_name = None;
    'collect: loop {
        use tokio::io::AsyncReadExt;
        let n = tokio::time::timeout(Duration::from_secs(3), gtk_test_side.read(&mut tmp))
            .await
            .expect("timed out collecting globals")
            .expect("read error");
        assert_ne!(n, 0);
        buf.extend_from_slice(&tmp[..n]);
        while let Some((msg, consumed)) = wire::take_message(&buf) {
            let header = wire::MessageHeader::parse(msg).unwrap();
            let payload = &msg[wire::HEADER_LEN..];
            if header.sender_id == 2 && header.opcode == 0 {
                if let (Some(name), Some((iface, _))) = (wire::read_u32(payload, 0), wire::read_str(payload, 4)) {
                    if iface == "wl_compositor" {
                        wl_compositor_name = Some(name);
                    }
                }
            } else if header.sender_id == 3 && header.opcode == 0 {
                buf.drain(..consumed);
                break 'collect;
            }
            buf.drain(..consumed);
        }
    }
    let wl_compositor_name = wl_compositor_name.expect("wl_compositor should have been advertised");

    // Deliberately bind BELOW the advertised maximum (6) -- exactly what a
    // real client does when its own compiled interface stub is older than
    // what the compositor supports.
    let mut p = Vec::new();
    wire::put_u32(&mut p, wl_compositor_name);
    wire::put_str(&mut p, "wl_compositor");
    wire::put_u32(&mut p, CLIENT_REQUESTED_VERSION);
    wire::put_u32(&mut p, 4);
    gtk_test_side.write_all(&wire::build_message(2, 0, &p)).await.expect("write bind");

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Crash. Second life advertises the SAME global at a HIGHER version (6)
    // than the client originally requested (3) -- exactly the scenario
    // that caused a real tilix's fatal wl_abort.
    first_life_task.abort();
    let (second_sink, mut second_sink_rx) = tokio::sync::mpsc::unbounded_channel();
    let host_listener_task = tokio::spawn(async move {
        let (second_host_accepted, _) =
            tokio::time::timeout(Duration::from_secs(3), host_listener.accept())
                .await
                .expect("proxy should reconnect within 3s")
                .expect("accept second host conn");
        serve_fake_compositor_life(second_host_accepted, GLOBALS, 101, second_sink).await;
    });

    let (_sender, opcode, payload) = tokio::time::timeout(Duration::from_secs(3), second_sink_rx.recv())
        .await
        .expect("timed out waiting for the recreated bind")
        .expect("sink closed early");
    assert_eq!(opcode, 0, "bind opcode");
    let iface = wire::read_str(&payload, 4).unwrap().0;
    assert_eq!(iface, "wl_compositor");
    let (_, next) = wire::read_str(&payload, 4).unwrap();
    let rebound_version = wire::read_u32(&payload, next).expect("version");
    assert_eq!(
        rebound_version, CLIENT_REQUESTED_VERSION,
        "recovery must re-bind at the version the client originally requested (3), not the new \
         compositor's higher advertised maximum (6) -- binding higher risks the compositor sending \
         events the client's own (older) compiled listener has no slot for, which is exactly what \
         made a real tilix hit libwayland-client's fatal wl_abort"
    );

    host_listener_task.abort();
}

/// Found live 2026-08-03 immediately after fixing the wl_abort/version bug
/// above (see plan-desktop-resilience.md): a real tilix then hit a
/// *different* fatal disconnect -- `wl_shm.create_pool sender has no
/// translation on the other side -- dropping` immediately followed by the
/// real compositor sending `wl_display.error(... "invalid object 19")` and
/// closing the connection outright. Root cause: `create_pool` carries a
/// `new_id` (it creates a `wl_shm_pool`), and the new_id-handling code
/// maps/allocates a host id for it *before* the later "sender has no
/// translation" check ever runs (`wl_shm` itself is outside the narrow
/// recreation graph, so it's stale after a reconnect, same as `wl_buffer`).
/// The message describing the new object never reached the host, but the
/// shadow table was left believing it existed there anyway -- a phantom
/// mapping. A later request against that same (still-live, per the shadow
/// table) guest id then got happily translated to the phantom host id and
/// forwarded, which the real compositor correctly rejected as invalid,
/// killing the whole connection. This test proves the fix: after a
/// create_pool attempt on a stale wl_shm is dropped, a follow-up request
/// against the guest id it *would* have created must also be dropped, not
/// forwarded with a phantom host id.
#[tokio::test]
async fn create_pool_on_a_stale_wl_shm_does_not_leave_a_phantom_mapping() {
    let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).try_init();
    use tokio::io::AsyncWriteExt;
    use wayland_proxy::wire;

    const GLOBALS: &[(&str, u32)] = &[("wl_shm", 1)];

    let tmp_dir =
        std::env::temp_dir().join(format!("wayland-proxy-phantom-mapping-test-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("create temp dir");
    let host_socket_path = tmp_dir.join("host.sock");
    let _ = std::fs::remove_file(&host_socket_path);
    let host_listener = tokio::net::UnixListener::bind(&host_socket_path).expect("bind host");

    let (gtk_proxy_side, mut gtk_test_side) = tokio::net::UnixStream::pair().expect("pair");
    let first_host_conn = tokio::net::UnixStream::connect(&host_socket_path)
        .await
        .expect("initial host connect");
    let (first_host_accepted, _) = host_listener.accept().await.expect("accept first host conn");

    let host_socket_path_for_proxy = host_socket_path.clone();
    tokio::spawn(async move {
        if let Err(e) =
            wayland_proxy::run_connection(gtk_proxy_side, first_host_conn, host_socket_path_for_proxy)
                .await
        {
            eprintln!("run_connection ended with error: {e:?}");
        }
    });

    let (first_sink, mut first_sink_rx) = tokio::sync::mpsc::unbounded_channel();
    let first_life_task = tokio::spawn(serve_fake_compositor_life(first_host_accepted, GLOBALS, 1, first_sink));

    // get_registry(2), sync(3), then bind wl_shm(name=1) -> guest id 4.
    let mut out = Vec::new();
    let mut p = Vec::new();
    wire::put_u32(&mut p, 2);
    out.extend(wire::build_message(1, 1, &p));
    let mut p = Vec::new();
    wire::put_u32(&mut p, 3);
    out.extend(wire::build_message(1, 0, &p));
    gtk_test_side.write_all(&out).await.expect("write get_registry+sync");
    read_n_messages(&mut gtk_test_side, 2).await;

    let mut p = Vec::new();
    wire::put_u32(&mut p, 1); // name
    wire::put_str(&mut p, "wl_shm");
    wire::put_u32(&mut p, 1); // version
    wire::put_u32(&mut p, 4); // new_id (guest) -- wl_shm
    gtk_test_side.write_all(&wire::build_message(2, 0, &p)).await.expect("write bind");

    let _ = tokio::time::timeout(Duration::from_secs(3), first_sink_rx.recv())
        .await
        .expect("timed out waiting for the bind to reach the first life")
        .expect("sink closed early");

    // Crash and reconnect. Second life advertises nothing -- wl_shm is
    // deliberately not part of the recreation graph, so it stays stale.
    first_life_task.abort();
    let (second_sink, mut second_sink_rx) = tokio::sync::mpsc::unbounded_channel();
    let host_listener_task = tokio::spawn(async move {
        let (second_host_accepted, _) =
            tokio::time::timeout(Duration::from_secs(3), host_listener.accept())
                .await
                .expect("proxy should reconnect within 3s")
                .expect("accept second host conn");
        serve_fake_compositor_life(second_host_accepted, &[], 1, second_sink).await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // wl_shm(4).create_pool(new_id=5, fd, size) -- the fd itself is never
    // actually sent via SCM_RIGHTS here (this test only needs to prove the
    // shadow table's own bookkeeping, not real fd passing); the proxy logs
    // a harmless "declares a fd argument that never arrived" warning and
    // proceeds structurally unaffected either way.
    let mut p = Vec::new();
    wire::put_u32(&mut p, 5); // new_id (guest) -- would-be wl_shm_pool
    wire::put_u32(&mut p, 4096); // size
    gtk_test_side.write_all(&wire::build_message(4, 0, &p)).await.expect("write create_pool");

    let forwarded = tokio::time::timeout(Duration::from_millis(300), second_sink_rx.recv()).await;
    assert!(forwarded.is_err(), "create_pool on a stale wl_shm must never reach the new compositor, but got: {forwarded:?}");

    // The actual proof: a follow-up request against guest id 5 (the
    // would-be wl_shm_pool create_pool tried to create) must ALSO be
    // dropped as untranslatable -- not forwarded using a phantom host id
    // the shadow table wrongly believes exists.
    gtk_test_side
        .write_all(&wire::build_message(5, 1, &[])) // wl_shm_pool(5).destroy()
        .await
        .expect("write destroy on the would-be pool");

    let forwarded = tokio::time::timeout(Duration::from_millis(300), second_sink_rx.recv()).await;
    assert!(
        forwarded.is_err(),
        "a request against the guest id create_pool would have allocated must never be forwarded -- \
         the shadow table must not retain a phantom mapping for an object the host was never told to \
         create, but got: {forwarded:?}"
    );

    host_listener_task.abort();
}

/// Found live 2026-08-03 (see plan-desktop-resilience.md): a real
/// gtk4-demo caught mid-render at the exact moment of a crash never
/// redrew again, even though its surface/xdg_toplevel otherwise recovered
/// fully seconds later. Root cause: `wl_surface.frame` registers a
/// promise the compositor must keep -- deliver `wl_callback.done` when
/// it's a good time to draw again -- which GTK's own frame clock blocks
/// on. `gtk.fill()` in `run_connection`'s select loop has no `if !frozen`
/// guard (unlike `host.fill()`), so client requests keep getting read and
/// processed the whole time the connection is frozen, including during
/// the window between `bump_generation()` (which runs synchronously,
/// immediately on reconnect, before `recover_state_after_reconnect` even
/// starts) and the client's own surface actually being remapped by it. A
/// `frame()` request landing in exactly that window hits the same
/// "sender has no translation" path as a permanently-stale object like
/// `wl_buffer` -- except a `wl_surface` isn't permanently stale, it's
/// just not remapped *yet*, and the old code dropped the request without
/// ever answering the promise it represented, stalling the client's
/// frame clock forever. This test proves the fix: synthesize
/// `wl_callback.done` (+ the `delete_id` a one-shot callback object is
/// owed) instead of silently dropping it.
#[tokio::test]
async fn frame_request_during_the_recovery_window_gets_a_synthesized_done() {
    let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).try_init();
    use tokio::io::AsyncWriteExt;
    use wayland_proxy::wire;

    const GLOBALS: &[(&str, u32)] = &[("wl_compositor", 6), ("xdg_wm_base", 6)];

    let tmp_dir =
        std::env::temp_dir().join(format!("wayland-proxy-frame-during-recovery-test-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("create temp dir");
    let host_socket_path = tmp_dir.join("host.sock");
    let _ = std::fs::remove_file(&host_socket_path);
    let host_listener = tokio::net::UnixListener::bind(&host_socket_path).expect("bind host");

    let (gtk_proxy_side, mut gtk_test_side) = tokio::net::UnixStream::pair().expect("pair");
    let first_host_conn = tokio::net::UnixStream::connect(&host_socket_path)
        .await
        .expect("initial host connect");
    let (first_host_accepted, _) = host_listener.accept().await.expect("accept first host conn");

    let host_socket_path_for_proxy = host_socket_path.clone();
    tokio::spawn(async move {
        if let Err(e) =
            wayland_proxy::run_connection(gtk_proxy_side, first_host_conn, host_socket_path_for_proxy)
                .await
        {
            eprintln!("run_connection ended with error: {e:?}");
        }
    });

    let (first_sink, _first_sink_rx) = tokio::sync::mpsc::unbounded_channel();
    let first_life_task = tokio::spawn(serve_fake_compositor_life(first_host_accepted, GLOBALS, 1, first_sink));

    // get_registry(2), sync(3), bind wl_compositor->4, create_surface->6 --
    // same guest-id scheme as the other full-chain reconnect tests, but
    // this test only needs the surface itself (wl_surface.frame is a
    // request on the surface directly), not the full xdg_surface/toplevel
    // chain.
    let mut out = Vec::new();
    let mut p = Vec::new();
    wire::put_u32(&mut p, 2);
    out.extend(wire::build_message(1, 1, &p));
    let mut p = Vec::new();
    wire::put_u32(&mut p, 3);
    out.extend(wire::build_message(1, 0, &p));
    gtk_test_side.write_all(&out).await.expect("write get_registry+sync");
    read_n_messages(&mut gtk_test_side, 2).await; // global(wl_compositor) + callback.done

    let mut p = Vec::new();
    wire::put_u32(&mut p, 1); // name
    wire::put_str(&mut p, "wl_compositor");
    wire::put_u32(&mut p, 6);
    wire::put_u32(&mut p, 4); // new_id (guest) -- wl_compositor
    out.clear();
    out.extend(wire::build_message(2, 0, &p));
    let mut p = Vec::new();
    wire::put_u32(&mut p, 6); // new_id (guest) -- wl_surface
    out.extend(wire::build_message(4, 0, &p)); // wl_compositor(4).create_surface -> wl_surface(6)
    gtk_test_side.write_all(&out).await.expect("write bind+create_surface");

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Crash. Reaching the actual bug requires reproducing exactly how
    // `run_connection`'s select loop schedules things (see this test's
    // own module-level doc comment): while `recover_state_after_reconnect`
    // is itself running (as the BODY of the `reconnect_with_backoff, if
    // frozen` arm), the select loop is not back at its own top level, so
    // `gtk.fill()` -- despite having no `if !frozen` guard -- isn't being
    // polled at all during that specific window. It's the GAP *between*
    // reconnect attempts (back at the top-level select!, frozen still
    // true, a fresh `reconnect_with_backoff(...)` racing against
    // `gtk.fill()`) where a client message can win and get processed
    // against now-stale (bump_generation() already ran once) but
    // not-yet-remapped objects. So: force the FIRST reconnect attempt to
    // fail partway (accept, then drop without responding, so
    // recover_state_after_reconnect's very first write fails) and close
    // the listener entirely for a moment, guaranteeing
    // reconnect_with_backoff's next connect() attempt genuinely fails and
    // backs off -- a real ~250ms window, not a hopeful race.
    first_life_task.abort();
    let (second_host_accepted, _) =
        tokio::time::timeout(Duration::from_secs(3), host_listener.accept())
            .await
            .expect("proxy should reconnect within 3s")
            .expect("accept second host conn");
    drop(second_host_accepted); // closed with nothing written -- recovery's first write fails
    drop(host_listener);
    std::fs::remove_file(&host_socket_path).expect("remove socket file so the next connect() genuinely fails");

    // Wait for the proxy to notice the failed attempt and go back to
    // retrying -- confirmed via its own log line from the frozen-recovery
    // fix earlier tonight, but here just give it a moment rather than
    // parsing logs. Comfortably longer than reconnect_with_backoff's own
    // fixed 250ms retry interval (see reconnect_with_backoff's source),
    // so this reliably lands in the gap between attempts even under
    // parallel test-suite load rather than racing it.
    tokio::time::sleep(Duration::from_millis(400)).await;

    // Now, while genuinely back at the top-level select loop (frozen,
    // reconnect_with_backoff mid-backoff since nothing is listening yet),
    // the client sends wl_surface(6).frame(new_id=50) -- exactly what a
    // real gtk4-demo mid-render does every frame. Guest id 6 is already
    // stale (bump_generation() ran when the first, failed reconnection
    // was accepted) but not yet remapped (recovery hasn't succeeded yet).
    let mut p = Vec::new();
    wire::put_u32(&mut p, 50); // new_id (guest) -- the wl_callback
    gtk_test_side.write_all(&wire::build_message(6, 3, &p)).await.expect("write frame()"); // opcode 3 = wl_surface.frame
    tokio::time::sleep(Duration::from_millis(150)).await; // let the select loop actually pick it up

    // Now provide the real, working host connection.
    let host_listener = tokio::net::UnixListener::bind(&host_socket_path).expect("rebind host");
    // serve_fake_compositor_life handles get_registry/sync/globals
    // automatically and forwards anything else it receives to this sink --
    // the clean way to prove frame() specifically never got forwarded,
    // without confusing it for recovery's own (expected) traffic on the
    // same connection.
    let (second_sink, mut second_sink_rx) = tokio::sync::mpsc::unbounded_channel();
    let host_listener_task = tokio::spawn(async move {
        let (second_host_accepted, _) =
            tokio::time::timeout(Duration::from_secs(3), host_listener.accept())
                .await
                .expect("proxy should reconnect within 3s")
                .expect("accept second host conn");
        serve_fake_compositor_life(second_host_accepted, GLOBALS, 101, second_sink).await;
    });

    // The client must get wl_callback.done for guest id 50, then
    // wl_display.delete_id(50) -- the promise wl_surface.frame made, even
    // though the real request never reached any compositor.
    let mut synthesized = read_n_messages(&mut gtk_test_side, 2).await.into_iter();
    let done = synthesized.next().unwrap();
    let done_header = wire::MessageHeader::parse(&done).expect("valid header");
    assert_eq!(done_header.sender_id, 50, "done should be sent from the callback's own guest id");
    assert_eq!(done_header.opcode, 0, "wl_callback.done event opcode");

    let delete_id = synthesized.next().unwrap();
    let delete_id_header = wire::MessageHeader::parse(&delete_id).expect("valid header");
    assert_eq!(delete_id_header.sender_id, 1, "delete_id is always sent from wl_display");
    assert_eq!(delete_id_header.opcode, 1, "delete_id event opcode");
    let deleted_id = wire::read_u32(&delete_id[wire::HEADER_LEN..], 0).expect("deleted id");
    assert_eq!(deleted_id, 50, "should free the callback's own guest id, matching a real compositor's one-shot lifecycle");

    // Recovery replays exactly 2 requests against this life: bind
    // (wl_compositor) and create_surface -- if the dropped frame() had
    // instead been forwarded, it would show up here as an unexpected
    // extra message.
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(3), second_sink_rx.recv())
            .await
            .expect("timed out waiting for a recreation request")
            .expect("sink closed early");
    }
    let unexpected = tokio::time::timeout(Duration::from_millis(300), second_sink_rx.recv()).await;
    assert!(
        unexpected.is_err(),
        "frame() on a stale-generation surface must never be forwarded, but the fake compositor \
         received something extra: {unexpected:?}"
    );

    host_listener_task.abort();
}

/// Proves just the reconnect *trigger* mechanism (`reconnect_with_backoff`
/// wired into `run_connection`'s select loop): a dropped compositor
/// connection freezes as before, and once the compositor socket accepts a
/// new connection again, the proxy actually notices and resumes relaying
/// on it -- rather than staying frozen forever, which was the case before
/// this landed.
///
/// Deliberately doesn't assert anything about *state* surviving the
/// reconnect (globals, surfaces, etc.) -- that's not wired in yet (see
/// `run_connection`'s doc comment); this only proves the socket-level
/// mechanism, which the fuller reconnect tests (once state recovery
/// lands) will build on rather than duplicate.
#[tokio::test]
async fn proxy_reconnects_to_a_restarted_compositor() {
    use tokio::io::AsyncWriteExt;

    let tmp_dir = std::env::temp_dir()
        .join(format!("wayland-proxy-reconnect-test-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("create temp dir");
    let host_socket_path = tmp_dir.join("host.sock");
    let _ = std::fs::remove_file(&host_socket_path);

    // One listener plays both compositor "lives" -- accept once (the
    // original connection), drop it (the crash), then accept again (the
    // restarted compositor) on the same still-listening socket.
    let host_listener = tokio::net::UnixListener::bind(&host_socket_path).expect("bind host");

    let (gtk_proxy_side, mut gtk_test_side) = tokio::net::UnixStream::pair().expect("pair");
    let first_host_conn = tokio::net::UnixStream::connect(&host_socket_path)
        .await
        .expect("initial host connect");
    let (first_host_accepted, _) = host_listener.accept().await.expect("accept first host conn");

    let host_socket_path_for_proxy = host_socket_path.clone();
    tokio::spawn(async move {
        if let Err(e) =
            wayland_proxy::run_connection(gtk_proxy_side, first_host_conn, host_socket_path_for_proxy)
                .await
        {
            eprintln!("run_connection ended with error: {e:?}");
        }
    });

    // Simulate the crash: drop the proxy's live host connection from the
    // "compositor" side, forcing an EOF on the proxy's read.
    drop(first_host_accepted);

    // The proxy should now be retrying reconnect_with_backoff (250ms
    // fixed backoff) -- accept its next attempt.
    let (mut second_host_accepted, _) =
        tokio::time::timeout(Duration::from_secs(3), host_listener.accept())
            .await
            .expect("proxy should reconnect within 3s")
            .expect("accept second host conn");

    // Prove relaying actually resumed on the NEW connection: wl_display(1).sync(new_id=99),
    // sent from the client now, should reach this second accepted stream.
    let probe = build_message(1, 0, &99u32.to_ne_bytes());
    gtk_test_side.write_all(&probe).await.expect("write probe");

    let received = tokio::time::timeout(Duration::from_secs(3), read_one_message(&mut second_host_accepted))
        .await
        .expect("should receive the post-reconnect message");

    let header = wayland_proxy::wire::MessageHeader::parse(&received).expect("valid header");
    assert_eq!(header.sender_id, 1, "wl_display is always id 1 on both sides");
    assert_eq!(header.opcode, 0, "sync request opcode");
}

/// Serves one compositor "life" on `stream`: answers `get_registry`+`sync`
/// with `globals` (using name numbers starting at `first_name` --
/// deliberately different across the two lives in
/// `full_reconnect_recreates_surface_chain_and_synthesizes_configure`
/// below, to prove nothing assumes global names persist across a
/// reconnect), then forwards every subsequent complete message it
/// receives into `sink` until the connection closes.
async fn serve_fake_compositor_life(
    mut stream: tokio::net::UnixStream,
    globals: &[(&str, u32)],
    first_name: u32,
    sink: tokio::sync::mpsc::UnboundedSender<(u32, u16, Vec<u8>)>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use wayland_proxy::wire;

    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let mut registry_id = None;
    loop {
        while let Some((msg, consumed)) = wire::take_message(&buf) {
            let header = wire::MessageHeader::parse(msg).expect("valid header");
            let payload = msg[wire::HEADER_LEN..].to_vec();
            if header.sender_id == 1 && header.opcode == 1 {
                registry_id = wire::read_u32(&payload, 0); // get_registry(new_id)
            } else if header.sender_id == 1 && header.opcode == 0 {
                // sync(new_id) -- respond with every global, then done.
                if let (Some(sync_id), Some(reg_id)) = (wire::read_u32(&payload, 0), registry_id) {
                    let mut out = Vec::new();
                    for (i, (iface, version)) in globals.iter().enumerate() {
                        let mut p = Vec::new();
                        wire::put_u32(&mut p, first_name + i as u32);
                        wire::put_str(&mut p, iface);
                        wire::put_u32(&mut p, *version);
                        out.extend(wire::build_message(reg_id, 0, &p));
                    }
                    let mut done_payload = Vec::new();
                    wire::put_u32(&mut done_payload, 0);
                    out.extend(wire::build_message(sync_id, 0, &done_payload));
                    let _ = stream.write_all(&out).await;
                }
            } else {
                let _ = sink.send((header.sender_id, header.opcode, payload));
            }
            buf.drain(..consumed);
        }
        match stream.read(&mut tmp).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
}

/// The full "On Server Reconnect" sequence, end to end: a hand-crafted
/// client binds `wl_compositor`+`xdg_wm_base` and creates a
/// surface -> xdg_surface -> xdg_toplevel chain against the first
/// compositor "life", which then crashes. A second life (different global
/// *names*, proving nothing assumes persistence) accepts the proxy's
/// reconnect, and this test verifies: the proxy re-fetches the registry,
/// re-binds both globals, replays the full chain with fresh host ids, and
/// synthesizes `xdg_surface.configure` to the client on the *original*
/// (unchanged) guest id.
#[tokio::test]
async fn full_reconnect_recreates_surface_chain_and_synthesizes_configure() {
    let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).try_init();
    use tokio::io::AsyncWriteExt;
    use wayland_proxy::wire;

    const GLOBALS: &[(&str, u32)] = &[("wl_compositor", 6), ("xdg_wm_base", 6)];

    let tmp_dir =
        std::env::temp_dir().join(format!("wayland-proxy-full-reconnect-test-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("create temp dir");
    let host_socket_path = tmp_dir.join("host.sock");
    let _ = std::fs::remove_file(&host_socket_path);
    let host_listener = tokio::net::UnixListener::bind(&host_socket_path).expect("bind host");

    let (gtk_proxy_side, mut gtk_test_side) = tokio::net::UnixStream::pair().expect("pair");
    let first_host_conn = tokio::net::UnixStream::connect(&host_socket_path)
        .await
        .expect("initial host connect");
    let (first_host_accepted, _) = host_listener.accept().await.expect("accept first host conn");

    let host_socket_path_for_proxy = host_socket_path.clone();
    tokio::spawn(async move {
        if let Err(e) =
            wayland_proxy::run_connection(gtk_proxy_side, first_host_conn, host_socket_path_for_proxy)
                .await
        {
            eprintln!("run_connection ended with error: {e:?}");
        }
    });

    // First life: names 1,2 (wl_compositor=1, xdg_wm_base=2). Its own
    // subsequent messages (the client's binds/creates) are irrelevant here
    // -- only the second life's are asserted on -- so its sink is dropped.
    let (first_sink, _first_sink_rx) = tokio::sync::mpsc::unbounded_channel();
    let first_life_task = tokio::spawn(serve_fake_compositor_life(first_host_accepted, GLOBALS, 1, first_sink));

    // Client: get_registry(2), sync(3), then -- once globals are known --
    // bind wl_compositor(4)/xdg_wm_base(5), create_surface->6,
    // get_xdg_surface->7, get_toplevel->8. Sent as two writes since the
    // bind names depend on reading the first life's global events back.
    let mut out = Vec::new();
    let mut p = Vec::new();
    wire::put_u32(&mut p, 2);
    out.extend(wire::build_message(1, 1, &p)); // get_registry
    let mut p = Vec::new();
    wire::put_u32(&mut p, 3);
    out.extend(wire::build_message(1, 0, &p)); // sync
    gtk_test_side.write_all(&out).await.expect("write get_registry+sync");

    // Collect first life's globals (names 1=wl_compositor, 2=xdg_wm_base --
    // known deterministically since GLOBALS/first_name are fixed above,
    // but read them back for real rather than hardcoding to prove the
    // client-side path works too).
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let mut wl_compositor_name = None;
    let mut xdg_wm_base_name = None;
    'collect: loop {
        use tokio::io::AsyncReadExt;
        let n = tokio::time::timeout(Duration::from_secs(3), gtk_test_side.read(&mut tmp))
            .await
            .expect("timed out collecting globals")
            .expect("read error");
        assert_ne!(n, 0);
        buf.extend_from_slice(&tmp[..n]);
        while let Some((msg, consumed)) = wire::take_message(&buf) {
            let header = wire::MessageHeader::parse(msg).unwrap();
            let payload = &msg[wire::HEADER_LEN..];
            if header.sender_id == 2 && header.opcode == 0 {
                if let (Some(name), Some((iface, _))) = (wire::read_u32(payload, 0), wire::read_str(payload, 4)) {
                    match iface.as_str() {
                        "wl_compositor" => wl_compositor_name = Some(name),
                        "xdg_wm_base" => xdg_wm_base_name = Some(name),
                        _ => {}
                    }
                }
            } else if header.sender_id == 3 && header.opcode == 0 {
                let consumed_len = consumed;
                buf.drain(..consumed_len);
                break 'collect;
            }
            let consumed_len = consumed;
            buf.drain(..consumed_len);
        }
    }
    let wl_compositor_name = wl_compositor_name.expect("wl_compositor should have been advertised");
    let xdg_wm_base_name = xdg_wm_base_name.expect("xdg_wm_base should have been advertised");

    let mut out = Vec::new();
    let mut p = Vec::new();
    wire::put_u32(&mut p, wl_compositor_name);
    wire::put_str(&mut p, "wl_compositor");
    wire::put_u32(&mut p, 6);
    wire::put_u32(&mut p, 4);
    out.extend(wire::build_message(2, 0, &p)); // wl_registry(2).bind -> wl_compositor(4)
    let mut p = Vec::new();
    wire::put_u32(&mut p, xdg_wm_base_name);
    wire::put_str(&mut p, "xdg_wm_base");
    wire::put_u32(&mut p, 6);
    wire::put_u32(&mut p, 5);
    out.extend(wire::build_message(2, 0, &p)); // wl_registry(2).bind -> xdg_wm_base(5)
    let mut p = Vec::new();
    wire::put_u32(&mut p, 6);
    out.extend(wire::build_message(4, 0, &p)); // wl_compositor(4).create_surface -> wl_surface(6)
    let mut p = Vec::new();
    wire::put_u32(&mut p, 7);
    wire::put_u32(&mut p, 6);
    out.extend(wire::build_message(5, 2, &p)); // xdg_wm_base(5).get_xdg_surface(surface=6) -> xdg_surface(7)
    let mut p = Vec::new();
    wire::put_u32(&mut p, 8);
    out.extend(wire::build_message(7, 1, &p)); // xdg_surface(7).get_toplevel -> xdg_toplevel(8)
    gtk_test_side.write_all(&out).await.expect("write bind+create chain");

    // Give the proxy a moment to process and forward the chain to the
    // (unobserved) first life before crashing it.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Crash: abort the first life's task, dropping its stream and closing
    // the connection -- forcing an EOF on the proxy's read, same as a real
    // compositor process dying. Second life uses different global names
    // (101, 102), proving nothing assumes persistence.
    first_life_task.abort();
    let (second_sink, mut second_sink_rx) = tokio::sync::mpsc::unbounded_channel();
    let host_listener_task = tokio::spawn(async move {
        let (second_host_accepted, _) =
            tokio::time::timeout(Duration::from_secs(3), host_listener.accept())
                .await
                .expect("proxy should reconnect within 3s")
                .expect("accept second host conn");
        serve_fake_compositor_life(second_host_accepted, GLOBALS, 101, second_sink).await;
    });

    // Recovery replays exactly 5 requests against the second life, in
    // order: bind(wl_compositor), bind(xdg_wm_base), create_surface,
    // get_xdg_surface, get_toplevel (get_registry/sync are answered
    // inline by serve_fake_compositor_life, never reaching the sink).
    let mut observed = Vec::new();
    for _ in 0..5 {
        let (sender, opcode, payload) = tokio::time::timeout(Duration::from_secs(3), second_sink_rx.recv())
            .await
            .expect("timed out waiting for a recreation request")
            .expect("sink closed early");
        observed.push((sender, opcode, payload));
    }

    // A bind REQUEST's own sender_id is the object it's invoked ON (the
    // registry) -- always the same for both binds -- not the object it
    // creates. That new id is the payload's own trailing new_id field
    // (after name, the interface string, and version), read out via
    // read_str's returned next-offset the same way real dispatch code
    // would (see walk_signature/SignatureWalk in src/lib.rs).
    fn bind_new_id(payload: &[u8]) -> u32 {
        let (_, next) = wire::read_str(payload, 4).expect("interface string");
        wire::read_u32(payload, next + 4).expect("new_id") // next+4 skips version
    }

    // bind(wl_compositor) using the SECOND life's name (101), not the first's.
    let name = wire::read_u32(&observed[0].2, 0).unwrap();
    let iface = wire::read_str(&observed[0].2, 4).unwrap().0;
    assert_eq!(observed[0].1, 0, "bind opcode");
    assert_eq!(iface, "wl_compositor");
    assert_eq!(name, 101, "should use the second life's global name, not the first life's");

    let name = wire::read_u32(&observed[1].2, 0).unwrap();
    let iface = wire::read_str(&observed[1].2, 4).unwrap().0;
    assert_eq!(iface, "xdg_wm_base");
    assert_eq!(name, 102);

    let recreated_compositor_host_id = bind_new_id(&observed[0].2);
    let recreated_wm_base_host_id = bind_new_id(&observed[1].2);

    // create_surface, sent to the freshly re-bound wl_compositor.
    assert_eq!(observed[2].0, recreated_compositor_host_id);
    assert_eq!(observed[2].1, 0, "create_surface opcode");
    let recreated_surface_host_id = wire::read_u32(&observed[2].2, 0).unwrap();

    // get_xdg_surface, sent to the freshly re-bound xdg_wm_base,
    // referencing the freshly recreated surface's NEW host id -- not its
    // stale first-life one.
    assert_eq!(observed[3].0, recreated_wm_base_host_id);
    assert_eq!(observed[3].1, 2, "get_xdg_surface opcode");
    let surface_arg = wire::read_u32(&observed[3].2, 4).unwrap();
    assert_eq!(
        surface_arg, recreated_surface_host_id,
        "get_xdg_surface should reference the surface's freshly-recreated host id"
    );
    let recreated_xdg_surface_host_id = wire::read_u32(&observed[3].2, 0).unwrap();

    // get_toplevel, sent to the freshly recreated xdg_surface.
    assert_eq!(observed[4].0, recreated_xdg_surface_host_id);
    assert_eq!(observed[4].1, 1, "get_toplevel opcode");

    // The client should first receive a synthesized xdg_toplevel.configure
    // on guest id 8 -- its ORIGINAL xdg_toplevel id, unchanged by the
    // reconnect -- width=0/height=0 (the protocol's "you decide" convention)
    // and an empty states array, the buffer-reallocation-forcing signal
    // added 2026-08-03 (see plan-desktop-resilience.md). Then the
    // synthesized xdg_surface.configure on guest id 7. Both are written
    // back-to-back with no delay and reliably land in one underlying
    // read(), so read_n_messages (not two read_one_message calls) is
    // required here -- see its doc comment.
    let mut synthesized = read_n_messages(&mut gtk_test_side, 2).await.into_iter();
    let toplevel_configure = synthesized.next().unwrap();
    let header = wire::MessageHeader::parse(&toplevel_configure).expect("valid header");
    assert_eq!(header.sender_id, 8, "toplevel configure should target the client's original xdg_toplevel guest id");
    assert_eq!(header.opcode, 0, "xdg_toplevel.configure event opcode");
    let toplevel_payload = &toplevel_configure[wire::HEADER_LEN..];
    assert_eq!(wire::read_u32(toplevel_payload, 0), Some(0), "width should be 0 (no suggested size)");
    assert_eq!(wire::read_u32(toplevel_payload, 4), Some(0), "height should be 0 (no suggested size)");
    assert_eq!(wire::read_u32(toplevel_payload, 8), Some(0), "states array should be empty");

    let configure = synthesized.next().unwrap();
    let header = wire::MessageHeader::parse(&configure).expect("valid header");
    assert_eq!(header.sender_id, 7, "configure should target the client's original xdg_surface guest id");
    assert_eq!(header.opcode, 0, "xdg_surface.configure event opcode");
    let synthetic_serial = wire::read_u32(&configure[wire::HEADER_LEN..], 0).expect("configure serial");

    // The client acks it exactly as a real client would -- this is where
    // labwc rejected the invented serial live ("wrong configure serial"),
    // see the 2026-07-30 entry in docs/debugging-notes.md. The proxy must
    // swallow this ack rather than forward it to the second life, which
    // never issued that serial itself.
    let mut ack_payload = Vec::new();
    wire::put_u32(&mut ack_payload, synthetic_serial);
    gtk_test_side
        .write_all(&wire::build_message(7, 4, &ack_payload))
        .await
        .expect("write ack_configure");

    // Proof of swallowing: a real request sent right after must be the
    // very next thing the second life observes -- if the ack had been
    // forwarded, it would show up first. wl_display.sync is answered
    // inline by serve_fake_compositor_life rather than reaching the sink
    // (see its own get_registry/sync special-casing), so use another
    // create_surface instead, which the sink does observe.
    let mut p = Vec::new();
    wire::put_u32(&mut p, 50);
    gtk_test_side.write_all(&wire::build_message(4, 0, &p)).await.expect("write create_surface"); // wl_compositor(4).create_surface -> 50

    let (sender, opcode, _payload) = tokio::time::timeout(Duration::from_secs(3), second_sink_rx.recv())
        .await
        .expect("timed out waiting for the post-ack create_surface")
        .expect("sink closed early");
    assert_eq!(
        sender, recreated_compositor_host_id,
        "ack_configure must not reach the second life -- the create_surface should be next"
    );
    assert_eq!(opcode, 0, "create_surface opcode");

    host_listener_task.abort();
}

/// implementation-constraints.md's "Buffer Lifetimes" rule: a
/// `wl_buffer.release` for a buffer the new compositor doesn't know about
/// must never reach the client. `wl_buffer` is deliberately not part of
/// the recreation graph, so its Shadow Table mapping never gets refreshed
/// on reconnect -- this proves the generation check that guards against a
/// *stale* mapping (see `ShadowTable::generation`'s doc comment: the new
/// compositor's own server-side id allocator restarts from the same
/// `0xff000000` baseline the old one used, so a stale host id can
/// numerically coincide with something unrelated).
///
/// Binds directly to "wl_buffer" as a top-level global via
/// `wl_registry.bind` -- not how a real client would ever obtain one (the
/// real path is `wl_shm.create_pool` + `wl_shm_pool.create_buffer`, which
/// needs a real fd) -- deliberately, to isolate the generation-check
/// mechanism itself without unrelated fd-passing setup.
#[tokio::test]
async fn stale_wl_buffer_release_is_dropped_after_reconnect() {
    let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).try_init();
    use tokio::io::AsyncWriteExt;
    use wayland_proxy::wire;

    const GLOBALS: &[(&str, u32)] = &[("wl_buffer", 1)];

    let tmp_dir =
        std::env::temp_dir().join(format!("wayland-proxy-stale-buffer-test-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("create temp dir");
    let host_socket_path = tmp_dir.join("host.sock");
    let _ = std::fs::remove_file(&host_socket_path);
    let host_listener = tokio::net::UnixListener::bind(&host_socket_path).expect("bind host");

    let (gtk_proxy_side, mut gtk_test_side) = tokio::net::UnixStream::pair().expect("pair");
    let first_host_conn = tokio::net::UnixStream::connect(&host_socket_path)
        .await
        .expect("initial host connect");
    let (first_host_accepted, _) = host_listener.accept().await.expect("accept first host conn");

    let host_socket_path_for_proxy = host_socket_path.clone();
    tokio::spawn(async move {
        if let Err(e) =
            wayland_proxy::run_connection(gtk_proxy_side, first_host_conn, host_socket_path_for_proxy)
                .await
        {
            eprintln!("run_connection ended with error: {e:?}");
        }
    });

    let (first_sink, mut first_sink_rx) = tokio::sync::mpsc::unbounded_channel();
    let first_life_task = tokio::spawn(serve_fake_compositor_life(first_host_accepted, GLOBALS, 1, first_sink));

    // get_registry(2), sync(3), then bind wl_buffer(name=1) -> guest id 4.
    let mut out = Vec::new();
    let mut p = Vec::new();
    wire::put_u32(&mut p, 2);
    out.extend(wire::build_message(1, 1, &p));
    let mut p = Vec::new();
    wire::put_u32(&mut p, 3);
    out.extend(wire::build_message(1, 0, &p));
    gtk_test_side.write_all(&out).await.expect("write get_registry+sync");

    // Drain exactly 2 messages (global(wl_buffer) + callback.done) using
    // one persistent buffer -- read_one_message starts a fresh buffer on
    // every call, so calling it twice in a row can silently drop a second
    // message that arrived in the same underlying read() as the first;
    // this must not leave anything unread before the final timeout
    // assertion below, or that assertion would trivially "pass" by reading
    // this leftover backlog instead of proving the stale release was
    // actually dropped.
    {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let mut drained = 0;
        while drained < 2 {
            while let Some((_msg, consumed)) = wire::take_message(&buf) {
                buf.drain(..consumed);
                drained += 1;
            }
            if drained >= 2 {
                break;
            }
            let n = tokio::time::timeout(Duration::from_secs(3), gtk_test_side.read(&mut tmp))
                .await
                .expect("timed out draining global/done")
                .expect("read error");
            assert_ne!(n, 0);
            buf.extend_from_slice(&tmp[..n]);
        }
        assert!(buf.is_empty(), "leftover bytes after draining exactly 2 messages");
    }

    let mut p = Vec::new();
    wire::put_u32(&mut p, 1); // name
    wire::put_str(&mut p, "wl_buffer");
    wire::put_u32(&mut p, 1); // version
    wire::put_u32(&mut p, 4); // new_id (guest)
    gtk_test_side.write_all(&wire::build_message(2, 0, &p)).await.expect("write bind");

    // Observe the bind as forwarded to the first life, to learn the host id
    // our own allocator assigned wl_buffer -- that's the id a stale release
    // must target for this test to mean anything.
    let (sender, opcode, payload) = tokio::time::timeout(Duration::from_secs(3), first_sink_rx.recv())
        .await
        .expect("timed out waiting for the bind to reach the first life")
        .expect("sink closed early");
    assert_eq!(opcode, 0, "bind opcode");
    let (_, next) = wire::read_str(&payload, 4).unwrap();
    let stale_buffer_host_id = wire::read_u32(&payload, next + 4).unwrap();
    let _ = sender; // the registry's host id, not needed further

    // Crash and reconnect. The second life is driven manually (not via
    // serve_fake_compositor_life) since this test needs to inject one
    // specific extra event -- the stale release -- after answering
    // get_registry/sync, which that helper has no hook for.
    first_life_task.abort();
    let (mut second_host_accepted, _) =
        tokio::time::timeout(Duration::from_secs(3), host_listener.accept())
            .await
            .expect("proxy should reconnect within 3s")
            .expect("accept second host conn");

    // Answer recovery's get_registry+sync with no globals at all (nothing
    // to recreate -- this test doesn't exercise wl_compositor/xdg_wm_base).
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let mut registry_host_id = None;
    'answer: loop {
        use tokio::io::{AsyncReadExt, AsyncWriteExt as _};
        let n = tokio::time::timeout(Duration::from_secs(3), second_host_accepted.read(&mut tmp))
            .await
            .expect("timed out waiting for recovery's get_registry/sync")
            .expect("read error");
        assert_ne!(n, 0);
        buf.extend_from_slice(&tmp[..n]);
        while let Some((msg, consumed)) = wire::take_message(&buf) {
            let header = wire::MessageHeader::parse(msg).unwrap();
            let payload = msg[wire::HEADER_LEN..].to_vec();
            if header.sender_id == 1 && header.opcode == 1 {
                registry_host_id = wire::read_u32(&payload, 0);
            } else if header.sender_id == 1 && header.opcode == 0 {
                if let Some(sync_id) = wire::read_u32(&payload, 0) {
                    let mut done_payload = Vec::new();
                    wire::put_u32(&mut done_payload, 0);
                    second_host_accepted
                        .write_all(&wire::build_message(sync_id, 0, &done_payload))
                        .await
                        .expect("write callback.done");
                    buf.drain(..consumed);
                    break 'answer;
                }
            }
            buf.drain(..consumed);
        }
    }
    let _ = registry_host_id;

    // Give recovery a moment to finish processing the (empty) globals and
    // unfreeze before sending the stale release.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // As the second life, send a release for the STALE host id -- exactly
    // the scenario a numeric coincidence with a freshly-allocated object
    // on the new compositor would produce.
    let release = wire::build_message(stale_buffer_host_id, 0, &[]); // wl_buffer.release()
    second_host_accepted.write_all(&release).await.expect("write stale release");

    // The client must never see it -- assert no message arrives within a
    // reasonable window (a timeout here is the *expected*, passing outcome).
    let result = tokio::time::timeout(Duration::from_millis(500), read_one_message(&mut gtk_test_side)).await;
    assert!(
        result.is_err(),
        "client should never receive a release for a pre-reconnect (stale-generation) buffer, but got: {result:?}"
    );
}

/// Found live 2026-08-03 (see the 2026-08-03 entries in
/// plan-desktop-resilience.md): a client holding a buffer across a real
/// crash and trying to reuse it afterward (`wl_surface.attach`, not just
/// `wl_buffer.release` as in `stale_wl_buffer_release_is_dropped_after_reconnect`
/// above -- the *client*-to-host direction this time, not host-to-client)
/// got its attach silently dropped, same generation-check mechanism, but
/// then the *real* compositor sent back a fatal protocol error
/// (`invalid arguments for wl_surface#N.frame`) and killed the client --
/// because the surface's own recreation left it with no buffer at all,
/// not because anything forwarded stale data. This test proves the
/// proxy's own half of that: the stale attach must never reach the new
/// compositor. It doesn't (and can't, without a real compositor in the
/// loop) prove a real client survives -- that needs the planned
/// `xdg_toplevel.configure` strengthening on top, verified against a real
/// client separately.
#[tokio::test]
async fn stale_wl_buffer_attach_is_dropped_not_forwarded_after_reconnect() {
    let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).try_init();
    use tokio::io::AsyncWriteExt;
    use wayland_proxy::wire;

    const GLOBALS: &[(&str, u32)] = &[("wl_compositor", 6), ("xdg_wm_base", 6), ("wl_buffer", 1)];

    let tmp_dir =
        std::env::temp_dir().join(format!("wayland-proxy-stale-attach-test-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("create temp dir");
    let host_socket_path = tmp_dir.join("host.sock");
    let _ = std::fs::remove_file(&host_socket_path);
    let host_listener = tokio::net::UnixListener::bind(&host_socket_path).expect("bind host");

    let (gtk_proxy_side, mut gtk_test_side) = tokio::net::UnixStream::pair().expect("pair");
    let first_host_conn = tokio::net::UnixStream::connect(&host_socket_path)
        .await
        .expect("initial host connect");
    let (first_host_accepted, _) = host_listener.accept().await.expect("accept first host conn");

    let host_socket_path_for_proxy = host_socket_path.clone();
    tokio::spawn(async move {
        if let Err(e) =
            wayland_proxy::run_connection(gtk_proxy_side, first_host_conn, host_socket_path_for_proxy)
                .await
        {
            eprintln!("run_connection ended with error: {e:?}");
        }
    });

    let (first_sink, _first_sink_rx) = tokio::sync::mpsc::unbounded_channel();
    let first_life_task = tokio::spawn(serve_fake_compositor_life(first_host_accepted, GLOBALS, 1, first_sink));

    // get_registry(2), sync(3).
    let mut out = Vec::new();
    let mut p = Vec::new();
    wire::put_u32(&mut p, 2);
    out.extend(wire::build_message(1, 1, &p));
    let mut p = Vec::new();
    wire::put_u32(&mut p, 3);
    out.extend(wire::build_message(1, 0, &p));
    gtk_test_side.write_all(&out).await.expect("write get_registry+sync");

    // Collect the three globals' names (deterministically 1,2,3 given
    // first_name=1 above, but read back for real like the other tests here).
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let mut wl_compositor_name = None;
    let mut xdg_wm_base_name = None;
    let mut wl_buffer_name = None;
    'collect: loop {
        use tokio::io::AsyncReadExt;
        let n = tokio::time::timeout(Duration::from_secs(3), gtk_test_side.read(&mut tmp))
            .await
            .expect("timed out collecting globals")
            .expect("read error");
        assert_ne!(n, 0);
        buf.extend_from_slice(&tmp[..n]);
        while let Some((msg, consumed)) = wire::take_message(&buf) {
            let header = wire::MessageHeader::parse(msg).unwrap();
            let payload = &msg[wire::HEADER_LEN..];
            if header.sender_id == 2 && header.opcode == 0 {
                if let (Some(name), Some((iface, _))) = (wire::read_u32(payload, 0), wire::read_str(payload, 4)) {
                    match iface.as_str() {
                        "wl_compositor" => wl_compositor_name = Some(name),
                        "xdg_wm_base" => xdg_wm_base_name = Some(name),
                        "wl_buffer" => wl_buffer_name = Some(name),
                        _ => {}
                    }
                }
            } else if header.sender_id == 3 && header.opcode == 0 {
                let consumed_len = consumed;
                buf.drain(..consumed_len);
                break 'collect;
            }
            let consumed_len = consumed;
            buf.drain(..consumed_len);
        }
    }
    let wl_compositor_name = wl_compositor_name.expect("wl_compositor advertised");
    let xdg_wm_base_name = xdg_wm_base_name.expect("xdg_wm_base advertised");
    let wl_buffer_name = wl_buffer_name.expect("wl_buffer advertised");

    // bind wl_compositor->4, xdg_wm_base->5, wl_buffer->9; create_surface->6,
    // get_xdg_surface->7, get_toplevel->8. Same guest-id scheme as
    // full_reconnect_recreates_surface_chain_and_synthesizes_configure,
    // plus the buffer bind.
    let mut out = Vec::new();
    let mut p = Vec::new();
    wire::put_u32(&mut p, wl_compositor_name);
    wire::put_str(&mut p, "wl_compositor");
    wire::put_u32(&mut p, 6);
    wire::put_u32(&mut p, 4);
    out.extend(wire::build_message(2, 0, &p));
    let mut p = Vec::new();
    wire::put_u32(&mut p, xdg_wm_base_name);
    wire::put_str(&mut p, "xdg_wm_base");
    wire::put_u32(&mut p, 6);
    wire::put_u32(&mut p, 5);
    out.extend(wire::build_message(2, 0, &p));
    let mut p = Vec::new();
    wire::put_u32(&mut p, wl_buffer_name);
    wire::put_str(&mut p, "wl_buffer");
    wire::put_u32(&mut p, 1);
    wire::put_u32(&mut p, 9);
    out.extend(wire::build_message(2, 0, &p));
    let mut p = Vec::new();
    wire::put_u32(&mut p, 6);
    out.extend(wire::build_message(4, 0, &p)); // wl_compositor(4).create_surface -> wl_surface(6)
    let mut p = Vec::new();
    wire::put_u32(&mut p, 7);
    wire::put_u32(&mut p, 6);
    out.extend(wire::build_message(5, 2, &p)); // xdg_wm_base(5).get_xdg_surface(surface=6) -> xdg_surface(7)
    let mut p = Vec::new();
    wire::put_u32(&mut p, 8);
    out.extend(wire::build_message(7, 1, &p)); // xdg_surface(7).get_toplevel -> xdg_toplevel(8)
    gtk_test_side.write_all(&out).await.expect("write bind+create chain");

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Crash. Second life uses different global names (101/102/103),
    // matching the other reconnect tests' proof that nothing assumes name
    // persistence.
    first_life_task.abort();
    let (second_sink, mut second_sink_rx) = tokio::sync::mpsc::unbounded_channel();
    let host_listener_task = tokio::spawn(async move {
        let (second_host_accepted, _) =
            tokio::time::timeout(Duration::from_secs(3), host_listener.accept())
                .await
                .expect("proxy should reconnect within 3s")
                .expect("accept second host conn");
        serve_fake_compositor_life(second_host_accepted, GLOBALS, 101, second_sink).await;
    });

    // Recovery replays exactly 5 requests to recreate wl_compositor,
    // xdg_wm_base, wl_surface, xdg_surface, xdg_toplevel -- wl_buffer is
    // deliberately NOT part of the recreation graph (see recreation.rs),
    // so no 6th replay for it.
    for _ in 0..5 {
        tokio::time::timeout(Duration::from_secs(3), second_sink_rx.recv())
            .await
            .expect("timed out waiting for a recreation request")
            .expect("sink closed early");
    }

    // Drain the synthesized xdg_toplevel.configure (guest id 8) then ack
    // the xdg_surface.configure (guest id 7), same sequence as
    // full_reconnect_recreates_surface_chain_and_synthesizes_configure --
    // this must happen before the stale attach below to prove the attach
    // itself (not some earlier-in-queue traffic) is what's being dropped.
    // Both configures land in one read(), so read_n_messages is required
    // (see its doc comment -- two read_one_message calls would hang).
    let mut synthesized = read_n_messages(&mut gtk_test_side, 2).await.into_iter();
    let toplevel_configure = synthesized.next().unwrap();
    let toplevel_header = wire::MessageHeader::parse(&toplevel_configure).expect("valid header");
    assert_eq!(toplevel_header.sender_id, 8, "toplevel configure should target the original xdg_toplevel guest id");

    let configure = synthesized.next().unwrap();
    let header = wire::MessageHeader::parse(&configure).expect("valid header");
    assert_eq!(header.sender_id, 7, "configure should target the original xdg_surface guest id");
    let synthetic_serial = wire::read_u32(&configure[wire::HEADER_LEN..], 0).expect("configure serial");
    let mut ack_payload = Vec::new();
    wire::put_u32(&mut ack_payload, synthetic_serial);
    gtk_test_side
        .write_all(&wire::build_message(7, 4, &ack_payload))
        .await
        .expect("write ack_configure");

    // The actual scenario: attach the STALE (pre-reconnect) buffer guest id
    // (9) to the freshly-recreated surface (guest id 6, unchanged by the
    // reconnect). wl_surface.attach(buffer: object, x: int, y: int) is
    // opcode 1 in core wayland.xml, stable since Wayland 1.0.
    let mut attach_payload = Vec::new();
    wire::put_u32(&mut attach_payload, 9); // buffer
    wire::put_u32(&mut attach_payload, 0); // x
    wire::put_u32(&mut attach_payload, 0); // y
    gtk_test_side
        .write_all(&wire::build_message(6, 1, &attach_payload))
        .await
        .expect("write attach");

    // Must never reach the new compositor -- a timeout here is the
    // expected, passing outcome, same negative-assertion shape as
    // stale_wl_buffer_release_is_dropped_after_reconnect above.
    let result = tokio::time::timeout(Duration::from_millis(500), second_sink_rx.recv()).await;
    assert!(
        result.is_err(),
        "attach referencing a pre-reconnect (stale-generation) buffer must be dropped, not forwarded, but got: {result:?}"
    );

    host_listener_task.abort();
}

/// The ClientToHost mirror of `stale_wl_buffer_release_is_dropped_after_reconnect`:
/// a client destroying a buffer it created *before* the crash, after
/// reconnecting. The host side never heard of this id in the new
/// generation (wl_buffer isn't part of the recreation graph, see
/// recreation.rs), so the destroy request can't be forwarded -- but unlike
/// a stale event that can just be discarded, this is a client-issued
/// destructor request: libwayland-client won't let the app reuse this
/// numeric id until it sees `wl_display.delete_id` for it (`wl_proxy_destroy`
/// parks it as a zombie otherwise). The proxy must synthesize that
/// delete_id itself rather than just silently dropping the request --
/// added 2026-08-03 as the "graceful handling for remaining stale-buffer
/// references" follow-up to the `xdg_toplevel.configure` strengthening
/// (see plan-desktop-resilience.md).
#[tokio::test]
async fn stale_wl_buffer_destroy_synthesizes_delete_id_after_reconnect() {
    let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).try_init();
    use tokio::io::AsyncWriteExt;
    use wayland_proxy::wire;

    const GLOBALS: &[(&str, u32)] = &[("wl_buffer", 1)];

    let tmp_dir =
        std::env::temp_dir().join(format!("wayland-proxy-stale-destroy-test-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("create temp dir");
    let host_socket_path = tmp_dir.join("host.sock");
    let _ = std::fs::remove_file(&host_socket_path);
    let host_listener = tokio::net::UnixListener::bind(&host_socket_path).expect("bind host");

    let (gtk_proxy_side, mut gtk_test_side) = tokio::net::UnixStream::pair().expect("pair");
    let first_host_conn = tokio::net::UnixStream::connect(&host_socket_path)
        .await
        .expect("initial host connect");
    let (first_host_accepted, _) = host_listener.accept().await.expect("accept first host conn");

    let host_socket_path_for_proxy = host_socket_path.clone();
    tokio::spawn(async move {
        if let Err(e) =
            wayland_proxy::run_connection(gtk_proxy_side, first_host_conn, host_socket_path_for_proxy)
                .await
        {
            eprintln!("run_connection ended with error: {e:?}");
        }
    });

    let (first_sink, mut first_sink_rx) = tokio::sync::mpsc::unbounded_channel();
    let first_life_task = tokio::spawn(serve_fake_compositor_life(first_host_accepted, GLOBALS, 1, first_sink));

    // get_registry(2), sync(3), then bind wl_buffer(name=1) -> guest id 4.
    let mut out = Vec::new();
    let mut p = Vec::new();
    wire::put_u32(&mut p, 2);
    out.extend(wire::build_message(1, 1, &p));
    let mut p = Vec::new();
    wire::put_u32(&mut p, 3);
    out.extend(wire::build_message(1, 0, &p));
    gtk_test_side.write_all(&out).await.expect("write get_registry+sync");

    // Drain exactly 2 messages (global(wl_buffer) + callback.done) using
    // one persistent buffer -- see read_n_messages's doc comment for why
    // two separate read_one_message calls can't be used here.
    read_n_messages(&mut gtk_test_side, 2).await;

    let mut p = Vec::new();
    wire::put_u32(&mut p, 1); // name
    wire::put_str(&mut p, "wl_buffer");
    wire::put_u32(&mut p, 1); // version
    wire::put_u32(&mut p, 4); // new_id (guest)
    gtk_test_side.write_all(&wire::build_message(2, 0, &p)).await.expect("write bind");

    // Wait for the bind to reach the first life -- proves the buffer
    // exists (as far as the first generation is concerned) before the
    // crash; this test doesn't need the resulting host id itself.
    let _ = tokio::time::timeout(Duration::from_secs(3), first_sink_rx.recv())
        .await
        .expect("timed out waiting for the bind to reach the first life")
        .expect("sink closed early");

    // Crash and reconnect. Second life advertises nothing -- this test
    // doesn't exercise recreation, only the stale-buffer-destroy path.
    first_life_task.abort();
    let (second_sink, mut second_sink_rx) = tokio::sync::mpsc::unbounded_channel();
    let host_listener_task = tokio::spawn(async move {
        let (second_host_accepted, _) =
            tokio::time::timeout(Duration::from_secs(3), host_listener.accept())
                .await
                .expect("proxy should reconnect within 3s")
                .expect("accept second host conn");
        serve_fake_compositor_life(second_host_accepted, &[], 1, second_sink).await;
    });

    // Give recovery a moment to finish processing the (empty) globals and
    // unfreeze before sending the stale destroy.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The client destroys its own pre-crash buffer (guest id 4) -- exactly
    // what a real client's buffer-pool cleanup path does, regardless of
    // whether a crash happened underneath it.
    gtk_test_side
        .write_all(&wire::build_message(4, 0, &[])) // wl_buffer.destroy()
        .await
        .expect("write destroy");

    // Must never reach the new compositor -- it never heard of this id in
    // this generation, there's nothing to forward the destroy to.
    let forwarded = tokio::time::timeout(Duration::from_millis(500), second_sink_rx.recv()).await;
    assert!(
        forwarded.is_err(),
        "destroy on a pre-reconnect (stale-generation) buffer must never reach the new compositor, but got: {forwarded:?}"
    );

    // But the client must still see a delete_id for its own guest id 4 --
    // otherwise libwayland-client parks it as a zombie forever and the app
    // can never reuse that numeric id again.
    let received = read_one_message(&mut gtk_test_side).await;
    let header = wire::MessageHeader::parse(&received).expect("valid header");
    assert_eq!(header.sender_id, 1, "delete_id is always sent from wl_display");
    assert_eq!(header.opcode, 1, "delete_id event opcode");
    let deleted_id = wire::read_u32(&received[wire::HEADER_LEN..], 0).expect("deleted id");
    assert_eq!(deleted_id, 4, "should free the client's own original guest id, not some translated host id");

    host_listener_task.abort();
}

/// implementation-constraints.md's "Grab State" rule, proven end to end
/// (GrabTracker's own unit tests cover the tracking logic in isolation; this
/// is the one piece of reconnect recovery that hadn't yet been proven
/// through an actual reconnect, unlike registry/surface recreation and
/// buffer lifetimes): a pointer enters a surface and presses a button
/// before the compositor crashes, and reconnecting must synthesize a
/// `wl_pointer.leave` and a button-release, addressed to the pointer's
/// original (unchanged) guest id, before any other post-reconnect traffic.
#[tokio::test]
async fn grab_state_is_released_before_traffic_resumes_after_reconnect() {
    let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).try_init();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use wayland_proxy::wire;

    const GLOBALS: &[(&str, u32)] = &[("wl_seat", 9)];

    let tmp_dir =
        std::env::temp_dir().join(format!("wayland-proxy-grab-state-test-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("create temp dir");
    let host_socket_path = tmp_dir.join("host.sock");
    let _ = std::fs::remove_file(&host_socket_path);
    let host_listener = tokio::net::UnixListener::bind(&host_socket_path).expect("bind host");

    let (gtk_proxy_side, mut gtk_test_side) = tokio::net::UnixStream::pair().expect("pair");
    let first_host_conn = tokio::net::UnixStream::connect(&host_socket_path)
        .await
        .expect("initial host connect");
    let (mut first_host_accepted, _) = host_listener.accept().await.expect("accept first host conn");

    let host_socket_path_for_proxy = host_socket_path.clone();
    tokio::spawn(async move {
        if let Err(e) =
            wayland_proxy::run_connection(gtk_proxy_side, first_host_conn, host_socket_path_for_proxy)
                .await
        {
            eprintln!("run_connection ended with error: {e:?}");
        }
    });

    // get_registry(2), sync(3), bind wl_seat(name=1) -> guest 4,
    // wl_seat(4).get_pointer(new_id=5) -> guest wl_pointer 5.
    // Sent as one pipelined batch -- Wayland requests don't need to wait
    // for a response before the next one is sent, and the fake compositor's
    // globals are already known (GLOBALS, first_name=1) so the bind's name
    // can be hardcoded rather than round-tripped first.
    let mut out = Vec::new();
    let mut p = Vec::new();
    wire::put_u32(&mut p, 2);
    out.extend(wire::build_message(1, 1, &p)); // get_registry(2)
    let mut p = Vec::new();
    wire::put_u32(&mut p, 3);
    out.extend(wire::build_message(1, 0, &p)); // sync(3)
    let mut p = Vec::new();
    wire::put_u32(&mut p, 1); // name
    wire::put_str(&mut p, "wl_seat");
    wire::put_u32(&mut p, 9); // version
    wire::put_u32(&mut p, 4); // new_id (guest wl_seat)
    out.extend(wire::build_message(2, 0, &p)); // wl_registry(2).bind -> wl_seat(4)
    let mut p = Vec::new();
    wire::put_u32(&mut p, 5); // new_id (guest wl_pointer)
    out.extend(wire::build_message(4, 0, &p)); // wl_seat(4).get_pointer -> wl_pointer(5)
    gtk_test_side.write_all(&out).await.expect("write get_registry+sync+bind+get_pointer");

    // Handle the first life manually: answer get_registry/sync, then
    // observe get_pointer to learn the host id it was assigned, then
    // inject synthetic enter+button events on it.
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let mut registry_host_id = None;
    let mut seat_host_id = None;
    'first_life: loop {
        let n = tokio::time::timeout(Duration::from_secs(3), first_host_accepted.read(&mut tmp))
            .await
            .expect("timed out in first life")
            .expect("read error");
        assert_ne!(n, 0);
        buf.extend_from_slice(&tmp[..n]);
        while let Some((msg, consumed)) = wire::take_message(&buf) {
            let header = wire::MessageHeader::parse(msg).unwrap();
            let payload = msg[wire::HEADER_LEN..].to_vec();
            if header.sender_id == 1 && header.opcode == 1 {
                registry_host_id = wire::read_u32(&payload, 0);
            } else if header.sender_id == 1 && header.opcode == 0 {
                if let (Some(sync_id), Some(reg_id)) = (wire::read_u32(&payload, 0), registry_host_id) {
                    let mut out = Vec::new();
                    for (i, (iface, version)) in GLOBALS.iter().enumerate() {
                        let mut gp = Vec::new();
                        wire::put_u32(&mut gp, 1 + i as u32);
                        wire::put_str(&mut gp, iface);
                        wire::put_u32(&mut gp, *version);
                        out.extend(wire::build_message(reg_id, 0, &gp));
                    }
                    let mut done_payload = Vec::new();
                    wire::put_u32(&mut done_payload, 0);
                    out.extend(wire::build_message(sync_id, 0, &done_payload));
                    first_host_accepted.write_all(&out).await.expect("write globals+done");
                }
            } else if Some(header.sender_id) == registry_host_id && header.opcode == 0 {
                // bind wl_seat -> record its host id
                seat_host_id = wire::read_u32(&payload, payload.len() - 4);
            } else if Some(header.sender_id) == seat_host_id && header.opcode == 0 {
                // get_pointer -> record the host id assigned, then inject
                // the synthetic enter+button, then we're done with this life.
                let ph = wire::read_u32(&payload, 0).expect("get_pointer payload should have a new_id");

                // `surface` must be a real, translatable id -- an
                // untranslatable Object-typed argument gets the whole
                // message dropped (see relay_ready_messages). The
                // pointer's own host/guest pair (already tracked) works
                // fine as a stand-in: translation only checks it's a
                // known id, not that it's semantically a surface.
                let mut enter_payload = Vec::new();
                wire::put_u32(&mut enter_payload, 1); // serial
                wire::put_u32(&mut enter_payload, ph); // surface (stand-in, see above)
                wire::put_u32(&mut enter_payload, 0); // surface_x (fixed)
                wire::put_u32(&mut enter_payload, 0); // surface_y (fixed)
                let enter = wire::build_message(ph, 0, &enter_payload);

                let mut button_payload = Vec::new();
                wire::put_u32(&mut button_payload, 2); // serial
                wire::put_u32(&mut button_payload, 0); // time
                wire::put_u32(&mut button_payload, 272); // button (BTN_LEFT)
                wire::put_u32(&mut button_payload, 1); // state: pressed
                let button = wire::build_message(ph, 3, &button_payload);

                first_host_accepted.write_all(&enter).await.expect("write enter");
                first_host_accepted.write_all(&button).await.expect("write button");
                buf.drain(..consumed);
                break 'first_life;
            }
            buf.drain(..consumed);
        }
    }

    // Drain exactly 4 messages the client should have received by now
    // (global(wl_seat), callback.done, the synthetic enter, the synthetic
    // button) using one persistent buffer -- read_one_message starts a
    // fresh buffer per call, so calling it repeatedly in a row can silently
    // drop a message that arrived in the same underlying read() as another
    // (see the stale-buffer test above for the same lesson).
    {
        use tokio::io::AsyncReadExt as _;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let mut drained = 0;
        while drained < 4 {
            while let Some((_msg, consumed)) = wire::take_message(&buf) {
                buf.drain(..consumed);
                drained += 1;
            }
            if drained >= 4 {
                break;
            }
            let n = tokio::time::timeout(Duration::from_secs(3), gtk_test_side.read(&mut tmp))
                .await
                .expect("timed out draining pre-crash messages")
                .expect("read error");
            assert_ne!(n, 0);
            buf.extend_from_slice(&tmp[..n]);
        }
        assert!(buf.is_empty(), "leftover bytes after draining exactly 4 pre-crash messages");
    }

    // Crash: drop the first life's stream, forcing an EOF on the proxy's read.
    drop(first_host_accepted);
    let (mut second_host_accepted, _) =
        tokio::time::timeout(Duration::from_secs(3), host_listener.accept())
            .await
            .expect("proxy should reconnect within 3s")
            .expect("accept second host conn");

    // Answer recovery's get_registry+sync with no globals -- irrelevant here.
    let mut buf = Vec::new();
    'second_life: loop {
        let n = tokio::time::timeout(Duration::from_secs(3), second_host_accepted.read(&mut tmp))
            .await
            .expect("timed out answering recovery's get_registry/sync")
            .expect("read error");
        assert_ne!(n, 0);
        buf.extend_from_slice(&tmp[..n]);
        while let Some((msg, consumed)) = wire::take_message(&buf) {
            let header = wire::MessageHeader::parse(msg).unwrap();
            let payload = msg[wire::HEADER_LEN..].to_vec();
            if header.sender_id == 1 && header.opcode == 0 {
                if let Some(sync_id) = wire::read_u32(&payload, 0) {
                    let mut done_payload = Vec::new();
                    wire::put_u32(&mut done_payload, 0);
                    second_host_accepted
                        .write_all(&wire::build_message(sync_id, 0, &done_payload))
                        .await
                        .expect("write callback.done");
                    buf.drain(..consumed);
                    break 'second_life;
                }
            }
            buf.drain(..consumed);
        }
    }

    // The client should now receive wl_pointer.leave and a button-release,
    // both addressed to guest id 5 -- the pointer's ORIGINAL id, unchanged
    // by the reconnect -- before anything else. Both read with one
    // persistent buffer (same reasoning as the earlier drains in this
    // file): they're sent back-to-back with no delay, so they likely
    // arrive in a single underlying read(), and read_one_message called
    // twice in a row would silently drop the second.
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let mut messages = Vec::new();
    while messages.len() < 2 {
        while let Some((msg, consumed)) = wire::take_message(&buf) {
            messages.push(msg.to_vec());
            let consumed_len = consumed;
            buf.drain(..consumed_len);
        }
        if messages.len() >= 2 {
            break;
        }
        let n = tokio::time::timeout(Duration::from_secs(3), gtk_test_side.read(&mut tmp))
            .await
            .expect("timed out waiting for leave+release")
            .expect("read error");
        assert_ne!(n, 0);
        buf.extend_from_slice(&tmp[..n]);
    }

    let header = wire::MessageHeader::parse(&messages[0]).expect("valid header");
    assert_eq!(header.sender_id, 5, "leave should target the pointer's original guest id");
    assert_eq!(header.opcode, 1, "wl_pointer.leave event opcode");

    let header = wire::MessageHeader::parse(&messages[1]).expect("valid header");
    assert_eq!(header.sender_id, 5);
    assert_eq!(header.opcode, 3, "wl_pointer.button event opcode");
    let payload = &messages[1][wire::HEADER_LEN..];
    assert_eq!(wire::read_u32(payload, 8), Some(272), "released button should match the one pressed pre-crash");
    assert_eq!(wire::read_u32(payload, 12), Some(0), "state should be Released");
}
