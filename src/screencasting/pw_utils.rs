use std::cell::RefCell;
use std::cmp::min;
use std::collections::HashMap;
use std::io::Cursor;
use std::iter::zip;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::Duration;
use std::{mem, slice};

use anyhow::{bail, ensure, Context as _};
use calloop::timer::{TimeoutAction, Timer};
use calloop::RegistrationToken;
use pipewire::context::ContextRc;
use pipewire::core::{CoreRc, PW_ID_CORE};
use pipewire::loop_::Timeout;
use pipewire::main_loop::MainLoopRc;
use pipewire::properties::PropertiesBox;
use pipewire::spa::buffer::DataType;
use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pipewire::spa::param::format_utils::parse_format;
use pipewire::spa::param::video::{VideoFormat, VideoInfoRaw};
use pipewire::spa::param::ParamType;
use pipewire::spa::pod::deserialize::PodDeserializer;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{self, ChoiceValue, Pod, PodPropFlags, Property, PropertyFlags};
use pipewire::spa::sys::*;
use pipewire::spa::utils::{
    Choice, ChoiceEnum, ChoiceFlags, Direction, Fraction, Rectangle, SpaTypes,
};
use pipewire::spa::{self};
use pipewire::stream::{Stream, StreamFlags, StreamListener, StreamRc, StreamState};
use pipewire::sys::{pw_buffer, pw_check_library_version, pw_stream_queue_buffer};
use smithay::backend::allocator::dmabuf::{AsDmabuf, Dmabuf};
use smithay::backend::allocator::format::FormatSet;
use smithay::backend::allocator::gbm::{GbmBuffer, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::utils::{Relocate, RelocateRenderElement};
use smithay::backend::renderer::element::{Element, RenderElement, RenderElementStates};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::ExportMem;
use smithay::output::{Output, OutputModeSource};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};
use smithay::reexports::gbm::Modifier;
use smithay::reexports::rustix::fd::OwnedFd;
use smithay::reexports::rustix::fs::{
    fcntl_add_seals, ftruncate, memfd_create, MemfdFlags, SealFlags,
};
use smithay::utils::{DeviceFd, Logical, Physical, Point, Scale, Size, Transform};
use zbus::object_server::SignalEmitter;

use crate::dbus::mutter_screen_cast::{self, CursorMode};
use crate::niri::{CastTarget, State};
use crate::render_helpers::{
    clear_dmabuf, encompassing_geo, render_and_download, render_and_download_with_damage,
    render_to_dmabuf,
};
use crate::screencasting::CastRenderElement;
use crate::utils::{get_monotonic_time, CastSessionId, CastStreamId};
use crate::virtual_output::ConsumerTag;

mod explicit_sync;
mod shm_mapping;
use explicit_sync::{BufferTimeline, SyncDevice, SYNC_BLOCKS};
use shm_mapping::ShmMapping;

// Give a 0.1 ms allowance for presentation time errors.
const CAST_DELAY_ALLOWANCE: Duration = Duration::from_micros(100);
const SHM_BLOCKS: usize = 1;
const SHM_BYTES_PER_PIXEL: usize = 4;

// RGBA like mutter: gnome-remote-desktop only accepts RGBA cursor bitmaps and treats anything
// else as a hidden cursor. Must match CURSOR_FOURCC (DRM ABGR8888 is R, G, B, A in memory).
const CURSOR_FORMAT: spa_video_format = SPA_VIDEO_FORMAT_RGBA;
const CURSOR_FOURCC: Fourcc = Fourcc::Abgr8888;
const CURSOR_BPP: u32 = 4;
const CURSOR_WIDTH: u32 = 384;
const CURSOR_HEIGHT: u32 = 384;
const CURSOR_BITMAP_SIZE: usize = (CURSOR_WIDTH * CURSOR_HEIGHT * CURSOR_BPP) as usize;
const CURSOR_META_SIZE: usize =
    mem::size_of::<spa_meta_cursor>() + mem::size_of::<spa_meta_bitmap>() + CURSOR_BITMAP_SIZE;
const BITMAP_META_OFFSET: usize = mem::size_of::<spa_meta_cursor>();
const BITMAP_DATA_OFFSET: usize = mem::size_of::<spa_meta_bitmap>();

pub struct PipeWire {
    _context: ContextRc,
    pub core: CoreRc,
    pub token: RegistrationToken,
    event_loop: LoopHandle<'static, State>,
    to_niri: calloop::channel::Sender<PwToNiri>,
}

pub enum PwToNiri {
    StopCast {
        session_id: CastSessionId,
    },
    Redraw {
        stream_id: CastStreamId,
    },
    /// The consumer picked a size for a flexible-size cast; resize the output to match.
    ResizeOutput {
        stream_id: CastStreamId,
        size: Size<u32, Physical>,
    },
    /// The consumer described the output it wants for a flexible-size cast.
    ConsumerTag {
        stream_id: CastStreamId,
        tag: ConsumerTag,
    },
    FatalError,
}

pub struct Cast {
    event_loop: LoopHandle<'static, State>,
    pub session_id: CastSessionId,
    pub stream_id: CastStreamId,
    // Listener is dropped before Stream to prevent a use-after-free.
    _listener: StreamListener<()>,
    pub stream: StreamRc,
    pub target: CastTarget,
    pub dynamic_target: bool,
    formats: FormatSet,
    offer_alpha: bool,
    /// The consumer picks the size, and the output (a virtual one) follows.
    pub flexible_size: bool,
    cursor_mode: CursorMode,
    last_frame_time: Duration,
    last_frame_interval: Duration,
    scheduled_redraw: Option<RegistrationToken>,
    // Incremented once per successful frame, stored in buffer meta.
    sequence_counter: u64,
    inner: Rc<RefCell<CastInner>>,
}

/// Mutable `Cast` state shared with PipeWire callbacks.
#[derive(Debug)]
struct CastInner {
    is_active: bool,
    node_id: Option<u32>,
    state: CastState,
    refresh: u32,
    min_time_between_frames: Duration,
    dmabufs: HashMap<i64, Dmabuf>,
    shmbufs: HashMap<i64, Shmbuf>,
    /// Buffers dequeued from PipeWire in process of rendering.
    ///
    /// This is an ordered list of buffers that we started rendering to and waiting for the
    /// rendering to complete. The completion can be checked from the `SyncPoint`s. The buffers are
    /// stored in order from oldest to newest, and the same ordering should be preserved when
    /// submitting completed buffers to PipeWire.
    rendering_buffers: Vec<(NonNull<pw_buffer>, SyncPoint)>,
    /// Set when the render node supports timeline syncobjs, so we can offer explicit sync.
    sync_device: Option<SyncDevice>,
    /// Timeline syncobjs of explicit-sync DMA-BUF buffers, keyed like `dmabufs`.
    timelines: HashMap<i64, BufferTimeline>,
    /// Dequeued explicit-sync buffers that the consumer hasn't released yet.
    ///
    /// We hold on to them and check again on the next dequeue, like mutter does.
    waiting_for_release: Vec<NonNull<pw_buffer>>,
}

#[derive(Debug, Clone, Copy)]
struct DmaNegotiation {
    modifier: Modifier,
    plane_count: i32,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum CastState {
    // dma_negotiation = Some(_) means DMA sharing
    // dma_negotiation = None    means SHM sharing
    ResizePending {
        pending_size: Size<u32, Physical>,
    },
    ConfirmationPending {
        size: Size<u32, Physical>,
        alpha: bool,
        dma_negotiation: Option<DmaNegotiation>,
    },
    Ready {
        size: Size<u32, Physical>,
        alpha: bool,
        dma_negotiation: Option<DmaNegotiation>,
        // Lazily-initialized to keep the initialization to a single place.
        damage_tracker: Option<OutputDamageTracker>,
        cursor_damage_tracker: Option<OutputDamageTracker>,
        /// `None` while the cursor isn't in this stream.
        last_cursor_location: Option<Point<i32, Physical>>,
    },
}

#[derive(PartialEq, Eq)]
pub enum CastSizeChange {
    Ready,
    Pending,
}

/// Data for drawing a cursor either as metadata or embedded.
///
/// The cursor elements are expected to be at the start of the main elements slice. `elem_count` is
/// the count of the pointer elements. This way, the full slice includes both main and cursor
/// elements for embedded mode, and `&elements[elem_count..]` gives just the main elements for
/// metadata mode.
///
/// We have weird borrowed references here in order to support both metadata and embedded cases.
/// The cursor damage tracker needs a slice of impl Element at (0, 0), so we pass it `relocated`
/// (luckily, &impl Element also impls Element). Then, if we need to embed the cursor, we use the
/// full elements slice which starts with non-relocated pointer elements (that we borrow from).
#[derive(Debug)]
pub struct CursorData<'a, E> {
    /// Count of the pointer elements in the slice (index of the first non-pointer element).
    elem_count: usize,
    /// Cursor elements relocated to (0, 0).
    relocated: Vec<RelocateRenderElement<&'a E>>,
    /// Location of the cursor's hotspot in the video buffer, or `None` if the cursor isn't in
    /// this stream at all (rather than in it but invisible).
    location: Option<Point<i32, Physical>>,
    /// Location of the cursor's hotspot on the cursor bitmap.
    hotspot: Point<i32, Physical>,
    /// Size of the elements' encompassing geo.
    size: Size<i32, Physical>,
    /// Scale the elements should be rendered at.
    scale: Scale<f64>,
}

