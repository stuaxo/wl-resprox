//! Command-line surface for the proxy. Every setting here also has a
//! same-named environment variable fallback (via clap's `env` feature),
//! so the existing `wayland-proxy.service` systemd unit's
//! `EnvironmentFile=` override mechanism keeps working unchanged --
//! precedence is always: explicit flag > environment variable > default.
//!
//! `XDG_RUNTIME_DIR` is deliberately NOT here -- it's a standard XDG
//! convention every Wayland/D-Bus tool on the system shares, not
//! proxy-specific configuration, and stays a plain `env::var` read in
//! `main.rs`.

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "wayland-proxy", about = "A crash-resilient Wayland proxy")]
pub struct Cli {
    /// Compositor socket to connect to and proxy for (a name under
    /// $XDG_RUNTIME_DIR, e.g. "wayland-0").
    #[arg(long, env = "WAYLAND_DISPLAY", default_value = "wayland-1")]
    pub display: String,

    /// Record every relayed message to this file, for post-mortem
    /// analysis of a session (see src/recorder.rs). Off by default --
    /// recording has a real per-message cost.
    #[arg(long, env = "WAYLAND_PROXY_RECORD")]
    pub record: Option<String>,

    /// Log filter, in tracing's directive syntax (e.g. "debug", or
    /// "wayland_proxy=debug,tokio=info").
    #[arg(long, env = "RUST_LOG", default_value = "info")]
    pub log_level: String,

    /// Socket name the proxy itself listens on -- what client
    /// applications set WAYLAND_DISPLAY to, to connect through it.
    #[arg(long, env = "WAYLAND_PROXY_LISTEN", default_value = "wayland-proxy-0")]
    pub listen: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_with_no_args() {
        let cli = Cli::try_parse_from(["wayland-proxy"]).unwrap();
        assert_eq!(cli.display, "wayland-1");
        assert_eq!(cli.record, None);
        assert_eq!(cli.log_level, "info");
        assert_eq!(cli.listen, "wayland-proxy-0");
    }

    #[test]
    fn flags_override_defaults() {
        let cli = Cli::try_parse_from([
            "wayland-proxy",
            "--display",
            "wayland-2",
            "--record",
            "/tmp/rec.log",
            "--log-level",
            "wayland_proxy=debug",
            "--listen",
            "wayland-proxy-1",
        ])
        .unwrap();
        assert_eq!(cli.display, "wayland-2");
        assert_eq!(cli.record.as_deref(), Some("/tmp/rec.log"));
        assert_eq!(cli.log_level, "wayland_proxy=debug");
        assert_eq!(cli.listen, "wayland-proxy-1");
    }

    #[test]
    fn unknown_flag_is_rejected() {
        assert!(Cli::try_parse_from(["wayland-proxy", "--bogus"]).is_err());
    }
}
