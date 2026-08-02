mod cli;

use anyhow::{Context, Result};
use clap::Parser;
use std::env;
use std::path::PathBuf;

use tokio::net::{UnixListener, UnixStream};
use tracing::{error, info};

use cli::Cli;
use wayland_proxy::run_connection;

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
                            if let Err(e) =
                                run_connection(gtk_stream, compositor_stream, target_path).await
                            {
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
