//! Desktop notification on crash recovery, via the freedesktop
//! `org.freedesktop.Notifications` session-bus interface -- so a user
//! watching their screen can tell a restart happened, not just that their
//! app kept working. Best-effort: no notification daemon (headless,
//! container test harness) must never affect recovery itself.

use std::collections::HashMap;

use tracing::warn;
use zbus::zvariant::Value;

/// Fires a "compositor recovered" desktop notification for one client.
/// Spawn this rather than awaiting it inline -- the session-bus round
/// trip has no bearing on whether relaying is safe to resume.
pub async fn notify_recovered(client_pid: Option<i32>) {
    let body = match client_pid.and_then(process_name) {
        Some(name) => format!("{name} reconnected after the compositor restarted."),
        None => "A client reconnected after the compositor restarted.".to_string(),
    };
    if let Err(e) = send(&body).await {
        warn!("desktop notification failed (no notification daemon running?): {e:?}");
    }
}

fn process_name(pid: i32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|s| s.trim().to_string())
}

async fn send(body: &str) -> zbus::Result<()> {
    let conn = zbus::Connection::session().await?;
    conn.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "Notify",
        &(
            "wl-resprox",
            0u32,
            "dialog-information",
            "Compositor recovered",
            body,
            Vec::<&str>::new(),
            HashMap::<&str, Value>::new(),
            5000i32,
        ),
    )
    .await?;
    Ok(())
}
