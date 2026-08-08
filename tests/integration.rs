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
            wayland_proxy::run_connection(gtk_stream, compositor_stream, host_socket_path_for_proxy, None, wayland_proxy::clipboard::ClipboardCache::new()).await
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
        if let Err(e) = wayland_proxy::run_connection(gtk_proxy_side, host_proxy_side, unused_path, None, wayland_proxy::clipboard::ClipboardCache::new()).await {
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
        if let Err(e) = wayland_proxy::run_connection(gtk_proxy_side, host_proxy_side, unused_path, None, wayland_proxy::clipboard::ClipboardCache::new()).await {
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
            wayland_proxy::run_connection(gtk_proxy_side, first_host_conn, host_socket_path_for_proxy, None, wayland_proxy::clipboard::ClipboardCache::new())
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
            wayland_proxy::run_connection(gtk_proxy_side, first_host_conn, host_socket_path_for_proxy, None, wayland_proxy::clipboard::ClipboardCache::new())
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
            wayland_proxy::run_connection(gtk_proxy_side, first_host_conn, host_socket_path_for_proxy, None, wayland_proxy::clipboard::ClipboardCache::new())
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
            wayland_proxy::run_connection(gtk_proxy_side, first_host_conn, host_socket_path_for_proxy, None, wayland_proxy::clipboard::ClipboardCache::new())
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
    use tokio::io::AsyncWriteExt;
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
        // Deliberately NOT a plain `stream.read()`: once a real test sends
        // an fd via SCM_RIGHTS (see wl_shm_pool_and_buffer_recipes_replay_correctly_after_reconnect,
        // the first test here to do so), a plain read() on an AF_UNIX
        // SOCK_STREAM socket stops exactly at the end of whatever skb
        // carried the ancillary data -- confirmed empirically: a read()
        // here returned every byte up through a create_pool message's
        // payload and no further, even though the next message
        // (create_buffer, no fd) had already been written by the proxy and
        // was sitting in the kernel receive buffer. The kernel won't merge
        // a later skb into the same read() as an ancillary-bearing one,
        // and -- unlike a raw `recvmsg` retrieving (and here, discarding)
        // the ancillary data itself -- a plain `read()`/`recv()` next call
        // never got woken for the remainder, hanging the test. Exactly the
        // class of hazard `Conn::fill()` (src/lib.rs) already documents and
        // works around the same way: `recv_with_fds` (which properly
        // retrieves, and here simply drops, any fds) via `try_io` so
        // tokio's readiness tracking stays correct across the boundary.
        let raw_fd = std::os::fd::AsRawFd::as_raw_fd(&stream);
        if stream.readable().await.is_err() {
            return;
        }
        let result = stream.try_io(tokio::io::Interest::READABLE, || {
            wayland_proxy::fdsocket::recv_with_fds(raw_fd, &mut tmp).map_err(std::io::Error::from)
        });
        match result {
            Ok((0, _fds)) => return,
            Ok((n, _fds)) => buf.extend_from_slice(&tmp[..n]), // _fds (if any) close on drop, unused by this fake compositor
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => return,
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
            wayland_proxy::run_connection(gtk_proxy_side, first_host_conn, host_socket_path_for_proxy, None, wayland_proxy::clipboard::ClipboardCache::new())
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

/// Regression test for a stale-bookkeeping bug found 2026-08-08:
/// `pending_configure_acks` (guest xdg_surface id -> our own invented
/// post-recovery serial, see `full_reconnect_recreates_surface_chain_and_synthesizes_configure`
/// above) was cleared from `objects`/`graph` on `wl_display.delete_id` but
/// never from `pending_configure_acks` itself. If a toplevel is destroyed
/// before ever ack'ing a synthesized post-recovery configure, and a *new*
/// xdg_surface later reuses that same now-free guest id (libwayland-client
/// reuses freed low ids), a real `ack_configure` for the new object could
/// numerically collide with the stale leftover serial and get wrongly
/// swallowed instead of forwarded to the real compositor, which is
/// legitimately waiting on it. Drives the second life's host connection
/// directly (no `serve_fake_compositor_life` background task) since this
/// test, unlike the ones using that helper, needs to inject a host-issued
/// `delete_id` mid-scenario, not just observe replayed requests.
#[tokio::test]
async fn pending_configure_ack_is_forgotten_when_its_surface_is_deleted_before_acking() {
    let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).try_init();
    use tokio::io::AsyncWriteExt;
    use wayland_proxy::wire;

    const GLOBALS: &[(&str, u32)] = &[("wl_compositor", 6), ("xdg_wm_base", 6)];

    let tmp_dir =
        std::env::temp_dir().join(format!("wayland-proxy-stale-configure-ack-test-{}", std::process::id()));
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
            wayland_proxy::run_connection(gtk_proxy_side, first_host_conn, host_socket_path_for_proxy, None, wayland_proxy::clipboard::ClipboardCache::new())
                .await
        {
            eprintln!("run_connection ended with error: {e:?}");
        }
    });

    let (first_sink, _first_sink_rx) = tokio::sync::mpsc::unbounded_channel();
    let first_life_task = tokio::spawn(serve_fake_compositor_life(first_host_accepted, GLOBALS, 1, first_sink));

    // First life: get_registry(2), sync(3), bind wl_compositor(4)/xdg_wm_base(5),
    // create_surface->6, get_xdg_surface->7, get_toplevel->8 -- identical
    // setup to full_reconnect_recreates_surface_chain_and_synthesizes_configure.
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
    out.extend(wire::build_message(2, 0, &p)); // bind -> wl_compositor(4)
    let mut p = Vec::new();
    wire::put_u32(&mut p, xdg_wm_base_name);
    wire::put_str(&mut p, "xdg_wm_base");
    wire::put_u32(&mut p, 6);
    wire::put_u32(&mut p, 5);
    out.extend(wire::build_message(2, 0, &p)); // bind -> xdg_wm_base(5)
    let mut p = Vec::new();
    wire::put_u32(&mut p, 6);
    out.extend(wire::build_message(4, 0, &p)); // create_surface -> wl_surface(6)
    let mut p = Vec::new();
    wire::put_u32(&mut p, 7);
    wire::put_u32(&mut p, 6);
    out.extend(wire::build_message(5, 2, &p)); // get_xdg_surface(surface=6) -> xdg_surface(7)
    let mut p = Vec::new();
    wire::put_u32(&mut p, 8);
    out.extend(wire::build_message(7, 1, &p)); // get_toplevel -> xdg_toplevel(8)
    gtk_test_side.write_all(&out).await.expect("write bind+create chain");

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Crash. Second life is driven directly (not via serve_fake_compositor_life)
    // so the test can write a host-issued delete_id into it partway through.
    first_life_task.abort();
    let (mut second_host_accepted, _) = tokio::time::timeout(Duration::from_secs(3), host_listener.accept())
        .await
        .expect("proxy should reconnect within 3s")
        .expect("accept second host conn");

    // The proxy's own internal registry re-fetch: get_registry(new_id),
    // sync(new_id) -- same auto-answer logic as serve_fake_compositor_life,
    // inlined here since this test needs direct write access afterward.
    let registry_and_sync = read_n_messages(&mut second_host_accepted, 2).await;
    let registry_header = wire::MessageHeader::parse(&registry_and_sync[0]).expect("valid header");
    assert_eq!(registry_header.opcode, 1, "get_registry opcode");
    let registry_id = wire::read_u32(&registry_and_sync[0][wire::HEADER_LEN..], 0).expect("registry new_id");
    let sync_header = wire::MessageHeader::parse(&registry_and_sync[1]).expect("valid header");
    assert_eq!(sync_header.opcode, 0, "sync opcode");
    let sync_id = wire::read_u32(&registry_and_sync[1][wire::HEADER_LEN..], 0).expect("sync new_id");

    let mut out = Vec::new();
    for (i, (iface, version)) in GLOBALS.iter().enumerate() {
        let mut p = Vec::new();
        wire::put_u32(&mut p, 101 + i as u32);
        wire::put_str(&mut p, iface);
        wire::put_u32(&mut p, *version);
        out.extend(wire::build_message(registry_id, 0, &p));
    }
    let mut done_payload = Vec::new();
    wire::put_u32(&mut done_payload, 0);
    out.extend(wire::build_message(sync_id, 0, &done_payload));
    second_host_accepted.write_all(&out).await.expect("write globals+done");

    // Recovery replays the same 5 requests as
    // full_reconnect_recreates_surface_chain_and_synthesizes_configure:
    // bind(wl_compositor), bind(xdg_wm_base), create_surface, get_xdg_surface,
    // get_toplevel.
    let replay = read_n_messages(&mut second_host_accepted, 5).await;
    let recreated_xdg_surface_host_id = wire::read_u32(&replay[3][wire::HEADER_LEN..], 0).unwrap();

    // Synthesized xdg_toplevel.configure + xdg_surface.configure, same
    // pair as full_reconnect_recreates_surface_chain_and_synthesizes_configure
    // -- the client never acks the second one, unlike that test.
    let synthesized = read_n_messages(&mut gtk_test_side, 2).await;
    let configure = &synthesized[1];
    let header = wire::MessageHeader::parse(configure).expect("valid header");
    assert_eq!(header.sender_id, 7, "configure should target the original xdg_surface guest id");
    let synthetic_serial = wire::read_u32(&configure[wire::HEADER_LEN..], 0).expect("configure serial");
    assert_eq!(
        synthetic_serial, 1,
        "next_configure_serial starts at 1 every reconnect -- exactly the collision-prone value this test relies on"
    );

    // The client destroys its xdg_surface(7) without ever ack'ing the
    // synthesized configure above -- e.g. the window was closed before it
    // got a chance to repaint. xdg_surface.destroy is opcode 0.
    gtk_test_side.write_all(&wire::build_message(7, 0, &[])).await.expect("write xdg_surface.destroy");
    let destroy = read_one_message(&mut second_host_accepted).await;
    let destroy_header = wire::MessageHeader::parse(&destroy).expect("valid header");
    assert_eq!(destroy_header.sender_id, recreated_xdg_surface_host_id);
    assert_eq!(destroy_header.opcode, 0, "xdg_surface.destroy opcode");

    // The real compositor confirms the destroy the way it always does --
    // this is the event the delete_id handler must use to forget the now-
    // stale pending_configure_acks(7 -> 1) entry too, not just the Shadow
    // Table/RecreationGraph entries it already forgot.
    let mut delete_id_payload = Vec::new();
    wire::put_u32(&mut delete_id_payload, recreated_xdg_surface_host_id);
    second_host_accepted
        .write_all(&wire::build_message(1, 1, &delete_id_payload))
        .await
        .expect("write delete_id");
    let delete_id_on_client = read_one_message(&mut gtk_test_side).await;
    let delete_id_header = wire::MessageHeader::parse(&delete_id_on_client).expect("valid header");
    assert_eq!(delete_id_header.sender_id, 1, "delete_id is always sent from wl_display");
    assert_eq!(delete_id_header.opcode, 1, "delete_id event opcode");
    assert_eq!(
        wire::read_u32(&delete_id_on_client[wire::HEADER_LEN..], 0),
        Some(7),
        "should free the client's own original guest id 7"
    );

    // The client creates a *new* xdg_surface, reusing guest id 7 -- exactly
    // what libwayland-client's own id allocator does with a freed low id.
    // Reuses the still-live wl_surface(6); nothing in this test needs a
    // fresh one.
    let mut p = Vec::new();
    wire::put_u32(&mut p, 7);
    wire::put_u32(&mut p, 6);
    gtk_test_side
        .write_all(&wire::build_message(5, 2, &p)) // xdg_wm_base(5).get_xdg_surface(surface=6) -> xdg_surface(7)
        .await
        .expect("write get_xdg_surface");
    let new_get_xdg_surface = read_one_message(&mut second_host_accepted).await;
    let new_get_xdg_surface_header = wire::MessageHeader::parse(&new_get_xdg_surface).expect("valid header");
    assert_eq!(new_get_xdg_surface_header.opcode, 2, "get_xdg_surface opcode");
    let new_xdg_surface_host_id = wire::read_u32(&new_get_xdg_surface[wire::HEADER_LEN..], 0).unwrap();
    assert_ne!(
        new_xdg_surface_host_id, recreated_xdg_surface_host_id,
        "the new xdg_surface must get its own fresh host id, even though it reuses the old guest id"
    );

    // The client acks a *real* configure for this new object with serial 1
    // -- numerically identical to the stale synthetic serial the destroyed
    // xdg_surface(7) never got around to ack'ing. Without the fix, this
    // gets wrongly matched against the leftover pending_configure_acks(7)
    // entry and silently swallowed instead of forwarded; the real
    // compositor would then be left waiting forever for an ack it's
    // legitimately owed.
    let mut ack_payload = Vec::new();
    wire::put_u32(&mut ack_payload, 1);
    gtk_test_side
        .write_all(&wire::build_message(7, 4, &ack_payload)) // xdg_surface(7).ack_configure(1)
        .await
        .expect("write ack_configure");
    let forwarded_ack = read_one_message(&mut second_host_accepted).await;
    let forwarded_ack_header = wire::MessageHeader::parse(&forwarded_ack).expect("valid header");
    assert_eq!(
        forwarded_ack_header.sender_id, new_xdg_surface_host_id,
        "a real ack_configure for the *new* xdg_surface must reach the real compositor, not be \
         swallowed as if it were answering the destroyed xdg_surface's stale synthetic configure"
    );
    assert_eq!(forwarded_ack_header.opcode, 4, "ack_configure opcode");
    assert_eq!(wire::read_u32(&forwarded_ack[wire::HEADER_LEN..], 0), Some(1));
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
            wayland_proxy::run_connection(gtk_proxy_side, first_host_conn, host_socket_path_for_proxy, None, wayland_proxy::clipboard::ClipboardCache::new())
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
            wayland_proxy::run_connection(gtk_proxy_side, first_host_conn, host_socket_path_for_proxy, None, wayland_proxy::clipboard::ClipboardCache::new())
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
            wayland_proxy::run_connection(gtk_proxy_side, first_host_conn, host_socket_path_for_proxy, None, wayland_proxy::clipboard::ClipboardCache::new())
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
            wayland_proxy::run_connection(gtk_proxy_side, first_host_conn, host_socket_path_for_proxy, None, wayland_proxy::clipboard::ClipboardCache::new())
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

/// ADR-0006, wl_shm half: the class of fatal crash-recovery gap this
/// closes is a client re-attaching its own pre-crash `wl_buffer` after a
/// reconnect, which used to hit "references untranslatable object N --
/// dropping" followed by the real compositor killing the connection
/// outright (confirmed live 2026-08-03 against a real gtk4-demo -- see
/// plan-desktop-resilience.md). This test proves the mechanism the fix
/// depends on, end to end: `wl_shm.create_pool` carries a REAL fd via
/// SCM_RIGHTS (unlike `create_pool_on_a_stale_wl_shm_does_not_leave_a_phantom_mapping`,
/// which only needed to prove shadow-table bookkeeping and skipped real fd
/// passing) which the proxy must retain -- not just forward and forget --
/// and on reconnect replay both `create_pool` (using its own retained copy
/// of that fd) and `create_buffer` against the fresh compositor, chaining
/// host ids correctly, before a client's subsequent `attach()` could ever
/// depend on it.
#[tokio::test]
async fn wl_shm_pool_and_buffer_recipes_replay_correctly_after_reconnect() {
    let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).try_init();
    use std::os::fd::AsRawFd;
    use tokio::io::AsyncWriteExt;
    use wayland_proxy::wire;

    const GLOBALS: &[(&str, u32)] = &[("wl_compositor", 6), ("xdg_wm_base", 6), ("wl_shm", 1)];

    let tmp_dir =
        std::env::temp_dir().join(format!("wayland-proxy-shm-buffer-reconnect-test-{}", std::process::id()));
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
            wayland_proxy::run_connection(gtk_proxy_side, first_host_conn, host_socket_path_for_proxy, None, wayland_proxy::clipboard::ClipboardCache::new())
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

    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let mut wl_compositor_name = None;
    let mut xdg_wm_base_name = None;
    let mut wl_shm_name = None;
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
                        "wl_shm" => wl_shm_name = Some(name),
                        _ => {}
                    }
                }
            } else if header.sender_id == 3 && header.opcode == 0 {
                buf.drain(..consumed);
                break 'collect;
            }
            buf.drain(..consumed);
        }
    }
    let wl_compositor_name = wl_compositor_name.expect("wl_compositor advertised");
    let xdg_wm_base_name = xdg_wm_base_name.expect("xdg_wm_base advertised");
    let wl_shm_name = wl_shm_name.expect("wl_shm advertised");

    // bind wl_compositor->4, xdg_wm_base->5, wl_shm->9; create_surface->6,
    // get_xdg_surface->7, get_toplevel->8 -- same guest-id scheme as the
    // other full-chain reconnect tests, wl_shm bound alongside the other
    // two roots.
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
    wire::put_u32(&mut p, wl_shm_name);
    wire::put_str(&mut p, "wl_shm");
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

    // wl_shm(9).create_pool(new_id=10, fd, size=4096) -- a REAL fd this
    // time: the whole point of ADR-0006 is that the proxy retains its own
    // copy of this fd, so the test needs a real one to retain. tokio's
    // plain write_all can't carry ancillary SCM_RIGHTS data (see
    // fdsocket.rs's own doc comment) -- fdsocket::send_with_fds is used
    // directly against the same underlying socket instead, same as
    // Conn::write_message does internally.
    let backing_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(tmp_dir.join("shm_pool_backing"))
        .expect("create backing file");
    backing_file.set_len(4096).expect("size backing file");
    let mut p = Vec::new();
    wire::put_u32(&mut p, 10); // new_id (guest) -- wl_shm_pool
    wire::put_u32(&mut p, 4096); // size
    let create_pool_msg = wire::build_message(9, 0, &p);
    wayland_proxy::fdsocket::send_with_fds(gtk_test_side.as_raw_fd(), &create_pool_msg, &[backing_file.as_raw_fd()])
        .expect("send create_pool with a real fd");

    // wl_shm_pool(10).create_buffer(new_id=11, offset=0, width=64,
    // height=64, stride=256, format=0/*ARGB8888*/) -- no fd of its own,
    // draws from the pool's already-retained backing memfd.
    let mut p = Vec::new();
    wire::put_u32(&mut p, 11); // new_id (guest) -- wl_buffer
    wire::put_u32(&mut p, 0); // offset
    wire::put_u32(&mut p, 64); // width
    wire::put_u32(&mut p, 64); // height
    wire::put_u32(&mut p, 256); // stride
    wire::put_u32(&mut p, 0); // format
    gtk_test_side.write_all(&wire::build_message(10, 0, &p)).await.expect("write create_buffer");

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Crash. Second life uses different global names, same as every other
    // reconnect test here.
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

    // Recovery replays exactly 8 requests, in the order the client
    // originally issued them: bind(wl_compositor), bind(xdg_wm_base),
    // bind(wl_shm), create_surface, get_xdg_surface, get_toplevel,
    // create_pool, create_buffer.
    let mut observed = Vec::new();
    for i in 0..8 {
        let (sender, opcode, payload) = tokio::time::timeout(Duration::from_secs(3), second_sink_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for recreation request #{i}, got so far: {observed:?}"))
            .expect("sink closed early");
        observed.push((sender, opcode, payload));
    }

    fn bind_new_id(payload: &[u8]) -> u32 {
        let (_, next) = wire::read_str(payload, 4).expect("interface string");
        wire::read_u32(payload, next + 4).expect("new_id") // next+4 skips version
    }

    let iface = wire::read_str(&observed[2].2, 4).unwrap().0;
    assert_eq!(iface, "wl_shm", "third bind should be wl_shm, matching the client's own bind order");
    let recreated_shm_host_id = bind_new_id(&observed[2].2);

    // create_pool: sent to the freshly re-bound wl_shm (not any earlier
    // object), carrying the ORIGINAL size (4096) -- not something derived
    // from the new compositor, which never advertised a size at all.
    assert_eq!(observed[6].0, recreated_shm_host_id, "create_pool should target wl_shm's freshly recreated host id");
    assert_eq!(observed[6].1, 0, "create_pool opcode");
    let recreated_pool_host_id = wire::read_u32(&observed[6].2, 0).unwrap();
    let replayed_size = wire::read_u32(&observed[6].2, 4).unwrap();
    assert_eq!(replayed_size, 4096, "replayed create_pool should carry the pool's originally-recorded size");

    // create_buffer: sent to the freshly recreated pool, referencing its
    // NEW host id -- not the pool's stale first-life one -- and carrying
    // the buffer's originally-recorded offset/width/height/stride/format.
    assert_eq!(observed[7].0, recreated_pool_host_id, "create_buffer should target the pool's freshly recreated host id");
    assert_eq!(observed[7].1, 0, "create_buffer opcode");
    assert_eq!(wire::read_u32(&observed[7].2, 4), Some(0), "offset");
    assert_eq!(wire::read_u32(&observed[7].2, 8), Some(64), "width");
    assert_eq!(wire::read_u32(&observed[7].2, 12), Some(64), "height");
    assert_eq!(wire::read_u32(&observed[7].2, 16), Some(256), "stride");
    assert_eq!(wire::read_u32(&observed[7].2, 20), Some(0), "format");

    host_listener_task.abort();
}

