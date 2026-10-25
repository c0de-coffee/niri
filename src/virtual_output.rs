//! Virtual outputs: outputs without a display, created for a screencast (`RecordVirtual`).
//!
//! They work on every backend. They aren't rendered, only screencast (see `crate::offscreen`),
//! and their size follows what the screencast consumer asks for. gnome-remote-desktop uses them
//! for its "extend" screen sharing mode, which remote login always uses.
//!
//! They show up in the IPC output list (and so in `niri msg outputs` and DisplayConfig) next to
//! the backend's outputs, but not in wlr-output-management: their mode isn't up to clients.

use niri_config::OutputName;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::utils::{Physical, Size};

use crate::backend::OutputId;
use crate::niri::Niri;
use crate::offscreen::DEFAULT_REFRESH_INTERVAL;

/// Size of a new virtual output, until the screencast consumer tells us what it wants.
const DEFAULT_SIZE: (i32, i32) = (1920, 1080);
const REFRESH_MHZ: i32 = 60_000;

/// Marks an output as virtual; its ID in the IPC output list.
struct VirtualOutput(OutputId);

pub fn is_virtual(output: &Output) -> bool {
    output.user_data().get::<VirtualOutput>().is_some()
}

fn ipc_mode(size: Size<i32, Physical>) -> niri_ipc::Mode {
    niri_ipc::Mode {
        width: size.w.clamp(0, u16::MAX as i32) as u16,
        height: size.h.clamp(0, u16::MAX as i32) as u16,
        refresh_rate: REFRESH_MHZ as u32,
        is_preferred: true,
    }
}

impl Niri {
    pub fn add_virtual_output(&mut self, name: &str) -> Output {
        let output = Output::new(
            name.to_owned(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "niri".to_owned(),
                model: "Virtual".to_owned(),
                serial_number: String::new(),
            },
        );

        let mode = Mode {
            size: DEFAULT_SIZE.into(),
            refresh: REFRESH_MHZ,
        };
        output.change_current_state(Some(mode), None, None, None);
        output.set_preferred(mode);

        output.user_data().insert_if_missing(|| OutputName {
            connector: name.to_owned(),
            make: Some("niri".to_owned()),
            model: Some("Virtual".to_owned()),
            serial: None,
        });
        let id = OutputId::next();
        output.user_data().insert_if_missing(|| VirtualOutput(id));

        debug!(name, "adding virtual output");
        self.add_output(output.clone(), Some(DEFAULT_REFRESH_INTERVAL), false);

        // State::refresh_ipc_outputs() fills in the logical part.
        let ipc_output = niri_ipc::Output {
            name: name.to_owned(),
            make: "niri".to_owned(),
            model: "Virtual".to_owned(),
            serial: None,
            physical_size: None,
            modes: vec![ipc_mode(mode.size)],
            current_mode: Some(0),
            is_custom_mode: false,
            vrr_supported: false,
            vrr_enabled: false,
            logical: None,
            max_bpc: None,
        };
        self.ipc_outputs.lock().unwrap().insert(id, ipc_output);

        self.on_virtual_outputs_changed();
        output
    }

    /// Whether `id` is a virtual output's ID in the IPC output list.
    pub fn is_virtual_output_id(&self, id: OutputId) -> bool {
        self.output_state.keys().any(|output| {
            output
                .user_data()
                .get::<VirtualOutput>()
                .is_some_and(|virt| virt.0 == id)
        })
    }

    pub fn resize_virtual_output(&mut self, output: &Output, size: Size<i32, Physical>) {
        if output.current_mode().is_some_and(|mode| mode.size == size) {
            return;
        }

        debug!(name = output.name(), ?size, "resizing virtual output");
        let mode = Mode {
            size,
            refresh: REFRESH_MHZ,
        };
        output.change_current_state(Some(mode), None, None, None);
        output.set_preferred(mode);
        self.output_resized(output);

        if let Some(VirtualOutput(id)) = output.user_data().get() {
            if let Some(ipc_output) = self.ipc_outputs.lock().unwrap().get_mut(id) {
                ipc_output.modes = vec![ipc_mode(size)];
            }
        }

        self.on_virtual_outputs_changed();
    }

    pub fn remove_virtual_output(&mut self, output: &Output) {
        if !is_virtual(output) || !self.output_state.contains_key(output) {
            return;
        }

        debug!(name = output.name(), "removing virtual output");
        self.remove_output(output);

        if let Some(VirtualOutput(id)) = output.user_data().get() {
            self.ipc_outputs.lock().unwrap().remove(id);
        }

        self.on_virtual_outputs_changed();
    }

    fn on_virtual_outputs_changed(&mut self) {
        // State::refresh_ipc_outputs() then tells IPC and D-Bus clients.
        self.ipc_outputs_changed = true;

        // Absolute pointer regions of remote desktop clients: right away, the screencast that
        // made the change may be waiting for them.
        #[cfg(feature = "xdp-gnome-screencast")]
        self.update_eis_regions();
    }
}
