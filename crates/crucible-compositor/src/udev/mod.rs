//! `udev`/DRM/KMS backend for running CDE on a real TTY across multiple outputs.
//!
//! This is the production backend, adapted from Smithay's `anvil` reference compositor and trimmed
//! to what CDE needs. It brings up:
//!
//! 1. **Session + devices** — a [`LibSeatSession`] (so CDE can open DRM/input fds without root) and a
//!    [`UdevBackend`] that enumerates GPUs and hotplug events. Each DRM device gets a
//!    [`DrmOutputManager`](smithay::backend::drm::output::DrmOutputManager) (GBM allocator +
//!    framebuffer exporter) and is registered for VBlank page-flip events. A [`GpuManager`]
//!    (`renderer_multi`) provides EGL/GLES rendering with multi-GPU fallback to the primary GPU.
//! 2. **Connectors → outputs** — [`DrmScanner::scan_connectors`](smithay_drm_extras::drm_scanner::DrmScanner)
//!    detects connect/disconnect; each connected connector becomes a Smithay [`Output`] mapped into
//!    the **shared** `Space` left-to-right, and layer maps are re-`arrange`d on every change.
//! 3. **Per-output rendering** — each output renders on its own VBlank through the `DrmOutput`,
//!    reusing the backend-agnostic render-element collection in [`crate::render`] so Wayland *and*
//!    XWayland windows appear on every output. Presentation feedback is delivered after each
//!    page-flip.
//! 4. **Input** — a [`LibinputInputBackend`] bound to the session feeds the **same** seat handlers
//!    the winit backend uses (see [`CrucibleState::process_input_event_libinput`]).
//! 5. **VT switching** — [`SessionEvent::PauseSession`]/[`SessionEvent::ActivateSession`] suspend and
//!    resume libinput and the DRM devices so `Ctrl+Alt+F<n>` VT switches work.
//!
//! Selected at runtime via `--tty-udev`. `zwp_linux_dmabuf_v1` is advertised with default feedback
//! from the primary GPU plus per-output render/scanout feedback (see [`output`] and [`render`]).
//! Compared to anvil this backend omits `drm_syncobj`, DRM leasing, the on-screen FPS overlay, DnD-icon
//! rendering, and the multi-GPU pointer-texture cache; CDE draws its self-contained default cursor
//! via [`PointerElement`](crate::drawing::PointerElement) just like the winit path.
//!
//! # Module map
//!
//! Each lifecycle has one owner, and they only ever call *downward*:
//!
//! - this module — session bring-up, the calloop wiring for libinput/udev/VT events, [`UdevData`],
//!   and the [`Backend`] impl;
//! - [`device`] — one DRM device's lifetime (open, GPU manager entry, VBlank source, rescan) plus
//!   both `unsafe` EGL/GLES constructions;
//! - [`output`] — connector ↔ [`Output`] lifecycle, the per-CRTC surface, and the execution of
//!   transition plans against the `Space` and the layout model;
//! - [`plan`] — the pure placement/transition rules, unit-tested with no DRM device present;
//! - [`render`] — compositing, frame submission, and VBlank-driven repaint scheduling.

mod device;
mod output;
mod plan;
mod render;

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::Duration;

use smithay::{
    backend::{
        allocator::dmabuf::Dmabuf,
        drm::{DrmDeviceFd, DrmNode, NodeType},
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            ImportDma, ImportMemWl,
            element::AsRenderElements,
            gles::GlesRenderer,
            multigpu::{GpuManager, MultiRenderer, gbm::GbmGlesBackend},
        },
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{UdevBackend, UdevEvent},
    },
    input::keyboard::LedState,
    output::Output,
    reexports::{
        calloop::EventLoop,
        input::{DeviceCapability, Libinput},
        wayland_server::{Display, protocol::wl_surface::WlSurface},
    },
    wayland::dmabuf::{
        DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier,
    },
};
use tracing::{error, info, warn};

use crate::{
    drawing::PointerElement,
    element::WindowElement,
    render::OutputRenderElements,
    state::{Backend, CrucibleState},
};

use self::device::{BackendData, DeviceAddError};
use self::output::UdevOutputId;

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
/// Matches the `(R, E)` pair [`crate::render::frame_elements`] yields for the udev renderer, so the same window
/// + cursor elements used by the winit path drive the DRM compositor.
type OutputElements<'a> = OutputRenderElements<
    UdevRenderer<'a>,
    <WindowElement as AsRenderElements<UdevRenderer<'a>>>::RenderElement,