/// Found live 2026-08-04 validating the test above against a REAL
/// gtk4-demo-like client (`scripts/gtk/basic_shm.py`) on the real laptop:
/// recreating a wl_buffer's protocol identity (the test above) isn't
/// enough on its own. A client attaches a buffer, commits, and then waits
/// for the compositor's own `wl_buffer.release` before it's willing to
/// reuse that memory for its next frame -- if the crash happens right
/// after that commit, the OLD compositor dies before ever sending that
/// release, and no amount of recreating the buffer's *identity* answers
/// the promise the client is actually blocked on. Confirmed live: a real
/// GTK4 client stalled forever after a clean reconnect (every object
/// recreated correctly, no fatal errors) specifically because of this.
/// This test proves the fix (`buffer_flow.rs`): a buffer still "in flight"
/// (attached + committed, no release seen) when the crash happens gets a
/// synthesized `wl_buffer.release` once it's been recreated, unblocking
/// the client the same way the earlier `wl_surface.frame` ->
/// `wl_callback.done` synthesis unblocks a stalled frame clock.
#[tokio::test]
async fn in_flight_buffer_gets_a_synthesized_release_after_reconnect() {
    let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).try_init();
    use std::os::fd::AsRawFd;
    use tokio::io::AsyncWriteExt;
    use wayland_proxy::wire;

    const GLOBALS: &[(&str, u32)] = &[("wl_compositor", 6), ("wl_shm", 1)];

    let tmp_dir =
        std::env::temp_dir().join(format!("wayland-proxy-in-flight-buffer-test-{}", std::process::id()));
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
            wayland_proxy::run_connection(gtk_proxy_side, first_host_conn, host_socket_path_for_proxy, None, wayland_proxy::clipboard::ClipboardCache::new())
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

    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let mut wl_compositor_name = None;
    let mut wl_shm_name = None;
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
                        "wl_shm" => wl_shm_name = Some(name),
                        _ => {}
                    }
                }
            } else if header.sender_id == 3 && header.opcode == 0 {
                buf.drain(..consumed);
                break 'collect;
            }
            buf.drain(..consumed);
        }
    }
    let wl_compositor_name = wl_compositor_name.expect("wl_compositor advertised");
    let wl_shm_name = wl_shm_name.expect("wl_shm advertised");

    // bind wl_compositor->4, wl_shm->9; create_surface->6.
    let mut out = Vec::new();
    let mut p = Vec::new();
    wire::put_u32(&mut p, wl_compositor_name);
    wire::put_str(&mut p, "wl_compositor");
    wire::put_u32(&mut p, 6);
    wire::put_u32(&mut p, 4);
    out.extend(wire::build_message(2, 0, &p));
    let mut p = Vec::new();
    wire::put_u32(&mut p, wl_shm_name);
    wire::put_str(&mut p, "wl_shm");
    wire::put_u32(&mut p, 1);
    wire::put_u32(&mut p, 9);
    out.extend(wire::build_message(2, 0, &p));
    let mut p = Vec::new();
    wire::put_u32(&mut p, 6);
    out.extend(wire::build_message(4, 0, &p)); // wl_compositor(4).create_surface -> wl_surface(6)
    gtk_test_side.write_all(&out).await.expect("write bind+create_surface");

    // wl_shm(9).create_pool(new_id=10, fd, size=4096) -- a REAL fd, same as
    // the recipe-replay test above: this test needs the pool to actually
    // recreate successfully, or the synthesis-time guard in
    // recover_state_after_reconnect (buffer must have a live host id)
    // would skip it, silently passing for the wrong reason.
    let backing_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(tmp_dir.join("shm_pool_backing"))
        .expect("create backing file");
    backing_file.set_len(4096).expect("size backing file");
    let mut p = Vec::new();
    wire::put_u32(&mut p, 10); // new_id (guest) -- wl_shm_pool
    wire::put_u32(&mut p, 4096); // size
    let create_pool_msg = wire::build_message(9, 0, &p);
    wayland_proxy::fdsocket::send_with_fds(gtk_test_side.as_raw_fd(), &create_pool_msg, &[backing_file.as_raw_fd()])
        .expect("send create_pool with a real fd");

    // wl_shm_pool(10).create_buffer(new_id=11, ...).
    let mut p = Vec::new();
    wire::put_u32(&mut p, 11); // new_id (guest) -- wl_buffer
    wire::put_u32(&mut p, 0); // offset
    wire::put_u32(&mut p, 64); // width
    wire::put_u32(&mut p, 64); // height
    wire::put_u32(&mut p, 256); // stride
    wire::put_u32(&mut p, 0); // format
    gtk_test_side.write_all(&wire::build_message(10, 0, &p)).await.expect("write create_buffer");

    // wl_surface(6).attach(buffer=11, x=0, y=0) [opcode 1], then
    // wl_surface(6).commit() [opcode 6] -- exactly the sequence a real
    // client's last pre-crash frame looked like on the wire (confirmed via
    // WAYLAND_DEBUG=1 against the real stall). No matching release is ever
    // sent by either fake compositor life -- that's the whole point: the
    // proxy itself must synthesize it, nothing upstream ever will.
    let mut attach_payload = Vec::new();
    wire::put_u32(&mut attach_payload, 11); // buffer
    wire::put_u32(&mut attach_payload, 0); // x
    wire::put_u32(&mut attach_payload, 0); // y
    gtk_test_side.write_all(&wire::build_message(6, 1, &attach_payload)).await.expect("write attach");
    gtk_test_side.write_all(&wire::build_message(6, 6, &[])).await.expect("write commit");

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Crash. Second life uses different global names, same as every other
    // reconnect test here.
    first_life_task.abort();
    let (second_sink, _second_sink_rx) = tokio::sync::mpsc::unbounded_channel();
    let host_listener_task = tokio::spawn(async move {
        let (second_host_accepted, _) =
            tokio::time::timeout(Duration::from_secs(3), host_listener.accept())
                .await
                .expect("proxy should reconnect within 3s")
                .expect("accept second host conn");
        serve_fake_compositor_life(second_host_accepted, GLOBALS, 101, second_sink).await;
    });

    // The proxy's own synthesized wl_buffer.release(11) -- sender_id=11
    // (the buffer's ORIGINAL, unchanged guest id), opcode=0 (wl_buffer's
    // only event) -- should be the only thing gtk_test_side receives after
    // reconnecting (no grabs were set up, and this test skipped
    // xdg_wm_base entirely, so there's no configure/ack traffic to race
    // against it).
    let msg = tokio::time::timeout(Duration::from_secs(3), read_one_message(&mut gtk_test_side))
        .await
        .expect("timed out waiting for the synthesized wl_buffer.release");
    let header = wire::MessageHeader::parse(&msg).expect("valid header");
    assert_eq!(header.sender_id, 11, "release should target the buffer's original, unchanged guest id");
    assert_eq!(header.opcode, 0, "wl_buffer.release event opcode");

    host_listener_task.abort();
}

