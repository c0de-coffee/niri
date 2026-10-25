//! `remote-dm session-manager`: a stand-in for gnome-session's `Logout` in our sessions.
//!
//! When the RDP session ends in a remote login greeter, gnome-remote-desktop's handover daemon
//! calls `org.gnome.SessionManager.Logout` on the session bus to end the greeter session. There is
//! no gnome-session here, so this provides that method and ends the logind session instead.
//! Started from the session's compositor (e.g. niri's `spawn-at-startup`).
//!
//! Others see the name and register as clients (at-spi's bus launcher does); `RegisterClient` hands
//! out client objects that accept the usual calls and do nothing. Clients never get asked to end:
//! the whole session goes away at once.

use anyhow::Context as _;
use zbus::object_server::ObjectServer;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};
use zbus::{fdo, interface};

use crate::logind::ManagerProxy;

const PATH: &str = "/org/gnome/SessionManager";

struct SessionManager {
    session_id: String,
    next_client: u32,
}

/// A registered client (`org.gnome.SessionManager.ClientPrivate`).
struct Client;

#[interface(name = "org.gnome.SessionManager.ClientPrivate")]
impl Client {
    /// The client's answer to `QueryEndSession` / `EndSession`, which we never send.
    fn end_session_response(&self, _is_ok: bool, _reason: &str) {}
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

    async fn register_client(
        &mut self,
        #[zbus(object_server)] server: &ObjectServer,
        app_id: &str,
        _client_startup_id: &str,
    ) -> fdo::Result<OwnedObjectPath> {
        self.next_client += 1;
        let path = OwnedObjectPath::try_from(format!("{PATH}/Client{}", self.next_client))
            .map_err(|err| fdo::Error::Failed(err.to_string()))?;
        server.at(&path, Client).await?;
        tracing::debug!(app_id, %path, "RegisterClient");
        Ok(path)
    }

    async fn unregister_client(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        client_id: ObjectPath<'_>,
    ) -> fdo::Result<()> {
        if !client_id.as_str().starts_with(&format!("{PATH}/Client")) {
            return Err(fdo::Error::InvalidArgs("not a client".to_owned()));
        }
        tracing::debug!(%client_id, "UnregisterClient");
        server.remove::<Client, _>(&client_id).await?;
        Ok(())
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
        .serve_at(
            PATH,
            SessionManager {
                session_id,
                next_client: 0,
            },
        )?
        .name("org.gnome.SessionManager")?
        .build()
        .await
        .context("error connecting to the session bus")?;
    tracing::info!("serving org.gnome.SessionManager");

    // Ends with the session.
    std::future::pending::<()>().await;
    Ok(())
}
