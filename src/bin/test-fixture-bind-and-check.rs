//! Test-only fixture, not shipped in the `.deb` (see Cargo.toml's
//! `[package.metadata.deb]` `assets` list -- deliberately not added
//! there). `socket-handoff`'s ptrace-based freeze method needs a real
//! target that calls `bind()` itself to have anything to catch; the
//! sigstop method's own tests get away with a plain `sleep` because
//! *they* create the "real" socket from the test process instead. This
//! binary is that real target: it binds the given path, then
//! immediately -- the very next operation, no delay of any kind --
//! checks whether that same path still exists and prints the answer.
//!
//! If `socket-handoff --freeze-method=ptrace` is genuinely watching this
//! process (not just reacting after the fact), the path must ALWAYS be
//! gone by the time this fixture's own very next instruction runs: the
//! whole point of ptrace-seizing a syscall-exit stop is that nothing of
//! the tracee's own can execute in between, so there's no weaker
//! "usually" to test for here, only "always" or "the mechanism is
//! broken."
//!
//! Usage: test-fixture-bind-and-check <path>
//! Prints exactly one line: `still_exists=true` or `still_exists=false`.

fn main() {
    let path = std::env::args().nth(1).expect("usage: test-fixture-bind-and-check <path>");
    // A real gnome-shell spends real wall-clock time on its own startup
    // (DRM/EGL init, ...) before it ever calls bind() -- long enough for
    // socket-handoff to have already started, ptrace-seized it, and be
    // waiting in its syscall-tracing loop. This fixture binds almost
    // immediately on its own; without a comparable head start, the test
    // harness spawning it can lose the race against socket-handoff's own
    // process startup, seize, and initial-stop handshake -- a test
    // artifact, not anything socket-handoff itself needs to tolerate in
    // real use.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let _listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
    let still_exists = std::path::Path::new(&path).exists();
    println!("still_exists={still_exists}");
}
