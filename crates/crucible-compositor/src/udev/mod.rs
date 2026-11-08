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
//! Compared to anvil this backend omits `drm_syncobj`, DRM leasing, the on-screen FPS overlay and the
//! multi-GPU pointer-texture cache (the DnD icon is composed by [`crate::render::frame_elements`]); CDE draws its self-contained default cursor
//! via [`Cursor`](crate::cursor::Cursor) just like the winit path.
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
        drm::control::{Device as ControlDevice, crtc},
        input::{DeviceCapability, Libinput},
        wayland_server::{Display, protocol::wl_surface::WlSurface},
    },
    wayland::dmabuf::{
        DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier,
    },
};
use tracing::{debug, error, info, warn};

use crate::{
    element::WindowElement,
    night_light::{Decision, model::ramp},
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
    gpus: GpuManager<UdevGles>,
    /// Per-DRM-device state, keyed by the device's [`DrmNode`].
    backends: HashMap<DrmNode, BackendData>,
    /// Keyboards currently present, tracked so LED state can be pushed to them.
    keyboards: Vec<smithay::reexports::input::Device>,
    /// The `zwp_linux_dmabuf_v1` state and global; `None` until the primary renderer exists (or
    /// when it could not be created).
    dmabuf_state: Option<(DmabufState, DmabufGlobal)>,
}

type UdevGles = GbmGlesBackend<GlesRenderer, DrmDeviceFd>;

impl UdevData {
    /// The renderer for a device whose render node is `render_node`: the same-GPU single renderer
    /// when it is the primary GPU, otherwise the cross-GPU one (which needs the scanout `format`).
    ///
    /// An associated function over the `gpus` field (not `&mut self`) so the caller keeps its
    /// split borrow of `backend_data.backends` beside the renderer.
    pub(super) fn renderer_for<'a>(
        gpus: &'a mut GpuManager<UdevGles>,
        primary_gpu: DrmNode,
        render_node: DrmNode,
        format: smithay::backend::allocator::Fourcc,
    ) -> Result<UdevRenderer<'a>, smithay::backend::renderer::multigpu::Error<UdevGles, UdevGles>>
    {
        if primary_gpu == render_node {
            gpus.single_renderer(&render_node)
        } else {
            gpus.renderer(&primary_gpu, &render_node, format)
        }
    }

    /// Every `(node, crtc)` that has an output surface.
    pub(super) fn crtcs(&self) -> Vec<(DrmNode, crtc::Handle)> {
        self.backends
            .iter()
            .flat_map(|(node, gpu)| gpu.surfaces.keys().map(move |crtc| (*node, *crtc)))
            .collect()
    }

    /// Powers one output off (`DrmCompositor::clear`, DPMS off) and marks it off. A failure (e.g. the
    /// session is paused) is logged and leaves the output unmarked, so it keeps rendering. Returns
    /// whether the output is now off.
    pub(super) fn power_off_surface(&mut self, node: DrmNode, crtc: crtc::Handle) -> bool {
        let Some(surface) = self
            .backends
            .get_mut(&node)
            .and_then(|gpu| gpu.surfaces.get_mut(&crtc))
        else {
            return false;
        };
        if surface.power.is_off() {
            return true;
        }
        match surface.drm_output.with_compositor(|c| c.clear()) {
            Ok(()) => {
                surface.power.powered_off();
                true
            }
            Err(err) => {
                warn!(%node, ?crtc, %err, "idle: DPMS off failed; output keeps rendering");
                false
            }
        }
    }
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
        crate::dmabuf::notify_import::<Self>(imported, &dmabuf, notifier, |dmabuf| {
            dmabuf.set_node(primary_gpu);
        });
    }
}