impl<'a, E: Element> CursorData<'a, E> {
    pub fn compute(
        elements: &'a [E],
        elem_count: usize,
        location: Option<Point<f64, Logical>>,
        scale: Scale<f64>,
    ) -> Self {
        let pointer_elements = &elements[..elem_count];
        let location = location.map(|loc| loc.to_physical_precise_round(scale));

        let geo = encompassing_geo(scale, pointer_elements.iter());
        let relocated = Vec::from_iter(pointer_elements.iter().map(|elem| {
            RelocateRenderElement::from_element(elem, geo.loc.upscale(-1), Relocate::Relative)
        }));

        Self {
            elem_count,
            relocated,
            location,
            hotspot: location.unwrap_or_default() - geo.loc,
            size: geo.size,
            scale,
        }
    }
}

/// Largest size we accept for casts whose size the consumer picks.
const MAX_FLEXIBLE_SIZE: u32 = 8192;

/// With `flexible_size`, offer any size (defaulting to `size`) and let the consumer pick; used for
/// virtual outputs, which then get resized to match.
fn make_video_params(
    format: VideoFormat,
    modifiers: &[Modifier],
    size: Size<u32, Physical>,
    refresh: u32,
    flexible_size: bool,
) -> pod::Object {
    let size_property = if flexible_size {
        pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: size.w,
                height: size.h,
            },
            Rectangle {
                width: 1,
                height: 1,
            },
            Rectangle {
                width: MAX_FLEXIBLE_SIZE,
                height: MAX_FLEXIBLE_SIZE,
            }
        )
    } else {
        pod::property!(
            FormatProperties::VideoSize,
            Rectangle,
            Rectangle {
                width: size.w,
                height: size.h,
            }
        )
    };

    let mut properties = vec![
        pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        size_property,
        pod::property!(
            FormatProperties::VideoFramerate,
            Fraction,
            Fraction { num: 0, denom: 1 }
        ),
        pod::property!(
            FormatProperties::VideoMaxFramerate,
            Choice,
            Range,
            Fraction,
            Fraction {
                num: refresh,
                denom: 1000
            },
            Fraction { num: 1, denom: 1 },
            Fraction {
                num: refresh,
                denom: 1000
            }
        ),
        pod::property!(FormatProperties::VideoFormat, Id, format),
    ];

    if !modifiers.is_empty() {
        let dont_fixate = if modifiers.len() > 1 {
            PropertyFlags::DONT_FIXATE
        } else {
            PropertyFlags::empty()
        };
        let flags = PropertyFlags::MANDATORY | dont_fixate;
        let modifiers_i64 = modifiers
            .iter()
            .map(|m| u64::from(*m) as i64)
            .collect::<Vec<_>>();

        let prop = Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags,
            value: pod::Value::Choice(ChoiceValue::Long(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default: modifiers_i64[0],
                    alternatives: modifiers_i64,
                },
            ))),
        };

        properties.push(prop);
    }

    pod::Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties,
    }
}

fn make_initial_video_params(
    possible_modifiers: &FormatSet,
    size: Size<u32, Physical>,
    refresh: u32,
    alpha: bool,
    flexible_size: bool,
) -> Vec<pod::Object> {
    let mut rv = Vec::new();

    let mut push_alpha = |alpha| {
        let format = if alpha {
            VideoFormat::BGRA
        } else {
            VideoFormat::BGRx
        };

        let fourcc = if alpha {
            Fourcc::Argb8888
        } else {
            Fourcc::Xrgb8888
        };

        let modifiers: Vec<_> = possible_modifiers
            .iter()
            .filter_map(|f| (f.code == fourcc).then_some(f.modifier))
            .collect();

        trace!("offering: {modifiers:?}");

        if !modifiers.is_empty() {
            rv.push(make_video_params(
                format,
                &modifiers,
                size,
                refresh,
                flexible_size,
            ));
        }
        rv.push(make_video_params(format, &[], size, refresh, flexible_size));
    };

    if alpha {
        push_alpha(true);
    }
    push_alpha(false);

    rv
}

macro_rules! make_params {
    ($params:ident, $formats:expr, $size:expr, $refresh:expr, $alpha:expr, $flexible_size:expr) => {
        let $params = make_initial_video_params($formats, $size, $refresh, $alpha, $flexible_size);
        let mut bufs = [const { Vec::new() }; 4]; // Maximum possible params len.
        let mut $params: Vec<_> = $params
            .into_iter()
            .zip(&mut bufs)
            .map(|(obj, buf)| make_pod(buf, obj))
            .collect();
    };
}

impl PipeWire {
    pub fn new(
        event_loop: LoopHandle<'static, State>,
        to_niri: calloop::channel::Sender<PwToNiri>,
    ) -> anyhow::Result<Self> {
        let main_loop = MainLoopRc::new(None).context("error creating MainLoop")?;
        let context = ContextRc::new(&main_loop, None).context("error creating Context")?;
        let core = context.connect_rc(None).context("error creating Core")?;

        let to_niri_ = to_niri.clone();
        let listener = core
            .add_listener_local()
            .error(move |id, seq, res, message| {
                warn!(id, seq, res, message, "pw error");

                // Reset PipeWire on connection errors.
                if id == PW_ID_CORE && res == -32 {
                    if let Err(err) = to_niri_.send(PwToNiri::FatalError) {
                        warn!("error sending FatalError to niri: {err:?}");
                    }
                }
            })
            .register();
        mem::forget(listener);

        struct AsFdWrapper(MainLoopRc);
        impl AsFd for AsFdWrapper {
            fn as_fd(&self) -> BorrowedFd<'_> {
                self.0.loop_().fd()
            }
        }
        let generic = Generic::new(AsFdWrapper(main_loop), Interest::READ, Mode::Level);
        let token = event_loop
            .insert_source(generic, move |_, wrapper, _| {
                let _span = tracy_client::span!("pipewire iteration");
                wrapper.0.loop_().iterate(Timeout::None);
                Ok(PostAction::Continue)
            })
            .unwrap();

