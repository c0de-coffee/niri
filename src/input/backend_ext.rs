use ::input as libinput;
use smithay::backend::input;
use smithay::backend::winit::WinitVirtualDevice;
use smithay::output::Output;
use smithay::wayland::virtual_keyboard::VirtualKeyboardDevice;
use smithay::wayland::virtual_pointer::VirtualPointerDevice;

use crate::niri::State;

pub trait NiriInputBackend: input::InputBackend<Device = Self::NiriDevice> {
    type NiriDevice: NiriInputDevice;
}
impl<T: input::InputBackend> NiriInputBackend for T
where
    Self::Device: NiriInputDevice,
{
    type NiriDevice = Self::Device;
}

pub trait NiriInputDevice: input::Device {
    // FIXME: this should maybe be per-event, not per-device,
    // but it's not clear that this matters in practice?
    // it might be more obvious once we implement it for libinput
    fn output(&self, state: &State) -> Option<Output>;

    /// Whether absolute positions from this device are already in global logical coordinates,
    /// rather than normalized to an output.
    fn absolute_position_is_global(&self) -> bool {
        false
    }

    /// For devices that send wheel scrolling as plain deltas without discrete (v120) steps: the
    /// delta that one wheel notch corresponds to.
    ///
    /// Wheel bindings need the discrete steps, so they get synthesized from this.
    fn delta_per_wheel_notch(&self) -> Option<f64> {
        None
    }
}

/// Remote desktop (EIS) devices send absolute positions in the global logical coordinates of
/// the regions we advertise.
impl NiriInputDevice for smithay::reexports::reis::request::Device {
    fn output(&self, _: &State) -> Option<Output> {
        None
    }

    fn absolute_position_is_global(&self) -> bool {
        true
    }

    // gnome-remote-desktop turns each RDP wheel notch into a scroll delta of 10, without a
    // discrete event (the same step that mutter/clutter use).
    fn delta_per_wheel_notch(&self) -> Option<f64> {
        Some(10.)
    }
}

impl NiriInputDevice for libinput::Device {
    fn output(&self, _state: &State) -> Option<Output> {
        // FIXME: Allow specifying the output per-device?
        None
    }
}

impl NiriInputDevice for WinitVirtualDevice {
    fn output(&self, _state: &State) -> Option<Output> {
        // FIXME: we should be returning the single output that the winit backend creates,
        // but for now, that will cause issues because the output is normally upside down,
        // so we apply Transform::Flipped180 to it and that would also cause
        // the cursor position to be flipped, which is not what we want.
        //
        // instead, we just return None and rely on the fact that it has only one output.
        // doing so causes the cursor to be placed in *global* output coordinates,
        // which are not flipped, and happen to be what we want.
        None
    }
}

impl NiriInputDevice for VirtualPointerDevice {
    fn output(&self, _: &State) -> Option<Output> {
        self.output()
    }
}

impl NiriInputDevice for VirtualKeyboardDevice {
    fn output(&self, _: &State) -> Option<Output> {
        None
    }
}