>;

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
    /// The `zwp_linux_dmabuf_v1` state and global; `None` until the primary renderer exists (or
    /// when it could not be created).
    dmabuf_state: Option<(DmabufState, DmabufGlobal)>,
}

impl DmabufHandler for CrucibleState<UdevData> {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        // The global only exists once `dmabuf_state` is `Some`, so no client can reach this handler
        // before it is filled in.
        &mut self
            .backend_data
            .dmabuf_state
            .as_mut()
            .expect("dmabuf requests only arrive after the global is created")
            .0
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        let primary_gpu = self.backend_data.primary_gpu;
        // Import directly through the primary GPU's own GLES renderer (not the `MultiRenderer`
        // import path, which may succeed on another GPU and record that node on the dmabuf), so
        // `set_node(primary_gpu)` below is always true.
        let imported = match self.backend_data.gpus.single_renderer(&primary_gpu) {
            Ok(mut renderer) => AsMut::<GlesRenderer>::as_mut(&mut renderer)
                .import_dmabuf(&dmabuf, None)
                .is_ok(),
            Err(_) => false,
        };
        if imported {
            dmabuf.set_node(primary_gpu);
            let _ = notifier.successful::<CrucibleState<UdevData>>();
        } else {
            notifier.failed();
        }
    }
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

    /// A rescaled output changes width, so the whole strip reflows (`apply_positions` re-arranges
    /// the layer maps, re-syncs the model rects and re-applies the layout).
    fn outputs_changed(state: &mut CrucibleState<Self>) {
        output::reflow_outputs(state);
    }

    fn change_vt(&mut self, vt: i32) -> bool {
        match self.session.change_vt(vt) {
            Ok(()) => true,
            Err(err) => {
                error!(vt, %err, "Failed to switch VT");
                warn!(
                    "VT switch: libseat's logind backend fails without setting errno, so the OS error above (e.g. EAGAIN) is unreliable; see libseat's preceding \"Could not switch session\" log line - common causes are a missing polkit daemon (logind needs it for org.freedesktop.login1.chvt as non-root) or this session not being the active session on seat0"
                );
                false
            }
        }
    }
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

    // Pick the GPU CDE renders on; an unusable `CDE_DRM_DEVICE` override is fatal.
    let Some(primary_gpu) = device::pick_primary_gpu(&session) else {
        return;
    };
    info!("Using {} as primary gpu.", primary_gpu);

    // Build the multi-GPU renderer manager (high-priority EGL contexts for the compositor).
    let gpus = match device::init_gpu_manager() {
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
        dmabuf_state: None,
    };
    let mut state = CrucibleState::init(display, event_loop.handle(), data, true);
    state.apply_config(crate::config::load_startup_config());
    // Persistence init: slot pins, IPC socket, 1 s auto-save/config tick.
    state.start_persistence();

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

    // Advertise linux-dmabuf with the primary GPU's formats. This must exist before any client can
    // connect (`start_session` may spawn restored apps; `start_xwayland` follows). Per-output
    // feedback is computed independently at connect time.
    match state.backend_data.gpus.single_renderer(&primary_gpu) {
        Ok(renderer) => {
            let formats = renderer.dmabuf_formats();
            match DmabufFeedbackBuilder::new(primary_gpu.dev_id(), formats).build() {
                Ok(default_feedback) => {
                    let mut dmabuf_state = DmabufState::new();
                    let global = dmabuf_state
                        .create_global_with_default_feedback::<CrucibleState<UdevData>>(
                            &display_handle,
                            &default_feedback,
                        );
                    state.backend_data.dmabuf_state = Some((dmabuf_state, global));
                }
                Err(err) => {
                    warn!(%err, "Failed to build dmabuf default feedback; running without linux-dmabuf")
                }
            }
        }
        Err(err) => warn!(%err, "No primary renderer; running without linux-dmabuf"),
    }

    // The initial connectors registered their outputs synchronously above (`device_added` ->
    // `device_changed` -> `connector_connected` -> `register_output`): restore the last session or
    // open the cold-start picker now, before hotplug is driven.
    state.start_session();

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
