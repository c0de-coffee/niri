//! Virtual outputs: outputs without a display, created for a screencast (`RecordVirtual`).
//!
//! They work on every backend. They aren't rendered, only screencast (see `crate::offscreen`),
//! and their size follows what the screencast consumer asks for. gnome-remote-desktop uses them
//! for its "extend" screen sharing mode, which remote login always uses.
//!
//! They show up in the IPC output list (and so in `niri msg outputs` and DisplayConfig) next to
//! the backend's outputs, but not in wlr-output-management: their mode isn't up to clients.

use std::cell::Cell;
use std::collections::HashMap;

use niri_config::OutputName;
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::utils::{Logical, Physical, Point, Rectangle, Size};

use crate::backend::OutputId;
use crate::niri::Niri;
use crate::offscreen::DEFAULT_REFRESH_INTERVAL;
use crate::utils::output_size;
use crate::utils::scale::closest_representable_scale;
use crate::window::InitialConfigureState;

/// Size of a new virtual output, until the screencast consumer tells us what it wants.
const DEFAULT_SIZE: (i32, i32) = (1920, 1080);
const REFRESH_MHZ: i32 = 60_000;

/// Marks an output as virtual.
struct VirtualOutput {
    /// ID in the IPC output list.
    id: OutputId,
    /// Scale the screencast consumer asked for, if any.
    preferred_scale: Cell<Option<f64>>,
    /// Position in the consumer's monitor layout (physical pixels, primary monitor at 0,0).
    client_position: Cell<Option<Point<i32, Physical>>>,
    /// Whether this is the consumer's primary monitor.
    primary: Cell<bool>,
}

/// What a screencast consumer told us about the output it wants, in its stream's tag.
///
/// gnome-remote-desktop sends the scale for each RDP client monitor; with our patch
/// (`patches/gnome-remote-desktop/0002-*`) also its place in the client's monitor layout.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct ConsumerTag {
    pub scale: Option<f64>,
    pub position: Option<Point<i32, Physical>>,
    pub primary: Option<bool>,
}

pub fn is_virtual(output: &Output) -> bool {
    output.user_data().get::<VirtualOutput>().is_some()
}

