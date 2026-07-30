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
        if let Err(e) = wayland_proxy::run_connection(gtk_stream, compositor_stream).await {
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
