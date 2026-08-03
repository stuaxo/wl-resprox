//! Permanent, opt-in sequence recorder: dumps every message the relay
//! processes (forwarded, dropped-unknown, or dropped-parse-error) to a
//! file, one line per message, for post-mortem analysis of a live run.
//!
//! Grew out of an ad-hoc `tracing::debug!` hex dump used manually while
//! chasing the intermittent "invalid arguments for wl_registry#2.bind"
//! failure against a real compositor (see the 2026-07-30 entries in
//! docs/debugging-notes.md). That worked, but only if you remembered to
//! set `RUST_LOG=debug` *before* the failure happened and were watching
//! the log live. This is the same idea made permanent and always
//! available: opt in with an env var, get a fixed artifact you can
//! inspect (or diff against a known-good run, e.g. from
//! tests/integration.rs) after the fact instead of re-triggering live.
//!
//! Recording has a real cost (a line written and flushed per message), so
//! it's off by default -- set `WAYLAND_PROXY_RECORD=/path/to/file` to
//! enable it.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Recorder {
    writer: Mutex<BufWriter<File>>,
}

static RECORD_PATH_OVERRIDE: OnceLock<Option<String>> = OnceLock::new();

/// Lets the CLI's `--record` flag win over `WAYLAND_PROXY_RECORD` when
/// given. Must be called at most once, before the first call to
/// `recorder()` (that first call is what actually reads this). Passing
/// `None` (flag not given) leaves the env var as the fallback, unchanged.
pub fn init(explicit_path: Option<String>) {
    let _ = RECORD_PATH_OVERRIDE.set(explicit_path);
}

impl Recorder {
    fn from_env() -> Option<Recorder> {
        let path = RECORD_PATH_OVERRIDE
            .get()
            .cloned()
            .flatten()
            .or_else(|| std::env::var("WAYLAND_PROXY_RECORD").ok())?;
        match File::create(&path) {
            Ok(file) => {
                eprintln!("recording message sequence to {path}");
                Some(Recorder { writer: Mutex::new(BufWriter::new(file)) })
            }
            Err(e) => {
                eprintln!("WAYLAND_PROXY_RECORD set to {path:?} but couldn't create it: {e}");
                None
            }
        }
    }

    /// Records one message. Tab-separated, hex-encoded bytes: greppable,
    /// diffable, and simple enough that a future replay tool doesn't need
    /// a serialization dependency to parse it back out.
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &self,
        direction: &str,
        outcome: &str,
        interface: &str,
        method: &str,
        sender_id: u32,
        opcode: u16,
        fds: usize,
        bytes: &[u8],
    ) {
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let hex = crate::wire::hex_encode(bytes);
        let mut w = self.writer.lock().unwrap();
        let _ = writeln!(
            w,
            "{}.{:06}\t{direction}\t{outcome}\t{interface}.{method}\tsender={sender_id}\topcode={opcode}\tfds={fds}\tbytes={hex}",
            ts.as_secs(),
            ts.subsec_micros(),
        );
        // Flush every record, not just on drop: a crash mid-burst is
        // exactly the scenario we're trying to capture, and an unflushed
        // BufWriter would lose precisely the tail we need.
        let _ = w.flush();
    }
}

static RECORDER: OnceLock<Option<Recorder>> = OnceLock::new();

/// Returns the process-wide recorder, initializing it from
/// `WAYLAND_PROXY_RECORD` on first access. Returns `None` (cheaply, just
/// an `OnceLock` check after the first call) if recording isn't enabled.
pub fn recorder() -> Option<&'static Recorder> {
    RECORDER.get_or_init(Recorder::from_env).as_ref()
}