/// Sends the `kelvin` ramp to `crtc`, sized from its gamma LUT; a CRTC without one is left alone.
fn write_gamma(
    device: &impl ControlDevice,
    crtc: crtc::Handle,
    kelvin: u32,
) -> std::io::Result<()> {
    let len = device.get_crtc(crtc)?.gamma_length() as usize;
    if len == 0 {
        debug!(?crtc, "night light: CRTC has no gamma LUT");
        return Ok(());
    }
    let (red, green, blue) = ramp(kelvin, len);
    device.set_gamma(crtc, &red, &green, &blue)
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

    fn capture_output(
        state: &mut CrucibleState<Self>,
        output: &Output,
    ) -> Result<crate::capture::Capture, crate::capture::CaptureError> {
        state.capture_surface(output)
    }

    fn output_powered_off(&self, output: &Output) -> bool {
        output
            .user_data()
            .get::<UdevOutputId>()
            .and_then(|id| self.backends.get(&id.device_id)?.surfaces.get(&id.crtc))
            .is_some_and(|surface| surface.power.is_off())
    }

    /// Off: `DrmCompositor::clear()` (DPMS off) on every output; a failure leaves that output
    /// rendering. On: clears every off mark and schedules exactly one full-redraw render per
    /// output that was off; its `queue_frame` re-enables the CRTC.
    fn set_outputs_powered(state: &mut CrucibleState<Self>, on: bool) {
        let crtcs = state.backend_data.crtcs();
        if on {
            for (node, crtc) in crtcs {
                let Some(surface) = state
                    .backend_data
                    .backends
                    .get_mut(&node)
                    .and_then(|gpu| gpu.surfaces.get_mut(&crtc))
                else {
                    continue;
                };
                if surface.power.wake() {
                    surface.drm_output.reset_buffers();
                    state
                        .handle
                        .insert_idle(move |data| data.render_surface(node, crtc));
                }
            }
            info!("idle: outputs powered on");
        } else {
            for (node, crtc) in crtcs {
                state.backend_data.power_off_surface(node, crtc);
            }
            info!("idle: outputs powered off");
        }
    }

    /// Writes the night-light ramp to every CRTC whose cache entry says it is needed. A paused
    /// device is skipped and left dirty (drm-rs calls bypass Smithay's pause state); a failed write
    /// warns once per CRTC per failure streak and is retried on the next evaluation.
    fn apply_night_light(state: &mut CrucibleState<Self>, kelvin: u32) {
        let enabled = state.night_light_enabled();
        let cache = &mut state.night_light.cache;
        for (node, gpu) in &state.backend_data.backends {
            let device = gpu.drm_output_manager.device();
            let active = device.is_active();
            for crtc in gpu.surfaces.keys() {
                let key = (*node, *crtc);
                if cache.decide(&key, kelvin, active, enabled) == Decision::Skip {
                    continue;
                }
                match write_gamma(device, *crtc, kelvin) {
                    Ok(()) => cache.record_success(key, kelvin),
                    Err(err) => {
                        if cache.record_failure(key) {
                            warn!(%err, ?crtc, kelvin, "night light: gamma write failed; will retry");
                        }
                    }
                }
            }
        }
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
        keyboards: Vec::new(),
        dmabuf_state: None,
    };
    let startup_config = crate::config::load_startup_config();
    let mut state = CrucibleState::init(
        display,
        event_loop.handle(),
        data,
        true,
        &startup_config.keyboard,
    );
    state.apply_config(startup_config);
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
                data.night_light.cache.invalidate_all();
                if let Err(err) = libinput_context.resume() {
                    error!("Failed to resume libinput context: {:?}", err);
                }
                let nodes: Vec<DrmNode> = data.backend_data.backends.keys().copied().collect();
                let was_off: Vec<(DrmNode, crtc::Handle)> = data
                    .backend_data
                    .crtcs()
                    .into_iter()
                    .filter(|(node, crtc)| {
                        data.backend_data.backends[node].surfaces[crtc]
                            .power
                            .is_off()
                    })
                    .collect();
                for node in &nodes {
                    if let Some(backend) = data.backend_data.backends.get_mut(node) {
                        if let Err(err) = backend.drm_output_manager.lock().activate(false) {
                            error!(%err, "Failed to re-activate drm device on session resume");
                        }
                    }
                }
                // Returning to the VT is user activity: restart the idle period and, if blanked,
                // wake. The wake path schedules the render of every output that was off, so only
                // the outputs that stayed on are re-kicked here (never two renders per CRTC).
                data.idle_activity();
                data.evaluate_night_light();
                for (node, crtc) in data.backend_data.crtcs() {
                    if !was_off.contains(&(node, crtc)) {
                        data.handle
                            .insert_idle(move |data| data.render_surface(node, crtc));
                    }
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
                    state.backend_data.dmabuf_state =
                        Some(crate::dmabuf::create_global::<CrucibleState<UdevData>>(
                            &display_handle,
                            // v5 takes its formats from the default feedback.
                            std::iter::empty(),
                            Some(&default_feedback),
                        ));
                }
                Err(err) => {
                    warn!(%err, "Failed to build dmabuf default feedback; running without linux-dmabuf")
                }
            }
        }
        Err(err) => warn!(%err, "No primary renderer; running without linux-dmabuf"),
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
    // The session (restore / cold-start picker / warm overlay host) starts at the first XWayland
    // outcome so its X11 spawns get a real `DISPLAY`; see `CrucibleState::on_xwayland_outcome`.
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