/// The other half of the same live finding as the test above
/// (`in_flight_buffer_gets_a_synthesized_release_after_reconnect`), found
/// immediately after it: even with a synthesized `wl_buffer.release`, a
/// real GTK4 client STILL stalled forever, because its last `frame()`
/// before the crash reached the old compositor successfully (forwarded
/// normally -- unlike `frame_request_during_the_recovery_window_gets_a_synthesized_done`,
/// which covers a frame() dropped because its surface was momentarily
/// untranslatable during the narrow post-`bump_generation()` window, THIS
/// frame() was never dropped at all) and the compositor died before ever
/// sending back the matching `wl_callback.done`. This test proves the
/// `pending_frames.rs` fix: a frame() confirmed to have actually reached
/// the (soon-to-die) compositor gets a synthesized `done`+`delete_id`
/// after reconnect, the same as the already-covered dropped-frame() case,
/// just triggered from the opposite situation.
#[tokio::test]
async fn frame_forwarded_before_a_crash_gets_a_synthesized_done_after_reconnect() {
    let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).try_init();
    use tokio::io::AsyncWriteExt;
    use wayland_proxy::wire;

    const GLOBALS: &[(&str, u32)] = &[("wl_compositor", 6)];

    let tmp_dir =
        std::env::temp_dir().join(format!("wayland-proxy-forwarded-frame-test-{}", std::process::id()));
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
            wayland_proxy::run_connection(gtk_proxy_side, first_host_conn, host_socket_path_for_proxy, None, wayland_proxy::clipboard::ClipboardCache::new())
                .await
        {
            eprintln!("run_connection ended with error: {e:?}");
        }
    });

    // This time the first life's sink IS read from -- proving frame()
    // genuinely reached it, the whole point being to distinguish this
    // case from the already-covered "dropped, never reached any host" one.
    let (first_sink, mut first_sink_rx) = tokio::sync::mpsc::unbounded_channel();
    let first_life_task = tokio::spawn(serve_fake_compositor_life(first_host_accepted, GLOBALS, 1, first_sink));

    // get_registry(2), sync(3), bind wl_compositor->4, create_surface->6.
    let mut out = Vec::new();
    let mut p = Vec::new();
    wire::put_u32(&mut p, 2);
    out.extend(wire::build_message(1, 1, &p));
    let mut p = Vec::new();
    wire::put_u32(&mut p, 3);
    out.extend(wire::build_message(1, 0, &p));
    gtk_test_side.write_all(&out).await.expect("write get_registry+sync");
    read_n_messages(&mut gtk_test_side, 2).await; // global(wl_compositor) + callback.done

    let mut out = Vec::new();
    let mut p = Vec::new();
    wire::put_u32(&mut p, 1); // name
    wire::put_str(&mut p, "wl_compositor");
    wire::put_u32(&mut p, 6);
    wire::put_u32(&mut p, 4); // new_id (guest) -- wl_compositor
    out.extend(wire::build_message(2, 0, &p));
    let mut p = Vec::new();
    wire::put_u32(&mut p, 6); // new_id (guest) -- wl_surface
    out.extend(wire::build_message(4, 0, &p)); // wl_compositor(4).create_surface -> wl_surface(6)
    gtk_test_side.write_all(&out).await.expect("write bind+create_surface");

    // wl_surface(6).frame(new_id=50) [opcode 3] -- and PROVE it actually
    // reached the first life before crashing (the whole distinction this
    // test exists to cover).
    let mut p = Vec::new();
    wire::put_u32(&mut p, 50);
    gtk_test_side.write_all(&wire::build_message(6, 3, &p)).await.expect("write frame()");
    // The sink also sees the earlier bind(wl_compositor) and
    // create_surface requests ahead of frame() -- read past those to get
    // to the one this test actually cares about.
    let mut last = None;
    for _ in 0..3 {
        last = Some(
            tokio::time::timeout(Duration::from_secs(3), first_sink_rx.recv())
                .await
                .expect("timed out waiting for frame() to reach the first life")
                .expect("sink closed early"),
        );
    }
    let (_sender, opcode, _payload) = last.unwrap();
    // _sender is the surface's HOST id here (the sink observes what the
    // fake compositor received, host-space), not its guest id 6 -- opcode
    // alone is enough to confirm this is the frame() request, not the
    // earlier bind/create_surface ones.
    assert_eq!(opcode, 3, "frame() should have reached the first life on wl_surface(6)");

    // Crash immediately -- no response to frame() was ever sent.
    first_life_task.abort();
    let (second_sink, _second_sink_rx) = tokio::sync::mpsc::unbounded_channel();
    let host_listener_task = tokio::spawn(async move {
        let (second_host_accepted, _) =
            tokio::time::timeout(Duration::from_secs(3), host_listener.accept())
                .await
                .expect("proxy should reconnect within 3s")
                .expect("accept second host conn");
        serve_fake_compositor_life(second_host_accepted, GLOBALS, 101, second_sink).await;
    });

    // The proxy's own synthesized wl_callback.done(0) -- sender_id=50 (the
    // callback's ORIGINAL, unchanged guest id), opcode=0 (wl_callback's
    // only event) -- followed by wl_display.delete_id(50), same pair
    // frame_request_during_the_recovery_window_gets_a_synthesized_done
    // already proves for the dropped-frame() case.
    let messages = read_n_messages(&mut gtk_test_side, 2).await;
    let done = wire::MessageHeader::parse(&messages[0]).expect("valid header");
    assert_eq!(done.sender_id, 50, "done should target the callback's original, unchanged guest id");
    assert_eq!(done.opcode, 0, "wl_callback.done event opcode");
    let delete_id = wire::MessageHeader::parse(&messages[1]).expect("valid header");
    assert_eq!(delete_id.sender_id, 1, "delete_id is always sent from wl_display");
    assert_eq!(delete_id.opcode, 1, "wl_display.delete_id event opcode");
    assert_eq!(wire::read_u32(&messages[1][wire::HEADER_LEN..], 0), Some(50), "delete_id should free the callback's own guest id");

    host_listener_task.abort();
}

