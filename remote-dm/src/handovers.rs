//! Watches gnome-remote-desktop's handover objects, to end greeters that nobody reached.
//!
//! grd redirects each new client to the greeter session started for it. If the client never
//! comes back (for example mstsc without `use redirection server name:i:1` in its .rdp file), grd
//! gives up after 30 seconds: it drops the client and unexports the handover object of the greeter
//! session. The `grd --handover` in that session doesn't notice, so the greeter would keep running
//! forever (GDM has the same gap). We end greeter sessions whose handover object went away.

use futures_util::StreamExt as _;
use tokio::sync::mpsc;

const BUS_NAME: &str = "org.gnome.RemoteDesktop";
const HANDOVERS_PATH: &str = "/org/gnome/RemoteDesktop/Rdp/Handovers";

/// Sends the logind session ID of every handover object grd removes.
pub async fn watch(
    conn: &zbus::Connection,
    removed_tx: mpsc::UnboundedSender<String>,
) -> zbus::Result<()> {
    let proxy = zbus::fdo::ObjectManagerProxy::builder(conn)
        .destination(BUS_NAME)?
        .path(HANDOVERS_PATH)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;
    let mut removed = proxy.receive_interfaces_removed().await?;

    tokio::spawn(async move {
        while let Some(signal) = removed.next().await {
            let Ok(args) = signal.args() else {
                continue;
            };
            if let Some(session_id) = session_id(args.object_path.as_str()) {
                if removed_tx.send(session_id.to_owned()).is_err() {
                    break;
                }
            }
        }
    });
    Ok(())
}

/// `/org/gnome/RemoteDesktop/Rdp/Handovers/session<ID>` → `<ID>`.
fn session_id(path: &str) -> Option<&str> {
    path.strip_prefix(HANDOVERS_PATH)?
        .strip_prefix("/session")
        .filter(|id| !id.is_empty() && !id.contains('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_from_path() {
        assert_eq!(session_id("/org/gnome/RemoteDesktop/Rdp/Handovers/sessionc12"), Some("c12"));
        assert_eq!(session_id("/org/gnome/RemoteDesktop/Rdp/Handovers/session7"), Some("7"));
        assert_eq!(session_id("/org/gnome/RemoteDesktop/Rdp/Handovers/session"), None);
        assert_eq!(session_id("/org/gnome/RemoteDesktop/Rdp/Handovers"), None);
        assert_eq!(session_id("/org/gnome/RemoteDesktop/Rdp/Handovers/sessionc1/x"), None);
        assert_eq!(session_id("/org/gnome/Other/sessionc1"), None);
    }
}
