//! The bits of logind we use.

use std::time::Duration;

use zbus::zvariant::OwnedObjectPath;
use zbus::{proxy, Connection};

#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
pub trait Manager {
    fn terminate_user(&self, uid: u32) -> zbus::Result<()>;
    fn terminate_session(&self, session_id: &str) -> zbus::Result<()>;
    /// (session id, uid, user name, seat, object path)
    fn list_sessions(&self) -> zbus::Result<Vec<(String, u32, String, String, OwnedObjectPath)>>;
}

#[proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1"
)]
pub trait Session {
    #[zbus(property)]
    fn class(&self) -> zbus::Result<String>;
    #[zbus(property, name = "Type")]
    fn type_(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
}

/// The user's graphical sessions that are still running, except `exclude`.
///
/// One user has one session bus, so two desktops of the same user fight over it: both niris
/// claim `org.gnome.Mutter.*`, and grd in one session ends up talking to the other one's niri.
/// GDM doesn't allow it either (`StopConflictingSession`).
pub async fn graphical_sessions(
    system_bus: &Connection,
    uid: u32,
    exclude: &[String],
) -> zbus::Result<Vec<String>> {
    let manager = ManagerProxy::new(system_bus).await?;
    let mut found = Vec::new();
    for (id, session_uid, _, _, path) in manager.list_sessions().await? {
        if session_uid != uid || exclude.contains(&id) {
            continue;
        }
        let session = SessionProxy::builder(system_bus)
            .path(path)?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?;
        // Sessions can go away while we look; skip those.
        let (Ok(class), Ok(type_), Ok(state)) =
            (session.class().await, session.type_().await, session.state().await)
        else {
            continue;
        };
        if class == "user" && matches!(type_.as_str(), "wayland" | "x11" | "mir") && state != "closing" {
            found.push(id);
        }
    }
    Ok(found)
}

/// Ends the sessions and waits (up to 10 seconds) until they're gone, so that their compositor
/// has let go of the session bus names before the next one starts.
pub async fn terminate_sessions_and_wait(system_bus: &Connection, ids: &[String]) {
    let manager = match ManagerProxy::new(system_bus).await {
        Ok(manager) => manager,
        Err(err) => {
            tracing::warn!("error connecting to logind: {err:?}");
            return;
        }
    };
    for id in ids {
        if let Err(err) = manager.terminate_session(id).await {
            tracing::warn!(session_id = id, "TerminateSession: {err}");
        }
    }
    for _ in 0..100 {
        let Ok(sessions) = manager.list_sessions().await else {
            return;
        };
        if !sessions.iter().any(|(id, ..)| ids.contains(id)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tracing::warn!(?ids, "sessions still there after terminating them");
}

/// Kills everything the user still has running (systemd user instance included).
///
/// Greeter users are ours and run nothing else; their processes can outlive the session (apps in
/// systemd scopes) and would get in the way of the next greeter using the same user.
pub async fn terminate_user(system_bus: &Connection, uid: u32) {
    match ManagerProxy::new(system_bus).await {
        Ok(manager) => {
            if let Err(err) = manager.terminate_user(uid).await {
                // Fails when the user has nothing running anymore, which is fine.
                tracing::debug!(uid, "TerminateUser: {err}");
            }
        }
        Err(err) => tracing::warn!("error connecting to logind: {err:?}"),
    }
}