/// ADR-0006's dmabuf half: `zwp_linux_dmabuf_v1.create_params()` -> one
/// `.add()` per plane -> `.create_immed()`, mirroring
/// `wl_shm_pool_and_buffer_recipes_replay_correctly_after_reconnect` but
/// for the multi-request dmabuf dance instead of a single `create_pool`
/// call. Confirms: the proxy's retained per-plane fd replays correctly
/// against a fresh `zwp_linux_dmabuf_v1` host id (not the pool's stale
/// first-life one), the disposable intermediate params object never
/// leaks into the client-visible guest id space, and the final buffer
/// lands back on its original, unchanged guest id.
#[tokio::test]
async fn dmabuf_buffer_recipe_replays_correctly_after_reconnect() {
    let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).try_init();
    use std::os::fd::AsRawFd;
    use tokio::io::AsyncWriteExt;
    use wayland_proxy::wire;

    const GLOBALS: &[(&str, u32)] = &[("wl_compositor", 6), ("zwp_linux_dmabuf_v1", 5)];

    let tmp_dir =
        std::env::temp_dir().join(format!("wayland-proxy-dmabuf-reconnect-test-{}", std::process::id()));
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
            wayland_proxy::run_connection(gtk_proxy_side, first_host_conn, host_socket_path_for_proxy, None, wayland_proxy::clipboard::ClipboardCache::new())
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

    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let mut wl_compositor_name = None;
    let mut dmabuf_name = None;
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
                        "zwp_linux_dmabuf_v1" => dmabuf_name = Some(name),
                        _ => {}
                    }
                }
            } else if header.sender_id == 3 && header.opcode == 0 {
                buf.drain(..consumed);
                break 'collect;
            }
            buf.drain(..consumed);
        }
    }
    let wl_compositor_name = wl_compositor_name.expect("wl_compositor advertised");
    let dmabuf_name = dmabuf_name.expect("zwp_linux_dmabuf_v1 advertised");

    // bind wl_compositor->4, zwp_linux_dmabuf_v1->9; create_params->10
    // [opcode 1 on zwp_linux_dmabuf_v1].
    let mut out = Vec::new();
    let mut p = Vec::new();
    wire::put_u32(&mut p, wl_compositor_name);
    wire::put_str(&mut p, "wl_compositor");
    wire::put_u32(&mut p, 6);
    wire::put_u32(&mut p, 4);
    out.extend(wire::build_message(2, 0, &p));
    let mut p = Vec::new();
    wire::put_u32(&mut p, dmabuf_name);
    wire::put_str(&mut p, "zwp_linux_dmabuf_v1");
    wire::put_u32(&mut p, 5);
    wire::put_u32(&mut p, 9);
    out.extend(wire::build_message(2, 0, &p));
    let mut p = Vec::new();
    wire::put_u32(&mut p, 10); // new_id (guest) -- zwp_linux_buffer_params_v1
    out.extend(wire::build_message(9, 1, &p)); // zwp_linux_dmabuf_v1(9).create_params -> params(10)
    gtk_test_side.write_all(&out).await.expect("write bind+create_params");

    // params(10).add(fd, plane_idx=0, offset=0, stride=256,
    // modifier_hi=0xAABBCCDD, modifier_lo=0xEEFF0011) [opcode 1] -- a REAL
    // fd via SCM_RIGHTS, same reasoning as the wl_shm recipe-replay test:
    // this test needs the plane to actually retain and replay, or the
    // synthesis-time guard in recover_state_after_reconnect (the dmabuf
    // global must have a live host id) would just skip it silently.
    let backing_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(tmp_dir.join("dmabuf_plane_backing"))
        .expect("create backing file");
    backing_file.set_len(4096).expect("size backing file");
    let mut p = Vec::new();
    wire::put_u32(&mut p, 0); // plane_idx
    wire::put_u32(&mut p, 0); // offset
    wire::put_u32(&mut p, 256); // stride
    wire::put_u32(&mut p, 0xAABBCCDD); // modifier_hi
    wire::put_u32(&mut p, 0xEEFF0011); // modifier_lo
    let add_msg = wire::build_message(10, 1, &p);
    wayland_proxy::fdsocket::send_with_fds(gtk_test_side.as_raw_fd(), &add_msg, &[backing_file.as_raw_fd()])
        .expect("send add() with a real fd");

    // params(10).create_immed(new_id=11, width=64, height=64, format=0,
    // flags=0) [opcode 3].
    let mut p = Vec::new();
    wire::put_u32(&mut p, 11); // new_id (guest) -- wl_buffer
    wire::put_u32(&mut p, 64); // width
    wire::put_u32(&mut p, 64); // height
    wire::put_u32(&mut p, 0); // format
    wire::put_u32(&mut p, 0); // flags
    gtk_test_side.write_all(&wire::build_message(10, 3, &p)).await.expect("write create_immed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Crash. Second life uses different global names, same as every
    // other reconnect test here.
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

    // Recovery replays exactly 5 requests, in order: bind(wl_compositor),
    // bind(zwp_linux_dmabuf_v1), create_params, add, create_immed -- the
    // disposable params object itself never gets its own recipe (see
    // DmabufBuffer's own doc comment in recreation.rs), so there's no 6th
    // replay for it.
    let mut observed = Vec::new();
    for i in 0..5 {
        let (sender, opcode, payload) = tokio::time::timeout(Duration::from_secs(3), second_sink_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for recreation request #{i}, got so far: {observed:?}"))
            .expect("sink closed early");
        observed.push((sender, opcode, payload));
    }

    fn bind_new_id(payload: &[u8]) -> u32 {
        let (_, next) = wire::read_str(payload, 4).expect("interface string");
        wire::read_u32(payload, next + 4).expect("new_id") // next+4 skips version
    }

    let iface = wire::read_str(&observed[1].2, 4).unwrap().0;
    assert_eq!(iface, "zwp_linux_dmabuf_v1", "second bind should be the dmabuf global, matching the client's own bind order");
    let recreated_dmabuf_host_id = bind_new_id(&observed[1].2);

    // create_params: sent to the freshly re-bound dmabuf global.
    assert_eq!(observed[2].0, recreated_dmabuf_host_id, "create_params should target the dmabuf global's freshly recreated host id");
    assert_eq!(observed[2].1, 1, "create_params opcode");
    let recreated_params_host_id = wire::read_u32(&observed[2].2, 0).unwrap();

    // add: sent to the freshly created params object, carrying the
    // plane's originally-recorded plane_idx/offset/stride/modifier.
    assert_eq!(observed[3].0, recreated_params_host_id, "add should target the freshly created params object");
    assert_eq!(observed[3].1, 1, "add opcode");
    assert_eq!(wire::read_u32(&observed[3].2, 0), Some(0), "plane_idx");
    assert_eq!(wire::read_u32(&observed[3].2, 4), Some(0), "offset");
    assert_eq!(wire::read_u32(&observed[3].2, 8), Some(256), "stride");
    assert_eq!(wire::read_u32(&observed[3].2, 12), Some(0xAABBCCDD), "modifier_hi");
    assert_eq!(wire::read_u32(&observed[3].2, 16), Some(0xEEFF0011), "modifier_lo");

    // create_immed: sent to the SAME params object, carrying the
    // buffer's originally-recorded width/height/format/flags.
    assert_eq!(observed[4].0, recreated_params_host_id, "create_immed should target the same params object as add");
    assert_eq!(observed[4].1, 3, "create_immed opcode");
    assert_eq!(wire::read_u32(&observed[4].2, 4), Some(64), "width");
    assert_eq!(wire::read_u32(&observed[4].2, 8), Some(64), "height");
    assert_eq!(wire::read_u32(&observed[4].2, 12), Some(0), "format");
    assert_eq!(wire::read_u32(&observed[4].2, 16), Some(0), "flags");

    host_listener_task.abort();
}

/// Found live 2026-08-04: the phantom-mapping rollback proven by
/// `create_pool_on_a_stale_wl_shm_does_not_leave_a_phantom_mapping`
/// above was itself incomplete -- it correctly forgets the GUEST-side
/// mapping for a dropped new_id-bearing message, but left the HOST id
/// counter permanently advanced past a number the real host never
/// actually saw. A real desktop crash test hit exactly this: two
/// dropped `wp_presentation.feedback` requests (stale surfaces, the
/// same "sender has no translation" path) right after a reconnect
/// burned two host ids, and the client's next entirely ordinary
/// `wl_shm.create_pool` -- landing on a host id two past what the real
/// compositor's own bookkeeping expected -- got rejected with a fatal
/// `wl_display.error("invalid arguments for wl_shm#N.create_pool")`.
/// `apt-get source wayland` + `strace` (see
/// docs/adr/adr-0006-recreate-buffers-via-fd-handover.md's "Open issue"
/// section for the full investigation) confirmed this precisely:
/// libwayland-server's own `wl_map_reserve_new` requires a client's own
/// new_ids to be gapless, rejecting anything else as "not a valid new
/// object id". This test proves the fix (`ShadowTable::unallocate_host_id`):
/// a legitimate new_id-bearing request sent right after a dropped one
/// gets the host id the dropped one *would* have used, not one past it.
#[tokio::test]
async fn dropped_new_id_message_does_not_burn_a_host_id() {
    let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).try_init();
    use tokio::io::AsyncWriteExt;
    use wayland_proxy::wire;

    const GLOBALS: &[(&str, u32)] = &[("wl_shm", 1)];
    const SECOND_LIFE_GLOBALS: &[(&str, u32)] = &[("wl_compositor", 6)];

    let tmp_dir =
        std::env::temp_dir().join(format!("wayland-proxy-host-id-gap-test-{}", std::process::id()));
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
            wayland_proxy::run_connection(gtk_proxy_side, first_host_conn, host_socket_path_for_proxy, None, wayland_proxy::clipboard::ClipboardCache::new())
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

    // Crash and reconnect. Second life advertises wl_compositor (NOT
    // wl_shm) -- wl_shm stays stale, same as the phantom-mapping test
    // above, but this time there's something legitimate to bind
    // afterward to observe the host id it actually lands on.
    first_life_task.abort();
    let (second_sink, mut second_sink_rx) = tokio::sync::mpsc::unbounded_channel();
    let host_listener_task = tokio::spawn(async move {
        let (second_host_accepted, _) =
            tokio::time::timeout(Duration::from_secs(3), host_listener.accept())
                .await
                .expect("proxy should reconnect within 3s")
                .expect("accept second host conn");
        serve_fake_compositor_life(second_host_accepted, SECOND_LIFE_GLOBALS, 101, second_sink).await;
    });

    // Give recover_state_after_reconnect time to finish its own
    // get_registry/sync round trip (registry_host_id=2, sync_host_id=3,
    // wl_shm's own Global recipe fails to recreate since this life
    // never advertises it -- so next_host_id is 4 once this settles,
    // matching the live bug's own first-allocation-after-reconnect
    // value) before sending anything.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // wl_shm(4).create_pool(new_id=5, size) on the now-stale wl_shm --
    // dropped, same as the phantom-mapping test above, but THIS time
    // proving the host id it allocated (would have been 4) gets given
    // back, not burned.
    let mut p = Vec::new();
    wire::put_u32(&mut p, 5); // new_id (guest) -- would-be wl_shm_pool
    wire::put_u32(&mut p, 4096); // size
    gtk_test_side.write_all(&wire::build_message(4, 0, &p)).await.expect("write create_pool");

    let forwarded = tokio::time::timeout(Duration::from_millis(300), second_sink_rx.recv()).await;
    assert!(forwarded.is_err(), "create_pool on a stale wl_shm must never reach the new compositor, but got: {forwarded:?}");

    // The actual proof: bind wl_compositor (legitimate, guest 6) via the
    // freshly re-fetched registry -- if the earlier drop burned a host
    // id, this bind lands on host id 5; if it was correctly given back,
    // this bind reuses host id 4.
    let mut p = Vec::new();
    wire::put_u32(&mut p, 101); // name -- SECOND_LIFE_GLOBALS' first (and only) entry
    wire::put_str(&mut p, "wl_compositor");
    wire::put_u32(&mut p, 6);
    wire::put_u32(&mut p, 6); // new_id (guest) -- wl_compositor
    gtk_test_side.write_all(&wire::build_message(2, 0, &p)).await.expect("write bind wl_compositor");

    let (_sender, opcode, payload) = tokio::time::timeout(Duration::from_secs(3), second_sink_rx.recv())
        .await
        .expect("timed out waiting for the wl_compositor bind to reach the second life")
        .expect("sink closed early");
    assert_eq!(opcode, 0, "bind opcode");
    fn bind_new_id(payload: &[u8]) -> u32 {
        let (_, next) = wire::read_str(payload, 4).expect("interface string");
        wire::read_u32(payload, next + 4).expect("new_id") // next+4 skips version
    }
    assert_eq!(
        bind_new_id(&payload),
        4,
        "the dropped create_pool's host id (4) should have been given back, not burned -- \
         a real desktop crash hit exactly this gap once live, see ShadowTable::unallocate_host_id's own doc comment"
    );

    host_listener_task.abort();
}

/// Reads one message off `stream`, also returning any fds that rode
/// alongside it via `SCM_RIGHTS` -- plain `AsyncReadExt::read` (as
/// `read_one_message` uses) silently discards ancillary data, which loses
/// exactly the fd a clipboard-tee test needs to inspect. Same
/// `try_io`/`recv_with_fds` pattern as `Conn::fill` and
/// `serve_fake_compositor_life` use for the same reason.
async fn read_one_message_with_fds(stream: &mut tokio::net::UnixStream) -> (Vec<u8>, Vec<OwnedFd>) {
    use std::os::fd::AsRawFd;
    let mut buf = Vec::new();
    let mut fds = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        if let Some((msg, _consumed)) = wayland_proxy::wire::take_message(&buf) {
            return (msg.to_vec(), fds);
        }
        stream.readable().await.expect("readable");
        let raw_fd = stream.as_raw_fd();
        match stream.try_io(tokio::io::Interest::READABLE, || {
            wayland_proxy::fdsocket::recv_with_fds(raw_fd, &mut tmp).map_err(std::io::Error::from)
        }) {
            Ok((0, _)) => panic!("closed before a full message arrived"),
            Ok((n, new_fds)) => {
                buf.extend_from_slice(&tmp[..n]);
                fds.extend(new_fds);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => panic!("read error: {e}"),
        }
    }
}

