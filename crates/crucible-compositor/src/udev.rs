//! `udev`/DRM/KMS backend for running CDE on a real TTY across multiple outputs.
//!
//! This is the production backend (WS-6), adapted from Smithay's `anvil` reference compositor and
//! trimmed to CDE's Phase-1 scope. It brings up:
//!
//! 1. **Session + devices** — a [`LibSeatSession`] (so CDE can open DRM/input fds without root) and a
//!    [`UdevBackend`] that enumerates GPUs and hotplug events. Each DRM device gets a
//!    [`DrmOutputManager`] (GBM allocator + framebuffer exporter) and is registered for VBlank
//!    page-flip events. A [`GpuManager`] (`renderer_multi`) provides EGL/GLES rendering with
//!    multi-GPU fallback to the primary GPU.
//! 2. **Connectors → outputs** — [`DrmScanner::scan_connectors`] detects connect/disconnect; each
//!    connected connector becomes a Smithay [`Output`] mapped into the **shared** [`Space`]
//!    left-to-right (see [`next_output_position`]). Layer maps are re-`arrange`d on every change via
//!    [`fixup_output_positions`].
//! 3. **Per-output rendering** — each output renders on its own VBlank through the
//!    [`DrmOutput`]/`DrmCompositor`, reusing the backend-agnostic render-element collection in
//!    [`crate::render`] so Wayland *and* XWayland windows appear on every output. Presentation
//!    feedback ([`take_presentation_feedback`]) is delivered after each page-flip.
//! 4. **Input** — a [`LibinputInputBackend`] bound to the session feeds the **same** seat handlers
//!    the winit backend uses (see [`CrucibleState::process_input_event_libinput`]).
//! 5. **VT switching** — [`SessionEvent::PauseSession`]/[`SessionEvent::ActivateSession`] suspend and
//!    resume libinput and the DRM devices so `Ctrl+Alt+F<n>` VT switches work.
//!
//! Selected at runtime via `--tty-udev`. Compared to anvil this backend omits (deferred to later
//! phases) DMA-BUF feedback, `drm_syncobj`, DRM leasing, the on-screen FPS overlay, DnD-icon
//! rendering, and the multi-GPU pointer-texture cache; CDE draws its self-contained default cursor
//! via [`PointerElement`](crate::drawing::PointerElement) just like the winit path.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Duration;

use smithay::{
    backend::{
        SwapBuffersError,
        allocator::{
            Fourcc, Modifier,
            format::FormatSet,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            CreateDrmNodeError, DrmAccessError, DrmDevice, DrmDeviceFd, DrmError, DrmEvent,
            DrmEventMetadata, DrmNode, NodeType,
            compositor::FrameFlags,
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
        },
        egl::{EGLContext, EGLDevice, EGLDisplay, context::ContextPriority},
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            ImportMemWl,
            damage::Error as OutputDamageTrackerError,
            element::AsRenderElements,
            gles::GlesRenderer,
            multigpu::{GpuManager, MultiRenderer, gbm::GbmGlesBackend},
        },
        session::{
            Event as SessionEvent, Session,
            libseat::{self, LibSeatSession},
        },
        udev::{UdevBackend, UdevEvent, all_gpus, primary_gpu},
    },
    desktop::utils::OutputPresentationFeedback,
    input::{keyboard::LedState, pointer::CursorImageStatus},
    output::{Mode as WlMode, Output, PhysicalProperties},
    reexports::{
        calloop::{
            EventLoop, RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        drm::control::{ModeTypeFlags, connector, crtc},
        input::{DeviceCapability, Libinput},
        rustix::fs::OFlags,
        wayland_protocols::wp::presentation_time::server::wp_presentation_feedback,
        wayland_server::{
            Display, DisplayHandle, backend::GlobalId, protocol::wl_surface::WlSurface,
        },
    },
    utils::{DeviceFd, IsAlive, Scale},
    wayland::presentation::Refresh,
};
use smithay_drm_extras::{
    display_info,
    drm_scanner::{DrmScanEvent, DrmScanner},
};
use tracing::{debug, error, info, warn};

use crate::{
    drawing::PointerElement,
    element::WindowElement,
    render::{CustomRenderElements, OutputRenderElements, output_elements},
    state::{Backend, CrucibleState, take_presentation_feedback, update_primary_scanout_output},
};

/// Color formats CDE asks the DRM compositor to use, in preference order.
///
/// 10-bit first (better gradients) with 8-bit fallback — both are widely supported by display
/// controllers. Mirrors anvil's choice.
const SUPPORTED_FORMATS: &[Fourcc] = &[
    Fourcc::Abgr2101010,
    Fourcc::Argb2101010,
    Fourcc::Abgr8888,
    Fourcc::Argb8888,
];

/// The multi-GPU renderer type used by the udev backend.
///
/// A [`MultiRenderer`] over GBM/GLES allows rendering on one GPU and scanning out on another (e.g.
/// laptop iGPU + dGPU), falling back to the primary GPU when a device has no render node.
type UdevRenderer<'a> = MultiRenderer<
    'a,
    'a,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