        Ok(Self {
            _context: context,
            core,
            token,
            event_loop,
            to_niri,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn start_cast(
        &self,
        gbm: Option<(GbmDevice<DeviceFd>, FormatSet)>,
        session_id: CastSessionId,
        stream_id: CastStreamId,
        target: CastTarget,
        size: Size<i32, Physical>,
        refresh: u32,
        alpha: bool,
        flexible_size: bool,
        mut cursor_mode: CursorMode,
        signal_ctx: SignalEmitter<'static>,
    ) -> anyhow::Result<Cast> {
        let _span = tracy_client::span!("PipeWire::start_cast");
        let _span = debug_span!("start_cast", %session_id).entered();

        let to_niri_ = self.to_niri.clone();
        let stop_cast = move || {
            if let Err(err) = to_niri_.send(PwToNiri::StopCast { session_id }) {
                warn!("error sending StopCast to niri: {err:?}");
            }
        };
        let to_niri_ = self.to_niri.clone();
        let redraw = move || {
            if let Err(err) = to_niri_.send(PwToNiri::Redraw { stream_id }) {
                warn!("error sending Redraw to niri: {err:?}");
            }
        };
        let redraw_ = redraw.clone();
        let to_niri_ = self.to_niri.clone();
        let resize_output = move |size| {
            if let Err(err) = to_niri_.send(PwToNiri::ResizeOutput { stream_id, size }) {
                warn!("error sending ResizeOutput to niri: {err:?}");
            }
        };
        let to_niri_ = self.to_niri.clone();
        let consumer_tag = move |tag| {
            if let Err(err) = to_niri_.send(PwToNiri::ConsumerTag { stream_id, tag }) {
                warn!("error sending ConsumerTag to niri: {err:?}");
            }
        };

        let stream = StreamRc::new(
            self.core.clone(),
            "niri-screen-cast-src",
            PropertiesBox::new(),
        )
        .context("error creating Stream")?;

        if cursor_mode == CursorMode::Metadata && !pw_version_supports_cursor_metadata() {
            debug!(
                "metadata cursor mode requested, but PipeWire is too old (need >= 1.4.8); \
                 switching to embedded cursor"
            );
            cursor_mode = CursorMode::Embedded;
        }

        let pending_size = Size::from((size.w as u32, size.h as u32));

        let (gbm, formats) = if let Some((gbm, formats)) = gbm {
            (Some(gbm), formats)
        } else {
            debug!("no gbm device; advertising only shm formats");
            (None, FormatSet::default())
        };

        let sync_device = gbm.as_ref().and_then(|gbm| SyncDevice::new(gbm.as_fd()));
        let explicit_sync = sync_device.is_some();

        // Like in good old wayland-rs times...
        let inner = Rc::new(RefCell::new(CastInner {
            is_active: false,
            node_id: None,
            state: CastState::ResizePending { pending_size },
            refresh,
            min_time_between_frames: Duration::ZERO,
            dmabufs: HashMap::new(),
            shmbufs: HashMap::new(),
            rendering_buffers: Vec::new(),
            sync_device,
            timelines: HashMap::new(),
            waiting_for_release: Vec::new(),
        }));

        let listener =
            stream
                .add_local_listener_with_user_data(())
                .state_changed({
                    let inner = inner.clone();
                    let stop_cast = stop_cast.clone();
                    move |stream, (), old, new| {
                        let _span = debug_span!("state_changed", %stream_id).entered();
                        debug!("{old:?} -> {new:?}");
                        let mut inner = inner.borrow_mut();

                        match new {
                            StreamState::Paused => {
                                if inner.node_id.is_none() {
                                    let id = stream.node_id();
                                    inner.node_id = Some(id);
                                    debug!("sending signal with {id}");

                                    let _span = tracy_client::span!("sending PipeWireStreamAdded");
                                    async_io::block_on(async {
                                        let res =
                                            mutter_screen_cast::Stream::pipe_wire_stream_added(
                                                &signal_ctx,
                                                id,
                                            )
                                            .await;

                                        if let Err(err) = res {
                                            warn!("error sending PipeWireStreamAdded: {err:?}");
                                            stop_cast();
                                        }
                                    });
                                }

                                inner.is_active = false;
                            }
                            StreamState::Error(_) => {
                                if inner.is_active {
                                    inner.is_active = false;
                                    stop_cast();
                                }
                            }
                            StreamState::Unconnected => (),
                            StreamState::Connecting => (),
                            StreamState::Streaming => {
                                inner.is_active = true;
                                redraw();
                            }
                        }
                    }
                })
                .param_changed({
                    let inner = inner.clone();
                    let stop_cast = stop_cast.clone();
                    let gbm = gbm.clone();
                    let formats = formats.clone();
                    move |stream, (), id, pod| {
                        // Not in libspa's ParamType.
                        if id == SPA_PARAM_Tag {
                            if flexible_size {
                                if let Some(tag) = pod.and_then(parse_consumer_tag) {
                                    debug!(%stream_id, ?tag, "consumer tag");
                                    consumer_tag(tag);
                                }
                            }
                            return;
                        }

                        let id = ParamType::from_raw(id);
                        trace!(%stream_id, ?id, "param_changed");
                        let mut inner = inner.borrow_mut();
                        let inner = &mut *inner;

                        if id != ParamType::Format {
                            return;
                        }

                        let _span = debug_span!("param_changed", %stream_id).entered();

                        let Some(pod) = pod else { return };

                        let (m_type, m_subtype) = match parse_format(pod) {
                            Ok(x) => x,
                            Err(err) => {
                                warn!("error parsing format: {err:?}");
                                return;
                            }
                        };

                        if m_type != MediaType::Video || m_subtype != MediaSubtype::Raw {
                            return;
                        }

                        let mut format = VideoInfoRaw::new();
                        format.parse(pod).unwrap();
                        debug!("got format = {format:?}");

                        let format_size = Size::from((format.size().width, format.size().height));

                        let state = &mut inner.state;
                        if flexible_size && format_size != state.expected_format_size() {
                            // The consumer picks the size; the output will follow.
                            debug!("consumer picked size {format_size:?}, resizing the output");
                            *state = CastState::ResizePending {
                                pending_size: format_size,
                            };
                            resize_output(format_size);
                        } else if format_size != state.expected_format_size() {
                            if !matches!(&*state, CastState::ResizePending { .. }) {
                                warn!("wrong size, but we're not resizing");
                                stop_cast();
                                return;
                            }

                            debug!("wrong size, waiting");
                            return;
                        }

                        let format_has_alpha = format.format() == VideoFormat::BGRA;
                        let fourcc = if format_has_alpha {
                            Fourcc::Argb8888
                        } else {
                            Fourcc::Xrgb8888
                        };

                        let max_frame_rate = format.max_framerate();
                        let min_frame_time = Duration::from_micros(
                            1_000_000 * u64::from(max_frame_rate.denom)
                                / u64::from(max_frame_rate.num),
                        );
                        inner.min_time_between_frames = min_frame_time;

                        // We have following cases when param_changed:
                        //
                        // 1. Modifier exists and its flags contain DONT_FIXATE
                        //
                        //    Do test allocation, set CastState to ConfirmationPending and send
                        //    param again.
                        //
                        // 2. Modifier exists and it doesn't need fixation
                        //
                        //    Do test allocation to ensure the modifier work, then set CastState to
                        //    Ready. Then set buffer to DMA.
                        //
                        // 3. Modifier doesn't exist
                        //
                        //    Set CastState to Ready and set buffer to SHM.

                        let object = pod.as_object().unwrap();
                        let prop_modifier =
                            object.find_prop(spa::utils::Id(FormatProperties::VideoModifier.0));

                        match prop_modifier {
                            Some(prop_modifier)
                                if prop_modifier.flags().contains(PodPropFlags::DONT_FIXATE) =>
                            {
                                debug!("fixating the modifier");

                                let Some(gbm) = &gbm else {
                                    error!("negotiated dmabuf without gbm");
                                    stop_cast();
                                    return;
                                };

                                let pod_modifier = prop_modifier.value();
                                let Ok((_, modifiers)) =
                                    PodDeserializer::deserialize_from::<Choice<i64>>(
                                        pod_modifier.as_bytes(),
                                    )
                                else {
                                    warn!("wrong modifier property type");
                                    stop_cast();
                                    return;
                                };

                                let ChoiceEnum::Enum { alternatives, .. } = modifiers.1 else {
                                    warn!("wrong modifier choice type");
                                    stop_cast();
                                    return;
                                };

                                let (modifier, plane_count) = match find_preferred_modifier(
                                    gbm,
                                    format_size,
                                    fourcc,
                                    alternatives,
                                ) {
                                    Ok(x) => x,
                                    Err(err) => {
                                        warn!("couldn't find preferred modifier: {err:?}");
                                        stop_cast();
                                        return;
                                    }
                                };

                                debug!(
                                    "allocation successful \
                                     (modifier={modifier:?}, plane_count={plane_count}), \
                                     moving to confirmation pending"
                                );

                                *state = CastState::ConfirmationPending {
                                    size: format_size,
                                    alpha: format_has_alpha,
                                    dma_negotiation: Some(DmaNegotiation {
                                        modifier,
                                        plane_count: plane_count as i32,
                                    }),
                                };

                                let o = make_video_params(
                                    format.format(),
                                    &[modifier],
                                    format_size,
                                    inner.refresh,
                                    false,
                                );
                                let mut b = Vec::new();
                                let pod = make_pod(&mut b, o);

                                make_params!(
                                    params,
                                    &formats,
                                    format_size,
                                    inner.refresh,
                                    format_has_alpha,
                                    flexible_size
                                );
                                params.insert(0, pod);

                                if let Err(err) = stream.update_params(&mut params) {
                                    warn!("error updating stream params: {err:?}");
                                    stop_cast();
                                }

                                return;
                            }
                            _ => (),
                        }

                        let (o1, o1_sync) = if prop_modifier.is_some() {
                            // Verify that alpha and modifier didn't change.
                            let plane_count = match &*state {
                                CastState::ConfirmationPending {
                                    size,
                                    alpha,
                                    dma_negotiation: Some(dma_negotiation),
                                }
                                | CastState::Ready {
                                    size,
                                    alpha,
                                    dma_negotiation: Some(dma_negotiation),
                                    ..
                                } if *alpha == format_has_alpha
                                    && dma_negotiation.modifier
                                        == Modifier::from(format.modifier()) =>
                                {
                                    let size = *size;
                                    let alpha = *alpha;
                                    let dma_negotiation = *dma_negotiation;

                                    let (damage_tracker, cursor_damage_tracker) =
                                        if let CastState::Ready {
                                            damage_tracker,
                                            cursor_damage_tracker,
                                            ..
                                        } = &mut *state
                                        {
                                            (damage_tracker.take(), cursor_damage_tracker.take())
                                        } else {
                                            (None, None)
                                        };

                                    debug!("moving to ready state");

                                    *state = CastState::Ready {
                                        size,
                                        alpha,
                                        dma_negotiation: Some(dma_negotiation),
                                        damage_tracker,
                                        cursor_damage_tracker,
                                        last_cursor_location: None,
                                    };

                                    dma_negotiation.plane_count
                                }
                                _ => {
                                    let Some(gbm) = &gbm else {
                                        error!("negotiated dmabuf without gbm");
                                        stop_cast();
                                        return;
                                    };

                                    // We're negotiating a single modifier, or alpha or modifier
                                    // changed, so we need to do a test allocation.
                                    let (modifier, plane_count) = match find_preferred_modifier(
                                        gbm,
                                        format_size,
                                        fourcc,
                                        vec![format.modifier() as i64],
                                    ) {
                                        Ok(x) => x,
                                        Err(err) => {
                                            warn!("test allocation failed: {err:?}");
                                            stop_cast();
                                            return;
                                        }
                                    };

                                    debug!(
                                        "allocation successful \
                                         (modifier={modifier:?}, plane_count={plane_count}), \
                                         moving to ready"
                                    );

                                    *state = CastState::Ready {
                                        size: format_size,
                                        alpha: format_has_alpha,
                                        dma_negotiation: Some(DmaNegotiation {
                                            modifier,
                                            plane_count: plane_count as i32,
                                        }),
                                        damage_tracker: None,
                                        cursor_damage_tracker: None,
                                        last_cursor_location: None,
                                    };

                                    plane_count as i32
                                }
                            };

                            (
                                buffers_param(plane_count, DataType::DmaBuf, false),
                                explicit_sync
                                    .then(|| buffers_param(plane_count, DataType::DmaBuf, true)),
                            )
                        } else {
                            debug!("negotiated inefficient shm stream, moving to ready state");

                            *state = CastState::Ready {
                                size: format_size,
                                alpha: format_has_alpha,
                                dma_negotiation: None,
                                damage_tracker: None,
                                cursor_damage_tracker: None,
                                last_cursor_location: None,
                            };
                            (
                                buffers_param(SHM_BLOCKS as i32, DataType::MemFd, false),
                                None,
                            )
                        };

                        let o2 = pod::object!(
                            SpaTypes::ObjectParamMeta,
                            ParamType::Meta,
                            Property::new(
                                SPA_PARAM_META_type,
                                pod::Value::Id(spa::utils::Id(SPA_META_Header))
                            ),
                            Property::new(
                                SPA_PARAM_META_size,
                                pod::Value::Int(size_of::<spa_meta_header>() as i32)
                            ),
                        );

                        let mut b1 = vec![];
                        let mut b2 = vec![];
                        let mut b_sync = vec![];
                        let mut b_sync_meta = vec![];

                        // Buffers params go in order of preference: the explicit sync one is
                        // only picked when the consumer also announces SyncTimeline support.
                        let mut params = vec![];
                        if let Some(o1_sync) = o1_sync {
                            params.push(make_pod(&mut b_sync, o1_sync));
                        }
                        let has_sync = !params.is_empty();
                        params.push(make_pod(&mut b1, o1));
                        params.push(make_pod(&mut b2, o2));
                        if has_sync {
                            let o_sync_meta = pod::object!(
                                SpaTypes::ObjectParamMeta,
                                ParamType::Meta,
                                Property::new(
                                    SPA_PARAM_META_type,
                                    pod::Value::Id(spa::utils::Id(SPA_META_SyncTimeline))
                                ),
                                Property::new(
                                    SPA_PARAM_META_size,
                                    pod::Value::Int(size_of::<spa_meta_sync_timeline>() as i32)
                                ),
                            );
                            params.push(make_pod(&mut b_sync_meta, o_sync_meta));
                        }

                        let mut b_cursor = vec![];
                        if cursor_mode == CursorMode::Metadata {
                            let o_cursor = pod::object!(
                                SpaTypes::ObjectParamMeta,
                                ParamType::Meta,
                                Property::new(
                                    SPA_PARAM_META_type,
                                    pod::Value::Id(spa::utils::Id(SPA_META_Cursor))
                                ),
                                Property::new(
                                    SPA_PARAM_META_size,
                                    pod::Value::Int(CURSOR_META_SIZE as i32)
                                ),
                            );
                            params.push(make_pod(&mut b_cursor, o_cursor));
                        }

                        if let Err(err) = stream.update_params(&mut params) {
                            warn!("error updating stream params: {err:?}");
                            stop_cast();
                        }
                    }
                })
                .add_buffer({
                    let inner = inner.clone();
                    let stop_cast = stop_cast.clone();
                    move |stream, (), buffer| {
                        let _span = debug_span!("add_buffer", %stream_id).entered();

                        match unsafe { inner.borrow_mut().on_add_buffer(gbm.as_ref(), buffer) } {
                            Ok(redraw) => {
                                // During size re-negotiation, the stream sometimes just keeps
                                // running, in which case we may need to force a redraw once we got
                                // a newly sized buffer.
                                if redraw && stream.state() == StreamState::Streaming {
                                    redraw_();
                                }
                            }
                            Err(err) => {
                                warn!("error adding pw buffer: {err:?}");
                                stop_cast();
                            }
                        };
                    }
                })
                .remove_buffer({
                    let inner = inner.clone();
                    move |_stream, (), buffer| {
                        let _span = debug_span!("remove_buffer", %stream_id).entered();

                        unsafe {
                            inner.borrow_mut().on_remove_buffer(buffer);
                        }
                    }
                })
                .register()
                .unwrap();

        trace!("starting pw stream with size={pending_size:?}, refresh={refresh:?}");

        make_params!(params, &formats, pending_size, refresh, alpha, flexible_size);
        stream
            .connect(
                Direction::Output,
                None,
                StreamFlags::DRIVER | StreamFlags::ALLOC_BUFFERS,
                &mut params,
            )
            .context("error connecting stream")?;

        let cast = Cast {
            event_loop: self.event_loop.clone(),
            session_id,
            stream_id,
            stream,
            _listener: listener,
            target,
            dynamic_target: false,
            formats,
            offer_alpha: alpha,
            flexible_size,
            cursor_mode,
            last_frame_time: Duration::ZERO,
            last_frame_interval: Duration::ZERO,
            scheduled_redraw: None,
            sequence_counter: 0,
            inner,
        };
        Ok(cast)
    }
}

impl Cast {
    pub fn is_active(&self) -> bool {
        self.inner.borrow().is_active
    }

    pub fn node_id(&self) -> Option<u32> {
        self.inner.borrow().node_id
    }

    pub fn ensure_size(&self, size: Size<i32, Physical>) -> anyhow::Result<CastSizeChange> {
        let mut inner = self.inner.borrow_mut();

        let new_size = Size::from((size.w as u32, size.h as u32));

        let state = &mut inner.state;
        if matches!(state, CastState::Ready { size, .. } if *size == new_size) {
            return Ok(CastSizeChange::Ready);
        }

        if self.flexible_size {
            // The output follows the stream, not the other way around: wait for it to get
            // resized instead of renegotiating.
            return Ok(CastSizeChange::Pending);
        }

        if state.pending_size() == Some(new_size) {
            debug!("stream size still hasn't changed, skipping frame");
            return Ok(CastSizeChange::Pending);
        }

        let _span = tracy_client::span!("Cast::ensure_size");
        debug!("cast size changed, updating stream size");

        *state = CastState::ResizePending {
            pending_size: new_size,
        };

        make_params!(
            params,
            &self.formats,
            new_size,
            inner.refresh,
            self.offer_alpha,
            self.flexible_size
        );
        self.stream
            .update_params(&mut params)
            .context("error updating stream params")?;

        Ok(CastSizeChange::Pending)
    }

    pub fn set_refresh(&mut self, refresh: u32) -> anyhow::Result<()> {
        let mut inner = self.inner.borrow_mut();

        if inner.refresh == refresh {
            return Ok(());
        }

        let _span = tracy_client::span!("Cast::set_refresh");
        debug!("cast FPS changed, updating stream FPS");
        inner.refresh = refresh;

        let size = inner.state.expected_format_size();
        make_params!(
            params,
            &self.formats,
            size,
            refresh,
            self.offer_alpha,
            self.flexible_size
        );
        self.stream
            .update_params(&mut params)
            .context("error updating stream params")?;

        Ok(())
    }

    pub fn record_frame_time(&mut self, recorded: Duration) {
        let interval = self.inner.borrow().min_time_between_frames;
        let ideal = self.last_frame_time + interval;

        // Absorb small (< 1 frame) differences in output refresh interval vs. screencast framerate
        // to keep the screencast time base consistent instead of shifting forward every frame.
        //
        // After a missed interval or a rate change, restart instead of catching up in a burst.
        self.last_frame_time = if self.last_frame_interval == interval
            && recorded >= self.last_frame_time
            && recorded < ideal + interval
        {
            ideal
        } else {
            recorded
        };
        self.last_frame_interval = interval;
    }

    fn compute_extra_delay(&self, target_frame_time: Duration) -> Duration {
        let inner = self.inner.borrow();

        let last = self.last_frame_time;
        let min = inner.min_time_between_frames;

        if last.is_zero() {
            trace!(?target_frame_time, ?last, "last is zero, recording");
            return Duration::ZERO;
        }

        if target_frame_time < last {
            // Record frame with a warning; in case it was an overflow this will fix it.
            warn!(
                ?target_frame_time,
                ?last,
                "target frame time is below last, did it overflow or did we mispredict?"
            );
            return Duration::ZERO;
        }

        let diff = target_frame_time - last;
        if diff < min {
            let delay = min - diff;
            trace!(
                ?target_frame_time,
                ?last,
                "frame is too soon: min={min:?}, delay={:?}",
                delay
            );
            return delay;
        } else {
            trace!("overshoot={:?}", diff - min);
        }

        Duration::ZERO
    }

    fn schedule_redraw(&mut self, output: Output, target_time: Duration) {
        if self.scheduled_redraw.is_some() {
            return;
        }

        let now = get_monotonic_time();
        let duration = target_time.saturating_sub(now);
        let timer = Timer::from_duration(duration);
        let token = self
            .event_loop
            .insert_source(timer, move |_, _, state| {
                // Guard against output disconnecting before the timer has a chance to run.
                if state.niri.output_state.contains_key(&output) {
                    state.niri.queue_redraw(&output);
                }

                TimeoutAction::Drop
            })
            .unwrap();
        self.scheduled_redraw = Some(token);
    }

    fn remove_scheduled_redraw(&mut self) {
        if let Some(token) = self.scheduled_redraw.take() {
            self.event_loop.remove(token);
        }
    }

    /// Checks whether this frame should be skipped because it's too soon.
    ///
    /// If the frame should be skipped, schedules a redraw and returns `true`. Otherwise, removes a
    /// scheduled redraw, if any, and returns `false`.
    ///
    /// When this method returns `false`, the calling code is assumed to follow up with
    /// [`Cast::dequeue_buffer_and_render()`].
    pub fn check_time_and_schedule(
        &mut self,
        output: &Output,
        target_frame_time: Duration,
    ) -> bool {
        let delay = self.compute_extra_delay(target_frame_time);
        if delay >= CAST_DELAY_ALLOWANCE {
            trace!("delay >= allowance, scheduling redraw");
            self.schedule_redraw(output.clone(), target_frame_time + delay);
            true
        } else {
            self.remove_scheduled_redraw();
            false
        }
    }

    fn dequeue_available_buffer(&mut self) -> Option<NonNull<pw_buffer>> {
        let mut inner = self.inner.borrow_mut();
        if inner.sync_device.is_none() {
            return unsafe { NonNull::new(self.stream.dequeue_raw_buffer()) };
        }

        unsafe {
            // First, buffers that we set aside earlier because they weren't released yet.
            let released = inner
                .waiting_for_release
                .iter()
                .position(|buffer| inner.is_released(*buffer));
            if let Some(idx) = released {
                return Some(inner.waiting_for_release.remove(idx));
            }

            loop {
                let buffer = NonNull::new(self.stream.dequeue_raw_buffer())?;
                if inner.is_released(buffer) {
                    return Some(buffer);
                }
                trace!("buffer not released by the consumer yet, setting aside");
                inner.waiting_for_release.push(buffer);
            }
        }
    }

    fn queue_completed_buffers(&mut self) {
        let mut inner = self.inner.borrow_mut();

        // We want to queue buffers in order, so find the first still-rendering buffer, and queue
        // everything up to that. Even if there are completed buffers past the first
        // still-rendering buffer, we do not want to queue them, since that would send frames out
        // of order.
        let first_in_progress_idx = inner
            .rendering_buffers
            .iter()
            .position(|(_, sync)| !sync.is_reached())
            .unwrap_or(inner.rendering_buffers.len());

        let completed: Vec<_> = inner
            .rendering_buffers
            .drain(..first_in_progress_idx)
            .map(|(buffer, _)| buffer)
            .collect();
        for buffer in completed {
            trace!("queueing completed buffer");
            unsafe {
                inner.advance_sync_points(buffer);
                pw_stream_queue_buffer(self.stream.as_raw_ptr(), buffer.as_ptr());
            }
        }
    }

    unsafe fn queue_after_sync(&mut self, pw_buffer: NonNull<pw_buffer>, sync_point: SyncPoint) {
        let _span = tracy_client::span!("Cast::queue_after_sync");

        let mut inner = self.inner.borrow_mut();

        let mut sync_point = sync_point;
        let sync_fd = match sync_point.export() {
            Some(sync_fd) => Some(sync_fd),
            None => {
                // There are two main ways this can happen. First is that the SyncPoint is
                // pre-signalled, then the buffer is already ready and no waiting is needed. Second
                // is that the SyncPoint is potentially still not signalled, but exporting a fence
                // fd had failed. In this case, there's not much we can do (perhaps do a blocking
                // wait for the SyncPoint, which itself might fail).
                //
                // So let's hope for the best and mark the buffer as submittable. We do not reuse
                // the original SyncPoint because if we do hit the second case (when it's not
                // signalled), then without a sync fd we cannot schedule a queue upon its
                // completion, effectively going stuck. It's better to queue an incomplete buffer
                // than getting stuck.
                sync_point = SyncPoint::signaled();
                None
            }
        };

        inner.rendering_buffers.push((pw_buffer, sync_point));
        drop(inner);

        match sync_fd {
            None => {
                trace!("sync_fd is None, queueing completed buffers");
                // In case this is the only buffer in the list, we will queue it right away.
                self.queue_completed_buffers();
            }
            Some(sync_fd) => {
                trace!("scheduling buffer to queue");
                let stream_id = self.stream_id;
                let source = Generic::new(sync_fd, Interest::READ, Mode::OneShot);
                self.event_loop
                    .insert_source(source, move |_, _, state| {
                        for cast in &mut state.niri.casting.casts {
                            if cast.stream_id == stream_id {
                                cast.queue_completed_buffers();
                            }
                        }

                        Ok(PostAction::Remove)
                    })
                    .unwrap();
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn dequeue_buffer_and_render(
        &mut self,
        renderer: &mut GlesRenderer,
        mut elements: &[CastRenderElement<GlesRenderer>],
        cursor_data: &CursorData<CastRenderElement<GlesRenderer>>,
        size: Size<i32, Physical>,
        scale: Scale<f64>,
    ) -> bool {
        let mut inner = self.inner.borrow_mut();

        let CastState::Ready {
            damage_tracker,
            cursor_damage_tracker,
            last_cursor_location,
            ..
        } = &mut inner.state
        else {
            error!("cast must be in Ready state to render");
            return false;
        };
        let damage_tracker = damage_tracker
            .get_or_insert_with(|| OutputDamageTracker::new(size, scale, Transform::Normal));
        let cursor_damage_tracker = cursor_damage_tracker.get_or_insert_with(|| {
            OutputDamageTracker::new(
                Size::from((CURSOR_WIDTH as _, CURSOR_HEIGHT as _)),
                scale,
                Transform::Normal,
            )
        });

        // Size change will drop the damage tracker, but scale change won't, so check it here.
        let OutputModeSource::Static { scale: t_scale, .. } = damage_tracker.mode() else {
            unreachable!();
        };
        if *t_scale != scale {
            *damage_tracker = OutputDamageTracker::new(size, scale, Transform::Normal);
            *cursor_damage_tracker = OutputDamageTracker::new(
                Size::from((CURSOR_WIDTH as _, CURSOR_HEIGHT as _)),
                scale,
                Transform::Normal,
            );
        }

        let mut has_cursor_update = false;
        let mut redraw_cursor = false;

        // For embedded cursor, pass the full slice (cursor + main) to the damage tracker.
        // For metadata or hidden cursor, pass only the main elements.
        if self.cursor_mode == CursorMode::Metadata || self.cursor_mode == CursorMode::Hidden {
            elements = &elements[cursor_data.elem_count..];
        }
        let (damage, states) = damage_tracker.damage_output(1, elements).unwrap();

        if self.cursor_mode == CursorMode::Metadata {
            if let Some(location) = cursor_data.location {
                let (damage, _states) = cursor_damage_tracker
                    .damage_output(1, &cursor_data.relocated)
                    .unwrap();
                // Send the bitmap again when the cursor comes back: the consumer may show one
                // cursor for several streams, so another one may have changed it meanwhile.
                redraw_cursor = damage.is_some() || last_cursor_location.is_none();
                has_cursor_update = redraw_cursor || *last_cursor_location != Some(location);
            } else {
                has_cursor_update = last_cursor_location.is_some();
            }
        }

        if damage.is_none() && !has_cursor_update {
            trace!("no damage, skipping frame");
            return false;
        }
        *last_cursor_location = cursor_data.location;
        drop(inner);

        let Some(pw_buffer) = self.dequeue_available_buffer() else {
            warn!("no available buffer in pw stream, skipping frame");
            return false;
        };
        let buffer = pw_buffer.as_ptr();

        let mut inner = self.inner.borrow_mut();
        let inner_ = &mut *inner;
        let CastState::Ready {
            damage_tracker,
            alpha,
            ..
        } = &mut inner_.state
        else {
            unreachable!()
        };
        let damage_tracker = damage_tracker.as_mut().unwrap();
        let alpha = *alpha;

        unsafe {
            let spa_buffer = (*buffer).buffer;

            if self.cursor_mode == CursorMode::Metadata {
                add_cursor_metadata(renderer, spa_buffer, cursor_data, redraw_cursor);
            }

            // FIXME: would be good to skip rendering the full frame if only the pointer changed.
            // Unfortunately, I think the OBS PipeWire code needs to be updated first to cleanly
            // allow for that codepath.
            let fd = (*(*spa_buffer).datas).fd;

            let res = match (*(*spa_buffer).datas).type_ {
                x if x == DataType::DmaBuf.as_raw() => {
                    let dmabuf = inner_.dmabufs[&fd].clone();
                    render_to_dmabuf(renderer, damage_tracker, dmabuf, elements, states)
                        .map(|x| (x, SharingBuf::Dma))
                }
                x if x == DataType::MemFd.as_raw() => {
                    let shmbuf = &inner_.shmbufs[&fd];

                    let fourcc = if alpha {
                        Fourcc::Argb8888
                    } else {
                        Fourcc::Xrgb8888
                    };

                    render_to_shmbuf(renderer, damage_tracker, shmbuf, fourcc, elements, states)
                        .map(|()| (SyncPoint::signaled(), SharingBuf::Shm(shmbuf.layout)))
                }
                _ => Err(anyhow::anyhow!(
                    "unknown data type in dequeue_buffer_and_render"
                )),
            };

            drop(inner);
            match res {
                Ok((sync_point, buf)) => {
                    mark_buffer_as_good(pw_buffer, &mut self.sequence_counter, buf);
                    trace!("queueing buffer with seq={}", self.sequence_counter);
                    self.queue_after_sync(pw_buffer, sync_point);
                    true
                }
                Err(err) => {
                    warn!("error rendering to buffer: {err:?}");
                    return_unused_buffer(&self.stream, &self.inner.borrow(), pw_buffer);
                    false
                }
            }
        }
    }

    pub fn dequeue_buffer_and_clear(&mut self, renderer: &mut GlesRenderer) -> bool {
        let mut inner = self.inner.borrow_mut();

        // Clear out the damage tracker if we're in Ready state.
        if let CastState::Ready {
            damage_tracker,
            cursor_damage_tracker,
            ..
        } = &mut inner.state
        {
            *damage_tracker = None;
            *cursor_damage_tracker = None;
        };
        drop(inner);

        let Some(pw_buffer) = self.dequeue_available_buffer() else {
            warn!("no available buffer in pw stream, skipping frame");
            return false;
        };
        let buffer = pw_buffer.as_ptr();

        unsafe {
            let spa_buffer = (*buffer).buffer;

            if self.cursor_mode == CursorMode::Metadata {
                add_invisible_cursor(spa_buffer);
            }

            let fd = (*(*spa_buffer).datas).fd;

            let res = match (*(*(*buffer).buffer).datas).type_ {
                x if x == DataType::DmaBuf.as_raw() => {
                    let dmabuf = self.inner.borrow().dmabufs[&fd].clone();
                    clear_dmabuf(renderer, dmabuf).map(|x| (x, SharingBuf::Dma))
                }
                x if x == DataType::MemFd.as_raw() => {
                    let inner = self.inner.borrow();
                    let shmbuf = &inner.shmbufs[&fd];
                    clear_shmbuf(shmbuf);
                    Ok((SyncPoint::signaled(), SharingBuf::Shm(shmbuf.layout)))
                }
                _ => Err(anyhow::anyhow!(
                    "unknown data type in dequeue_buffer_and_clear"
                )),
            };

            match res {
                Ok((sync_point, buf)) => {
                    mark_buffer_as_good(pw_buffer, &mut self.sequence_counter, buf);
                    trace!("queueing clear buffer with seq={}", self.sequence_counter);
                    self.queue_after_sync(pw_buffer, sync_point);
                    true
                }
                Err(err) => {
                    warn!("error clearing buffer: {err:?}");
                    return_unused_buffer(&self.stream, &self.inner.borrow(), pw_buffer);
                    false
                }
            }
        }
    }
}

impl CastInner {
    unsafe fn on_add_buffer(
        &mut self,
        gbm: Option<&GbmDevice<DeviceFd>>,
        buffer: *mut pw_buffer,
    ) -> anyhow::Result<bool> {
        let CastState::Ready {
            size,
            alpha,
            dma_negotiation,
            ..
        } = self.state
        else {
            trace!("pw stream: add_buffer, but not ready yet");
            return Ok(false);
        };

        match dma_negotiation {
            Some(DmaNegotiation { modifier, .. }) => {
                trace!(
                    "pw stream: add_buffer (dma), size={size:?}, \
                     alpha={alpha}, modifier={modifier:?}"
                );

                let Some(gbm) = gbm else {
                    error!("add_buffer(dma) without gbm");
                    bail!("missing gbm");
                };

                unsafe {
                    let spa_buffer = (*buffer).buffer;

                    let fourcc = if alpha {
                        Fourcc::Argb8888
                    } else {
                        Fourcc::Xrgb8888
                    };

                    let dmabuf = allocate_dmabuf(gbm, size, fourcc, modifier)
                        .context("error allocating dmabuf")?;

                    let plane_count = dmabuf.num_planes();
                    let has_sync = explicit_sync::find_sync_timeline(spa_buffer).is_some();
                    let sync_blocks = if has_sync { SYNC_BLOCKS } else { 0 };
                    assert_eq!((*spa_buffer).n_datas as usize, plane_count + sync_blocks);

                    for (i, (fd, (stride, offset))) in
                        zip(dmabuf.handles(), zip(dmabuf.strides(), dmabuf.offsets())).enumerate()
                    {
                        let spa_data = (*spa_buffer).datas.add(i);
                        assert!((*spa_data).type_ & (1 << DataType::DmaBuf.as_raw()) > 0);

                        (*spa_data).type_ = DataType::DmaBuf.as_raw();

                        // With DMA-BUFs, consumers should ignore the maxsize field, and
                        // producers are allowed to set it to 0.
                        //
                        // https://docs.pipewire.org/page_dma_buf.html
                        (*spa_data).maxsize = 1;
                        (*spa_data).fd = fd.as_raw_fd() as i64;
                        (*spa_data).flags = SPA_DATA_FLAG_READWRITE;

                        let chunk = (*spa_data).chunk;
                        (*chunk).stride = stride as i32;
                        (*chunk).offset = offset;

                        trace!(
                            "pw buffer plane: fd={}, stride={stride}, offset={offset}",
                            (*spa_data).fd
                        );
                    }

                    let fd = (*(*spa_buffer).datas).fd;

                    if has_sync {
                        let Some(device) = &self.sync_device else {
                            bail!("explicit sync negotiated without a sync device");
                        };
                        let timeline = device
                            .create_timeline()
                            .context("error creating timeline syncobj")?;
                        explicit_sync::fill_sync_blocks(spa_buffer, &timeline);
                        trace!("pw buffer uses explicit sync");
                        assert!(self.timelines.insert(fd, timeline).is_none());
                    }

                    assert!(self.dmabufs.insert(fd, dmabuf).is_none());
                }

                Ok(self.dmabufs.len() == 1)
            }
            None => {
                trace!("pw stream: add_buffer (shm), size={size:?}, alpha={alpha}");
                unsafe {
                    let spa_buffer = (*buffer).buffer;

                    let shmbuf = allocate_shmbuf(size).context("error allocating shmbuf")?;

                    assert_eq!((*spa_buffer).n_datas as usize, SHM_BLOCKS);

                    let spa_data = (*spa_buffer).datas;
                    assert!((*spa_data).type_ & (1 << DataType::MemFd.as_raw()) > 0);

                    (*spa_data).type_ = DataType::MemFd.as_raw();
                    (*spa_data).maxsize = shmbuf.layout.size;
                    (*spa_data).fd = shmbuf.fd.as_raw_fd() as i64;
                    (*spa_data).flags = SPA_DATA_FLAG_READWRITE;

                    let chunk = (*spa_data).chunk;
                    (*chunk).stride = shmbuf.layout.stride;
                    (*chunk).offset = 0;

                    let fd = (*(*spa_buffer).datas).fd;
                    assert!(self.shmbufs.insert(fd, shmbuf).is_none());
                }

                Ok(self.shmbufs.len() == 1)
            }
        }
    }

    unsafe fn on_remove_buffer(&mut self, buffer: *mut pw_buffer) {
        self.rendering_buffers
            .retain(|(buf, _)| buf.as_ptr() != buffer);
        self.waiting_for_release
            .retain(|buf| buf.as_ptr() != buffer);

        unsafe {
            let spa_buffer = (*buffer).buffer;
            let spa_data = (*spa_buffer).datas;

            if (*spa_data).type_ == DataType::DmaBuf.as_raw() {
                trace!("pw stream: remove_buffer (dma)");
                assert!((*spa_buffer).n_datas > 0);

                let fd = (*spa_data).fd;
                self.dmabufs.remove(&fd);
                if let Some(timeline) = self.timelines.remove(&fd) {
                    if let Some(device) = &self.sync_device {
                        device.destroy_timeline(timeline);
                    }
                }
            } else if (*spa_data).type_ == DataType::MemFd.as_raw() {
                trace!("pw stream: remove_buffer (shm)");
                assert_eq!((*spa_buffer).n_datas, SHM_BLOCKS as u32);

                let fd = (*spa_data).fd;
                self.shmbufs.remove(&fd);
            } else {
                error!(
                    "pw stream: remove_buffer (unknown type): {:?}",
                    (*spa_data).type_
                );
            }
        }
    }

    /// Moves the buffer's sync points forward and signals the acquire point.
    ///
    /// Must be called right before queueing every buffer. Buffers are only queued after
    /// rendering completed, so the data is ready as soon as we queue.
    unsafe fn advance_sync_points(&self, pw_buffer: NonNull<pw_buffer>) {
        let spa_buffer = (*pw_buffer.as_ptr()).buffer;
        let Some(meta) = explicit_sync::find_sync_timeline(spa_buffer) else {
            return;
        };
        let fd = (*(*spa_buffer).datas).fd;
        let (Some(device), Some(timeline)) = (&self.sync_device, self.timelines.get(&fd)) else {
            return;
        };

        let meta = meta.as_ptr();
        (*meta).acquire_point = (*meta).release_point + 1;
        (*meta).release_point = (*meta).acquire_point + 1;

        if let Err(err) = device.signal(timeline, (*meta).acquire_point) {
            warn!("error signaling acquire point: {err:?}");
        }
    }

    /// Whether the consumer is done with the buffer, so we can render into it.
    unsafe fn is_released(&self, pw_buffer: NonNull<pw_buffer>) -> bool {
        let spa_buffer = (*pw_buffer.as_ptr()).buffer;
        let Some(meta) = explicit_sync::find_sync_timeline(spa_buffer) else {
            return true;
        };
        let fd = (*(*spa_buffer).datas).fd;
        let (Some(device), Some(timeline)) = (&self.sync_device, self.timelines.get(&fd)) else {
            return true;
        };

        let release_point = (*meta.as_ptr()).release_point;
        device
            .is_reached(timeline, release_point)
            .unwrap_or_else(|err| {
                // Better to risk a torn frame than to get stuck.
                warn!("error querying release point: {err:?}");
                true
            })
    }
}

impl CastState {
    fn pending_size(&self) -> Option<Size<u32, Physical>> {
        match self {
            CastState::ResizePending { pending_size } => Some(*pending_size),
            CastState::ConfirmationPending { size, .. } => Some(*size),
            CastState::Ready { .. } => None,
        }
    }

    fn expected_format_size(&self) -> Size<u32, Physical> {
        match self {
            CastState::ResizePending { pending_size } => *pending_size,
            CastState::ConfirmationPending { size, .. } => *size,
            CastState::Ready { size, .. } => *size,
        }
    }
}

fn pw_version_supports_cursor_metadata() -> bool {
    // This PipeWire version fixed a critical memory issue with cursor metadata:
    // https://gitlab.freedesktop.org/pipewire/pipewire/-/merge_requests/2538
    unsafe { pw_check_library_version(1, 4, 8) }
}

/// `SPA_PARAM_Buffers` for `blocks` data blocks of `data_type`.
///
/// With `explicit_sync`, two more blocks are added for the acquire and release syncobjs, and the
/// param requires `SPA_META_SyncTimeline`, so that PipeWire only picks it when both sides support
/// explicit sync.
fn buffers_param(blocks: i32, data_type: DataType, explicit_sync: bool) -> pod::Object {
    let mut properties = vec![
        Property::new(
            SPA_PARAM_BUFFERS_buffers,
            pod::Value::Choice(ChoiceValue::Int(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Range {
                    default: 8,
                    min: 2,
                    max: 16,
                },
            ))),
        ),
        Property::new(
            SPA_PARAM_BUFFERS_blocks,
            pod::Value::Int(if explicit_sync {
                blocks + SYNC_BLOCKS as i32
            } else {
                blocks
            }),
        ),
        Property::new(
            SPA_PARAM_BUFFERS_dataType,
            pod::Value::Choice(ChoiceValue::Int(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Flags {
                    default: 1 << data_type.as_raw(),
                    flags: vec![1 << data_type.as_raw()],
                },
            ))),
        ),
    ];

    if explicit_sync {
        properties.push(Property {
            key: SPA_PARAM_BUFFERS_metaType,
            flags: PropertyFlags::MANDATORY,
            value: pod::Value::Int(1 << SPA_META_SyncTimeline),
        });
    }

    pod::Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties,
    }
}

fn make_pod(buffer: &mut Vec<u8>, object: pod::Object) -> &Pod {
    PodSerializer::serialize(Cursor::new(&mut *buffer), &pod::Value::Object(object)).unwrap();
    Pod::from_bytes(buffer).unwrap()
}

fn find_preferred_modifier(
    gbm: &GbmDevice<DeviceFd>,
    size: Size<u32, Physical>,
    fourcc: Fourcc,
    modifiers: Vec<i64>,
) -> anyhow::Result<(Modifier, usize)> {
    debug!("find_preferred_modifier: size={size:?}, fourcc={fourcc}, modifiers={modifiers:?}");

    let (buffer, modifier) = allocate_buffer(gbm, size, fourcc, &modifiers)?;

    let dmabuf = buffer
        .export()
        .context("error exporting GBM buffer object as dmabuf")?;
    let plane_count = dmabuf.num_planes();

    // FIXME: Ideally this also needs to try binding the dmabuf for rendering.

    Ok((modifier, plane_count))
}

fn allocate_buffer(
    gbm: &GbmDevice<DeviceFd>,
    size: Size<u32, Physical>,
    fourcc: Fourcc,
    modifiers: &[i64],
) -> anyhow::Result<(GbmBuffer, Modifier)> {
    let (w, h) = (size.w, size.h);
    let flags = GbmBufferFlags::RENDERING;

    if modifiers.len() == 1 && Modifier::from(modifiers[0] as u64) == Modifier::Invalid {
        let bo = gbm
            .create_buffer_object::<()>(w, h, fourcc, flags)
            .context("error creating GBM buffer object")?;

        let buffer = GbmBuffer::from_bo(bo, true);
        Ok((buffer, Modifier::Invalid))
    } else {
        let modifiers = modifiers
            .iter()
            .map(|m| Modifier::from(*m as u64))
            .filter(|m| *m != Modifier::Invalid);

        let bo = gbm
            .create_buffer_object_with_modifiers2::<()>(w, h, fourcc, modifiers, flags)
            .context("error creating GBM buffer object")?;

        let modifier = bo.modifier();
        let buffer = GbmBuffer::from_bo(bo, false);
        Ok((buffer, modifier))
    }
}

fn allocate_dmabuf(
    gbm: &GbmDevice<DeviceFd>,
    size: Size<u32, Physical>,
    fourcc: Fourcc,
    modifier: Modifier,
) -> anyhow::Result<Dmabuf> {
    let (buffer, _modifier) = allocate_buffer(gbm, size, fourcc, &[u64::from(modifier) as i64])?;
    let dmabuf = buffer
        .export()
        .context("error exporting GBM buffer object as dmabuf")?;
    Ok(dmabuf)
}

#[derive(Debug)]
pub struct Shmbuf {
    fd: OwnedFd,
    layout: ShmLayout,
    mapping: ShmMapping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShmLayout {
    stride: i32,
    size: u32,
}

impl ShmLayout {
    fn new(size: Size<u32, Physical>) -> anyhow::Result<Self> {
        let stride = size
            .w
            .checked_mul(SHM_BYTES_PER_PIXEL as u32)
            .context("SHM stride overflows u32")?;
        let buffer_size = stride
            .checked_mul(size.h)
            .context("SHM buffer size overflows u32")?;

        Ok(Self {
            stride: stride.try_into().context("SHM stride exceeds i32")?,
            size: buffer_size,
        })
    }

    fn size_usize(self) -> usize {
        self.size as usize
    }
}

enum SharingBuf {
    Dma,
    Shm(ShmLayout),
}

fn allocate_shmbuf(size: Size<u32, Physical>) -> anyhow::Result<Shmbuf> {
    let layout = ShmLayout::new(size)?;
    let fd = memfd_create(
        "niri-pw-stream-memfd",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )
    .context("error creating memfd")?;
    ftruncate(&fd, layout.size.into()).context("error setting size of the fd")?;
    fcntl_add_seals(&fd, SealFlags::SEAL | SealFlags::SHRINK | SealFlags::GROW)
        .context("error sealing the fd")?;
    // SAFETY: The file has the requested size and is sealed against shrinking.
    // We only access the mapping while we own the dequeued PipeWire buffer.
    let mapping = unsafe { ShmMapping::new(fd.as_fd(), layout.size_usize()) }?;
    Ok(Shmbuf {
        fd,
        layout,
        mapping,
    })
}

unsafe fn return_unused_buffer(stream: &Stream, inner: &CastInner, pw_buffer: NonNull<pw_buffer>) {
    // pw_stream_return_buffer() requires too new PipeWire (1.4.0). So, mark as
    // corrupted and queue.
    let pw_buffer = pw_buffer.as_ptr();
    let spa_buffer = (*pw_buffer).buffer;
    let chunk = (*(*spa_buffer).datas).chunk;
    // Some (older?) consumers will check for size == 0 instead of the CORRUPTED flag.
    (*chunk).size = 0;
    (*chunk).flags = SPA_CHUNK_FLAG_CORRUPTED as i32;

    if let Some(header) = find_meta_header(spa_buffer) {
        let header = header.as_ptr();
        (*header).flags = SPA_META_HEADER_FLAG_CORRUPTED;
    }

    // Consumers read new sync points from every buffer they get, corrupted ones included, and
    // signal the release point when they give it back.
    inner.advance_sync_points(NonNull::new_unchecked(pw_buffer));
    pw_stream_queue_buffer(stream.as_raw_ptr(), pw_buffer);
}

unsafe fn mark_buffer_as_good(pw_buffer: NonNull<pw_buffer>, sequence: &mut u64, buf: SharingBuf) {
    let pw_buffer = pw_buffer.as_ptr();
    let spa_buffer = (*pw_buffer).buffer;
    let chunk = (*(*spa_buffer).datas).chunk;

    match buf {
        SharingBuf::Dma => {
            // With DMA-BUFs, consumers should ignore the size field, and producers are allowed
            // to set it to 0.
            //
            // https://docs.pipewire.org/page_dma_buf.html
            //
            // However, OBS checks for size != 0 as a workaround for old compositor versions,
            // so we set it to 1.
            (*chunk).size = 1;
            // Clear the corrupted flag we may have set before.
            (*chunk).flags = SPA_CHUNK_FLAG_NONE as i32;
        }
        SharingBuf::Shm(layout) => {
            (*chunk).size = layout.size;
            (*chunk).flags = SPA_CHUNK_FLAG_NONE as i32;
        }
    }

    *sequence = sequence.wrapping_add(1);
    if let Some(header) = find_meta_header(spa_buffer) {
        let header = header.as_ptr();
        // Clear the corrupted flag we may have set before.
        (*header).flags = 0;
        (*header).seq = *sequence;

        // Set buffer timestamp as unknown.
        //
        // FIXME: we could try passing real presentation timestamps for rendered frames here.
        // However, then we must also ensure that the time base never jumps (e.g. when switching a
        // dynamic cast between outputs) as this would mess up the timing downstream.
        (*header).pts = -1;
    }
}

unsafe fn find_meta_header(buffer: *mut spa_buffer) -> Option<NonNull<spa_meta_header>> {
    let p = spa_buffer_find_meta_data(buffer, SPA_META_Header, size_of::<spa_meta_header>()).cast();
    NonNull::new(p)
}

unsafe fn add_invisible_cursor(spa_buffer: *mut spa_buffer) {
    unsafe {
        let cursor_meta_ptr: *mut spa_meta_cursor = spa_buffer_find_meta_data(
            spa_buffer,
            SPA_META_Cursor,
            mem::size_of::<spa_meta_cursor>(),
        )
        .cast();
        let Some(cursor_meta) = cursor_meta_ptr.as_mut() else {
            return;
        };

        // The cursor is present but invisible.
        cursor_meta.id = 1;
        cursor_meta.position.x = 0;
        cursor_meta.position.y = 0;
        cursor_meta.hotspot.x = 0;
        cursor_meta.hotspot.y = 0;
        cursor_meta.bitmap_offset = BITMAP_META_OFFSET as _;

        let bitmap_meta_ptr = cursor_meta_ptr
            .byte_add(BITMAP_META_OFFSET)
            .cast::<spa_meta_bitmap>();
        let bitmap_meta = &mut *bitmap_meta_ptr;

        // HACK: PipeWire docs say offset = 0 means invisible.
        //
        // Unfortunately, OBS doesn't actually check that, instead it checks that size isn't zero:
        // https://github.com/obsproject/obs-studio/blob/f4aaa5f0417c5ec40a3799551e125129fce1e007/plugins/linux-pipewire/pipewire.c#L900
        //
        // Unfortunately, libwebrtc, on top of ignoring offset, also treats size = 0 as "preserve
        // previous cursor":
        // https://webrtc.googlesource.com/src/+/97b46e12582606a238d4f0c8524365cf5bdcb411/modules/desktop_capture/linux/wayland/shared_screencast_stream.cc#765
        //
        // So, send a 1x1 transparent pixel instead...
        bitmap_meta.offset = BITMAP_DATA_OFFSET as _;
        bitmap_meta.size.width = 1;
        bitmap_meta.size.height = 1;
        bitmap_meta.stride = CURSOR_BPP as i32;
        bitmap_meta.format = CURSOR_FORMAT;

        let bitmap_data = bitmap_meta_ptr.cast::<u8>().add(BITMAP_DATA_OFFSET);
        let bitmap_slice = slice::from_raw_parts_mut(bitmap_data, CURSOR_BITMAP_SIZE);
        bitmap_slice[..4].copy_from_slice(&[0, 0, 0, 0]);
    }
}

unsafe fn add_cursor_metadata(
    renderer: &mut GlesRenderer,
    spa_buffer: *mut spa_buffer,
    cursor_data: &CursorData<impl RenderElement<GlesRenderer>>,
    redraw: bool,
) {
    unsafe {
        let cursor_meta_ptr: *mut spa_meta_cursor = spa_buffer_find_meta_data(
            spa_buffer,
            SPA_META_Cursor,
            mem::size_of::<spa_meta_cursor>(),
        )
        .cast();
        let Some(cursor_meta) = cursor_meta_ptr.as_mut() else {
            return;
        };

        let Some(location) = cursor_data.location else {
            // Not in this stream: an invalid cursor (zero ID), like mutter.
            cursor_meta.id = 0;
            cursor_meta.bitmap_offset = 0;
            return;
        };

        cursor_meta.id = 1;
        cursor_meta.position.x = location.x;
        cursor_meta.position.y = location.y;
        cursor_meta.hotspot.x = cursor_data.hotspot.x;
        cursor_meta.hotspot.y = cursor_data.hotspot.y;

        if !redraw {
            trace!("cursor not damaged, skipping rerendering");
            cursor_meta.bitmap_offset = 0;
            return;
        }

        cursor_meta.bitmap_offset = BITMAP_META_OFFSET as _;

        let bitmap_meta_ptr = cursor_meta_ptr
            .byte_add(BITMAP_META_OFFSET)
            .cast::<spa_meta_bitmap>();
        let bitmap_meta = &mut *bitmap_meta_ptr;

        // Start with a 1x1 transparent pixel; see comment in add_invisible_cursor().
        bitmap_meta.offset = BITMAP_DATA_OFFSET as _;
        bitmap_meta.size.width = 1;
        bitmap_meta.size.height = 1;
        bitmap_meta.stride = CURSOR_BPP as i32;
        bitmap_meta.format = CURSOR_FORMAT;

        let bitmap_data = bitmap_meta_ptr.cast::<u8>().add(BITMAP_DATA_OFFSET);
        let bitmap_slice = slice::from_raw_parts_mut(bitmap_data, CURSOR_BITMAP_SIZE);
        bitmap_slice[..4].copy_from_slice(&[0, 0, 0, 0]);

        let size = Size::new(
            min(cursor_data.size.w, CURSOR_WIDTH as i32),
            min(cursor_data.size.h, CURSOR_HEIGHT as i32),
        );
        if size.w == 0 || size.h == 0 {
            trace!("cursor is invisible, skipping rendering");
            return;
        }

        let _span = tracy_client::span!("add_cursor_metadata render cursor");

        // FIXME: use a reliable buffer whenever we're rendering the cursor.
        //
        // PipeWire buffers are not normally guaranteed to reach the destination, so our buffer
        // with the rendered cursor bitmap may not reach the consumer.
        //
        // Reliable buffers should be available starting from 1.6.0:
        // https://gitlab.freedesktop.org/pipewire/pipewire/-/issues/4885
        let mapping = match render_and_download(
            renderer,
            size,
            cursor_data.scale,
            Transform::Normal,
            CURSOR_FOURCC,
            cursor_data.relocated.iter().rev(),
        ) {
            Ok(mapping) => mapping,
            Err(err) => {
                warn!("error rendering cursor: {err:?}");
                return;
            }
        };
        let pixels = match renderer.map_texture(&mapping) {
            Ok(pixels) => pixels,
            Err(err) => {
                warn!("error mapping cursor texture: {err:?}");
                return;
            }
        };

        bitmap_slice[..pixels.len()].copy_from_slice(pixels);

        // Fill the metadata now that everything succeeded.
        bitmap_meta.size.width = size.w as _;
        bitmap_meta.size.height = size.h as _;
        bitmap_meta.stride = size.w * CURSOR_BPP as i32;
    }
}

fn render_to_shmbuf(
    renderer: &mut GlesRenderer,
    damage_tracker: &mut OutputDamageTracker,
    buffer: &Shmbuf,
    fourcc: Fourcc,
    elements: &[impl RenderElement<GlesRenderer>],
    states: RenderElementStates,
) -> anyhow::Result<()> {
    let _span = tracy_client::span!();
    let (size, _scale, _transform) = damage_tracker.mode().try_into().unwrap();
    let expected_size = size.w as usize * size.h as usize * SHM_BYTES_PER_PIXEL;
    ensure!(
        buffer.layout.size_usize() == expected_size,
        "invalid buffer size"
    );

    let mapping =
        render_and_download_with_damage(renderer, damage_tracker, fourcc, elements, states)?;

    let bytes = renderer
        .map_texture(&mapping)
        .context("error mapping texture")?;

    buffer.mapping.copy_frame(bytes);
    Ok(())
}

fn clear_shmbuf(buffer: &Shmbuf) {
    buffer.mapping.clear();
}

/// Reads what we use from a consumer's tag param.
///
/// gnome-remote-desktop sends `org.gnome.preferred-scale` for each RDP client monitor
/// (`add_tag_params` in `grd-rdp-pipewire-stream.c`), and with our patch also
/// `org.gnome.position-x`, `org.gnome.position-y` and `org.gnome.is-primary`. A tag is an object
/// whose info property is a struct of the item count followed by key and value strings.
fn parse_consumer_tag(pod: &Pod) -> Option<ConsumerTag> {
    let (_, value) = PodDeserializer::deserialize_any_from(pod.as_bytes()).ok()?;
    let pod::Value::Object(object) = value else {
        return None;
    };
    let info = object
        .properties
        .iter()
        .find(|prop| prop.key == SPA_PARAM_TAG_info)?;
    let pod::Value::Struct(items) = &info.value else {
        return None;
    };

    let mut strings = items.iter().skip(1).map(|item| match item {
        pod::Value::String(s) => Some(s.as_str()),
        _ => None,
    });
    let mut tag = ConsumerTag::default();
    let (mut x, mut y) = (None, None);
    while let (Some(key), Some(value)) = (strings.next(), strings.next()) {
        let (Some(key), Some(value)) = (key, value) else {
            continue;
        };
        match key {
            "org.gnome.preferred-scale" => {
                tag.scale = value
                    .parse::<f64>()
                    .ok()
                    .filter(|scale| scale.is_finite() && *scale > 0.);
            }
            "org.gnome.position-x" => x = value.parse::<i32>().ok(),
            "org.gnome.position-y" => y = value.parse::<i32>().ok(),
            "org.gnome.is-primary" => tag.primary = value.parse::<bool>().ok(),
            _ => (),
        }
    }
    if let (Some(x), Some(y)) = (x, y) {
        tag.position = Some(Point::from((x, y)));
    }

    (tag != ConsumerTag::default()).then_some(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shm_layout_uses_spa_representable_dimensions() {
        let layout = ShmLayout::new(Size::from((3840, 2160))).unwrap();
        assert_eq!(layout.stride, 15360);
        assert_eq!(layout.size, 33_177_600);

        assert!(ShmLayout::new(Size::from((536_870_912, 1))).is_err());
        assert!(ShmLayout::new(Size::from((500_000_000, 3))).is_err());
    }

    /// A tag like gnome-remote-desktop's (`spa_tag_build_add_dict`).
    fn tag_pod(items: &[(&str, &str)]) -> Vec<u8> {
        let mut info = vec![pod::Value::Int(items.len() as i32)];
        for (key, value) in items {
            info.push(pod::Value::String(key.to_string()));
            info.push(pod::Value::String(value.to_string()));
        }
        let object = pod::Value::Object(pod::Object {
            type_: SPA_TYPE_OBJECT_ParamTag,
            id: SPA_PARAM_Tag,
            properties: vec![
                Property {
                    key: SPA_PARAM_TAG_direction,
                    flags: PropertyFlags::empty(),
                    value: pod::Value::Id(spa::utils::Id(SPA_DIRECTION_INPUT)),
                },
                Property {
                    key: SPA_PARAM_TAG_info,
                    flags: PropertyFlags::empty(),
                    value: pod::Value::Struct(info),
                },
            ],
        });
        PodSerializer::serialize(Cursor::new(Vec::new()), &object)
            .unwrap()
            .0
            .into_inner()
    }

    fn parse(items: &[(&str, &str)]) -> Option<ConsumerTag> {
        let bytes = tag_pod(items);
        parse_consumer_tag(Pod::from_bytes(&bytes).unwrap())
    }

    #[test]
    fn consumer_tag_from_grd() {
        assert_eq!(
            parse(&[("org.gnome.preferred-scale", "1.5")]),
            Some(ConsumerTag {
                scale: Some(1.5),
                ..Default::default()
            })
        );
        assert_eq!(
            parse(&[
                ("org.gnome.preferred-scale", "1"),
                ("org.gnome.position-x", "-2560"),
                ("org.gnome.position-y", "0"),
                ("org.gnome.is-primary", "false"),
            ]),
            Some(ConsumerTag {
                scale: Some(1.),
                position: Some(Point::from((-2560, 0))),
                primary: Some(false),
            })
        );
        assert_eq!(parse(&[("something.else", "1")]), None);
        assert_eq!(parse(&[("org.gnome.preferred-scale", "nan")]), None);
    }
}