/// End-to-end proof of the clipboard tee (see docs/adr/adr-0009-clipboard-
/// persistence.md and src/clipboard.rs): a fake compositor sends
/// `wl_data_source.send` carrying a real pipe fd, exactly as Mutter does
/// once a client has set itself as clipboard owner. The client must
/// receive a *different* fd than the one the compositor sent (the tee
/// substitutes its own pipe); writing into that substitute fd must both
/// reach the real one AND end up in the shared clipboard cache.
#[tokio::test]
async fn wl_data_source_send_is_teed_into_the_clipboard_cache() {
    let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).try_init();
    use tokio::io::AsyncWriteExt;
    use wayland_proxy::wire;

    let (gtk_proxy_side, mut gtk_test_side) = tokio::net::UnixStream::pair().expect("pair");
    let (host_proxy_side, mut host_test_side) = tokio::net::UnixStream::pair().expect("pair");

    let cache = wayland_proxy::clipboard::ClipboardCache::new();
    let cache_for_proxy = cache.clone();
    tokio::spawn(async move {
        let unused_path = std::path::PathBuf::from("/nonexistent/unused-in-this-test");
        if let Err(e) =
            wayland_proxy::run_connection(gtk_proxy_side, host_proxy_side, unused_path, None, cache_for_proxy)
                .await
        {
            eprintln!("run_connection ended with error: {e:?}");
        }
    });

    // get_registry(2), then bind wl_data_device_manager(name=1) -> guest id
    // 3 -- no wl_registry.global roundtrip needed, resolve_child_interface
    // only needs the interface name string bind() itself carries.
    let mut p = Vec::new();
    wire::put_u32(&mut p, 2);
    gtk_test_side.write_all(&wire::build_message(1, 1, &p)).await.expect("write get_registry");
    let _ = read_one_message(&mut host_test_side).await; // drain forwarded get_registry

    let mut p = Vec::new();
    wire::put_u32(&mut p, 1);
    wire::put_str(&mut p, "wl_data_device_manager");
    wire::put_u32(&mut p, 3);
    wire::put_u32(&mut p, 3); // new_id (guest) -- wl_data_device_manager
    gtk_test_side.write_all(&wire::build_message(2, 0, &p)).await.expect("write bind");
    let _ = read_one_message(&mut host_test_side).await; // drain forwarded bind

    // wl_data_device_manager(3).create_data_source(new_id=4) -- learn the
    // HOST id the proxy allocated, since the fake compositor below has to
    // address its send() event to that, not the guest one.
    let mut p = Vec::new();
    wire::put_u32(&mut p, 4); // new_id (guest) -- wl_data_source
    gtk_test_side.write_all(&wire::build_message(3, 0, &p)).await.expect("write create_data_source");
    let forwarded = read_one_message(&mut host_test_side).await;
    let host_data_source_id = u32::from_ne_bytes(forwarded[8..12].try_into().unwrap());

    // The fake compositor's own pipe -- exactly what Mutter would create:
    // it keeps the read end, hands the write end to the (source) client via
    // send()'s fd argument. required for clipboard copy: this is the real
    // fd the tee must relay bytes into unchanged.
    let (mutter_read, mutter_write) = nix::unistd::pipe().expect("create mutter-side pipe");

    const MIME_TYPE: &str = "text/plain";
    let mut p = Vec::new();
    wire::put_str(&mut p, MIME_TYPE);
    let send_msg = wire::build_message(host_data_source_id, 1, &p); // wl_data_source.send
    wayland_proxy::fdsocket::send_with_fds(
        std::os::fd::AsRawFd::as_raw_fd(&host_test_side),
        &send_msg,
        &[std::os::fd::AsRawFd::as_raw_fd(&mutter_write)],
    )
    .expect("send wl_data_source.send with a real fd");
    drop(mutter_write); // our copy: sendmsg already dup'd it onto the wire

    // Client's side: must receive a DIFFERENT fd than mutter_write's
    // (the tee's whole point), carrying the same mime_type unchanged.
    let (client_msg, mut client_fds) = tokio::time::timeout(Duration::from_secs(5), read_one_message_with_fds(&mut gtk_test_side))
        .await
        .expect("timed out waiting for wl_data_source.send to reach the client");
    let client_header = wire::MessageHeader::parse(&client_msg).expect("valid header");
    assert_eq!(client_header.opcode, 1, "wl_data_source.send event opcode");
    let (received_mime, _) = wire::read_str(&client_msg[wire::HEADER_LEN..], 0).expect("mime_type string");
    assert_eq!(received_mime, MIME_TYPE);
    assert_eq!(client_fds.len(), 1, "send() carries exactly one fd");
    let client_facing_fd = client_fds.remove(0);

    // Client writes the clipboard content into its (substitute) fd, then
    // closes it -- exactly what a real copying app's toolkit does once
    // it's written everything for this mime type.
    const CONTENT: &[u8] = b"tee'd clipboard content";
    {
        let mut pipe = std::fs::File::from(client_facing_fd);
        std::io::Write::write_all(&mut pipe, CONTENT).expect("client write into substitute fd");
    } // dropped here -- closes the fd, signalling EOF to the tee's pump

    // Real fd must receive the SAME bytes, unmodified, then EOF -- proves
    // the relay side of the tee, not just that a substitute fd was handed
    // out. read_to_end() blocks (this is a plain pipe, no async wrapper
    // needed for a test) -- spawn_blocking so it doesn't starve the
    // single-threaded test runtime the tee's own pump() task needs to run
    // on to ever close its end and unblock this read.
    let relayed = tokio::task::spawn_blocking(move || {
        let mut real_side = std::fs::File::from(mutter_read);
        let mut relayed = Vec::new();
        std::io::Read::read_to_end(&mut real_side, &mut relayed).expect("read relayed bytes");
        relayed
    })
    .await
    .expect("spawn_blocking join");
    assert_eq!(relayed, CONTENT, "the real fd must see exactly what the client wrote, byte for byte");

    // By the time the real fd's read hit EOF, ClipboardCache::store already
    // ran in the same task, strictly before it (see clipboard.rs's pump():
    // store() happens before `to_host` drops, which is what closes
    // mutter_read's peer) -- no extra sleep/retry needed here.
    assert_eq!(
        cache.get(MIME_TYPE).as_deref(),
        Some(CONTENT),
        "the tee must cache the same bytes it relayed"
    );
}