>;

/// The render-element type a [`DrmOutput`] is parameterized over for CDE's element collection.
///
/// Matches the `(R, E)` pair [`output_elements`] yields for the udev renderer, so the same window
/// + cursor elements used by the winit path drive the DRM compositor.
type OutputElements<'a> = OutputRenderElements<
    UdevRenderer<'a>,
    <WindowElement as AsRenderElements<UdevRenderer<'a>>>::RenderElement,
>;

/// Identifies the [`Output`] driven by a particular `(device, crtc)` pair.
///
/// Stored in each output's `user_data` so VBlank/render callbacks can find the Smithay [`Output`]
/// that corresponds to the CRTC the event fired for.
#[derive(Debug, PartialEq)]
struct UdevOutputId {
    /// The DRM device (GPU) node the output lives on.
    device_id: DrmNode,
    /// The CRTC scanning out this output.
    crtc: crtc::Handle,
}

/// Backend data for the `udev`/DRM/KMS TTY backend.
///
/// Owns the [`LibSeatSession`], the multi-GPU [`GpuManager`], and the per-device [`BackendData`]
/// (one per GPU). Stored in [`CrucibleState::backend_data`] and accessed through the [`Backend`]
/// trait plus the `udev`-specific `impl` in this module.
pub struct UdevData {
    /// The `libseat` session granting access to DRM and input devices without root.
    pub session: LibSeatSession,
    /// The GPU chosen for rendering when a device has no render node of its own.
    primary_gpu: DrmNode,
    /// Multi-GPU renderer manager (EGL/GLES per GPU, with cross-GPU import).
    gpus: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    /// Per-DRM-device state, keyed by the device's [`DrmNode`].
    backends: HashMap<DrmNode, BackendData>,
    /// The compositor's self-contained default cursor (shared with the winit path).
    pointer_element: PointerElement,
    /// Keyboards currently present, tracked so LED state can be pushed to them.
    keyboards: Vec<smithay::reexports::input::Device>,
}

impl Backend for UdevData {
    // Real input devices deliver relative pointer motion and gestures.
    const HAS_RELATIVE_MOTION: bool = true;
    const HAS_GESTURES: bool = true;

    fn seat_name(&self) -> String {
        self.session.seat()
    }

    fn reset_buffers(&mut self, output: &Output) {
        if let Some(id) = output.user_data().get::<UdevOutputId>() {
            if let Some(gpu) = self.backends.get_mut(&id.device_id) {
                if let Some(surface) = gpu.surfaces.get_mut(&id.crtc) {
                    surface.drm_output.reset_buffers();
                }
            }
        }
    }

    fn early_import(&mut self, surface: &WlSurface) {
        if let Err(err) = self.gpus.early_import(self.primary_gpu, surface) {
            warn!("Early buffer import failed: {}", err);
        }
    }

    fn update_led_state(&mut self, led_state: LedState) {
        for keyboard in self.keyboards.iter_mut() {
            keyboard.led_update(led_state.into());
        }
    }

    fn change_vt(&mut self, vt: i32) -> bool {
        match self.session.change_vt(vt) {
            Ok(()) => true,
            Err(err) => {
                error!(vt, %err, "Failed to switch VT");
                false
            }
        }
    }
}

/// One mapped output (connector/CRTC) on a DRM device.
///
/// Holds the Smithay [`Output`], its `wl_output` global, and the [`DrmOutput`] that owns the
/// per-output swapchain and page-flip scheduling.
struct SurfaceData {
    /// Display handle, used to remove the `wl_output` global on drop.
    dh: DisplayHandle,
    /// The Smithay output mapped into the shared [`Space`].
    output: Output,
    /// The `wl_output` global advertised to clients (removed on drop).
    global: Option<GlobalId>,
    /// The DRM compositor for this CRTC: owns the GBM swapchain and frame submission.
    drm_output: DrmOutput<
        GbmAllocator<DrmDeviceFd>,
        GbmFramebufferExporter<DrmDeviceFd>,
        Option<OutputPresentationFeedback>,
        DrmDeviceFd,
    >,
}

impl Drop for SurfaceData {
    fn drop(&mut self) {
        self.output.leave_all();
        if let Some(global) = self.global.take() {
            self.dh.remove_global::<CrucibleState<UdevData>>(global);
        }
    }
}

/// Per-DRM-device state: its output manager, connector scanner, and live outputs.
struct BackendData {
    /// The mapped outputs on this device, keyed by CRTC.
    surfaces: HashMap<crtc::Handle, SurfaceData>,
    /// Manages the GBM-backed [`DrmOutput`]s for this device's CRTCs.
    drm_output_manager: DrmOutputManager<
        GbmAllocator<DrmDeviceFd>,
        GbmFramebufferExporter<DrmDeviceFd>,
        Option<OutputPresentationFeedback>,
        DrmDeviceFd,
    >,
    /// Tracks connector connect/disconnect across `scan_connectors` calls.
    drm_scanner: DrmScanner,
    /// The render node for this device, if it has one (else rendering falls back to the primary).
    render_node: Option<DrmNode>,
    /// calloop token for this device's DRM event (VBlank) source, removed on device removal.
    registration_token: RegistrationToken,
}

