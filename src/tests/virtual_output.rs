use niri_config::Config;
use smithay::utils::Size;

use super::*;

/// The IPC output list, sorted by name.
fn ipc_outputs(f: &mut Fixture) -> Vec<niri_ipc::Output> {
    let state = f.niri_state();
    state.refresh_ipc_outputs();
    let mut outputs: Vec<_> = state
        .backend
        .ipc_outputs()
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect();
    outputs.sort_by(|a, b| a.name.cmp(&b.name));
    outputs
}

fn names(outputs: &[niri_ipc::Output]) -> Vec<&str> {
    outputs.iter().map(|o| o.name.as_str()).collect()
}

#[test]
fn virtual_outputs_are_in_the_ipc_output_list() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let output = f.niri().add_virtual_output("Virtual-1");
    let outputs = ipc_outputs(&mut f);
    assert_eq!(names(&outputs), ["Virtual-1", "headless-1"]);
    let virt = &outputs[0];
    assert_eq!(
        (virt.make.as_str(), virt.model.as_str()),
        ("niri", "Virtual")
    );
    assert_eq!(virt.current_mode, Some(0));
    assert_eq!((virt.modes[0].width, virt.modes[0].height), (1920, 1080));
    let logical = virt.logical.unwrap();
    assert_eq!((logical.width, logical.height), (1920, 1080));

    f.niri()
        .resize_virtual_output(&output, Size::from((1280, 720)));
    let outputs = ipc_outputs(&mut f);
    let virt = &outputs[0];
    assert_eq!((virt.modes[0].width, virt.modes[0].height), (1280, 720));
    let logical = virt.logical.unwrap();
    assert_eq!((logical.width, logical.height), (1280, 720));

    f.niri().remove_virtual_output(&output);
    assert_eq!(names(&ipc_outputs(&mut f)), ["headless-1"]);
}

fn logical_rect(outputs: &[niri_ipc::Output], name: &str) -> (i32, i32, u32, u32) {
    let logical = outputs
        .iter()
        .find(|o| o.name == name)
        .unwrap()
        .logical
        .unwrap();
    (logical.x, logical.y, logical.width, logical.height)
}

#[test]
fn resized_virtual_outputs_dont_overlap() {
    let mut f = Fixture::new();
    let first = f.niri().add_virtual_output("Virtual-1");
    let second = f.niri().add_virtual_output("Virtual-2");

    // grd creates both at the default size, then resizes them to the client's monitors.
    f.niri()
        .resize_virtual_output(&first, Size::from((2560, 1440)));
    f.niri()
        .resize_virtual_output(&second, Size::from((2560, 1440)));

    let outputs = ipc_outputs(&mut f);
    assert_eq!(logical_rect(&outputs, "Virtual-1"), (0, 0, 2560, 1440));
    assert_eq!(logical_rect(&outputs, "Virtual-2"), (2560, 0, 2560, 1440));
}

#[test]
fn virtual_outputs_use_the_preferred_scale() {
    let mut f = Fixture::new();
    let first = f.niri().add_virtual_output("Virtual-1");
    let second = f.niri().add_virtual_output("Virtual-2");
    f.niri()
        .resize_virtual_output(&first, Size::from((2560, 1440)));
    f.niri()
        .resize_virtual_output(&second, Size::from((2560, 1440)));

    f.niri().set_virtual_output_scale(&first, 1.5);

    let outputs = ipc_outputs(&mut f);
    let virt = outputs.iter().find(|o| o.name == "Virtual-1").unwrap();
    assert_eq!(virt.logical.unwrap().scale, 1.5);
    assert_eq!(logical_rect(&outputs, "Virtual-1"), (0, 0, 1706, 960));
    assert_eq!(logical_rect(&outputs, "Virtual-2"), (1707, 0, 2560, 1440));

    // Reloading the output config keeps it.
    f.niri_state().reload_output_config();
    let outputs = ipc_outputs(&mut f);
    let virt = outputs.iter().find(|o| o.name == "Virtual-1").unwrap();
    assert_eq!(virt.logical.unwrap().scale, 1.5);
}

