//! Headless backend.
//!
//! Two modes:
//!
//! - Tests (`Headless::new()`): no GPU, outputs and an optional surfaceless renderer are added by
//!   the test fixture, nothing is actually rendered.
//! - Real sessions (`Headless::with_gpu()`, `niri --headless`): no display and no seat, but a GPU
//!   renderer on a render node, with dmabuf support and screencasting. There are no outputs of our
//!   own: remote desktop clients create virtual outputs (`RecordVirtual`) as they need them, and
//!   those are only screencast, never rendered (see `crate::offscreen`). Input comes in through
//!   remote desktop (EIS).

use std::mem;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context as _};
use niri_config::{Config, OutputName};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::gbm::GbmDevice;
use smithay::backend::drm::DrmNode;
use smithay::backend::egl::native::EGLSurfacelessDisplay;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::element::RenderElementStates;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::ImportDma;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::utils::{DeviceFd, Size};
use smithay::wayland::dmabuf::{DmabufFeedbackBuilder, DmabufGlobal};
use smithay::wayland::presentation::Refresh;

use super::{IpcOutputMap, OutputId, RenderResult};
use crate::niri::{Niri, RedrawState, State};
use crate::render_helpers::{resources, shaders};
use crate::utils::{get_monotonic_time, logical_output};

pub struct Headless {
    renderer: Option<GlesRenderer>,
    ipc_outputs: Arc<Mutex<IpcOutputMap>>,
    /// Set in real headless sessions, `None` in tests.
    gpu: Option<Gpu>,
}

struct Gpu {
    render_node: Option<DrmNode>,
    gbm: Option<GbmDevice<DeviceFd>>,
    dmabuf_global: Option<DmabufGlobal>,
}

impl Headless {
    /// Headless backend for tests.
    pub fn new() -> Self {
        Self {
            renderer: None,
            ipc_outputs: Default::default(),
            gpu: None,
        }
    }

    /// Headless backend for real sessions, rendering on a GPU render node.
    pub fn with_gpu() -> Self {
        Self {
            renderer: None,
            ipc_outputs: Default::default(),
            gpu: Some(Gpu {
                render_node: None,
                gbm: None,
                dmabuf_global: None,
            }),
        }
    }

    pub fn init(&mut self, niri: &mut Niri) {
        if self.gpu.is_none() {
            return;
        }

        if let Err(err) = self.init_gpu(niri) {
            // Without a renderer niri can still run, clients just can't use dmabufs and nothing
            // can be screencast. Not useful for a remote session, but better than crashing.
            warn!("error initializing the GPU for the headless backend: {err:?}");
        }

        // No outputs: remote desktop clients create virtual outputs as they need them.
    }

    fn init_gpu(&mut self, niri: &mut Niri) -> anyhow::Result<()> {
        let gpu = self.gpu.as_mut().unwrap();

        let path = render_node_path(&niri.config.borrow())?;
        debug!("headless: using render node {path:?}");

        let file = std::fs::File::options()
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("error opening {path:?}"))?;
        let gbm = GbmDevice::new(DeviceFd::from(std::os::fd::OwnedFd::from(file)))
            .context("error creating GBM device")?;
        gpu.render_node = DrmNode::from_path(&path).ok();

        let mut renderer = unsafe {
            let display = EGLDisplay::new(gbm.clone()).context("error creating EGL display")?;
            let context = EGLContext::new(&display).context("error creating EGL context")?;
            GlesRenderer::new(context).context("error creating renderer")?
        };

        resources::init(&mut renderer);
        shaders::init(&mut renderer);

        {
            let config = niri.config.borrow();
            let animations = &config.animations;
            if let Some(src) = animations.window_resize.custom_shader.as_deref() {
                shaders::set_custom_resize_program(&mut renderer, Some(src));
            }
            if let Some(src) = animations.window_close.custom_shader.as_deref() {
                shaders::set_custom_close_program(&mut renderer, Some(src));
            }
            if let Some(src) = animations.window_open.custom_shader.as_deref() {
                shaders::set_custom_open_program(&mut renderer, Some(src));
            }
        }

        // Same as the winit backend: default feedback when possible, v3 otherwise.
        let formats = renderer.dmabuf_formats();
        let feedback = gpu.render_node.as_ref().map(|node| {
            DmabufFeedbackBuilder::new(node.dev_id(), formats.clone())
                .build()
                .context("error building dmabuf feedback")
        });
        let dmabuf_global = match feedback {
            Some(Ok(feedback)) => niri
                .dmabuf_state
                .create_global_with_default_feedback::<State>(&niri.display_handle, &feedback),
            _ => niri
                .dmabuf_state
                .create_global::<State>(&niri.display_handle, formats),
        };
        gpu.dmabuf_global = Some(dmabuf_global);
        gpu.gbm = Some(gbm);

        self.renderer = Some(renderer);
        niri.update_shaders();