/// Errors that can occur while adding a DRM device.
#[derive(Debug, thiserror::Error)]
enum DeviceAddError {
    /// `libseat` failed to open the device node.
    #[error("Failed to open device using libseat: {0}")]
    DeviceOpen(libseat::Error),
    /// The DRM device could not be initialized.
    #[error("Failed to initialize drm device: {0}")]
    DrmDevice(DrmError),
    /// The GBM device could not be initialized.
    #[error("Failed to initialize gbm device: {0}")]
    GbmDevice(std::io::Error),
    /// The device path did not resolve to a DRM node.
    #[error("Failed to access drm node: {0}")]
    DrmNode(CreateDrmNodeError),
    /// The render node could not be added to the [`GpuManager`].
    #[error("Failed to add device to GpuManager: {0}")]
    AddNode(smithay::backend::egl::Error),
    /// The device exposes no usable render node and no primary GPU is available to borrow from.
    #[error("The device has no render node and no primary GPU is available")]
    NoAllocator,
}

/// Computes the x-offset for a new output given the widths of the already-mapped outputs.
///
/// CDE lays outputs out **left-to-right** at `y = 0`: a new output's left edge is the sum of the
/// widths of all existing outputs. **Pure** (takes raw widths) so the layout rule is unit-tested
/// without a GPU/DRM. Returns the `(x, 0)` position for the new output.
fn next_output_position(existing_widths: impl IntoIterator<Item = i32>) -> (i32, i32) {
    (existing_widths.into_iter().sum(), 0)
}

/// Re-lays every mapped output left-to-right and re-arranges each output's layer map.
///
/// Called after any connector change so the outputs form a contiguous left-to-right strip with no
/// gaps or overlaps (anvil's `fixup_positions`). Mirrors the layout rule in [`next_output_position`].
fn fixup_output_positions(state: &mut CrucibleState<UdevData>) {
    let mut offset_x = 0;
    let outputs: Vec<Output> = state.space.outputs().cloned().collect();
    for output in outputs {
        let width = state
            .space
            .output_geometry(&output)
            .map(|geo| geo.size.w)
            .unwrap_or(0);
        state.space.map_output(&output, (offset_x, 0));
        output.change_current_state(None, None, None, Some((offset_x, 0).into()));
        smithay::desktop::layer_map_for_output(&output).arrange();
        offset_x += width;
    }
    // Output positions/sizes changed: re-assert model-driven geometry (idempotent).
    state.apply_geometry();
}

/// Picks any available GPU as a last resort when the seat has no obvious primary GPU.
fn fallback_gpu(session: &LibSeatSession) -> DrmNode {
    all_gpus(session.seat())
        .unwrap()
        .into_iter()
        .find_map(|x| DrmNode::from_path(x).ok())
        .expect("No GPU!")
}