/// End-to-end proof of the lazy-splice reclaim (see docs/adr/adr-0009-
/// clipboard-persistence.md and attempt_clipboard_splice in src/lib.rs): a
/// client copies something (tee'd into the cache), the compositor crashes
/// and restarts, and the first real input serial the client sees
/// afterward is enough for the proxy to reclaim the clipboard on its own,
/// entirely synthetic, data source -- proven by answering a (fake)
/// compositor's own `wl_data_source.send` with the exact bytes cached
/// before the crash.
#[tokio::test]
async fn clipboard_is_reclaimed_on_first_real_serial_after_reconnect() {
    let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).try_init();
    use tokio::io::AsyncWriteExt;
    use wayland_proxy::wire;

    let tmp_dir =
        std::env::temp_dir().join(format!("wayland-proxy-clipboard-reclaim-test-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("create temp dir");
    let host_socket_path = tmp_dir.join("host.sock");
    let _ = std::fs::remove_file(&host_socket_path);
    let host_listener = tokio::net::UnixListener::bind(&host_socket_path).expect("bind host");

    let (gtk_proxy_side, mut gtk_test_side) = tokio::net::UnixStream::pair().expect("pair");
    let first_host_conn =
        tokio::net::UnixStream::connect(&host_socket_path).await.expect("initial host connect");
    let (mut first_host, _) = host_listener.accept().await.expect("accept first host conn");

    let cache = wayland_proxy::clipboard::ClipboardCache::new();
    let host_socket_path_for_proxy = host_socket_path.clone();
    let cache_for_proxy = cache.clone();
    tokio::spawn(async move {
        if let Err(e) = wayland_proxy::run_connection(
            gtk_proxy_side,
            first_host_conn,
            host_socket_path_for_proxy,
            None,
            cache_for_proxy,
        )
        .await
        {
            eprintln!("run_connection ended with error: {e:?}");
        }
    });

    // --- First life ---
    // Host ids are deterministic (the proxy's allocator starts at 2 and
    // increments per new_id, in send order) -- no need to read anything
    // back to know them: get_registry->2, bind(wl_seat)->3,
    // bind(wl_data_device_manager)->4, get_pointer->5,
    // create_data_source->6, get_data_device->7. wl_registry.bind's real
    // signature is [Uint, Str, Uint, NewId] (see ADR-0007) -- binding
    // doesn't depend on having seen a matching wl_registry.global event
    // first, so none is sent here.
    let mut out = Vec::new();
    let mut p = Vec::new();
    wire::put_u32(&mut p, 2);
    out.extend(wire::build_message(1, 1, &p)); // get_registry -> host 2
    p.clear();
    wire::put_u32(&mut p, 1); // name
    wire::put_str(&mut p, "wl_seat");
    wire::put_u32(&mut p, 8);
    wire::put_u32(&mut p, 3); // guest id
    out.extend(wire::build_message(2, 0, &p)); // bind wl_seat -> host 3
    p.clear();
    wire::put_u32(&mut p, 2); // name
    wire::put_str(&mut p, "wl_data_device_manager");
    wire::put_u32(&mut p, 3);
    wire::put_u32(&mut p, 4); // guest id
    out.extend(wire::build_message(2, 0, &p)); // bind ddm -> host 4
    p.clear();
    wire::put_u32(&mut p, 5); // guest id
    out.extend(wire::build_message(3, 0, &p)); // wl_seat.get_pointer -> host 5
    p.clear();
    wire::put_u32(&mut p, 6); // guest id
    out.extend(wire::build_message(4, 0, &p)); // create_data_source -> host 6
    p.clear();
    const MIME_TYPE: &str = "text/plain";
    wire::put_str(&mut p, MIME_TYPE);
    out.extend(wire::build_message(6, 0, &p)); // wl_data_source.offer
    p.clear();
    wire::put_u32(&mut p, 7); // guest id
    wire::put_u32(&mut p, 3); // seat
    out.extend(wire::build_message(4, 1, &p)); // get_data_device -> host 7
    p.clear();
    wire::put_u32(&mut p, 6); // source
    wire::put_u32(&mut p, 0); // serial -- the fake host never validates it
    out.extend(wire::build_message(7, 1, &p)); // set_selection
    gtk_test_side.write_all(&out).await.expect("write first-life burst");

    // Must confirm the whole burst reached the fake host BEFORE sending
    // the host's own send() below -- otherwise it's a race between this
    // task's two independent writes (one via gtk_test_side, one raw on
    // first_host) and the proxy could process the host's send() first,
    // finding no wl_data_source (id 6) yet mapped at all.
    let _ = read_n_messages(&mut first_host, 8).await;

    // Fake host eagerly fetches (mirroring Mutter's own real behavior),
    // exactly like wl_data_source_send_is_teed_into_the_clipboard_cache.
    let (mutter_read, mutter_write) = nix::unistd::pipe().expect("create mutter-side pipe");
    let mut p = Vec::new();
    wire::put_str(&mut p, MIME_TYPE);
    let send_msg = wire::build_message(6, 1, &p); // wl_data_source(host 6).send
    wayland_proxy::fdsocket::send_with_fds(
        std::os::fd::AsRawFd::as_raw_fd(&first_host),
        &send_msg,
        &[std::os::fd::AsRawFd::as_raw_fd(&mutter_write)],
    )
    .expect("send wl_data_source.send with a real fd");
    drop(mutter_write);

    let (_client_msg, mut client_fds) =
        tokio::time::timeout(Duration::from_secs(5), read_one_message_with_fds(&mut gtk_test_side))
            .await
            .expect("timed out waiting for wl_data_source.send to reach the client");
    let client_facing_fd = client_fds.remove(0);
    const CONTENT: &[u8] = b"clipboard content that must survive the crash";
    {
        let mut pipe = std::fs::File::from(client_facing_fd);
        std::io::Write::write_all(&mut pipe, CONTENT).expect("client write into substitute fd");
    }
    tokio::task::spawn_blocking(move || {
        let mut real_side = std::fs::File::from(mutter_read);
        let mut relayed = Vec::new();
        std::io::Read::read_to_end(&mut real_side, &mut relayed).expect("read relayed bytes");
        relayed
    })
    .await
    .expect("spawn_blocking join");
    assert_eq!(
        cache.get(MIME_TYPE).as_deref(),
        Some(CONTENT),
        "cache must be populated before the crash for this test to prove anything"
    );

    // --- Crash. Second life. ---
    drop(first_host);

    let (mut second_host, _) =
        tokio::time::timeout(Duration::from_secs(3), host_listener.accept())
            .await
            .expect("proxy should reconnect within 3s")
            .expect("accept second host conn");

    // recover_state_after_reconnect's own internal get_registry(2)+sync(3)
    // -- host ids restart at 2 after bump_generation(). Answer with the
    // globals it needs: wl_seat (a Recreatable::Global the client bound)
    // and wl_data_device_manager (not part of the graph, but
    // attempt_clipboard_splice needs its name/version cached for later).
    // read_n_messages, not two read_one_message calls -- both requests are
    // written back-to-back with no delay and reliably land in one read(),
    // and read_one_message's own fresh-buffer-per-call would silently
    // lose whichever one didn't come first (see its own doc comment).
    let msgs = read_n_messages(&mut second_host, 2).await;
    assert_eq!(wire::MessageHeader::parse(&msgs[0]).unwrap().opcode, 1, "get_registry");
    let registry_id = u32::from_ne_bytes(msgs[0][8..12].try_into().unwrap());
    let sync_id = u32::from_ne_bytes(msgs[1][8..12].try_into().unwrap());

    let mut out = Vec::new();
    let mut p = Vec::new();
    wire::put_u32(&mut p, 201);
    wire::put_str(&mut p, "wl_seat");
    wire::put_u32(&mut p, 8);
    out.extend(wire::build_message(registry_id, 0, &p));
    p.clear();
    wire::put_u32(&mut p, 202);
    wire::put_str(&mut p, "wl_data_device_manager");
    wire::put_u32(&mut p, 3);
    out.extend(wire::build_message(registry_id, 0, &p));
    p.clear();
    wire::put_u32(&mut p, 0);
    out.extend(wire::build_message(sync_id, 0, &p));
    second_host.write_all(&out).await.expect("answer registry");

    // Recovery replays: bind(wl_seat) then get_pointer (Recreatable::
    // SeatDevice) -- wl_data_device_manager is NOT replayed automatically,
    // that's attempt_clipboard_splice's own job, later. read_n_messages,
    // same back-to-back-writes reasoning as the registry answer above.
    let msgs = read_n_messages(&mut second_host, 2).await;
    let header = wire::MessageHeader::parse(&msgs[0]).unwrap();
    assert_eq!((header.sender_id, header.opcode), (registry_id, 0), "bind wl_seat");
    let seat_host_id2 = u32::from_ne_bytes(msgs[0][msgs[0].len() - 4..].try_into().unwrap());

    let header = wire::MessageHeader::parse(&msgs[1]).unwrap();
    assert_eq!((header.sender_id, header.opcode), (seat_host_id2, 0), "get_pointer");
    let pointer_host_id2 = u32::from_ne_bytes(msgs[1][8..12].try_into().unwrap());

    // Client stays quiet from here -- everything else is the proxy's own
    // synthetic traffic, triggered by the pointer event below.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The trigger: a real, compositor-issued serial, for something
    // completely unrelated to clipboard (a button press). button/state
    // are plain uints -- no object argument, so no shadow-table
    // translation is needed for this message to go through.
    const RECLAIM_SERIAL: u32 = 4242;
    let mut p = Vec::new();
    wire::put_u32(&mut p, RECLAIM_SERIAL);
    wire::put_u32(&mut p, 0); // time
    wire::put_u32(&mut p, 272); // button: BTN_LEFT
    wire::put_u32(&mut p, 1); // state: pressed
    second_host.write_all(&wire::build_message(pointer_host_id2, 3, &p)).await.expect("write wl_pointer.button");

    // The splice: bind(ddm), create_data_source, offer, get_data_device,
    // set_selection -- all synthetic, all sent by the proxy on the
    // client's behalf, back-to-back -- read_n_messages, same reasoning.
    let msgs = read_n_messages(&mut second_host, 5).await;

    let header = wire::MessageHeader::parse(&msgs[0]).unwrap();
    assert_eq!((header.sender_id, header.opcode), (registry_id, 0), "splice: bind wl_data_device_manager");
    let (iface, _) = wire::read_str(&msgs[0][wire::HEADER_LEN..], 4).unwrap();
    assert_eq!(iface, "wl_data_device_manager");
    let ddm_host_id2 = u32::from_ne_bytes(msgs[0][msgs[0].len() - 4..].try_into().unwrap());

    let header = wire::MessageHeader::parse(&msgs[1]).unwrap();
    assert_eq!((header.sender_id, header.opcode), (ddm_host_id2, 0), "splice: create_data_source");
    let source_host_id2 = u32::from_ne_bytes(msgs[1][8..12].try_into().unwrap());

    let header = wire::MessageHeader::parse(&msgs[2]).unwrap();
    assert_eq!((header.sender_id, header.opcode), (source_host_id2, 0), "splice: offer");
    let (offered_mime, _) = wire::read_str(&msgs[2][wire::HEADER_LEN..], 0).unwrap();
    assert_eq!(offered_mime, MIME_TYPE);

    let header = wire::MessageHeader::parse(&msgs[3]).unwrap();
    assert_eq!((header.sender_id, header.opcode), (ddm_host_id2, 1), "splice: get_data_device");
    let device_host_id2 = u32::from_ne_bytes(msgs[3][8..12].try_into().unwrap());
    let seat_arg = u32::from_ne_bytes(msgs[3][12..16].try_into().unwrap());
    assert_eq!(seat_arg, seat_host_id2, "get_data_device must use the recreated (second-life) seat");

    let header = wire::MessageHeader::parse(&msgs[4]).unwrap();
    assert_eq!((header.sender_id, header.opcode), (device_host_id2, 1), "splice: set_selection");
    let source_arg = u32::from_ne_bytes(msgs[4][8..12].try_into().unwrap());
    let serial_arg = u32::from_ne_bytes(msgs[4][12..16].try_into().unwrap());
    assert_eq!(source_arg, source_host_id2);
    assert_eq!(
        serial_arg, RECLAIM_SERIAL,
        "set_selection must reuse the real serial the button press carried, per ADR-0009's finding \
         that a fabricated one is rejected but a real one (even borrowed for something else) isn't"
    );

    // Finally: the fake host (mirroring Mutter's own eager-fetch, exactly
    // as it did in the first life) asks the proxy's synthetic source for
    // the content -- must get back exactly what was cached before the
    // crash, from an entirely different (second-life) connection.
    let (verify_read, verify_write) = nix::unistd::pipe().expect("create verify pipe");
    let mut p = Vec::new();
    wire::put_str(&mut p, MIME_TYPE);
    let send_msg = wire::build_message(source_host_id2, 1, &p);
    wayland_proxy::fdsocket::send_with_fds(
        std::os::fd::AsRawFd::as_raw_fd(&second_host),
        &send_msg,
        &[std::os::fd::AsRawFd::as_raw_fd(&verify_write)],
    )
    .expect("send wl_data_source.send to the synthetic source");
    drop(verify_write);

    let recovered = tokio::task::spawn_blocking(move || {
        let mut f = std::fs::File::from(verify_read);
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut f, &mut bytes).expect("read reclaimed bytes");
        bytes
    })
    .await
    .expect("spawn_blocking join");
    assert_eq!(
        recovered, CONTENT,
        "the reclaimed clipboard must serve exactly what was cached before the crash"
    );
}

/// Plays "compositor" for one paste, end to end: sends
/// `wl_data_device.data_offer` + `wl_data_offer.offer` +
/// `wl_data_device.selection` to `host` (so the real proxy forwards a
/// fresh offer to `client`), reads `client`'s resulting
/// `wl_data_offer.receive(mime, fd)` request back off `host`, bridges
/// that fd onto `source_host`/`source_host_id`'s own connection as a
/// real `wl_data_source.send` event (exactly what Mutter does -- the
/// data never touches the compositor, it's relayed peer-to-peer through
/// whichever fd the pasting client handed over), then blocking-reads the
/// bytes that eventually arrive back on `client`'s own kept pipe read
/// end. `offer_host_id` just needs to be unique on this connection
/// generation -- picked far from the low sequential ids real requests
/// use so it never collides with one.
async fn paste_via_compositor(
    host: &mut tokio::net::UnixStream,
    client: &mut tokio::net::UnixStream,
    device_host_id: u32,
    offer_host_id: u32,
    mime_type: &str,
    source_host: &mut tokio::net::UnixStream,
    source_host_id: u32,
) -> Vec<u8> {
    use tokio::io::AsyncWriteExt;
    use wayland_proxy::wire;

    let mut out = Vec::new();
    let mut p = Vec::new();
    wire::put_u32(&mut p, offer_host_id);
    out.extend(wire::build_message(device_host_id, 0, &p)); // wl_data_device.data_offer
    p.clear();
    wire::put_str(&mut p, mime_type);
    out.extend(wire::build_message(offer_host_id, 0, &p)); // wl_data_offer.offer
    p.clear();
    wire::put_u32(&mut p, offer_host_id);
    out.extend(wire::build_message(device_host_id, 5, &p)); // wl_data_device.selection
    host.write_all(&out).await.expect("write data_offer+offer+selection");

    // Client observes: data_offer(new guest id), offer(mime), selection(id).
    let msgs = read_n_messages(client, 3).await;
    let offer_guest_id = u32::from_ne_bytes(msgs[0][wire::HEADER_LEN..wire::HEADER_LEN + 4].try_into().unwrap());
    let header = wire::MessageHeader::parse(&msgs[1]).unwrap();
    assert_eq!(header.sender_id, offer_guest_id, "offer() must target the just-created data_offer");
    let (offered_mime, _) = wire::read_str(&msgs[1][wire::HEADER_LEN..], 0).unwrap();
    assert_eq!(offered_mime, mime_type);

    // Client "pastes": creates its own pipe, keeps the read end, sends
    // the write end via wl_data_offer(offer_guest_id).receive(mime, fd)
    // -- exactly what a real pasting client does.
    let (paste_read, paste_write) = nix::unistd::pipe().expect("create paste pipe");
    let mut p = Vec::new();
    wire::put_str(&mut p, mime_type);
    wayland_proxy::fdsocket::send_with_fds(
        std::os::fd::AsRawFd::as_raw_fd(client),
        &wire::build_message(offer_guest_id, 1, &p), // wl_data_offer.receive -- opcode 1, NOT 0 (0 is accept)
        &[std::os::fd::AsRawFd::as_raw_fd(&paste_write)],
    )
    .expect("send receive()");
    drop(paste_write); // our copy: sendmsg already dup'd it onto the wire

    // Compositor relays the request: reads receive()'s fd back off `host`.
    let (_msg, mut fds) = read_one_message_with_fds(host).await;
    let received_fd = fds.pop().expect("receive() must carry exactly one fd");

    // Compositor hands that fd to the source, mirroring Mutter exactly:
    // the data flows source-client -> pasting-client, never through the
    // compositor itself.
    let mut p = Vec::new();
    wire::put_str(&mut p, mime_type);
    wayland_proxy::fdsocket::send_with_fds(
        std::os::fd::AsRawFd::as_raw_fd(source_host),
        &wire::build_message(source_host_id, 1, &p), // wl_data_source.send
        &[std::os::fd::AsRawFd::as_raw_fd(&received_fd)],
    )
    .expect("send wl_data_source.send");
    drop(received_fd);

    // Whatever answers this (a real client's own write, or
    // attempt_clipboard_splice's cache-backed answer for a reclaimed
    // synthetic source) eventually closes the real fd, giving EOF here.
    tokio::task::spawn_blocking(move || {
        let mut f = std::fs::File::from(paste_read);
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut f, &mut bytes).expect("read pasted bytes");
        bytes
    })
    .await
    .expect("spawn_blocking join")
}

/// The other half of a *real* (non-reclaimed) paste: reads the
/// wl_data_source.send the proxy's tee forwards to a genuine source
/// client (client_side is that client's own gtk-facing socket) and
/// writes real content into the substitute fd it hands back -- exactly
/// what a real copying app's toolkit does. Must run concurrently with
/// paste_via_compositor (via tokio::join!), not before or after it:
/// paste_via_compositor's own final read blocks until this write (and
/// the resulting close) actually happens.
async fn write_real_source_content(client_side: &mut tokio::net::UnixStream, content: &[u8]) {
    let (_msg, mut fds) = read_one_message_with_fds(client_side).await;
    let fd = fds.pop().expect("wl_data_source.send must carry a fd");
    let mut pipe = std::fs::File::from(fd);
    std::io::Write::write_all(&mut pipe, content).expect("write real source content");
}

