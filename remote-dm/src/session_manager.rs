//! `remote-dm session-manager`: a stand-in for gnome-session's `Logout` in our sessions.
//!
//! When the RDP session ends in a remote login greeter, gnome-remote-desktop's handover daemon
//! calls `org.gnome.SessionManager.Logout` on the session bus to end the greeter session. There is
//! no gnome-session here, so this provides that one method and ends the logind session instead.
//! Started from the session's compositor (e.g. niri's `spawn-at-startup`).

use anyhow::Context as _;
use zbus::{fdo, interface};

use crate::logind::ManagerProxy;

struct SessionManager {
    session_id: String,
}

#[interface(name = "org.gnome.SessionManager")]
impl SessionManager {
    /// `mode`: 0 normal, 1 no confirmation, 2 forced. We always log out right away.
    async fn logout(&self, mode: u32) -> fdo::Result<()> {
        tracing::info!(mode, session_id = self.session_id, "Logout");

        let system_bus = zbus::Connection::system().await?;
        let manager = ManagerProxy::new(&system_bus).await?;
        manager
            .terminate_session(&self.session_id)
            .await
            .map_err(|err| fdo::Error::Failed(format!("error ending the session: {err}")))
    }
}

pub fn main() -> anyhow::Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
    let session_id = std::env::var("XDG_SESSION_ID").context("XDG_SESSION_ID isn't set")?;

    let _conn = zbus::connection::Builder::session()?
        .serve_at("/org/gnome/SessionManager", SessionManager { session_id })?
        .name("org.gnome.SessionManager")?
        .build()
        .await
        .context("error connecting to the session bus")?;
    tracing::info!("serving org.gnome.SessionManager");

    // Ends with the session.
    std::future::pending::<()>().await;
    Ok(())
}
