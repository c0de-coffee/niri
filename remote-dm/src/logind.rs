//! The bits of logind we use.

use zbus::{proxy, Connection};

#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
pub trait Manager {
    fn terminate_user(&self, uid: u32) -> zbus::Result<()>;
    fn terminate_session(&self, session_id: &str) -> zbus::Result<()>;
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
