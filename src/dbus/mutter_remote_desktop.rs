//! `org.gnome.Mutter.RemoteDesktop`, the private interface gnome-remote-desktop uses for input.
//!
//! Input goes through libei: the client calls `ConnectToEIS` and gets one end of a socket pair,
//! niri runs the EIS side of it (see `crate::remote_desktop`). The legacy `Notify*` methods are
//! not implemented, current gnome-remote-desktop doesn't call them.
//!
//! A screencast session created with `remote-desktop-session-id` is linked to a remote desktop
//! session: starting or stopping the remote desktop session starts or stops the screencast too.
//!
//! Clipboard: once enabled, `SetSelection` makes the client's clipboard ours (niri then owns the
//! Wayland clipboard on its behalf, and asks for data with `SelectionTransfer`, answered with
//! `SelectionWrite`), and local clipboard changes are announced with `SelectionOwnerChanged`, read
//! with `SelectionRead`.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use smithay::reexports::reis::eis;
use smithay::reexports::rustix::pipe::{pipe_with, PipeFlags};
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
    clipboard_enabled: Arc<AtomicBool>,
    /// Fds of `SelectionTransfer` requests, until the client picks them up with `SelectionWrite`.
    transfers: Arc<Mutex<HashMap<u32, OwnedFd>>>,
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
    /// The client's clipboard has these types (empty: it has nothing anymore).
    SetSelection {
        session_id: String,
        mime_types: Vec<String>,
    },
    /// The client disabled the clipboard; drop the selection if it's theirs.
    ClipboardDisabled { session_id: String },
    /// Write the local clipboard in this type to `fd`.
    ReadSelection { mime_type: String, fd: OwnedFd },
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
            clipboard_enabled: Arc::new(AtomicBool::new(false)),
            transfers: Arc::new(Mutex::new(HashMap::new())),
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

    async fn enable_clipboard(&self, options: HashMap<&str, Value<'_>>) -> fdo::Result<()> {
        debug!(id = self.id, ?options, "enable_clipboard");
        self.clipboard_enabled.store(true, Ordering::SeqCst);

        // The client may already have something in its clipboard.
        let mime_types = mime_types(&options);
        if !mime_types.is_empty() {
            self.send(RemoteDesktopToNiri::SetSelection {
                session_id: self.id.clone(),
                mime_types,
            })?;
        }
        Ok(())
    }

    async fn disable_clipboard(&self) -> fdo::Result<()> {
        debug!(id = self.id, "disable_clipboard");
        self.clipboard_enabled.store(false, Ordering::SeqCst);
        // Unanswered transfers fail (the readers get EOF).
        self.transfers.lock().unwrap().clear();
        self.send(RemoteDesktopToNiri::ClipboardDisabled {
            session_id: self.id.clone(),
        })
    }

    async fn set_selection(&self, options: HashMap<&str, Value<'_>>) -> fdo::Result<()> {
        self.check_clipboard_enabled()?;
        let mime_types = mime_types(&options);
        debug!(id = self.id, ?mime_types, "set_selection");
        self.send(RemoteDesktopToNiri::SetSelection {
            session_id: self.id.clone(),
            mime_types,
        })
    }

    async fn selection_write(&self, serial: u32) -> fdo::Result<zvariant::OwnedFd> {
        self.check_clipboard_enabled()?;
        let fd = self.transfers.lock().unwrap().remove(&serial);
        let fd = fd.ok_or_else(|| fdo::Error::Failed(format!("no transfer {serial}")))?;
        Ok(zvariant::OwnedFd::from(fd))
    }

    async fn selection_write_done(&self, serial: u32, success: bool) -> fdo::Result<()> {
        debug!(id = self.id, serial, success, "selection_write_done");
        // Normally picked up by SelectionWrite already.
        self.transfers.lock().unwrap().remove(&serial);
        Ok(())
    }

    async fn selection_read(&self, mime_type: &str) -> fdo::Result<zvariant::OwnedFd> {
        self.check_clipboard_enabled()?;
        debug!(id = self.id, mime_type, "selection_read");

        let (read, write) = pipe_with(PipeFlags::CLOEXEC)
            .map_err(|err| fdo::Error::Failed(format!("error creating pipe: {err}")))?;
        self.send(RemoteDesktopToNiri::ReadSelection {
            mime_type: mime_type.to_owned(),
            fd: write,
        })?;
        Ok(zvariant::OwnedFd::from(read))
    }

    #[zbus(signal)]
    pub async fn selection_owner_changed(
        ctxt: &SignalEmitter<'_>,
        options: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn selection_transfer(
        ctxt: &SignalEmitter<'_>,
        mime_type: &str,
        serial: u32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn closed(ctxt: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// The "mime-types" option of the clipboard methods.
fn mime_types(options: &HashMap<&str, Value<'_>>) -> Vec<String> {
    options
        .get("mime-types")
        .and_then(|value| <Vec<String>>::try_from(value.try_clone().ok()?).ok())
        .unwrap_or_default()
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
    fn send(&self, msg: RemoteDesktopToNiri) -> fdo::Result<()> {
        self.to_niri
            .send(msg)
            .map_err(|_| fdo::Error::Failed("niri is gone".to_owned()))
    }

    fn check_clipboard_enabled(&self) -> fdo::Result<()> {
        if self.clipboard_enabled.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(fdo::Error::Failed("the clipboard isn't enabled".to_owned()))
        }
    }

    pub fn is_clipboard_enabled(&self) -> bool {
        self.clipboard_enabled.load(Ordering::SeqCst)
    }

    /// Keeps the fd of a `SelectionTransfer` for the client's `SelectionWrite`.
    pub fn add_transfer(&self, serial: u32, fd: OwnedFd) {
        self.transfers.lock().unwrap().insert(serial, fd);
    }

    pub fn id(&self) -> &str {
        &self.id
    }

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
