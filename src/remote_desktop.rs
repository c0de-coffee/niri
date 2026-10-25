//! Compositor side of `org.gnome.Mutter.RemoteDesktop`: one EIS connection per session, and
//! clipboard sharing.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU32, Ordering};

use calloop::RegistrationToken;
use smithay::backend::libei::{EiInput, EiInputConnection, EiInputEvent, EiInputSeat, EiRegion};
use smithay::wayland::selection::data_device::{
    clear_data_device_selection, current_data_device_selection_userdata,
    request_data_device_client_selection, set_data_device_selection,
};
use zbus::zvariant::{StructureBuilder, Value};

use crate::dbus::mutter_remote_desktop::{self, RemoteDesktopToNiri};
use crate::handlers::SelectionData;
use crate::niri::{Niri, State};

pub struct EisClient {
    token: RegistrationToken,
    seat: Option<EiInputSeat>,
}

impl State {
    pub fn on_remote_desktop_msg(&mut self, msg: RemoteDesktopToNiri) {
        match msg {
            RemoteDesktopToNiri::ConnectEis {
                session_id,
                context,
            } => {
                let _span = debug_span!("ConnectEis", session_id).entered();

                let source = EiInput::new(context);
                let id = session_id.clone();
                let res = self
                    .niri
                    .event_loop
                    .insert_source(source, move |event, connection, state| {
                        state.on_eis_event(&id, event, connection)
                    });
                match res {
                    Ok(token) => {
                        let client = EisClient { token, seat: None };
                        self.niri.remote_desktop.insert(session_id, client);
                    }
                    Err(err) => {
                        warn!("error inserting EIS source: {:?}", err.error);
                        self.niri.stop_remote_desktop(&session_id);
                    }
                }
            }
            RemoteDesktopToNiri::Stopped {
                session_id,
                cast_session_id,
            } => {
                let _span = debug_span!("remote desktop Stopped", session_id).entered();

                if let Some(client) = self.niri.remote_desktop.remove(&session_id) {
                    if let Some(seat) = client.seat {
                        seat.remove();
                    }
                    self.niri.event_loop.remove(client.token);
                }

                if let Some(cast_session_id) = cast_session_id {
                    self.niri.stop_cast(cast_session_id);
                }

                self.niri.drop_remote_selection(&session_id);
            }
            RemoteDesktopToNiri::SetSelection {
                session_id,
                mime_types,
            } => {
                if mime_types.is_empty() {
                    self.niri.drop_remote_selection(&session_id);
                } else {
                    debug!(
                        session_id,
                        ?mime_types,
                        "remote desktop client owns the clipboard"
                    );
                    set_data_device_selection(
                        &self.niri.display_handle,
                        &self.niri.seat,
                        mime_types,
                        SelectionData::RemoteDesktop { session_id },
                    );
                }
            }
            RemoteDesktopToNiri::ClipboardDisabled { session_id } => {
                self.niri.drop_remote_selection(&session_id);
            }
            RemoteDesktopToNiri::ReadSelection { mime_type, fd } => {
                // On error, dropping the fd gives the reader EOF.
                if let Err(err) =
                    request_data_device_client_selection(&self.niri.seat, mime_type, fd)
                {
                    debug!("error reading the clipboard for remote desktop: {err}");
                }
            }
        }
    }

    fn on_eis_event(
        &mut self,
        session_id: &str,
        event: EiInputEvent,
        connection: &mut EiInputConnection,
    ) {
        match event {
            EiInputEvent::Connected => {
                debug!(session_id, "EIS client connected");

                let seat = connection.add_seat("default");

                let xkb = self.niri.config.borrow().input.keyboard.xkb.clone();
                if let Err(err) = seat.add_keyboard("virtual keyboard", xkb.to_xkb_config()) {
                    warn!("error adding EIS keyboard: {err:?}");
                }
                seat.add_pointer("virtual pointer");

                let regions = self.niri.eis_regions();
                seat.add_pointer_absolute("virtual absolute pointer", &regions);
                seat.add_touch("virtual touch", &regions);

                if let Some(client) = self.niri.remote_desktop.get_mut(session_id) {
                    client.seat = Some(seat);
                }
            }
            EiInputEvent::Disconnected => {
                // The client went away (likely crashed); tear the whole session down so that
                // the screencast doesn't keep running.
                debug!(session_id, "EIS client disconnected");
                self.niri.stop_remote_desktop(session_id);
            }
            EiInputEvent::Event(event) => self.process_input_event(event),
            EiInputEvent::TextKeysym { .. } | EiInputEvent::TextUtf8 { .. } => {
                // We don't add a text device to the seat.
            }
        }
    }
}

impl Niri {
    /// One region per output, in global logical coordinates, tied to the output's screencast
    /// stream by the output name.
    fn eis_regions(&self) -> Vec<EiRegion> {
        self.global_space
            .outputs()
            .map(|output| {
                let geo = self.global_space.output_geometry(output).unwrap();
                EiRegion {
                    rect: geo,
                    scale: output.current_scale().fractional_scale() as f32,
                    mapping_id: Some(output.name()),
                }
            })
            .collect()
    }