/// End-to-end proof of the exact scenario a hand-designed integration
/// test for this would use: two separate real clients (not a fake
/// compositor standing in for one side, per the earlier, narrower tee/
/// splice tests). Copy from A, paste into B; crash the shared
/// compositor; paste the ORIGINAL text into B again (proving the
/// reclaim survives, not just that a mime type got cached); copy NEW
/// text from A; paste that into B too (proving normal copy/paste still
/// works cleanly afterward, and that the reclaimed synthetic source's
/// `cancelled` handling -- untested until now -- doesn't leave anything
/// stuck).
#[tokio::test]
async fn two_real_clients_copy_paste_survives_a_crash_and_keeps_working_after() {
    let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).try_init();
    use tokio::io::AsyncWriteExt;
    use wayland_proxy::wire;

    const MIME_TYPE: &str = "text/plain";
    const ORIGINAL_TEXT: &[u8] = b"original clipboard content, copied before the crash";
    const NEW_TEXT: &[u8] = b"new clipboard content, copied after recovery";

    let cache = wayland_proxy::clipboard::ClipboardCache::new();

    // --- Client A: connects, binds wl_seat+get_pointer (needed later to
    // trigger its own reclaim), wl_data_device_manager, and copies
    // ORIGINAL_TEXT. Separate host socket from B's, which isn't a loss of
    // realism here: this test hand-implements every bit of
    // compositor-side clipboard brokering regardless, so nothing depends
    // on A and B sharing one literal listener.
    let a_tmp = std::env::temp_dir().join(format!("wl-proxy-clipboard-2client-a-{}", std::process::id()));
    std::fs::create_dir_all(&a_tmp).expect("create temp dir");
    let a_host_path = a_tmp.join("host.sock");
    let _ = std::fs::remove_file(&a_host_path);
    let a_listener = tokio::net::UnixListener::bind(&a_host_path).expect("bind A host");
    let (a_gtk_proxy, mut a_gtk_test) = tokio::net::UnixStream::pair().expect("pair");
    let a_first_conn = tokio::net::UnixStream::connect(&a_host_path).await.expect("A connect");
    let (mut a_host1, _) = a_listener.accept().await.expect("accept A host1");
    {
        let path = a_host_path.clone();
        let cache = cache.clone();
        tokio::spawn(async move {
            let _ = wayland_proxy::run_connection(a_gtk_proxy, a_first_conn, path, None, cache).await;
        });
    }

    // get_registry(2), bind wl_seat(guest=3)->host3, get_pointer(guest=4)
    // ->host4, bind wl_data_device_manager(guest=5)->host5,
    // create_data_source(guest=6)->host6, offer, get_data_device
    // (guest=7)->host7, set_selection. Host ids are deterministic (the
    // proxy's allocator starts at 2, increments per new_id in send
    // order) -- see the earlier clipboard tests' own comments on this.
    let mut out = Vec::new();
    let mut p = Vec::new();
    wire::put_u32(&mut p, 2);
    out.extend(wire::build_message(1, 1, &p)); // get_registry
    p.clear();
    wire::put_u32(&mut p, 1);
    wire::put_str(&mut p, "wl_seat");
    wire::put_u32(&mut p, 8);
    wire::put_u32(&mut p, 3);
    out.extend(wire::build_message(2, 0, &p)); // bind wl_seat -> host3
    p.clear();
    wire::put_u32(&mut p, 4);
    out.extend(wire::build_message(3, 0, &p)); // wl_seat.get_pointer -> host4
    p.clear();
    wire::put_u32(&mut p, 2);
    wire::put_str(&mut p, "wl_data_device_manager");
    wire::put_u32(&mut p, 3);
    wire::put_u32(&mut p, 5);
    out.extend(wire::build_message(2, 0, &p)); // bind ddm -> host5
    p.clear();
    wire::put_u32(&mut p, 6);
    out.extend(wire::build_message(5, 0, &p)); // create_data_source -> host6
    p.clear();
    wire::put_str(&mut p, MIME_TYPE);
    out.extend(wire::build_message(6, 0, &p)); // offer
    p.clear();
    wire::put_u32(&mut p, 7);
    wire::put_u32(&mut p, 3);
    out.extend(wire::build_message(5, 1, &p)); // get_data_device -> host7
    p.clear();
    wire::put_u32(&mut p, 6);
    wire::put_u32(&mut p, 1);
    out.extend(wire::build_message(7, 1, &p)); // set_selection
    a_gtk_test.write_all(&out).await.expect("write A's first-life burst");
    let _ = read_n_messages(&mut a_host1, 8).await; // sync barrier -- see earlier tests' identical reasoning

    // --- Client B: connects, binds wl_seat + wl_data_device_manager +
    // get_data_device. No copying -- B only ever pastes.
    let b_tmp = std::env::temp_dir().join(format!("wl-proxy-clipboard-2client-b-{}", std::process::id()));
    std::fs::create_dir_all(&b_tmp).expect("create temp dir");
    let b_host_path = b_tmp.join("host.sock");
    let _ = std::fs::remove_file(&b_host_path);
    let b_listener = tokio::net::UnixListener::bind(&b_host_path).expect("bind B host");
    let (b_gtk_proxy, mut b_gtk_test) = tokio::net::UnixStream::pair().expect("pair");
    let b_first_conn = tokio::net::UnixStream::connect(&b_host_path).await.expect("B connect");
    let (mut b_host1, _) = b_listener.accept().await.expect("accept B host1");
    {
        let path = b_host_path.clone();
        let cache = cache.clone();
        tokio::spawn(async move {
            let _ = wayland_proxy::run_connection(b_gtk_proxy, b_first_conn, path, None, cache).await;
        });
    }

    let mut out = Vec::new();
    let mut p = Vec::new();
    wire::put_u32(&mut p, 2);
    out.extend(wire::build_message(1, 1, &p)); // get_registry -> host2
    p.clear();
    wire::put_u32(&mut p, 1);
    wire::put_str(&mut p, "wl_seat");
    wire::put_u32(&mut p, 8);
    wire::put_u32(&mut p, 3);
    out.extend(wire::build_message(2, 0, &p)); // bind wl_seat -> host3
    p.clear();
    wire::put_u32(&mut p, 2);
    wire::put_str(&mut p, "wl_data_device_manager");
    wire::put_u32(&mut p, 3);
    wire::put_u32(&mut p, 4);
    out.extend(wire::build_message(2, 0, &p)); // bind ddm -> host4
    p.clear();
    wire::put_u32(&mut p, 5);
    wire::put_u32(&mut p, 3);
    out.extend(wire::build_message(4, 1, &p)); // get_data_device -> host5
    b_gtk_test.write_all(&out).await.expect("write B's first-life burst");
    let _ = read_n_messages(&mut b_host1, 4).await;

    // --- Step 1/2: B pastes what A just copied, pre-crash. A's own
    // answer must run concurrently -- paste_via_compositor's final read
    // blocks on it.
    let (pasted, ()) = tokio::join!(
        paste_via_compositor(&mut b_host1, &mut b_gtk_test, 5, 900, MIME_TYPE, &mut a_host1, 6),
        write_real_source_content(&mut a_gtk_test, ORIGINAL_TEXT),
    );
    assert_eq!(pasted, ORIGINAL_TEXT, "B must be able to paste what A copied, before any crash");

    // --- Step 3: crash the shared compositor -- both connections lose
    // their host at once, same as one real Mutter serving both.
    drop(a_host1);
    drop(b_host1);

    let (mut a_host2, _) =
        tokio::time::timeout(Duration::from_secs(3), a_listener.accept()).await.expect("A reconnect").expect("accept A host2");
    let (mut b_host2, _) =
        tokio::time::timeout(Duration::from_secs(3), b_listener.accept()).await.expect("B reconnect").expect("accept B host2");

    // A's recovery: internal get_registry(2)+sync(3), then replays
    // bind(wl_seat) and get_pointer (both Recreatable) -- 2 messages.
    // Must also advertise wl_data_device_manager here -- not for
    // recovery's own replay (it's not Recreatable), but because
    // attempt_clipboard_splice needs its (name, version) cached from
    // this exact registry re-fetch to bind it fresh later.
    let msg = read_one_message(&mut a_host2).await;
    assert_eq!(wire::MessageHeader::parse(&msg).unwrap().opcode, 1, "get_registry");
    let mut out = Vec::new();
    let mut p = Vec::new();
    wire::put_u32(&mut p, 201);
    wire::put_str(&mut p, "wl_seat");
    wire::put_u32(&mut p, 8);
    out.extend(wire::build_message(2, 0, &p));
    p.clear();
    wire::put_u32(&mut p, 202);
    wire::put_str(&mut p, "wl_data_device_manager");
    wire::put_u32(&mut p, 3);
    out.extend(wire::build_message(2, 0, &p));
    p.clear();
    wire::put_u32(&mut p, 0);
    out.extend(wire::build_message(3, 0, &p)); // sync done
    a_host2.write_all(&out).await.expect("answer A's registry");
    let msgs = read_n_messages(&mut a_host2, 2).await;
    let _a_seat_host2 = u32::from_ne_bytes(msgs[0][msgs[0].len() - 4..].try_into().unwrap());
    let a_pointer_host2 = u32::from_ne_bytes(msgs[1][8..12].try_into().unwrap());

    // B's recovery: same shape (its own wl_seat is Recreatable too).
    let msg = read_one_message(&mut b_host2).await;
    assert_eq!(wire::MessageHeader::parse(&msg).unwrap().opcode, 1, "get_registry");
    let mut out = Vec::new();
    let mut p = Vec::new();
    wire::put_u32(&mut p, 201);
    wire::put_str(&mut p, "wl_seat");
    wire::put_u32(&mut p, 8);
    out.extend(wire::build_message(2, 0, &p));
    p.clear();
    wire::put_u32(&mut p, 0);
    out.extend(wire::build_message(3, 0, &p));
    b_host2.write_all(&out).await.expect("answer B's registry");
    let msg = read_one_message(&mut b_host2).await;
    let header = wire::MessageHeader::parse(&msg).unwrap();
    assert_eq!((header.sender_id, header.opcode), (2, 0), "bind wl_seat");
    let _b_seat_host2 = u32::from_ne_bytes(msg[msg.len() - 4..].try_into().unwrap());

    tokio::time::sleep(Duration::from_millis(100)).await; // let both connections fully unfreeze

    // --- Trigger A's reclaim with a real (repurposed) input serial --
    // same mechanism ADR-0009 verified live, same as the earlier
    // single-client reclaim test.
    const RECLAIM_SERIAL: u32 = 4242;
    let mut p = Vec::new();
    wire::put_u32(&mut p, RECLAIM_SERIAL);
    wire::put_u32(&mut p, 0);
    wire::put_u32(&mut p, 272);
    wire::put_u32(&mut p, 1);
    a_host2.write_all(&wire::build_message(a_pointer_host2, 3, &p)).await.expect("write wl_pointer.button");

    // This button event is also a normal, legitimately forwarded event
    // to A's own client side (a real client would just process it) --
    // drain it now, or it sits unread in a_gtk_test's buffer and gets
    // mistaken for the wl_data_source.send a later write_real_source_
    // content call is actually waiting for (found chasing exactly that
    // failure while writing this test).
    let _ = read_one_message(&mut a_gtk_test).await;

    // The splice: bind(ddm), create_data_source, offer, get_data_device,
    // set_selection.
    let msgs = read_n_messages(&mut a_host2, 5).await;
    let header = wire::MessageHeader::parse(&msgs[0]).unwrap();
    assert_eq!((header.sender_id, header.opcode), (2, 0), "splice: bind wl_data_device_manager");
    let a_ddm_reclaim_host = u32::from_ne_bytes(msgs[0][msgs[0].len() - 4..].try_into().unwrap());
    let header = wire::MessageHeader::parse(&msgs[1]).unwrap();
    assert_eq!((header.sender_id, header.opcode), (a_ddm_reclaim_host, 0), "splice: create_data_source");
    let a_source_reclaim_host = u32::from_ne_bytes(msgs[1][8..12].try_into().unwrap());
    let header = wire::MessageHeader::parse(&msgs[4]).unwrap();
    assert_eq!(header.opcode, 1, "splice: set_selection");
    let serial_arg = u32::from_ne_bytes(msgs[4][12..16].try_into().unwrap());
    assert_eq!(serial_arg, RECLAIM_SERIAL);

    // --- B rebinds its own (non-recreatable) ddm + data_device on the
    // second life -- its wl_seat survived the reconnect (Recreatable),
    // so it's reused directly (guest id 3, unchanged).
    let mut out = Vec::new();
    let mut p = Vec::new();
    wire::put_u32(&mut p, 2);
    wire::put_str(&mut p, "wl_data_device_manager");
    wire::put_u32(&mut p, 3);
    wire::put_u32(&mut p, 6);
    out.extend(wire::build_message(2, 0, &p)); // bind ddm (guest=6)
    p.clear();
    wire::put_u32(&mut p, 7);
    wire::put_u32(&mut p, 3);
    out.extend(wire::build_message(6, 1, &p)); // get_data_device (guest=7)
    b_gtk_test.write_all(&out).await.expect("write B's second-life rebind");
    let msgs = read_n_messages(&mut b_host2, 2).await;
    let b_ddm_host2 = u32::from_ne_bytes(msgs[0][msgs[0].len() - 4..].try_into().unwrap());
    let b_device_host2 = u32::from_ne_bytes(msgs[1][8..12].try_into().unwrap()); // get_data_device's new_id -- first payload field
    let _ = b_ddm_host2;

    // --- Step 4: B pastes again -- must get the ORIGINAL text back,
    // served from the cache by the reclaimed synthetic source, not from
    // any client actually writing it. No concurrent writer needed here
    // -- attempt_clipboard_splice's own cache-backed answer is what
    // production code does, unaided.
    let pasted =
        paste_via_compositor(&mut b_host2, &mut b_gtk_test, b_device_host2, 900, MIME_TYPE, &mut a_host2, a_source_reclaim_host).await;
    assert_eq!(pasted, ORIGINAL_TEXT, "B must still be able to paste the ORIGINAL text after the crash, via the reclaim");

    // --- Step 5: A copies NEW_TEXT for real, on its second-life
    // connection -- a completely fresh wl_data_device_manager/
    // wl_data_source/wl_data_device, none of which are Recreatable.
    // Real Mutter would cancel the previous (reclaimed) owner here --
    // wl_data_source.cancelled takes no arguments.
    a_host2.write_all(&wire::build_message(a_source_reclaim_host, 2, &[])).await.expect("write cancelled");

    let mut out = Vec::new();
    let mut p = Vec::new();
    wire::put_u32(&mut p, 2);
    wire::put_str(&mut p, "wl_data_device_manager");
    wire::put_u32(&mut p, 3);
    wire::put_u32(&mut p, 8);
    out.extend(wire::build_message(2, 0, &p)); // bind ddm (guest=8)
    p.clear();
    wire::put_u32(&mut p, 9);
    out.extend(wire::build_message(8, 0, &p)); // create_data_source (guest=9)
    p.clear();
    wire::put_str(&mut p, MIME_TYPE);
    out.extend(wire::build_message(9, 0, &p)); // offer
    p.clear();
    wire::put_u32(&mut p, 10);
    wire::put_u32(&mut p, 4); // A's original wl_seat guest id (Recreatable, unchanged)
    out.extend(wire::build_message(8, 1, &p)); // get_data_device (guest=10)
    p.clear();
    wire::put_u32(&mut p, 9);
    wire::put_u32(&mut p, 2);
    out.extend(wire::build_message(10, 1, &p)); // set_selection
    a_gtk_test.write_all(&out).await.expect("write A's second-life real copy");
    let msgs = read_n_messages(&mut a_host2, 5).await;
    let header = wire::MessageHeader::parse(&msgs[1]).unwrap();
    assert_eq!(header.opcode, 0, "create_data_source");
    let a_source_host2b = u32::from_ne_bytes(msgs[1][8..12].try_into().unwrap());

    // --- Step 6: B pastes once more -- must get NEW_TEXT this time,
    // written for real by A's own client code, proving ordinary
    // copy/paste still works cleanly after the whole recovery cycle.
    let (pasted, ()) = tokio::join!(
        paste_via_compositor(&mut b_host2, &mut b_gtk_test, b_device_host2, 901, MIME_TYPE, &mut a_host2, a_source_host2b),
        write_real_source_content(&mut a_gtk_test, NEW_TEXT),
    );
    assert_eq!(pasted, NEW_TEXT, "B must be able to paste the NEW text A copied after recovery");
}