/// Brings up and runs the `udev`/DRM/KMS backend (the `--tty-udev` entry point).
///
/// Initializes the `libseat` session, the multi-GPU renderer, the udev device monitor, and the
/// libinput source, adds every present GPU (primary first), starts XWayland, then runs the calloop
/// event loop until [`CrucibleState::running`] is cleared. Rendering is driven per-output on VBlank;
/// see [`CrucibleState::frame_finish`] / [`CrucibleState::render_surface`].
pub fn run_udev() {
    let mut event_loop = match EventLoop::try_new() {
        Ok(ev) => ev,
        Err(err) => {
            error!("Failed to create the event loop: {}", err);
            return;
        }
    };
    let display = Display::new().unwrap();
    let mut display_handle = display.handle();

    // Initialize the libseat session (grants DRM + input access without root).
    let (session, notifier) = match LibSeatSession::new() {
        Ok(ret) => ret,
        Err(err) => {
            error!("Could not initialize a session: {}", err);
            return;
        }
    };

    // Pick the primary GPU: prefer its render node, else any GPU.
    let primary_gpu = if let Ok(var) = std::env::var("CDE_DRM_DEVICE") {
        match DrmNode::from_path(&var) {
            Ok(node) => node,
            Err(err) => {
                error!(path = var, %err, "Invalid CDE_DRM_DEVICE path");
                return;
            }
        }
    } else {
        match primary_gpu(session.seat()) {
            Ok(Some(path)) => DrmNode::from_path(&path)
                .ok()
                .and_then(|node| node.node_with_type(NodeType::Render).and_then(|n| n.ok()))
                .or_else(|| DrmNode::from_path(&path).ok())
                .unwrap_or_else(|| fallback_gpu(&session)),
            _ => fallback_gpu(&session),
        }
    };
    info!("Using {} as primary gpu.", primary_gpu);

    // Build the multi-GPU renderer manager (high-priority EGL contexts for the compositor).
    let gpus = match GpuManager::new(GbmGlesBackend::with_factory(|display| {
        let context = EGLContext::new_with_priority(display, ContextPriority::High)?;
        // SAFETY: the GLES renderer is created from a freshly built EGL context that Smithay owns
        // for the lifetime of this GPU node; no other renderer shares it.
        #[allow(unsafe_code)]
        let capabilities = unsafe { GlesRenderer::supported_capabilities(&context)? };
        #[allow(unsafe_code)]
        let renderer = unsafe { GlesRenderer::with_capabilities(context, capabilities)? };
        Ok(renderer)
    })) {
        Ok(gpus) => gpus,
        Err(err) => {
            error!("Failed to initialize the GPU manager: {}", err);
            return;
        }
    };

    let data = UdevData {
        session,
        primary_gpu,
        gpus,
        backends: HashMap::new(),
        pointer_element: PointerElement::default(),
        keyboards: Vec::new(),
    };
    let mut state = CrucibleState::init(display, event_loop.handle(), data, true);

    // Initialize the udev backend (GPU enumeration + hotplug).
    let udev_backend = match UdevBackend::new(&state.seat_name) {
        Ok(ret) => ret,
        Err(err) => {
            error!(error = ?err, "Failed to initialize udev backend");
            return;
        }
    };

    // Initialize libinput, bound to our session, and feed its events into the shared seat handlers.
    let mut libinput_context = Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(
        state.backend_data.session.clone().into(),
    );
    if libinput_context.udev_assign_seat(&state.seat_name).is_err() {
        error!("Failed to assign libinput seat");
        return;
    }
    let libinput_backend = LibinputInputBackend::new(libinput_context.clone());

    event_loop
        .handle()
        .insert_source(libinput_backend, move |mut event, _, data| {
            // Track keyboards for LED sync (Caps/Num lock) before handing the event to the seat.
            if let InputEvent::DeviceAdded { device } = &mut event {
                if device.has_capability(DeviceCapability::Keyboard) {
                    if let Some(led_state) = data
                        .seat
                        .get_keyboard()
                        .map(|keyboard| keyboard.led_state())
                    {
                        device.led_update(led_state.into());
                    }
                    data.backend_data.keyboards.push(device.clone());
                }
            } else if let InputEvent::DeviceRemoved { ref device } = event {
                if device.has_capability(DeviceCapability::Keyboard) {
                    data.backend_data.keyboards.retain(|item| item != device);
                }
            }
            data.process_input_event_libinput(event);
        })
        .unwrap();

    // VT switching: pause/resume libinput and the DRM devices on session pause/activate.
    event_loop
        .handle()
        .insert_source(notifier, move |event, &mut (), data| match event {
            SessionEvent::PauseSession => {
                info!("pausing session");
                libinput_context.suspend();
                for backend in data.backend_data.backends.values_mut() {
                    backend.drm_output_manager.pause();
                }
            }
            SessionEvent::ActivateSession => {
                info!("resuming session");
                if let Err(err) = libinput_context.resume() {
                    error!("Failed to resume libinput context: {:?}", err);
                }
                let nodes: Vec<DrmNode> = data.backend_data.backends.keys().copied().collect();
                for node in nodes {
                    if let Some(backend) = data.backend_data.backends.get_mut(&node) {
                        if let Err(err) = backend.drm_output_manager.lock().activate(false) {
                            error!(%err, "Failed to re-activate drm device on session resume");
                        }
                    }
                    // Re-kick rendering for every output on this device.
                    data.handle.insert_idle(move |data| data.render(node, None));
                }
            }
        })
        .unwrap();

    // Add the primary device first so display-only devices can fall back to it for rendering.
    let primary_node = primary_gpu
        .node_with_type(NodeType::Primary)
        .and_then(|node| node.ok());
    let primary_device = udev_backend.device_list().find(|(device_id, _)| {
        primary_node
            .map(|primary_node| *device_id == primary_node.dev_id())
            .unwrap_or(false)
            || *device_id == primary_gpu.dev_id()
    });
    if let Some((device_id, path)) = primary_device {
        if let Ok(node) = DrmNode::from_dev_id(device_id) {
            if let Err(err) = state.device_added(node, path) {
                error!("Failed to initialize primary node: {err}");
            }
        }
    }
    let primary_device_id = primary_device.map(|(device_id, _)| device_id);
    for (device_id, path) in udev_backend.device_list() {
        if Some(device_id) == primary_device_id {
            continue;
        }
        if let Err(err) = DrmNode::from_dev_id(device_id)
            .map_err(DeviceAddError::DrmNode)
            .and_then(|node| state.device_added(node, path))
        {
            error!("Skipping device {device_id}: {err}");
        }
    }

    // Advertise the primary GPU's SHM formats to clients.
    if let Ok(renderer) = state.backend_data.gpus.single_renderer(&primary_gpu) {
        state.shm_state.update_formats(renderer.shm_formats());
    }

    // Drive hotplug: add/change/remove DRM devices as udev reports them.
    event_loop
        .handle()
        .insert_source(udev_backend, move |event, _, data| match event {
            UdevEvent::Added { device_id, path } => {
                if let Err(err) = DrmNode::from_dev_id(device_id)
                    .map_err(DeviceAddError::DrmNode)
                    .and_then(|node| data.device_added(node, &path))
                {
                    error!("Skipping device {device_id}: {err}");
                }
            }
            UdevEvent::Changed { device_id } => {
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    data.device_changed(node)
                }
            }
            UdevEvent::Removed { device_id } => {
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    data.device_removed(node)
                }
            }
        })
        .unwrap();

    // Bring up XWayland so X11 apps run alongside Wayland clients.
    state.start_xwayland();

    info!("Initialization completed, starting the main loop.");
    while state.running.load(Ordering::SeqCst) {
        let result = event_loop.dispatch(Some(Duration::from_millis(16)), &mut state);
        if result.is_err() {
            state.running.store(false, Ordering::SeqCst);
        } else {
            state.space.refresh();
            state.popups.cleanup();
            display_handle.flush_clients().unwrap();
        }
    }
}

