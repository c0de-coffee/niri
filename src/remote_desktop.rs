//! Compositor side of `org.gnome.Mutter.RemoteDesktop`: one EIS connection per session.

use calloop::RegistrationToken;
use smithay::backend::libei::{EiInput, EiInputConnection, EiInputEvent, EiInputSeat, EiRegion};

use crate::dbus::mutter_remote_desktop::{self, RemoteDesktopToNiri};
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