/// Drives one full client lifecycle -- bind, create a toplevel, set its
/// title/app_id, crash its own first-life compositor, reconnect to a
/// fresh second life -- and returns the (title, app_id) pair the second
/// life actually observed replayed onto the recreated toplevel. Each
/// client gets its own dedicated host socket/listener (not a shared one)
/// specifically so multiple instances of this can run concurrently
/// without needing to disambiguate which accepted connection belongs to
/// which logical client -- the point of running several concurrently is
/// to stress any state that's accidentally shared across connections
/// within the same proxy process (there shouldn't be any -- see
/// concurrent_reconnects_dont_mix_up_toplevel_identity's own doc comment).
async fn run_client_reconnect_scenario(label: &'static str, title: &str, app_id: &str) -> (String, String) {
    use tokio::io::AsyncWriteExt;
    use wayland_proxy::wire;

    const GLOBALS: &[(&str, u32)] = &[("wl_compositor", 6), ("xdg_wm_base", 6)];

    let tmp_dir = std::env::temp_dir()
        .join(format!("wayland-proxy-icon-identity-test-{label}-{}", std::process::id()));
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
        if let Err(e) = wayland_proxy::run_connection(
            gtk_proxy_side,
            first_host_conn,
            host_socket_path_for_proxy,
            None,
            wayland_proxy::clipboard::ClipboardCache::new(),
        )
        .await
        {
            eprintln!("run_connection ({label}) ended with error: {e:?}");
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
                let n = consumed;
                buf.drain(..n);
                break 'collect;
            }
            let n = consumed;
            buf.drain(..n);
        }
    }
    let wl_compositor_name = wl_compositor_name.expect("wl_compositor advertised");
    let xdg_wm_base_name = xdg_wm_base_name.expect("xdg_wm_base advertised");

    // bind wl_compositor(4)/xdg_wm_base(5), create_surface->6,
    // get_xdg_surface->7, get_toplevel->8, then set_title/set_app_id on
    // the toplevel -- this is what seeds RecreationGraph's
    // Recreatable::XdgToplevel title/app_id fields (see
    // update_toplevel_title/update_toplevel_app_id in src/recreation.rs),
    // required for anything to be replayed at all after the crash below.
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
    wire::put_u32(&mut p, 6);
    out.extend(wire::build_message(4, 0, &p)); // create_surface -> 6
    let mut p = Vec::new();
    wire::put_u32(&mut p, 7);
    wire::put_u32(&mut p, 6);
    out.extend(wire::build_message(5, 2, &p)); // get_xdg_surface -> 7
    let mut p = Vec::new();
    wire::put_u32(&mut p, 8);
    out.extend(wire::build_message(7, 1, &p)); // get_toplevel -> 8
    let mut p = Vec::new();
    wire::put_str(&mut p, title);
    out.extend(wire::build_message(8, 2, &p)); // xdg_toplevel(8).set_title
    let mut p = Vec::new();
    wire::put_str(&mut p, app_id);
    out.extend(wire::build_message(8, 3, &p)); // xdg_toplevel(8).set_app_id
    gtk_test_side.write_all(&out).await.expect("write bind+create+identity chain");

    // Deliberately short and jittered per-label -- with several of these
    // running concurrently (see the test below), this keeps their crashes
    // landing close together but not in lockstep, closer to how several
    // real apps' connections all notice the same compositor crash within
    // a similarly tight window rather than one at a time.
    let jitter_ms = (label.bytes().map(u64::from).sum::<u64>() % 40) + 20;
    tokio::time::sleep(Duration::from_millis(jitter_ms)).await;

    first_life_task.abort();
    let (second_sink, mut second_sink_rx) = tokio::sync::mpsc::unbounded_channel();
    let host_listener_task = tokio::spawn(async move {
        let (second_host_accepted, _) = tokio::time::timeout(Duration::from_secs(5), host_listener.accept())
            .await
            .expect("proxy should reconnect within 5s")
            .expect("accept second host conn");
        serve_fake_compositor_life(second_host_accepted, GLOBALS, 101, second_sink).await;
    });

    // Recovery replays exactly 7 requests, in the deterministic order
    // recover_state_after_reconnect sends them: bind(wl_compositor),
    // bind(xdg_wm_base), create_surface, get_xdg_surface, get_toplevel,
    // set_title, set_app_id (get_registry/sync are answered inline by
    // serve_fake_compositor_life, never reaching the sink). Reading by
    // fixed position, not by opcode match -- opcodes are only unique
    // *within* one interface's own request table, and get_xdg_surface
    // (xdg_wm_base, opcode 2) collides with set_title (xdg_toplevel,
    // opcode 2) across different interfaces.
    let mut observed = Vec::with_capacity(7);
    for _ in 0..7 {
        let (sender, opcode, payload) = tokio::time::timeout(Duration::from_secs(5), second_sink_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("{label}: timed out waiting for the recreation replay"))
            .unwrap_or_else(|| panic!("{label}: sink closed before replay completed"));
        observed.push((sender, opcode, payload));
    }
    let (title_sender, title_opcode, title_payload) = &observed[5];
    let (app_id_sender, app_id_opcode, app_id_payload) = &observed[6];
    assert_eq!(*title_sender, *app_id_sender, "{label}: set_title/set_app_id must target the same recreated toplevel");
    assert_eq!(*title_opcode, 2, "{label}: expected set_title at replay position 5");
    assert_eq!(*app_id_opcode, 3, "{label}: expected set_app_id at replay position 6");
    let observed_title = wire::read_str(title_payload, 0).expect("title string").0;
    let observed_app_id = wire::read_str(app_id_payload, 0).expect("app_id string").0;

    host_listener_task.abort();
    (observed_title, observed_app_id)
}

/// Live-found 2026-08-07: after several near-simultaneous crash-recovery
/// cycles under wl-resprox, GNOME Shell's Activities overview showed one
/// real app's window (Tilix) under a completely different real app's
/// icon (ZapZap) -- while a third (Firefox) recreated correctly. Traced
/// the actual cause to a Mutter-internal bug
/// (`meta_window_set_stack_position_no_sync` assertion failures in
/// Mutter's own log, timestamp-correlated to the crash), not wl-resprox:
/// `RecreationGraph` (recreation.rs) is a plain struct owned entirely
/// inside one `run_connection` task, with no static/shared state, so
/// there is no code path by which one client's connection could read or
/// write another's `title`/`app_id`. This test is the concrete proof of
/// that reasoning: several clients' toplevels, each with a distinct
/// identity, crash-recover concurrently within the same proxy process,
/// and each must come back with exactly its own (title, app_id) pair --
/// never a neighbor's.
#[tokio::test]
async fn concurrent_reconnects_dont_mix_up_toplevel_identity() {
    let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).try_init();

    // At least 3 distinct icons, per the live report -- real app ids so a
    // failure reads the same way the live bug did.
    const APPS: &[(&str, &str, &str)] = &[
        ("firefox", "Mozilla Firefox", "org.mozilla.firefox"),
        ("tilix", "Tilix", "com.gexperts.Tilix"),
        ("zapzap", "ZapZap", "com.rtosta.zapzap"),
    ];

    let handles: Vec<_> = APPS
        .iter()
        .map(|&(label, title, app_id)| tokio::spawn(run_client_reconnect_scenario(label, title, app_id)))
        .collect();

    let mut got = Vec::new();
    for (handle, &(label, ..)) in handles.into_iter().zip(APPS) {
        got.push(handle.await.unwrap_or_else(|e| panic!("{label} task panicked: {e:?}")));
    }
    got.sort();

    let mut expected: Vec<(String, String)> =
        APPS.iter().map(|&(_, title, app_id)| (title.to_string(), app_id.to_string())).collect();
    expected.sort();

    assert_eq!(
        got, expected,
        "each reconnected toplevel must replay its OWN title/app_id pair, never a neighbor's -- \
         an off-by-one or shared-state bug here would show up as e.g. Tilix's title paired with \
         ZapZap's app_id, exactly the shape of the live symptom (wrong icon in the overview)"
    );
}