impl CrucibleState<UdevData> {
    /// Opens a DRM device, registers its VBlank source, and adds it to the GPU manager.
    fn device_added(&mut self, node: DrmNode, path: &Path) -> Result<(), DeviceAddError> {
        let fd = self
            .backend_data
            .session
            .open(
                path,
                OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
            )
            .map_err(DeviceAddError::DeviceOpen)?;
        let fd = DrmDeviceFd::new(DeviceFd::from(fd));

        let (drm, notifier) =
            DrmDevice::new(fd.clone(), true).map_err(DeviceAddError::DrmDevice)?;
        let gbm = GbmDevice::new(fd).map_err(DeviceAddError::GbmDevice)?;

        // Drive page-flip completion (VBlank) for every CRTC on this device.
        let registration_token = self
            .handle
            .insert_source(notifier, move |event, metadata, data| match event {
                DrmEvent::VBlank(crtc) => {
                    data.frame_finish(node, crtc, metadata);
                }
                DrmEvent::Error(error) => {
                    error!("{:?}", error);
                }
            })
            .unwrap();

        // Add this device's render node to the GPU manager (falls back to the primary on failure).
        let render_node = (|| {
            // SAFETY: `gbm` is a live GBM device owned for this DRM node's lifetime; the EGL display
            // borrows it and is dropped before the device is.
            #[allow(unsafe_code)]
            let display = unsafe { EGLDisplay::new(gbm.clone()).map_err(DeviceAddError::AddNode)? };
            let egl_device =
                EGLDevice::device_for_display(&display).map_err(DeviceAddError::AddNode)?;
            let render_node = egl_device
                .try_get_render_node()
                .ok()
                .flatten()
                .unwrap_or(node);
            self.backend_data
                .gpus
                .as_mut()
                .add_node(render_node, gbm.clone())
                .map_err(DeviceAddError::AddNode)?;
            Ok::<DrmNode, DeviceAddError>(render_node)
        })()
        .inspect_err(|err| warn!(?err, "failed to initialize gpu render node"))
        .ok();

        // Use this device's own allocator if it can render, else borrow the primary GPU's.
        let allocator = render_node
            .is_some()
            .then(|| {
                GbmAllocator::new(
                    gbm.clone(),
                    GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
                )
            })
            .or_else(|| {
                self.backend_data
                    .backends
                    .values()
                    .find(|backend| backend.render_node == Some(self.backend_data.primary_gpu))
                    .map(|backend| backend.drm_output_manager.allocator().clone())
            })
            .ok_or(DeviceAddError::NoAllocator)?;

        let framebuffer_exporter = GbmFramebufferExporter::new(gbm.clone(), render_node.into());

        let mut renderer = self
            .backend_data
            .gpus
            .single_renderer(&render_node.unwrap_or(self.backend_data.primary_gpu))
            .unwrap();
        // Only allow non-linear modifiers when this device has its own render node.
        let render_formats = renderer
            .as_mut()
            .egl_context()
            .dmabuf_render_formats()
            .iter()
            .filter(|format| render_node.is_some() || format.modifier == Modifier::Linear)
            .copied()
            .collect::<FormatSet>();
        drop(renderer);

        let drm_output_manager = DrmOutputManager::new(
            drm,
            allocator,
            framebuffer_exporter,
            Some(gbm),
            SUPPORTED_FORMATS.iter().copied(),
            render_formats,
        );

        self.backend_data.backends.insert(
            node,
            BackendData {
                registration_token,
                drm_output_manager,
                drm_scanner: DrmScanner::new(),
                render_node,
                surfaces: HashMap::new(),
            },
        );

        self.device_changed(node);
        Ok(())
    }