        Ok(())
    }

    pub fn add_renderer(&mut self) -> anyhow::Result<()> {
        if self.renderer.is_some() {
            error!("add_renderer: renderer must not already exist");
            return Ok(());
        }

        let mut renderer = unsafe {
            let display =
                EGLDisplay::new(EGLSurfacelessDisplay).context("error creating EGL display")?;
            let context = EGLContext::new(&display).context("error creating EGL context")?;
            GlesRenderer::new(context).context("error creating renderer")?
        };

        resources::init(&mut renderer);
        shaders::init(&mut renderer);

        self.renderer = Some(renderer);
        Ok(())
    }

    pub fn add_output(&mut self, niri: &mut Niri, n: u8, size: (u16, u16)) {
        let connector = format!("headless-{n}");
        let make = "niri".to_string();
        let model = "headless".to_string();
        let serial = n.to_string();

        let output = Output::new(
            connector.clone(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: make.clone(),
                model: model.clone(),
                serial_number: serial.clone(),
            },
        );

        let mode = Mode {
            size: Size::from((i32::from(size.0), i32::from(size.1))),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, None);
        output.set_preferred(mode);

        output.user_data().insert_if_missing(|| OutputName {
            connector,
            make: Some(make),
            model: Some(model),
            serial: Some(serial),
        });

        let physical_properties = output.physical_properties();
        self.ipc_outputs.lock().unwrap().insert(
            OutputId::next(),
            niri_ipc::Output {
                name: output.name(),
                make: physical_properties.make,
                model: physical_properties.model,
                serial: None,
                physical_size: None,
                modes: vec![niri_ipc::Mode {
                    width: size.0,
                    height: size.1,
                    refresh_rate: 60_000,
                    is_preferred: true,
                }],
                current_mode: Some(0),
                is_custom_mode: true,
                vrr_supported: false,
                vrr_enabled: false,
                logical: Some(logical_output(&output)),
                max_bpc: None,
            },
        );

        // A real headless output has a refresh rate, so that frame callbacks get paced.
        let refresh_interval = self
            .gpu
            .is_some()
            .then(|| Duration::from_micros(1_000_000_000 / u64::from(mode.refresh as u32)));
        niri.add_output(output, refresh_interval, false);
    }

    pub fn seat_name(&self) -> String {
        "headless".to_owned()
    }

    pub fn with_primary_renderer<T>(
        &mut self,
        f: impl FnOnce(&mut GlesRenderer) -> T,
    ) -> Option<T> {
        self.renderer.as_mut().map(f)
    }

    pub fn primary_render_node(&mut self) -> Option<DrmNode> {
        self.gpu.as_ref().and_then(|gpu| gpu.render_node)
    }

    pub fn gbm_device(&self) -> Option<GbmDevice<DeviceFd>> {
        self.gpu.as_ref().and_then(|gpu| gpu.gbm.clone())
    }

    pub fn render(
        &mut self,
        niri: &mut Niri,
        output: &Output,
        target_presentation_time: Duration,
    ) -> RenderResult {
        if self.gpu.is_some() {
            if let Some(renderer) = &mut self.renderer {
                // Nobody looks at the output, only screencasts of it are rendered.
                return niri.render_offscreen(renderer, output, target_presentation_time);
            }
        }

        let states = RenderElementStates::default();
        let mut presentation_feedbacks = niri.take_presentation_feedbacks(output, &states);
        presentation_feedbacks.presented::<_, smithay::utils::Monotonic>(
            get_monotonic_time(),
            Refresh::Unknown,
            0,
            wp_presentation_feedback::Kind::empty(),
        );

        let output_state = niri.output_state.get_mut(output).unwrap();
        match mem::replace(&mut output_state.redraw_state, RedrawState::Idle) {
            RedrawState::Idle => unreachable!(),
            RedrawState::Queued => (),
            RedrawState::WaitingForVBlank { .. } => unreachable!(),
            RedrawState::WaitingForEstimatedVBlank(_) => unreachable!(),
            RedrawState::WaitingForEstimatedVBlankAndQueued(_) => unreachable!(),
        }

        output_state.frame_callback_sequence = output_state.frame_callback_sequence.wrapping_add(1);

        // FIXME: request redraw on unfinished animations remain

        RenderResult::Submitted
    }

    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        let Some(renderer) = self.renderer.as_mut().filter(|_| self.gpu.is_some()) else {
            unimplemented!()
        };

        match renderer.import_dmabuf(dmabuf, None) {
            Ok(_texture) => true,
            Err(err) => {
                debug!("error importing dmabuf: {err:?}");
                false
            }
        }
    }

    pub fn ipc_outputs(&self) -> Arc<Mutex<IpcOutputMap>> {
        self.ipc_outputs.clone()
    }
}

impl Default for Headless {
    fn default() -> Self {
        Self::new()
    }
}

/// The configured render node (`debug { render-drm-device }`), or the first one.
fn render_node_path(config: &Config) -> anyhow::Result<PathBuf> {
    if let Some(path) = &config.debug.render_drm_device {
        return Ok(path.clone());
    }

    let mut nodes: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .context("error reading /dev/dri")?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("renderD"))
        })
        .collect();
    nodes.sort();

    match nodes.into_iter().next() {
        Some(path) => Ok(path),
        None => bail!("no render node found in /dev/dri"),
    }
}
