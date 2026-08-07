//! Clipboard content cache, tee'd from `wl_data_source.send` traffic so it
//! survives a compositor crash or the copying client quitting -- see
//! docs/adr/adr-0009-clipboard-persistence.md for why this is possible at
//! all. `ReclaimState` and `attempt_clipboard_splice` (src/lib.rs, since
//! they need `Conn`/wire access) consume the cache: the first real,
//! compositor-issued input serial a connection sees after a reconnect gets
//! borrowed to re-establish the proxy as clipboard owner from cached bytes
//! -- ADR-0009 live-verified that works even though the serial was issued
//! for something else entirely.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::warn;

/// Matches the OOM cap from the original clipboard-cache proposal this
/// module implements -- a dragged-in multi-GB file must never be buffered
/// in full.
const MAX_CACHED_BYTES: usize = 5 * 1024 * 1024;

/// Only cache well-understood, small, text/image selections. Anything else
/// (arbitrary app-specific mime types, file lists, ...) still tees fine at
/// the protocol level, but caching it is more likely to be large binary
/// data than something worth restoring after a crash.
const CACHEABLE_MIME_TYPES: &[&str] =
    &["text/plain", "text/plain;charset=utf-8", "UTF8_STRING", "STRING", "TEXT", "image/png"];

pub fn is_cacheable_mime(mime_type: &str) -> bool {
    CACHEABLE_MIME_TYPES.contains(&mime_type)
}

pub struct ClipboardCache {
    by_mime_type: Mutex<HashMap<String, Vec<u8>>>,
}

pub type SharedClipboardCache = Arc<ClipboardCache>;

impl ClipboardCache {
    pub fn new() -> SharedClipboardCache {
        Arc::new(ClipboardCache { by_mime_type: Mutex::new(HashMap::new()) })
    }

    fn store(&self, mime_type: String, bytes: Vec<u8>) {
        tracing::info!("clipboard: cached {} bytes ({mime_type})", bytes.len());
        self.by_mime_type.lock().unwrap().insert(mime_type, bytes);
    }

    pub fn get(&self, mime_type: &str) -> Option<Vec<u8>> {
        self.by_mime_type.lock().unwrap().get(mime_type).cloned()
    }

    pub fn cached_mime_types(&self) -> Vec<String> {
        self.by_mime_type.lock().unwrap().keys().cloned().collect()
    }
}

/// Per-connection state for re-offering cached clipboard content after a
/// reconnect -- see this module's doc comment and ADR-0009. Owned by
/// `run_connection`, threaded through `relay_ready_messages`.
#[derive(Default)]
pub struct ReclaimState {
    /// Set whenever `recover_state_after_reconnect` completes; cleared
    /// after the first real input serial this connection sees afterward
    /// is spent trying to reclaim the clipboard -- one attempt per
    /// reconnect, not a retry loop.
    pub pending: bool,
    /// (name, version) of `wl_data_device_manager` from the most recent
    /// registry re-fetch -- needed to bind it fresh for the reclaim
    /// attempt, since the connection may never have bound it itself.
    pub data_device_manager_global: Option<(u32, u32)>,
    /// Host-space id of our own synthetic `wl_data_source`, once a reclaim
    /// has actually been attempted. It has no guest-side counterpart at
    /// all (the real client never sees it), so a later
    /// `wl_data_source.send` addressed to it needs recognizing here rather
    /// than falling into the normal guest-id-driven relay, which would
    /// just see an untranslatable object and drop it. Reset to `None` on
    /// every reconnect -- a stale id from a previous compositor life could
    /// otherwise coincide with an unrelated fresh object's id.
    pub active_source_host_id: Option<u32>,
}

/// Substitutes `real_fd` (the pipe write end `wl_data_source.send` handed
/// us, which the client is meant to write clipboard bytes into) with a
/// fresh pipe of our own, and spawns a task that mirrors everything the
/// client writes into `real_fd` unchanged while also caching it. Returns
/// the fd to forward to the client in `real_fd`'s place, or `None` if
/// anything here failed -- the caller then forwards nothing for this
/// message's fd argument, same tolerance already applied elsewhere in the
/// relay for a fd that didn't arrive as expected.
pub fn start_tee(real_fd: OwnedFd, mime_type: String, cache: SharedClipboardCache) -> Option<OwnedFd> {
    let (client_sender, client_receiver) = match tokio::net::unix::pipe::pipe() {
        Ok(pair) => pair,
        Err(e) => {
            warn!("clipboard tee: failed to create pipe for {mime_type}: {e}");
            return None;
        }
    };
    let host_sender = match tokio::net::unix::pipe::Sender::from_owned_fd(real_fd) {
        Ok(s) => s,
        Err(e) => {
            warn!("clipboard tee: real fd for {mime_type} wasn't a writable pipe: {e}");
            return None;
        }
    };
    let client_facing_fd = match client_sender.into_nonblocking_fd() {
        Ok(fd) => fd,
        Err(e) => {
            warn!("clipboard tee: failed to extract a forwardable fd for {mime_type}: {e}");
            return None;
        }
    };
    tokio::spawn(pump(client_receiver, host_sender, mime_type, cache));
    Some(client_facing_fd)
}

async fn pump(
    mut from_client: tokio::net::unix::pipe::Receiver,
    mut to_host: tokio::net::unix::pipe::Sender,
    mime_type: String,
    cache: SharedClipboardCache,
) {
    let mut buf = [0u8; 16 * 1024];
    // None once the transfer has exceeded MAX_CACHED_BYTES -- OOM
    // protection: stop accumulating, but keep tee-ing real bytes through
    // for the rest of the transfer.
    let mut collected: Option<Vec<u8>> = Some(Vec::new());
    loop {
        let n = match from_client.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                warn!("clipboard tee: read error for {mime_type}: {e}");
                return;
            }
        };
        if let Err(e) = to_host.write_all(&buf[..n]).await {
            // The client's write already landed in our pipe; if we can't
            // mirror it onward, the real compositor gets a truncated
            // transfer either way -- don't also cache a partial one.
            warn!("clipboard tee: write error forwarding {mime_type} to the compositor: {e}");
            return;
        }
        if let Some(so_far) = collected.as_mut() {
            if so_far.len() + n > MAX_CACHED_BYTES {
                warn!("clipboard tee: {mime_type} exceeded {MAX_CACHED_BYTES} bytes -- dropping the cache, still forwarding");
                collected = None;
            } else {
                so_far.extend_from_slice(&buf[..n]);
            }
        }
    }
    if let Some(bytes) = collected {
        if !bytes.is_empty() {
            cache.store(mime_type, bytes);
        }
    }
}