    /// Re-scans a device's connectors and creates/destroys outputs for connect/disconnect events.
    fn device_changed(&mut self, node: DrmNode) {
        let Some(device) = self.backend_data.backends.get_mut(&node) else {
            return;
        };

        let scan_result = match device
            .drm_scanner
            .scan_connectors(device.drm_output_manager.device())
        {
            Ok(result) => result,
            Err(err) => {
                warn!(?err, "Failed to scan connectors");
                return;
            }
        };

        for event in scan_result {
            match event {
                DrmScanEvent::Connected {
                    connector,
                    crtc: Some(crtc),
                } => self.connector_connected(node, connector, crtc),
                DrmScanEvent::Disconnected {
                    connector,
                    crtc: Some(crtc),
                } => self.connector_disconnected(node, connector, crtc),
                _ => {}
            }
        }

        // Re-lay all outputs left-to-right and re-arrange layer maps.
        fixup_output_positions(self);
    }

    /// Sets up a newly connected connector as a mapped [`Output`] and kicks off its first frame.
    fn connector_connected(
        &mut self,
        node: DrmNode,
        connector: connector::Info,
        crtc: crtc::Handle,
    ) {
        let Some(device) = self.backend_data.backends.get_mut(&node) else {
            return;
        };
        let render_node = device.render_node.unwrap_or(self.backend_data.primary_gpu);
        let mut renderer = self
            .backend_data
            .gpus
            .single_renderer(&render_node)
            .unwrap();

        let output_name = format!(
            "{}-{}",
            connector.interface().as_str(),
            connector.interface_id()
        );
        info!(?crtc, "Setting up connector {}", output_name);

        let drm_device = device.drm_output_manager.device();
        let display_info = display_info::for_connector(drm_device, connector.handle());
        let make = display_info
            .as_ref()
            .and_then(|info| info.make())
            .unwrap_or_else(|| "Unknown".into());
        let model = display_info
            .as_ref()
            .and_then(|info| info.model())
            .unwrap_or_else(|| "Unknown".into());
        let serial_number = display_info
            .as_ref()
            .and_then(|info| info.serial())
            .unwrap_or_else(|| "Unknown".into());

        // Pick the connector's preferred mode (else the first one).
        let mode_id = connector
            .modes()
            .iter()
            .position(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
            .unwrap_or(0);
        let drm_mode = connector.modes()[mode_id];
        let wl_mode = WlMode::from(drm_mode);

        let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
        let output = Output::new(
            output_name,
            PhysicalProperties {
                size: (phys_w as i32, phys_h as i32).into(),
                subpixel: connector.subpixel().into(),
                make,
                model,
                serial_number,
            },
        );
        let global = output.create_global::<CrucibleState<UdevData>>(&self.display_handle);

        // Lay this output to the right of all existing ones.
        let existing_widths: Vec<i32> = self
            .space
            .outputs()
            .map(|o| self.space.output_geometry(o).map(|g| g.size.w).unwrap_or(0))
            .collect();
        let position = next_output_position(existing_widths);

        output.set_preferred(wl_mode);
        output.change_current_state(Some(wl_mode), None, None, Some(position.into()));
        self.space.map_output(&output, position);
        output.user_data().insert_if_missing(|| UdevOutputId {
            crtc,
            device_id: node,
        });

        let planes = match drm_device.planes(&crtc) {
            Ok(planes) => planes,
            Err(err) => {
                warn!("Failed to query crtc planes: {}", err);
                return;
            }
        };

        let drm_output = match device
            .drm_output_manager
            .lock()
            .initialize_output::<_, OutputElements<'_>>(
                crtc,
                drm_mode,
                &[connector.handle()],
                &output,
                Some(planes),
                &mut renderer,
                &DrmOutputRenderElements::default(),
            ) {
            Ok(drm_output) => drm_output,
            Err(err) => {
                warn!("Failed to initialize drm output: {}", err);
                return;
            }
        };

        let surface = SurfaceData {
            dh: self.display_handle.clone(),
            output,
            global: Some(global),
            drm_output,
        };
        device.surfaces.insert(crtc, surface);

        // Kick off rendering for the new output.
        self.handle
            .insert_idle(move |state| state.render_surface(node, crtc));
    }

    /// Tears down the output for a disconnected connector.
    fn connector_disconnected(
        &mut self,
        node: DrmNode,
        _connector: connector::Info,
        crtc: crtc::Handle,
    ) {
        let Some(device) = self.backend_data.backends.get_mut(&node) else {
            return;
        };
        if let Some(surface) = device.surfaces.remove(&crtc) {
            self.space.unmap_output(&surface.output);
            self.space.refresh();
        }
    }

    /// Drops all state for a removed DRM device (GPU unplugged or session torn down).
    fn device_removed(&mut self, node: DrmNode) {
        let Some(device) = self.backend_data.backends.get_mut(&node) else {
            return;
        };
        let crtcs: Vec<_> = device
            .drm_scanner
            .crtcs()
            .map(|(info, crtc)| (info.clone(), crtc))
            .collect();
        for (connector, crtc) in crtcs {
            self.connector_disconnected(node, connector, crtc);
        }

        if let Some(backend_data) = self.backend_data.backends.remove(&node) {
            if let Some(render_node) = backend_data.render_node {
                self.backend_data.gpus.as_mut().remove_node(&render_node);
            }
            self.handle.remove(backend_data.registration_token);
            debug!("Dropped device {}", node);
        }

        fixup_output_positions(self);
    }

