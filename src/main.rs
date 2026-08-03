mod cli;

use anyhow::{Context, Result};
use clap::Parser;
use std::env;
use std::path::{Path, PathBuf};

use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};
use tracing::{error, info, Instrument};

use cli::Cli;
use wayland_proxy::run_connection;

/// Removes a stale socket file at `path` if present, then binds and starts
/// listening fresh. Used both at startup and to rebind after `SIGUSR1` --
/// see that signal's own doc comment in `main` for why the proxy needs to
/// be able to redo this without restarting the whole process.
fn bind_listener(path: &Path) -> Result<UnixListener> {
    if path.exists() {
        std::fs::remove_file(path).context("removing stale proxy socket")?;
    }
    UnixListener::bind(path).with_context(|| format!("binding to {}", path.display()))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let filter = tracing_subscriber::EnvFilter::try_new(&cli.log_level)
        .with_context(|| format!("invalid --log-level/RUST_LOG filter: {:?}", cli.log_level))?;
    tracing_subscriber::fmt().with_env_filter(filter).init();
    wayland_proxy::recorder::init(cli.record.clone());

    info!("Starting Wayland proxy (hand-rolled wire protocol relay)...");

    let runtime_dir =
        env::var("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR environment variable is not set")?;

    let target_display = cli.display.clone();
    let target_socket_path = PathBuf::from(&runtime_dir).join(&target_display);

    let proxy_display = cli.listen.clone();
    let proxy_socket_path = PathBuf::from(&runtime_dir).join(&proxy_display);

    let mut listener = bind_listener(&proxy_socket_path)?;

    info!("Proxy listening on: {}", proxy_socket_path.display());
    info!("Forwarding connections to: {}", target_socket_path.display());
    info!("To test, run: WAYLAND_DISPLAY={} gtk4-demo", proxy_display);

    // See docs/adr/adr-0005-route-shell-launched-clients-through-the-proxy.md:
    // when the compositor session wrapper (re)starts gnome-shell, gnome-shell
    // is itself given --wayland-display=<this proxy's own public name>, so
    // that ITS OWN self-belief (what it hands to apps it spawns directly,
    // e.g. Super-key/dock launches -- confirmed via mutter's own source,
    // set_gnome_env in src/wayland/meta-wayland.c) matches the name real
    // clients look for. The wrapper immediately renames gnome-shell's
    // freshly-created socket file out from under that name to a private
    // path (this proxy's own --display= target) and sends this process
    // SIGUSR1 once that's done, as the signal to reclaim the now-vacant
    // public name. Deliberately NOT a full process restart: that would
    // drop every already-`accept()`ed client connection, which are
    // entirely independent of the listening socket and must survive this
    // -- surviving exactly that kind of disruption is this whole proxy's
    // reason to exist. Fires on every gnome-shell crash-restart, not just
    // the first startup (mutter's own socket-claiming has no liveness
    // check, so a restarting gnome-shell always steals the name back --
    // see the ADR for the confirmed libwayland behavior behind that).
    let mut rebind_signal =
        signal(SignalKind::user_defined1()).context("installing SIGUSR1 handler")?;

    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((gtk_stream, _addr)) => {
                    // Best-effort: the connecting process's own pid, purely for
                    // tagging every log line this connection's task ever emits
                    // (via the span below) -- with several clients each
                    // independently reconnecting after a compositor crash (see
                    // plan-desktop-resilience.md's 2026-08-03 entries on the
                    // reconnect race), the proxy's log otherwise interleaves
                    // multiple clients' recovery sequences with no way to tell
                    // which lines belong to which real process. `None` (peer_cred
                    // failing) just means "unknown", never fatal.
                    let client_pid = gtk_stream.peer_cred().ok().and_then(|c| c.pid());
                    let span = tracing::info_span!("client", pid = client_pid);
                    info!(parent: &span, "New Wayland client connected!");
                    let target_path = target_socket_path.clone();
                    tokio::spawn(
                        async move {
                            match UnixStream::connect(&target_path).await {
                                Ok(compositor_stream) => {
                                    if let Err(e) =
                                        run_connection(gtk_stream, compositor_stream, target_path).await
                                    {
                                        error!("proxy session ended with error: {e:?}");
                                    }
                                }
                                Err(e) => error!("failed to connect to compositor socket {target_path:?}: {e}"),
                            }
                        }
                        .instrument(span),
                    );
                }
                Err(e) => error!("failed to accept incoming client connection: {e}"),
            },
            _ = rebind_signal.recv() => {
                info!("SIGUSR1 received -- rebinding public listener at {}", proxy_socket_path.display());
                match bind_listener(&proxy_socket_path) {
                    Ok(new_listener) => listener = new_listener,
                    Err(e) => error!("failed to rebind {}: {e:?} -- keeping the old (now orphaned-by-name) listener", proxy_socket_path.display()),
                }
            }
        }
    }
}