    pub fn update_eis_regions(&self) {
        if self.remote_desktop.is_empty() {
            return;
        }

        let regions = self.eis_regions();
        for client in self.remote_desktop.values() {
            if let Some(seat) = &client.seat {
                seat.update_regions(&regions);
            }
        }
    }

    /// Whether the clipboard currently is the remote desktop client's of `session_id`.
    fn remote_owns_selection(&self, session_id: &str) -> bool {
        current_data_device_selection_userdata(&self.seat).is_some_and(|data| {
            matches!(&*data, SelectionData::RemoteDesktop { session_id: id } if id == session_id)
        })
    }

    /// Clears the clipboard if it's the remote desktop client's of `session_id`.
    fn drop_remote_selection(&mut self, session_id: &str) {
        if self.remote_owns_selection(session_id) {
            debug!(session_id, "dropping remote desktop clipboard");
            clear_data_device_selection(&self.display_handle, &self.seat);
        }
    }

    /// Tells remote desktop clients with the clipboard enabled that a Wayland client set the
    /// clipboard.
    ///
    /// Called before the new selection is in place. It always is a Wayland client's (selections
    /// that niri sets for remote desktop clients don't come through here), so no session is the
    /// owner.
    pub fn on_clipboard_changed(&mut self, mime_types: Vec<String>) {
        let sessions: Vec<_> = self.remote_desktop_sessions_with_clipboard();
        if !sessions.is_empty() {
            debug!(
                ?mime_types,
                "clipboard changed, telling remote desktop clients"
            );
        }
        for session in sessions {
            // Like mutter, the mime types are in a tuple: "(as)" (grd ignores them otherwise).
            let mime_types = match StructureBuilder::new()
                .add_field(mime_types.clone())
                .build()
            {
                Ok(mime_types) => mime_types,
                Err(err) => {
                    warn!("error packing mime types: {err:?}");
                    return;
                }
            };
            let mut options = HashMap::new();
            options.insert("mime-types", Value::from(mime_types));
            options.insert("session-is-owner", Value::from(false));
            self.with_remote_desktop_session(session.id(), |iface| {
                async_io::block_on(mutter_remote_desktop::Session::selection_owner_changed(
                    iface.signal_emitter(),
                    options,
                ))
            });
        }
    }

    /// A Wayland client pastes the remote desktop client's clipboard: ask it for the data.
    pub fn remote_desktop_selection_transfer(
        &mut self,
        session_id: &str,
        mime_type: String,
        fd: OwnedFd,
    ) {
        static SERIAL: AtomicU32 = AtomicU32::new(1);

        let Some(session) = self
            .remote_desktop_sessions_with_clipboard()
            .into_iter()
            .find(|session| session.id() == session_id)
        else {
            // Dropping the fd gives the pasting client EOF.
            return;
        };

        let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
        session.add_transfer(serial, fd);
        self.with_remote_desktop_session(session_id, |iface| {
            async_io::block_on(mutter_remote_desktop::Session::selection_transfer(
                iface.signal_emitter(),
                &mime_type,
                serial,
            ))
        });
    }

    fn remote_desktop_sessions_with_clipboard(&self) -> Vec<mutter_remote_desktop::Session> {
        let Some(dbus) = &self.dbus else {
            return Vec::new();
        };
        let sessions = dbus.remote_desktop_sessions.lock().unwrap();
        sessions
            .values()
            .filter(|session| session.is_clipboard_enabled())
            .cloned()
            .collect()
    }

    fn with_remote_desktop_session(
        &self,
        session_id: &str,
        f: impl FnOnce(
            zbus::blocking::object_server::InterfaceRef<mutter_remote_desktop::Session>,
        ) -> zbus::Result<()>,
    ) {
        let Some(conn) = self
            .dbus
            .as_ref()
            .and_then(|dbus| dbus.conn_remote_desktop.as_ref())
        else {
            return;
        };
        let path = mutter_remote_desktop::session_path(session_id);
        match conn.object_server().interface(path) {
            Ok(iface) => {
                if let Err(err) = f(iface) {
                    warn!("error emitting remote desktop signal: {err:?}");
                }
            }
            Err(err) => debug!("no remote desktop session {session_id}: {err}"),
        }
    }

    pub fn stop_remote_desktop(&mut self, session_id: &str) {
        let _span = tracy_client::span!("Niri::stop_remote_desktop");

        let Some(dbus) = &self.dbus else { return };
        let Some(conn) = &dbus.conn_remote_desktop else {
            return;
        };

        let server = conn.object_server();
        let path = mutter_remote_desktop::session_path(session_id);
        if let Ok(iface) = server.interface::<_, mutter_remote_desktop::Session>(path) {
            async_io::block_on(async move {
                iface
                    .get()
                    .stop(server.inner(), iface.signal_emitter().clone())
                    .await
            });
        }
    }
}