    /// Handles a completed page-flip (VBlank): delivers presentation feedback and schedules the next
    /// frame.
    fn frame_finish(
        &mut self,
        dev_id: DrmNode,
        crtc: crtc::Handle,
        metadata: &mut Option<DrmEventMetadata>,
    ) {
        let Some(device_backend) = self.backend_data.backends.get_mut(&dev_id) else {
            error!("Trying to finish frame on non-existent backend {}", dev_id);
            return;
        };
        let Some(surface) = device_backend.surfaces.get_mut(&crtc) else {
            error!("Trying to finish frame on non-existent crtc {:?}", crtc);
            return;
        };

        let Some(output) = self.space.outputs().find(|o| {
            o.user_data().get::<UdevOutputId>()
                == Some(&UdevOutputId {
                    device_id: dev_id,
                    crtc,
                })
        }) else {
            return;
        };
        let output = output.clone();

        let Some(frame_duration) = output
            .current_mode()
            .map(|mode| Duration::from_secs_f64(1_000f64 / mode.refresh as f64))
        else {
            return;
        };

        // Derive the presentation clock + flags from the VBlank metadata.
        let tp = metadata.as_ref().and_then(|metadata| match metadata.time {
            smithay::backend::drm::DrmEventTime::Monotonic(tp) => (!tp.is_zero()).then_some(tp),
            smithay::backend::drm::DrmEventTime::Realtime(_) => None,
        });
        let seq = metadata.as_ref().map(|m| m.sequence).unwrap_or(0);
        let (clock, flags) = if let Some(tp) = tp {
            (
                tp.into(),
                wp_presentation_feedback::Kind::Vsync
                    | wp_presentation_feedback::Kind::HwClock
                    | wp_presentation_feedback::Kind::HwCompletion,
            )
        } else {
            (self.clock.now(), wp_presentation_feedback::Kind::Vsync)
        };

        let submit_result = surface
            .drm_output
            .frame_submitted()
            .map_err(Into::<SwapBuffersError>::into);

        let schedule_render = match submit_result {
            Ok(user_data) => {
                if let Some(mut feedback) = user_data.flatten() {
                    feedback.presented(clock, Refresh::fixed(frame_duration), seq as u64, flags);
                }
                true
            }
            Err(err) => {
                warn!("Error during frame submission: {:?}", err);
                match err {
                    SwapBuffersError::AlreadySwapped => true,
                    // A deactivated device will be re-kicked on session resume; don't reschedule.
                    SwapBuffersError::TemporaryFailure(err) => !matches!(
                        err.downcast_ref::<DrmError>(),
                        Some(&DrmError::DeviceInactive)
                    ),
                    SwapBuffersError::ContextLost(err) => panic!("Rendering loop lost: {err}"),
                }
            }
        };

        if schedule_render {
            // Schedule the next repaint roughly one frame out.
            let timer = Timer::from_duration(frame_duration);
            self.handle
                .insert_source(timer, move |_, _, data| {
                    data.render(dev_id, Some(crtc));
                    TimeoutAction::Drop
                })
                .expect("failed to schedule frame timer");
        }
    }

    /// Renders one CRTC (when `crtc` is `Some`) or all CRTCs on `node`.
    fn render(&mut self, node: DrmNode, crtc: Option<crtc::Handle>) {
        let Some(device_backend) = self.backend_data.backends.get(&node) else {
            error!("Trying to render on non-existent backend {}", node);
            return;
        };
        match crtc {
            Some(crtc) => self.render_surface(node, crtc),
            None => {
                let crtcs: Vec<_> = device_backend.surfaces.keys().copied().collect();
                for crtc in crtcs {
                    self.render_surface(node, crtc);
                }
            }
        }
    }

