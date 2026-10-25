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
    assert_eq!((virt.make.as_str(), virt.model.as_str()), ("niri", "Virtual"));
    assert_eq!(virt.current_mode, Some(0));
    assert_eq!((virt.modes[0].width, virt.modes[0].height), (1920, 1080));
    let logical = virt.logical.unwrap();
    assert_eq!((logical.width, logical.height), (1920, 1080));

    f.niri().resize_virtual_output(&output, Size::from((1280, 720)));
    let outputs = ipc_outputs(&mut f);
    let virt = &outputs[0];
    assert_eq!((virt.modes[0].width, virt.modes[0].height), (1280, 720));
    let logical = virt.logical.unwrap();
    assert_eq!((logical.width, logical.height), (1280, 720));

    f.niri().remove_virtual_output(&output);
    assert_eq!(names(&ipc_outputs(&mut f)), ["headless-1"]);
}
