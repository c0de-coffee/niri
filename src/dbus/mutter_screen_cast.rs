use std::collections::HashMap;
use std::mem;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::zvariant::{DeserializeDict, OwnedObjectPath, SerializeDict, Type, Value};
use zbus::{fdo, interface, ObjectServer};

use super::mutter_remote_desktop::RemoteDesktopSessions;
use super::{request_name, Start};
use crate::backend::IpcOutputMap;
use crate::utils::{CastSessionId, CastStreamId};

#[derive(Clone)]
pub struct ScreenCast {
    ipc_outputs: Arc<Mutex<IpcOutputMap>>,
    to_niri: calloop::channel::Sender<ScreenCastToNiri>,
    #[allow(clippy::type_complexity)]
    sessions: Arc<Mutex<Vec<(Session, InterfaceRef<Session>)>>>,
    remote_desktop_sessions: RemoteDesktopSessions,
}

#[derive(Clone)]
pub struct Session {
    id: CastSessionId,
    ipc_outputs: Arc<Mutex<IpcOutputMap>>,
    to_niri: calloop::channel::Sender<ScreenCastToNiri>,
    #[allow(clippy::type_complexity)]
    streams: Arc<Mutex<Vec<(Stream, InterfaceRef<Stream>)>>>,
    stopped: Arc<AtomicBool>,
    /// Set when this session was created for a remote desktop session. Such a session is
    /// started and stopped together with the remote desktop session.
    remote_desktop_session_id: Option<String>,
}

#[derive(Debug, Default, Deserialize, Type, Clone, Copy, PartialEq, Eq)]
pub enum CursorMode {
    #[default]
    Hidden = 0,
    Embedded = 1,
    Metadata = 2,
}

#[derive(Debug, DeserializeDict, Type)]
#[zvariant(signature = "dict")]
struct RecordMonitorProperties {
    #[zvariant(rename = "cursor-mode")]
    cursor_mode: Option<CursorMode>,
    #[zvariant(rename = "is-recording")]
    _is_recording: Option<bool>,
}

#[derive(Debug, DeserializeDict, Type)]
#[zvariant(signature = "dict")]
struct RecordWindowProperties {
    #[zvariant(rename = "window-id")]
    window_id: u64,
    #[zvariant(rename = "cursor-mode")]
    cursor_mode: Option<CursorMode>,
    #[zvariant(rename = "is-recording")]
    _is_recording: Option<bool>,
}

#[derive(Clone)]
pub struct Stream {
    id: CastStreamId,
    session_id: CastSessionId,
    target: StreamTarget,
    cursor_mode: CursorMode,
    was_started: Arc<AtomicBool>,
    to_niri: calloop::channel::Sender<ScreenCastToNiri>,
}

#[derive(Clone)]
enum StreamTarget {
    // FIXME: update on scale changes and whatnot.
    Output(niri_ipc::Output),
    Window { id: u64 },
}

#[derive(Debug, Clone)]
pub enum StreamTargetId {
    Output { name: String },
    Window { id: u64 },
}

#[derive(Debug, SerializeDict, Type, Value)]
#[zvariant(signature = "dict")]
struct StreamParameters {
    /// Position of the stream in logical coordinates.
    position: (i32, i32),
    /// Size of the stream in logical coordinates.
    size: (i32, i32),
    /// Ties the stream to an EIS region with the same mapping ID (the output name).
    #[zvariant(rename = "mapping-id")]
    mapping_id: Option<String>,
}

pub enum ScreenCastToNiri {
    StartCast {
        session_id: CastSessionId,
        stream_id: CastStreamId,
        target: StreamTargetId,
        cursor_mode: CursorMode,
        signal_ctx: SignalEmitter<'static>,
    },
    StopCast {
        session_id: CastSessionId,
    },
    StopStream {
        session_id: CastSessionId,
        stream_id: CastStreamId,
    },
    StopRemoteDesktop {
        session_id: String,
    },
}

#[interface(name = "org.gnome.Mutter.ScreenCast")]
impl ScreenCast {
    async fn create_session(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        properties: HashMap<&str, Value<'_>>,
    ) -> fdo::Result<OwnedObjectPath> {
        let remote_desktop_session_id = match properties.get("remote-desktop-session-id") {
            None => None,
            Some(Value::Str(id)) => Some(id.to_string()),
            Some(_) => {
                return Err(fdo::Error::InvalidArgs(
                    "remote-desktop-session-id must be a string".to_owned(),
                ))
            }
        };

        let session_id = CastSessionId::next();
        let path = format!("/org/gnome/Mutter/ScreenCast/Session/u{}", session_id.get());
        let path = OwnedObjectPath::try_from(path).unwrap();

        let session = Session::new(
            session_id,
            self.ipc_outputs.clone(),
            self.to_niri.clone(),
            remote_desktop_session_id.clone(),
        );

        if let Some(rd_id) = &remote_desktop_session_id {
            let rd_session = self
                .remote_desktop_sessions
                .lock()
                .unwrap()
                .get(rd_id)
                .cloned();
            let Some(rd_session) = rd_session else {
                return Err(fdo::Error::Failed(
                    "no such remote desktop session".to_owned(),
                ));
            };
            rd_session.link_cast_session(session.clone())?;
        }

        match server.at(&path, session.clone()).await {
            Ok(true) => {
                let iface = server.interface(&path).await.unwrap();
                self.sessions.lock().unwrap().push((session, iface));
            }
            Ok(false) => return Err(fdo::Error::Failed("session path already exists".to_owned())),
            Err(err) => {
                return Err(fdo::Error::Failed(format!(
                    "error creating session object: {err:?}"
                )))
            }
        }

        Ok(path)
    }