#[test]
fn configured_scale_wins_over_the_preferred_one() {
    let config = Config::parse_mem(r#"output "Virtual-1" { scale 2; }"#).unwrap();
    let mut f = Fixture::with_config(config);
    let output = f.niri().add_virtual_output("Virtual-1");

    f.niri().set_virtual_output_scale(&output, 1.5);

    let outputs = ipc_outputs(&mut f);
    assert_eq!(outputs[0].logical.unwrap().scale, 2.);
}

use smithay::output::Output;

use crate::virtual_output::ConsumerTag;

fn client_tag(x: i32, y: i32, primary: bool, scale: f64) -> ConsumerTag {
    ConsumerTag {
        scale: Some(scale),
        position: Some(smithay::utils::Point::from((x, y))),
        primary: Some(primary),
    }
}

/// Two 2560×1440 client monitors like an RDP client with a 150% laptop and a 100% monitor; the
/// laptop's stream comes first (grd creates them in the client's monitor order).
fn laptop_and_monitor(f: &mut Fixture, laptop_x: i32) -> (Output, Output) {
    let laptop = f.niri().add_virtual_output("Virtual-1");
    let monitor = f.niri().add_virtual_output("Virtual-2");
    f.niri()
        .resize_virtual_output(&laptop, Size::from((2560, 1440)));
    f.niri()
        .resize_virtual_output(&monitor, Size::from((2560, 1440)));
    f.niri()
        .apply_virtual_output_tag(&laptop, client_tag(laptop_x, 0, false, 1.5));
    f.niri()
        .apply_virtual_output_tag(&monitor, client_tag(0, 0, true, 1.));
    (laptop, monitor)
}

#[test]
fn virtual_outputs_follow_the_client_layout() {
    // Client: monitor (primary) on the left, laptop on the right.
    let mut f = Fixture::new();
    laptop_and_monitor(&mut f, 2560);
    let outputs = ipc_outputs(&mut f);
    assert_eq!(logical_rect(&outputs, "Virtual-2"), (0, 0, 2560, 1440));
    assert_eq!(logical_rect(&outputs, "Virtual-1"), (2560, 0, 1706, 960));

    // Client: laptop on the left. Adjacent despite the different scales (positions round the 1706.67 logical width up).
    let mut f = Fixture::new();
    laptop_and_monitor(&mut f, -2560);
    let outputs = ipc_outputs(&mut f);
    assert_eq!(logical_rect(&outputs, "Virtual-1"), (0, 0, 1706, 960));
    assert_eq!(logical_rect(&outputs, "Virtual-2"), (1707, 0, 2560, 1440));
}

#[test]
fn virtual_outputs_stacked_vertically() {
    let mut f = Fixture::new();
    let top = f.niri().add_virtual_output("Virtual-1");
    let bottom = f.niri().add_virtual_output("Virtual-2");
    f.niri()
        .resize_virtual_output(&top, Size::from((1920, 1080)));
    f.niri()
        .resize_virtual_output(&bottom, Size::from((1920, 1080)));
    f.niri()
        .apply_virtual_output_tag(&bottom, client_tag(0, 0, true, 1.));
    f.niri()
        .apply_virtual_output_tag(&top, client_tag(0, -1080, false, 1.));

    let outputs = ipc_outputs(&mut f);
    assert_eq!(logical_rect(&outputs, "Virtual-1"), (0, 0, 1920, 1080));
    assert_eq!(logical_rect(&outputs, "Virtual-2"), (0, 1080, 1920, 1080));
}

#[test]
fn configured_position_wins_over_the_client_layout() {
    let config = Config::parse_mem(
        r#"
        output "Virtual-1" { position x=0 y=0; }
        output "Virtual-2" { position x=1707 y=0; }
        "#,
    )
    .unwrap();
    let mut f = Fixture::with_config(config);
    laptop_and_monitor(&mut f, 2560);
    let outputs = ipc_outputs(&mut f);
    assert_eq!(logical_rect(&outputs, "Virtual-1").0, 0);
    assert_eq!(logical_rect(&outputs, "Virtual-2").0, 1707);
}

#[test]
fn windows_move_to_the_primary_virtual_output() {
    let mut f = Fixture::new();

    // A greeter opened before there were any outputs.
    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);
    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    // The laptop comes first and gets the window, then the primary monitor shows up.
    let (_laptop, monitor) = laptop_and_monitor(&mut f, 2560);

    let niri = f.niri();
    assert_eq!(niri.layout.active_output(), Some(&monitor));
    let (mon, _) = niri.layout.windows().next().unwrap();
    assert_eq!(mon.unwrap().output(), &monitor);
}

#[test]
fn windows_configured_before_the_primary_is_known_open_on_it() {
    let mut f = Fixture::new();

    // Both outputs are there but grd hasn't told us yet which one is primary when the greeter
    // gets its initial configure (targeting the active output, the first one).
    let laptop = f.niri().add_virtual_output("Virtual-1");
    let monitor = f.niri().add_virtual_output("Virtual-2");
    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    f.niri()
        .apply_virtual_output_tag(&laptop, client_tag(2560, 0, false, 1.));
    f.niri()
        .apply_virtual_output_tag(&monitor, client_tag(0, 0, true, 1.));

    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    let (mon, _) = f.niri().layout.windows().next().unwrap();
    assert_eq!(mon.unwrap().output(), &monitor);
}