/// Scale the screencast consumer asked for, used when the config doesn't set one.
///
/// gnome-remote-desktop asks for each RDP client monitor's scale, so a 150% monitor on the client
/// is a 1.5 output here.
pub fn virtual_output_scale(output: &Output) -> Option<f64> {
    output
        .user_data()
        .get::<VirtualOutput>()
        .and_then(|virt| virt.preferred_scale.get())
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
        output.user_data().insert_if_missing(|| VirtualOutput {
            id,
            preferred_scale: Cell::new(None),
            client_position: Cell::new(None),
            primary: Cell::new(false),
        });

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
                .is_some_and(|virt| virt.id == id)
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
        // Outputs are placed side by side by size, so a resized one may now overlap its neighbor.
        self.reposition_outputs(None);

        if let Some(VirtualOutput { id, .. }) = output.user_data().get() {
            if let Some(ipc_output) = self.ipc_outputs.lock().unwrap().get_mut(id) {
                ipc_output.modes = vec![ipc_mode(size)];
            }
        }

        self.on_virtual_outputs_changed();
    }

    /// Applies what the screencast consumer told us about the output.
    pub fn apply_virtual_output_tag(&mut self, output: &Output, tag: ConsumerTag) {
        if let Some(scale) = tag.scale {
            self.set_virtual_output_scale(output, scale);
        }

        let Some(virt) = output.user_data().get::<VirtualOutput>() else {
            return;
        };
        let moved = tag
            .position
            .is_some_and(|pos| virt.client_position.replace(Some(pos)) != Some(pos));
        let became_primary = match tag.primary {
            Some(primary) => !virt.primary.replace(primary) && primary,
            None => false,
        };

        if moved {
            debug!(name = output.name(), position = ?tag.position, "virtual output moved");
            self.reposition_outputs(None);
            self.on_virtual_outputs_changed();
        }
        if became_primary {
            self.gather_on_primary_virtual_output(output);
        }
    }

    /// Moves the workspaces with windows from the other virtual outputs to the consumer's primary
    /// monitor and focuses it.
    ///
    /// Windows opened before the outputs existed (a greeter, startup apps, everything after a
    /// reconnect) all end up on the first output that appears, which is whichever monitor the
    /// consumer happened to create first.
    fn gather_on_primary_virtual_output(&mut self, primary: &Output) {
        debug!(
            name = primary.name(),
            "gathering windows on the primary virtual output"
        );

        // Windows that got their initial configure before we knew which monitor is primary (a
        // greeter starting while the outputs appear) are headed to the output that was active
        // then; send them to the primary one instead. Their sizes are resolved again on mapping.
        for unmapped in self.unmapped_windows.values_mut() {
            if let InitialConfigureState::Configured {
                output: Some(output),
                ..
            } = &mut unmapped.state
            {
                if output != primary && is_virtual(output) {
                    *output = primary.clone();
                }
            }
        }

        // Bounded in case a move doesn't happen.
        for _ in 0..100 {
            let found = self.layout.workspaces().find_map(|(mon, idx, ws)| {
                let from = mon?.output();
                (from != primary && is_virtual(from) && ws.has_windows_or_name())
                    .then(|| (from.clone(), idx))
            });
            let Some((from, idx)) = found else {
                break;
            };
            self.layout
                .move_workspace_to_output_by_id(idx, Some(from), primary);
        }

        self.layout.focus_output(primary);
        self.queue_redraw_all();
    }

    /// Applies the scale the screencast consumer asked for, unless the config sets one.
    pub fn set_virtual_output_scale(&mut self, output: &Output, preferred: f64) {
        let Some(virt) = output.user_data().get::<VirtualOutput>() else {
            return;
        };
        virt.preferred_scale.set(Some(preferred));

        let name = output.user_data().get::<OutputName>().unwrap();
        let configured = self
            .config
            .borrow()
            .outputs
            .find(name)
            .and_then(|c| c.scale)
            .map(|s| s.0);
        let scale = closest_representable_scale(configured.unwrap_or(preferred).clamp(0.1, 10.));
        if output.current_scale().fractional_scale() == scale {
            return;
        }

        debug!(name = output.name(), scale, "setting virtual output scale");
        output.change_current_state(None, None, Some(Scale::Fractional(scale)), None);
        self.output_resized(output);
        // The logical size changed, so the outputs need placing again.
        self.reposition_outputs(None);

        self.on_virtual_outputs_changed();
    }

    pub fn remove_virtual_output(&mut self, output: &Output) {
        if !is_virtual(output) || !self.output_state.contains_key(output) {
            return;
        }

        debug!(name = output.name(), "removing virtual output");
        self.remove_output(output);

        if let Some(VirtualOutput { id, .. }) = output.user_data().get() {
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

/// Logical positions that arrange virtual outputs like the consumer's monitors.
///
/// Starting from the primary monitor at 0,0, each monitor that shares an edge with an already
/// placed one goes next to it on the same side. Working edge by edge in logical coordinates keeps
/// monitors with different scales adjacent (their client positions are in physical pixels).
/// Outputs without a client position, or not touching any placed one, are left out (automatic
/// placement).
pub fn client_layout_positions<'a>(
    outputs: impl Iterator<Item = &'a Output>,
) -> HashMap<Output, Point<i32, Logical>> {
    struct Monitor {
        output: Output,
        client: Rectangle<i32, Physical>,
        size: Size<i32, Logical>,
        scale: f64,
        primary: bool,
    }

    let monitors: Vec<Monitor> = outputs
        .filter_map(|output| {
            let virt = output.user_data().get::<VirtualOutput>()?;
            let pos = virt.client_position.get()?;
            Some(Monitor {
                output: output.clone(),
                client: Rectangle::new(pos, output.current_mode()?.size),
                size: output_size(output).to_i32_round(),
                scale: output.current_scale().fractional_scale(),
                primary: virt.primary.get(),
            })
        })
        .collect();

    let Some(start) = monitors.iter().position(|m| m.primary).or_else(|| {
        monitors
            .iter()
            .position(|m| m.client.loc == Point::from((0, 0)))
    }) else {
        return HashMap::new();
    };

    // Client monitor sizes come from the stream size, which grd may round a little.
    const SLACK: i32 = 8;
    let near = |a: i32, b: i32| (a - b).abs() <= SLACK;

    let mut placed: Vec<Option<Point<i32, Logical>>> = vec![None; monitors.len()];
    placed[start] = Some(Point::from((0, 0)));
    loop {
        let mut progress = false;
        for u in 0..monitors.len() {
            if placed[u].is_some() {
                continue;
            }
            let um = &monitors[u];
            let (uc, pc_iter) = (um.client, monitors.iter().zip(&placed));
            for (pm, p_pos) in pc_iter {
                let Some(p_pos) = *p_pos else { continue };
                let pc = pm.client;
                let overlap_x = uc.loc.x < pc.loc.x + pc.size.w && pc.loc.x < uc.loc.x + uc.size.w;
                let overlap_y = uc.loc.y < pc.loc.y + pc.size.h && pc.loc.y < uc.loc.y + uc.size.h;
                // Offset along the shared edge, in the placed monitor's logical units.
                let dx = (f64::from(uc.loc.x - pc.loc.x) / pm.scale).round() as i32;
                let dy = (f64::from(uc.loc.y - pc.loc.y) / pm.scale).round() as i32;

                let pos = if overlap_y && near(uc.loc.x, pc.loc.x + pc.size.w) {
                    (p_pos.x + pm.size.w, p_pos.y + dy)
                } else if overlap_y && near(uc.loc.x + uc.size.w, pc.loc.x) {
                    (p_pos.x - um.size.w, p_pos.y + dy)
                } else if overlap_x && near(uc.loc.y, pc.loc.y + pc.size.h) {
                    (p_pos.x + dx, p_pos.y + pm.size.h)
                } else if overlap_x && near(uc.loc.y + uc.size.h, pc.loc.y) {
                    (p_pos.x + dx, p_pos.y - um.size.h)
                } else {
                    continue;
                };
                placed[u] = Some(Point::from(pos));
                progress = true;
                break;
            }
        }
        if !progress {
            break;
        }
    }

    // Keep everything at non-negative coordinates.
    let min_x = placed.iter().flatten().map(|p| p.x).min().unwrap_or(0);
    let min_y = placed.iter().flatten().map(|p| p.y).min().unwrap_or(0);
    monitors
        .into_iter()
        .zip(placed)
        .filter_map(|(m, pos)| Some((m.output, pos? - Point::from((min_x, min_y)))))
        .collect()
}
