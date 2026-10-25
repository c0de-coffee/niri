//! `org.gnome.Mutter.RemoteDesktop`, the private interface gnome-remote-desktop uses for input.
//!
//! Input goes through libei: the client calls `ConnectToEIS` and gets one end of a socket pair,
//! niri runs the EIS side of it (see `crate::remote_desktop`). The legacy `Notify*` methods are
//! not implemented, current gnome-remote-desktop doesn't call them.
//!
//! A screencast session created with `remote-desktop-session-id` is linked to a remote desktop
//! session: starting or stopping the remote desktop session starts or stops the screencast too.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use smithay::reexports::reis::eis;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{self, OwnedObjectPath, Value};
use zbus::{fdo, interface, ObjectServer};

use super::mutter_screen_cast;
use super::{request_name, Start};
use crate::utils::id::IdCounter;
use crate::utils::CastSessionId;

/// Remote desktop sessions by session ID, shared with the ScreenCast server for linking.
pub type RemoteDesktopSessions = Arc<Mutex<HashMap<String, Session>>>;

// From mutter's ClutterInputDeviceType bits as exposed over D-Bus.
const DEVICE_TYPE_KEYBOARD: u32 = 1 << 0;
const DEVICE_TYPE_POINTER: u32 = 1 << 1;
const DEVICE_TYPE_TOUCHSCREEN: u32 = 1 << 2;

#[derive(Clone)]
pub struct RemoteDesktop {
    sessions: RemoteDesktopSessions,
    to_niri: calloop::channel::Sender<RemoteDesktopToNiri>,
}

#[derive(Clone)]
pub struct Session {
    id: String,
    sessions: RemoteDesktopSessions,
    to_niri: calloop::channel::Sender<RemoteDesktopToNiri>,
    cast_session: Arc<Mutex<Option<mutter_screen_cast::Session>>>,
    started: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    eis_connected: Arc<AtomicBool>,
}

pub enum RemoteDesktopToNiri {
    ConnectEis {
        session_id: String,
        context: eis::Context,
    },
    Stopped {
        session_id: String,
        cast_session_id: Option<CastSessionId>,
    },
}

#[interface(name = "org.gnome.Mutter.RemoteDesktop")]
impl RemoteDesktop {
    async fn create_session(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<OwnedObjectPath> {
        static COUNTER: IdCounter = IdCounter::new();
        let id = COUNTER.next().to_string();

        let path = session_path(&id);
        let session = Session {
            id: id.clone(),
            sessions: self.sessions.clone(),
            to_niri: self.to_niri.clone(),
            cast_session: Arc::new(Mutex::new(None)),
            started: Arc::new(AtomicBool::new(false)),
            stopped: Arc::new(AtomicBool::new(false)),
            eis_connected: Arc::new(AtomicBool::new(false)),
        };

        match server.at(&path, session.clone()).await {
            Ok(true) => {
                self.sessions.lock().unwrap().insert(id, session);
            }
            Ok(false) => return Err(fdo::Error::Failed("session path already exists".to_owned())),
            Err(err) => {
                return Err(fdo::Error::Failed(format!(
                    "error creating session object: {err:?}"
                )))
            }
        }

        debug!(%path, "created remote desktop session");
        Ok(path)
    }

    #[zbus(property)]
    async fn supported_device_types(&self) -> u32 {
        DEVICE_TYPE_KEYBOARD | DEVICE_TYPE_POINTER | DEVICE_TYPE_TOUCHSCREEN
    }

    #[zbus(property)]
    async fn version(&self) -> i32 {
        2
    }
}

#[interface(name = "org.gnome.Mutter.RemoteDesktop.Session")]
impl Session {
    #[zbus(property)]
    async fn session_id(&self) -> String {
        self.id.clone()
    }

