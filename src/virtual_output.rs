//! Virtual outputs: outputs without a display, created for a screencast (`RecordVirtual`).
//!
//! They work on every backend. They aren't rendered, only screencast (see `crate::offscreen`),
//! and their size follows what the screencast consumer asks for. gnome-remote-desktop uses them
//! for its "extend" screen sharing mode, which remote login always uses.

use niri_config::OutputName;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::utils::{Physical, Size};

use crate::niri::Niri;
use crate::offscreen::DEFAULT_REFRESH_INTERVAL;

/// Size of a new virtual output, until the screencast consumer tells us what it wants.
const DEFAULT_SIZE: (i32, i32) = (1920, 1080);
const REFRESH_MHZ: i32 = 60_000;

/// Marks an output as virtual.
struct VirtualOutput;

pub fn is_virtual(output: &Output) -> bool {
    output.user_data().get::<VirtualOutput>().is_some()
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
        output.user_data().insert_if_missing(|| VirtualOutput);

        debug!(name, "adding virtual output");
        self.add_output(output.clone(), Some(DEFAULT_REFRESH_INTERVAL), false);
        self.on_virtual_outputs_changed();
        output
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
        self.on_virtual_outputs_changed();
    }

    pub fn remove_virtual_output(&mut self, output: &Output) {
        if !is_virtual(output) || !self.output_state.contains_key(output) {
            return;
        }

        debug!(name = output.name(), "removing virtual output");
        self.remove_output(output);
        self.on_virtual_outputs_changed();
    }

    /// Virtual outputs aren't in the backend's IPC output list, so things keyed off that list
    /// changing need a nudge.
    fn on_virtual_outputs_changed(&self) {
        // Absolute pointer regions of remote desktop clients.
        #[cfg(feature = "xdp-gnome-screencast")]
        self.update_eis_regions();
    }
}