    #[zbus(property)]
    async fn version(&self) -> i32 {
        4
    }
}

#[interface(name = "org.gnome.Mutter.ScreenCast.Session")]
impl Session {
    async fn start(&self) -> fdo::Result<()> {
        debug!("start");

        if self.remote_desktop_session_id.is_some() {
            return Err(fdo::Error::Failed(
                "must be started through the remote desktop session".to_owned(),
            ));
        }

        self.start_streams();
        Ok(())
    }

    pub async fn stop(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(signal_context)] ctxt: SignalEmitter<'_>,
    ) {
        debug!("stop");

        if self.stopped.swap(true, Ordering::SeqCst) {
            // Already stopped.
            return;
        }

        Session::closed(&ctxt).await.unwrap();

        if let Err(err) = self.to_niri.send(ScreenCastToNiri::StopCast {
            session_id: self.id,
        }) {
            warn!("error sending StopCast to niri: {err:?}");
        }

        if let Some(id) = &self.remote_desktop_session_id {
            if let Err(err) = self.to_niri.send(ScreenCastToNiri::StopRemoteDesktop {
                session_id: id.clone(),
            }) {
                warn!("error sending StopRemoteDesktop to niri: {err:?}");
            }
        }

        let streams = mem::take(&mut *self.streams.lock().unwrap());
        for (_, iface) in streams.iter() {
            // The stream may have been stopped and removed on its own already.
            let _ = server
                .remove::<Stream, _>(iface.signal_emitter().path())
                .await;
        }

        server.remove::<Session, _>(ctxt.path()).await.unwrap();
    }

    async fn record_monitor(
        &mut self,
        #[zbus(object_server)] server: &ObjectServer,
        connector: &str,
        properties: RecordMonitorProperties,
    ) -> fdo::Result<OwnedObjectPath> {
        debug!(connector, ?properties, "record_monitor");

        let output = {
            let ipc_outputs = self.ipc_outputs.lock().unwrap();
            if connector.is_empty() {
                // Empty connector means the primary monitor (gnome-remote-desktop mirror mode).
                // niri has no primary monitor, so take the top-left enabled one.
                ipc_outputs
                    .values()
                    .filter_map(|o| o.logical.as_ref().map(|l| (o, (l.y, l.x))))
                    .min_by_key(|(_, pos)| *pos)
                    .map(|(o, _)| o.clone())
            } else {
                ipc_outputs.values().find(|o| o.name == connector).cloned()
            }
        };
        let Some(output) = output else {
            return Err(fdo::Error::Failed("no such monitor".to_owned()));
        };

        if output.logical.is_none() {
            return Err(fdo::Error::Failed("monitor is disabled".to_owned()));
        }

        let stream_id = CastStreamId::next();
        let path = format!("/org/gnome/Mutter/ScreenCast/Stream/u{}", stream_id.get());
        let path = OwnedObjectPath::try_from(path).unwrap();

        let cursor_mode = properties.cursor_mode.unwrap_or_default();

        let target = StreamTarget::Output(output);
        let stream = Stream::new(
            stream_id,
            self.id,
            target,
            cursor_mode,
            self.to_niri.clone(),
        );
        match server.at(&path, stream.clone()).await {
            Ok(true) => {
                let iface = server.interface(&path).await.unwrap();
                self.streams.lock().unwrap().push((stream, iface));
            }
            Ok(false) => return Err(fdo::Error::Failed("stream path already exists".to_owned())),
            Err(err) => {
                return Err(fdo::Error::Failed(format!(
                    "error creating stream object: {err:?}"
                )))
            }
        }

        Ok(path)
    }

    async fn record_window(
        &mut self,
        #[zbus(object_server)] server: &ObjectServer,
        properties: RecordWindowProperties,
    ) -> fdo::Result<OwnedObjectPath> {
        debug!(?properties, "record_window");

        let stream_id = CastStreamId::next();
        let path = format!("/org/gnome/Mutter/ScreenCast/Stream/u{}", stream_id.get());
        let path = OwnedObjectPath::try_from(path).unwrap();

        let cursor_mode = properties.cursor_mode.unwrap_or_default();

        let target = StreamTarget::Window {
            id: properties.window_id,
        };
        let stream = Stream::new(
            stream_id,
            self.id,
            target,
            cursor_mode,
            self.to_niri.clone(),
        );
        match server.at(&path, stream.clone()).await {
            Ok(true) => {
                let iface = server.interface(&path).await.unwrap();
                self.streams.lock().unwrap().push((stream, iface));
            }
            Ok(false) => return Err(fdo::Error::Failed("stream path already exists".to_owned())),
            Err(err) => {
                return Err(fdo::Error::Failed(format!(
                    "error creating stream object: {err:?}"
                )))
            }
        }

        Ok(path)
    }

    #[zbus(signal)]
    async fn closed(ctxt: &SignalEmitter<'_>) -> zbus::Result<()>;
}