    /// Composites and submits one frame for the output on `(node, crtc)`.
    ///
    /// Reuses the backend-agnostic [`output_elements`] collection (so Wayland *and* XWayland windows
    /// render), submits the frame through the [`DrmOutput`], records the primary-scanout output for
    /// every surface, queues presentation feedback, and sends frame callbacks. Re-schedules a repaint
    /// when the frame produced no damage.
    fn render_surface(&mut self, node: DrmNode, crtc: crtc::Handle) {
        let Some(output) = self.space.outputs().find(|o| {
            o.user_data().get::<UdevOutputId>()
                == Some(&UdevOutputId {
                    device_id: node,
                    crtc,
                })
        }) else {
            return;
        };
        let output = output.clone();

        let primary_gpu = self.backend_data.primary_gpu;
        let Some(device) = self.backend_data.backends.get(&node) else {
            return;
        };
        if !device.surfaces.contains_key(&crtc) {
            return;
        }
        let render_node = device.render_node.unwrap_or(primary_gpu);

        // Reset a dead client cursor surface to the default before drawing.
        if let CursorImageStatus::Surface(ref s) = self.cursor_status {
            if !s.alive() {
                self.cursor_status = CursorImageStatus::default_named();
            }
        }
        self.backend_data
            .pointer_element
            .set_status(self.cursor_status.clone());

        let scale = Scale::from(output.current_scale().fractional_scale());
        let output_geo = self.space.output_geometry(&output).unwrap();
        let pointer_location = self.pointer.current_location();

        // Select a renderer: same-GPU is a single renderer, cross-GPU needs the scanout format.
        let format = self.backend_data.backends[&node].surfaces[&crtc]
            .drm_output
            .format();
        let mut renderer = if primary_gpu == render_node {
            self.backend_data.gpus.single_renderer(&render_node)
        } else {
            self.backend_data
                .gpus
                .renderer(&primary_gpu, &render_node, format)
        }
        .unwrap();

        // Cursor element, drawn only on the output the pointer is over.
        let mut custom_elements: Vec<CustomRenderElements<UdevRenderer<'_>>> = Vec::new();
        if output_geo.to_f64().contains(pointer_location) {
            let cursor_pos = pointer_location - output_geo.loc.to_f64();
            custom_elements = self.backend_data.pointer_element.render_elements(
                &mut renderer,
                cursor_pos.to_physical(scale).to_i32_round(),
                scale,
                1.0,
            );
        }

        let (elements, clear_color) =
            output_elements(&output, &self.space, custom_elements, &mut renderer);

        let surface = self
            .backend_data
            .backends
            .get_mut(&node)
            .unwrap()
            .surfaces
            .get_mut(&crtc)
            .unwrap();
        let result = surface
            .drm_output
            .render_frame(&mut renderer, &elements, clear_color, FrameFlags::DEFAULT)
            .map(|res| (!res.is_empty, res.states))
            .map_err(|err| match err {
                smithay::backend::drm::compositor::RenderFrameError::PrepareFrame(err) => {
                    SwapBuffersError::from(err)
                }
                smithay::backend::drm::compositor::RenderFrameError::RenderFrame(
                    OutputDamageTrackerError::Rendering(err),
                ) => SwapBuffersError::from(err),
                _ => unreachable!(),
            });

        let reschedule = match result {
            Ok((rendered, states)) => {
                update_primary_scanout_output(&self.space, &output, &states);
                if rendered {
                    let feedback = take_presentation_feedback(&output, &self.space, &states);
                    if let Err(err) = surface.drm_output.queue_frame(Some(feedback)) {
                        warn!("Failed to queue frame: {:?}", err);
                    }
                }
                // Send frame callbacks so clients draw their next frame.
                let now = self.clock.now();
                let throttle = Some(Duration::from_secs(1));
                self.space.elements().for_each(|window| {
                    window.send_frame(&output, now, throttle, |_, _| Some(output.clone()));
                });

                !rendered
            }
            Err(err) => {
                warn!("Error during rendering: {:?}", err);
                match err {
                    SwapBuffersError::AlreadySwapped => false,
                    // Retry on a deactivated device (session will resume it) or a transient
                    // permission-denied access (e.g. mid VT-switch); give up on anything else.
                    SwapBuffersError::TemporaryFailure(err) => {
                        match err.downcast_ref::<DrmError>() {
                            Some(DrmError::DeviceInactive) => true,
                            Some(DrmError::Access(DrmAccessError { source, .. })) => {
                                source.kind() == io::ErrorKind::PermissionDenied
                            }
                            _ => false,
                        }
                    }
                    SwapBuffersError::ContextLost(err) => panic!("Rendering loop lost: {err}"),
                }
            }
        };

        if reschedule {
            if let Some(mode) = output.current_mode() {
                let refresh = Duration::from_millis(1_000_000 / mode.refresh as u64);
                let timer = Timer::from_duration(refresh);
                self.handle
                    .insert_source(timer, move |_, _, data| {
                        data.render(node, Some(crtc));
                        TimeoutAction::Drop
                    })
                    .expect("failed to schedule frame timer");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::next_output_position;

    #[test]
    fn first_output_starts_at_origin() {
        // No existing outputs -> the first output is placed at (0, 0).
        assert_eq!(next_output_position(std::iter::empty()), (0, 0));
    }

    #[test]
    fn outputs_lay_out_left_to_right() {
        // A second output goes immediately to the right of a 1920-wide first output.
        assert_eq!(next_output_position([1920]), (1920, 0));
        // A third output's x is the sum of the two to its left.
        assert_eq!(next_output_position([1920, 1280]), (3200, 0));
    }

    #[test]
    fn always_aligns_to_top_row() {
        // Outputs are stacked horizontally at y = 0 regardless of widths.
        assert_eq!(next_output_position([800, 600, 1024]).1, 0);
    }
}