    async fn start(&self) -> fdo::Result<()> {
        debug!(id = self.id, "start");

        if self.stopped.load(Ordering::SeqCst) {
            return Err(fdo::Error::Failed("session is stopped".to_owned()));
        }
        if self.started.swap(true, Ordering::SeqCst) {
            return Err(fdo::Error::Failed("session is already started".to_owned()));
        }

        if let Some(cast_session) = &*self.cast_session.lock().unwrap() {
            cast_session.start_streams();
        }

        Ok(())
    }

    pub async fn stop(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(signal_context)] ctxt: SignalEmitter<'_>,
    ) {
        debug!(id = self.id, "stop");

        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }

        if let Err(err) = Session::closed(&ctxt).await {
            warn!("error emitting Closed: {err:?}");
        }

        self.sessions.lock().unwrap().remove(&self.id);

        let cast_session_id = self.cast_session.lock().unwrap().take().map(|s| s.id());
        if let Err(err) = self.to_niri.send(RemoteDesktopToNiri::Stopped {
            session_id: self.id.clone(),
            cast_session_id,
        }) {
            warn!("error sending Stopped to niri: {err:?}");
        }

        let _ = server.remove::<Session, _>(ctxt.path()).await;
    }

    // zbus would turn the method name into "ConnectToEis".
    #[zbus(name = "ConnectToEIS")]
    async fn connect_to_eis(
        &self,
        options: HashMap<&str, Value<'_>>,
    ) -> fdo::Result<zvariant::OwnedFd> {
        debug!(id = self.id, ?options, "connect_to_eis");

        if self.stopped.load(Ordering::SeqCst) {
            return Err(fdo::Error::Failed("session is stopped".to_owned()));
        }
        if self.eis_connected.swap(true, Ordering::SeqCst) {
            return Err(fdo::Error::Failed("already connected to EIS".to_owned()));
        }

        let (ours, theirs) = UnixStream::pair()
            .map_err(|err| fdo::Error::Failed(format!("error creating socket pair: {err}")))?;
        let context = eis::Context::new(ours)
            .map_err(|err| fdo::Error::Failed(format!("error creating EIS context: {err}")))?;

        self.to_niri
            .send(RemoteDesktopToNiri::ConnectEis {
                session_id: self.id.clone(),
                context,
            })
            .map_err(|_| fdo::Error::Failed("niri is gone".to_owned()))?;

        Ok(zvariant::OwnedFd::from(OwnedFd::from(theirs)))
    }

    #[zbus(signal)]
    async fn closed(ctxt: &SignalEmitter<'_>) -> zbus::Result<()>;
}

impl RemoteDesktop {
    pub fn new(
        sessions: RemoteDesktopSessions,
        to_niri: calloop::channel::Sender<RemoteDesktopToNiri>,
    ) -> Self {
        Self { sessions, to_niri }
    }
}

impl Start for RemoteDesktop {
    fn start(self, monitor: bool) -> anyhow::Result<zbus::blocking::Connection> {
        let conn = zbus::blocking::Connection::session()?;
        conn.object_server()
            .at("/org/gnome/Mutter/RemoteDesktop", self)?;
        request_name(&conn, "org.gnome.Mutter.RemoteDesktop", monitor)?;

        Ok(conn)
    }
}

impl Session {
    /// Links a screencast session to this remote desktop session.
    pub fn link_cast_session(&self, cast_session: mutter_screen_cast::Session) -> fdo::Result<()> {
        if self.stopped.load(Ordering::SeqCst) {
            return Err(fdo::Error::Failed(
                "remote desktop session is stopped".to_owned(),
            ));
        }

        let mut guard = self.cast_session.lock().unwrap();
        if guard.is_some() {
            return Err(fdo::Error::Failed(
                "remote desktop session already has a screencast session".to_owned(),
            ));
        }
        *guard = Some(cast_session);
        Ok(())
    }
}

pub fn session_path(id: &str) -> OwnedObjectPath {
    let path = format!("/org/gnome/Mutter/RemoteDesktop/Session/u{id}");
    OwnedObjectPath::try_from(path).unwrap()
}