#[interface(name = "org.gnome.Mutter.ScreenCast.Stream")]
impl Stream {
    #[zbus(name = "Start")]
    async fn start_dbus(&self, #[zbus(signal_context)] ctxt: SignalEmitter<'_>) {
        debug!(stream_id = %self.id, "stream start");
        self.start_cast(ctxt.to_owned());
    }

    #[zbus(name = "Stop")]
    async fn stop_dbus(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(signal_context)] ctxt: SignalEmitter<'_>,
    ) {
        debug!(stream_id = %self.id, "stream stop");

        if let Err(err) = self.to_niri.send(ScreenCastToNiri::StopStream {
            session_id: self.session_id,
            stream_id: self.id,
        }) {
            warn!("error sending StopStream to niri: {err:?}");
        }

        let _ = server.remove::<Stream, _>(ctxt.path()).await;
    }

    #[zbus(signal)]
    pub async fn pipe_wire_stream_added(ctxt: &SignalEmitter<'_>, node_id: u32)
        -> zbus::Result<()>;

    #[zbus(property)]
    async fn parameters(&self) -> StreamParameters {
        match &self.target {
            StreamTarget::Output(output) => {
                let logical = output.logical.as_ref().unwrap();
                StreamParameters {
                    position: (logical.x, logical.y),
                    size: (logical.width as i32, logical.height as i32),
                    mapping_id: Some(output.name.clone()),
                }
            }
            StreamTarget::Window { .. } => {
                // Does any consumer need this?
                StreamParameters {
                    position: (0, 0),
                    size: (1, 1),
                    mapping_id: None,
                }
            }
        }
    }
}

impl ScreenCast {
    pub fn new(
        ipc_outputs: Arc<Mutex<IpcOutputMap>>,
        to_niri: calloop::channel::Sender<ScreenCastToNiri>,
        remote_desktop_sessions: RemoteDesktopSessions,
    ) -> Self {
        Self {
            ipc_outputs,
            to_niri,
            sessions: Arc::new(Mutex::new(vec![])),
            remote_desktop_sessions,
        }
    }
}

impl Start for ScreenCast {
    fn start(self, monitor: bool) -> anyhow::Result<zbus::blocking::Connection> {
        let conn = zbus::blocking::Connection::session()?;
        conn.object_server()
            .at("/org/gnome/Mutter/ScreenCast", self)?;
        request_name(&conn, "org.gnome.Mutter.ScreenCast", monitor)?;

        Ok(conn)
    }
}

impl Session {
    pub fn new(
        id: CastSessionId,
        ipc_outputs: Arc<Mutex<IpcOutputMap>>,
        to_niri: calloop::channel::Sender<ScreenCastToNiri>,
        remote_desktop_session_id: Option<String>,
    ) -> Self {
        Self {
            id,
            ipc_outputs,
            streams: Arc::new(Mutex::new(vec![])),
            to_niri,
            stopped: Arc::new(AtomicBool::new(false)),
            remote_desktop_session_id,
        }
    }

    pub fn id(&self) -> CastSessionId {
        self.id
    }

    pub fn remote_desktop_session_id(&self) -> Option<&str> {
        self.remote_desktop_session_id.as_deref()
    }

    /// Starts all streams recorded so far.
    pub fn start_streams(&self) {
        for (stream, iface) in &*self.streams.lock().unwrap() {
            stream.start_cast(iface.signal_emitter().clone());
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.to_niri.send(ScreenCastToNiri::StopCast {
            session_id: self.id,
        });
    }
}

impl Stream {
    fn new(
        id: CastStreamId,
        session_id: CastSessionId,
        target: StreamTarget,
        cursor_mode: CursorMode,
        to_niri: calloop::channel::Sender<ScreenCastToNiri>,
    ) -> Self {
        Self {
            id,
            session_id,
            target,
            cursor_mode,
            was_started: Arc::new(AtomicBool::new(false)),
            to_niri,
        }
    }

    fn start_cast(&self, ctxt: SignalEmitter<'static>) {
        // Both Session.Start and Stream.Start can get here; only start once.
        if self.was_started.swap(true, Ordering::SeqCst) {
            return;
        }

        let msg = ScreenCastToNiri::StartCast {
            session_id: self.session_id,
            stream_id: self.id,
            target: self.target.make_id(),
            cursor_mode: self.cursor_mode,
            signal_ctx: ctxt,
        };

        if let Err(err) = self.to_niri.send(msg) {
            warn!("error sending StartCast to niri: {err:?}");
        }
    }
}

impl StreamTarget {
    fn make_id(&self) -> StreamTargetId {
        match self {
            StreamTarget::Output(output) => StreamTargetId::Output {
                name: output.name.clone(),
            },
            StreamTarget::Window { id } => StreamTargetId::Window { id: *id },
        }
    }
}
