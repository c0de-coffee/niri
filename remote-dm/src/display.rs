//! GDM's remote display D-Bus interface, as gnome-remote-desktop's system daemon uses it.
//!
//! - `/org/gnome/DisplayManager/RemoteDisplayFactory`: grd calls `CreateRemoteDisplay` for every
//!   new RDP connection that has no routing token yet.
//! - `/org/gnome/DisplayManager/Displays`: an ObjectManager; each remote display is an object
//!   below it with `org.gnome.DisplayManager.RemoteDisplay` (`RemoteId`, `SessionId`). Once
//!   `SessionId` is set, grd offers the connection to the `grd --handover` running in that logind
//!   session.

use std::collections::HashMap;

use tokio::sync::mpsc;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::{fdo, interface, message::Header, Connection};

pub const BUS_NAME: &str = "org.gnome.DisplayManager";
pub const FACTORY_PATH: &str = "/org/gnome/DisplayManager/RemoteDisplayFactory";
pub const DISPLAYS_PATH: &str = "/org/gnome/DisplayManager/Displays";

/// The system user gnome-remote-desktop's system daemon runs as.
const GRD_USER: &str = "gnome-remote-desktop";

/// What grd asks for when a new client connects.
#[derive(Debug)]
pub struct RemoteDisplayRequest {
    /// Identifies the RDP client in grd (`/org/gnome/RemoteDesktop/Client/N`).
    pub remote_id: OwnedObjectPath,
    /// Client host name, if grd knows it.
    pub hostname: Option<String>,
    /// User already authenticated by the RDP layer (Kerberos / single sign-on), if any.
    pub preauthenticated_user: Option<String>,
}

pub struct Factory {
    requests: mpsc::UnboundedSender<RemoteDisplayRequest>,
    /// Another user allowed to call us, besides root and grd (for testing on a session bus).
    extra_allowed_uid: Option<u32>,
}

impl Factory {
    pub fn new(
        requests: mpsc::UnboundedSender<RemoteDisplayRequest>,
        extra_allowed_uid: Option<u32>,
    ) -> Self {
        Self {
            requests,
            extra_allowed_uid,
        }
    }
}

#[interface(name = "org.gnome.DisplayManager.RemoteDisplayFactory")]
impl Factory {
    async fn create_remote_display(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        properties: HashMap<String, OwnedValue>,
    ) -> fdo::Result<()> {
        check_caller(conn, &header, self.extra_allowed_uid).await?;

        let remote_id = properties
            .get("remote-id")
            .and_then(|v| OwnedObjectPath::try_from(v.try_clone().ok()?).ok())
            .ok_or_else(|| fdo::Error::InvalidArgs("missing remote-id".to_owned()))?;
        let string = |key: &str| {
            properties
                .get(key)
                .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
                .filter(|s| !s.is_empty())
        };

        let request = RemoteDisplayRequest {
            remote_id,
            hostname: string("hostname"),
            preauthenticated_user: string("preauthenticated-user"),
        };
        tracing::info!(?request, "CreateRemoteDisplay");

        self.requests
            .send(request)
            .map_err(|_| fdo::Error::Failed("shutting down".to_owned()))
    }
}

/// Only grd's system daemon (and root) may ask us to start login sessions.
async fn check_caller(
    conn: &Connection,
    header: &Header<'_>,
    extra_allowed_uid: Option<u32>,
) -> fdo::Result<()> {
    let sender = header
        .sender()
        .ok_or_else(|| fdo::Error::AccessDenied("no sender".to_owned()))?;
    let dbus = fdo::DBusProxy::new(conn).await?;
    let uid = dbus
        .get_connection_unix_user(sender.clone().into())
        .await?;

    let allowed = uid == 0 || uid_of(GRD_USER) == Some(uid) || extra_allowed_uid == Some(uid);
    if !allowed {
        tracing::warn!(%sender, uid, "refusing CreateRemoteDisplay");
        return Err(fdo::Error::AccessDenied("not allowed".to_owned()));
    }
    Ok(())
}

fn uid_of(user: &str) -> Option<u32> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        (fields.next()? == user).then_some(())?;
        fields.nth(1)?.parse().ok()
    })
}

pub struct RemoteDisplay {
    remote_id: OwnedObjectPath,
    session_id: String,
}

impl RemoteDisplay {
    pub fn new(remote_id: OwnedObjectPath, session_id: String) -> Self {
        Self {
            remote_id,
            session_id,
        }
    }
}

#[interface(name = "org.gnome.DisplayManager.RemoteDisplay")]
impl RemoteDisplay {
    #[zbus(property)]
    fn remote_id(&self) -> OwnedObjectPath {
        self.remote_id.clone()
    }

    #[zbus(property)]
    fn session_id(&self) -> String {
        self.session_id.clone()
    }
}

/// Remote display objects below `DISPLAYS_PATH`.
pub struct Displays {
    conn: Connection,
    next_id: u64,
}

impl Displays {
    pub fn new(conn: Connection) -> Self {
        Self { conn, next_id: 1 }
    }

    /// Exports a new remote display; returns its object path. `session_id` may be empty and set
    /// later.
    pub async fn add(
        &mut self,
        remote_id: OwnedObjectPath,
        session_id: &str,
    ) -> zbus::Result<OwnedObjectPath> {
        let path = format!("{DISPLAYS_PATH}/Remote{}", self.next_id);
        self.next_id += 1;
        let path = OwnedObjectPath::try_from(path).unwrap();

        self.conn
            .object_server()
            .at(&path, RemoteDisplay::new(remote_id, session_id.to_owned()))
            .await?;
        tracing::debug!(%path, "added remote display");
        Ok(path)
    }

    pub async fn set_session_id(&self, path: &OwnedObjectPath, session_id: &str) -> zbus::Result<()> {
        let iface = self
            .conn
            .object_server()
            .interface::<_, RemoteDisplay>(path)
            .await?;
        iface.get_mut().await.session_id = session_id.to_owned();
        let emitter: &SignalEmitter<'_> = iface.signal_emitter();
        iface.get().await.session_id_changed(emitter).await?;
        tracing::debug!(%path, session_id, "set remote display session");
        Ok(())
    }

    pub async fn set_remote_id(
        &self,
        path: &OwnedObjectPath,
        remote_id: OwnedObjectPath,
    ) -> zbus::Result<()> {
        let iface = self
            .conn
            .object_server()
            .interface::<_, RemoteDisplay>(path)
            .await?;
        iface.get_mut().await.remote_id = remote_id;
        iface.get().await.remote_id_changed(iface.signal_emitter()).await?;
        Ok(())
    }

    pub async fn remove(&self, path: &OwnedObjectPath) -> zbus::Result<()> {
        self.conn
            .object_server()
            .remove::<RemoteDisplay, _>(path)
            .await?;
        tracing::debug!(%path, "removed remote display");
        Ok(())
    }
}
