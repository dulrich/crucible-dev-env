//! Connector/output lifecycle for the `udev` backend: turning a connected connector into a
//! mapped [`Output`], tearing it down again, and keeping the output strip laid out.
//!
//! This module owns everything between a DRM connector and CDE's model of an output:
//! [`UdevOutputId`] (the `(device, crtc)` identity stamped into every output's user data),
//! [`SurfaceData`] (the per-CRTC swapchain and `wl_output` global), and the execution of the
//! transition plans computed by [`super::plan`]. Placement arithmetic lives in the planner; this
//! module only applies it, and it binds outputs to the model exclusively through the controller
//! methods on [`CrucibleState`] (`register_output`/`unregister_output`/`sync_output_rect`/
//! `apply_layout`) — it never reaches into `layout.world` itself.

use std::collections::HashMap;

use smithay::{
    backend::{
        allocator::{format::FormatSet, gbm::GbmAllocator},
        drm::{
            DrmDeviceFd, DrmNode, DrmSurface, Planes,
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputRenderElements},
        },
        renderer::{
            ImportDma,
            gles::GlesRenderer,
            multigpu::{GpuManager, gbm::GbmGlesBackend},
        },
    },
    desktop::{Space, utils::OutputPresentationFeedback},
    output::{Mode as WlMode, Output, PhysicalProperties},
    reexports::{
        drm::control::{self as drm_control, Device as _, ModeTypeFlags, connector, crtc},
        wayland_protocols::wp::linux_dmabuf::zv1::server::zwp_linux_dmabuf_feedback_v1::TrancheFlags,
        wayland_server::{DisplayHandle, backend::GlobalId},
    },
    wayland::dmabuf::{DmabufFeedback, DmabufFeedbackBuilder},
};
use smithay_drm_extras::display_info;
use tracing::{debug, info, warn};

use crate::{
    element::WindowElement,
    render::{frame_elements, frame_view},
    state::CrucibleState,
};

use super::plan::{self, MappedOutput, OutputPlan};
use super::{OutputElements, UdevData};

/// Identifies the [`Output`] driven by a particular `(device, crtc)` pair.
///
/// Stored in each output's `user_data` so VBlank/render callbacks can find the Smithay [`Output`]
/// that corresponds to the CRTC the event fired for.
///
/// `Copy` so it can double as the planner's opaque output key (see [`super::plan`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct UdevOutputId {
    /// The DRM device (GPU) node the output lives on.
    pub device_id: DrmNode,
    /// The CRTC scanning out this output.
    pub crtc: crtc::Handle,
}

/// The dmabuf feedback a surface on one output is sent, depending on whether it was scanned out.
#[derive(Debug, Clone)]
pub(super) struct SurfaceDmabufFeedback {
    /// Sent while the surface is composited: render-node tranche only.
    pub render_feedback: DmabufFeedback,
    /// Sent while the surface is (or nearly) scanned out: leads with the plane-format tranche.
    pub scanout_feedback: DmabufFeedback,
}

/// The formats of the scanout tranche: the plane formats that can also be rendered.
///
/// Limiting the tranche to renderable formats guarantees a composition fallback when a client
/// buffer cannot be scanned out directly.
fn scanout_tranche_formats(plane_formats: &FormatSet, render_formats: &FormatSet) -> FormatSet {
    plane_formats
        .intersection(render_formats)
        .copied()
        .collect()
}

/// Computes the render and scanout dmabuf feedback for a surface on `scanout_node`.
///
/// Mirrors anvil's `get_surface_dmabuf_feedback`; `None` when a renderer is unavailable or a
/// feedback cannot be built (the surface then keeps the global's default feedback).
fn get_surface_dmabuf_feedback(
    primary_gpu: DrmNode,
    render_node: Option<DrmNode>,
    scanout_node: DrmNode,
    gpus: &mut GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    surface: &DrmSurface,
) -> Option<SurfaceDmabufFeedback> {
    let primary_formats = gpus.single_renderer(&primary_gpu).ok()?.dmabuf_formats();
    let render_formats = match render_node {
        Some(render_node) => gpus.single_renderer(&render_node).ok()?.dmabuf_formats(),
        None => FormatSet::default(),
    };
    let all_render_formats: FormatSet = primary_formats
        .iter()
        .chain(render_formats.iter())
        .copied()
        .collect();

    let planes = surface.planes().clone();
    let plane_formats: FormatSet = surface
        .plane_info()
        .formats
        .iter()
        .copied()
        .chain(planes.overlay.into_iter().flat_map(|plane| plane.formats))
        .collect();
    let scanout_formats = scanout_tranche_formats(&plane_formats, &all_render_formats);

    let builder = DmabufFeedbackBuilder::new(primary_gpu.dev_id(), primary_formats);
    let render_feedback = match render_node {
        Some(render_node) => builder
            .clone()
            .add_preference_tranche(render_node.dev_id(), None, render_formats.clone())
            .build()
            .ok()?,
        None => builder.clone().build().ok()?,
    };
    let scanout_feedback = builder
        .add_preference_tranche(
            surface.device_fd().dev_id().ok()?,
            Some(TrancheFlags::Scanout),
            scanout_formats,
        )
        .add_preference_tranche(scanout_node.dev_id(), None, render_formats)
        .build()
        .ok()?;

    Some(SurfaceDmabufFeedback {
        render_feedback,
        scanout_feedback,
    })
}

/// One mapped output (connector/CRTC) on a DRM device.
///
/// Holds the Smithay [`Output`], its `wl_output` global, and the [`DrmOutput`] that owns the
/// per-output swapchain and page-flip scheduling.
pub(super) struct SurfaceData {
    /// The output's `wl_output` global and leave-all cleanup (handed over from the
    /// [`PendingAdoption`]; declared first so it runs before the swapchain is dropped).
    _global: OutputGlobal,
    /// The Smithay output mapped into the shared [`Space`].
    output: Output,
    /// The DRM compositor for this CRTC: owns the GBM swapchain and frame submission.
    pub drm_output: DrmOutput<
        GbmAllocator<DrmDeviceFd>,
        GbmFramebufferExporter<DrmDeviceFd>,
        Option<OutputPresentationFeedback>,
        DrmDeviceFd,
    >,
    /// Per-output dmabuf feedback sent after each frame; `None` keeps the default feedback.
    pub dmabuf_feedback: Option<SurfaceDmabufFeedback>,
    /// Idle power state (DPMS off via `DrmCompositor::clear`).
    pub power: PowerMark,
    /// Once-flags for the `startup:` timing lines (first frame queued, first flip completed).
    pub first_frame: crate::startup::FirstOnly,
    pub first_flip: crate::startup::FirstOnly,
}

/// What [`CrucibleState::frame_finish`] does with a page-flip completion, given the power mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FlipAction {
    /// Normal: deliver feedback and schedule the next repaint.
    Schedule,
    /// Deliver nothing more and schedule nothing (a stale flip from before a
    /// power-off arriving after the wake render already runs its own chain).
    Drop,
}

/// Per-output idle power bookkeeping, kept Smithay-free so it is unit-testable.
///
/// `off` is set only when `clear()` succeeded. `stale_flip` covers the page-flip that may have
/// been pending at `clear()` time: its completion can still arrive after a wake, and must not
/// start a second render chain beside the wake render's.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct PowerMark {
    off: bool,
    stale_flip: bool,
}

impl PowerMark {
    /// Whether the output is powered off (render nothing).
    pub(super) fn is_off(self) -> bool {
        self.off
    }

    /// Records a successful `clear()` (DPMS off).
    pub(super) fn powered_off(&mut self) {
        self.off = true;
        self.stale_flip = true;
    }

    /// Wakes the output; returns whether it was off (so the caller schedules exactly one render).
    pub(super) fn wake(&mut self) -> bool {
        std::mem::take(&mut self.off)
    }

    /// Decides a page-flip completion of an output that is on. `had_frame` is whether the
    /// compositor still had a pending frame for it (`frame_submitted` returned `Some`).
    pub(super) fn on_flip(&mut self, had_frame: bool) -> FlipAction {
        // The handler returns before `frame_submitted` while off, so `on_flip` only sees live flips.
        debug_assert!(
            !self.off,
            "on_flip while off: the handler drops the flip first"
        );
        if std::mem::take(&mut self.stale_flip) && !had_frame {
            return FlipAction::Drop;
        }
        FlipAction::Schedule
    }
}

/// The cleanup a mapped output owes on teardown: every surface leaves it and its `wl_output`
/// global is removed (plan 44 invariant 6).
///
/// Owned first by the output's [`PendingAdoption`] and then, from its adoption on, by its
/// [`SurfaceData`], so an output torn down before adoption still loses its global.
pub(super) struct OutputGlobal {
    /// Display handle, used to remove the global on drop.
    dh: DisplayHandle,
    /// The output every surface leaves on drop.
    output: Output,
    /// The `wl_output` global advertised to clients.
    global: Option<GlobalId>,
}

impl Drop for OutputGlobal {
    fn drop(&mut self) {
        self.output.leave_all();
        if let Some(global) = self.global.take() {
            self.dh.remove_global::<CrucibleState<UdevData>>(global);
        }
    }
}

/// A start-up output whose DRM adoption (its first commit) waits for its wallpaper (plan 44).
///
/// The output is already mapped, registered and advertised; this record holds what the deferred
/// `initialize_output` needs and owns the output's [`OutputGlobal`] until adoption.
pub(super) struct PendingAdoption {
    global: OutputGlobal,
    output: Output,
    connector: connector::Handle,
    mode: drm_control::Mode,
    planes: Planes,
}

/// One mapped output's backend record: adopted (it has a swapchain) or still pending adoption.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Mapped<S, P> {
    Adopted(S),
    Pending(P),
}

/// Takes `key`'s record out of whichever map holds it (an output is in at most one of them).
pub(super) fn take_mapped<K: Eq + std::hash::Hash, S, P>(
    surfaces: &mut HashMap<K, S>,
    pending: &mut HashMap<K, P>,
    key: &K,
) -> Option<Mapped<S, P>> {
    surfaces
        .remove(key)
        .map(Mapped::Adopted)
        .or_else(|| pending.remove(key).map(Mapped::Pending))
}

/// The Space's outputs as the planner sees them, in Space order.
///
/// Every output this backend maps carries a [`UdevOutputId`] (it is stamped in
/// [`CrucibleState::connector_connected`] before the output is mapped), so an output without one
/// is not ours and is left out of the strip.
pub(super) fn output_strip(space: &Space<WindowElement>) -> Vec<MappedOutput<UdevOutputId>> {
    space
        .outputs()
        .filter_map(|output| {
            let key = *output.user_data().get::<UdevOutputId>()?;
            let geometry = space.output_geometry(output);
            Some(MappedOutput {
                key,
                width: geometry.map(|geo| geo.size.w).unwrap_or(0),
                position: geometry.map(|geo| (geo.loc.x, geo.loc.y)).unwrap_or((0, 0)),
            })
        })
        .collect()
}

/// Looks up the mapped [`Output`] driven by `key`.
pub(super) fn output_for(space: &Space<WindowElement>, key: UdevOutputId) -> Option<Output> {
    space
        .outputs()
        .find(|output| output.user_data().get::<UdevOutputId>() == Some(&key))
        .cloned()
}

/// Executes a transition plan: drops the outputs it removes, then re-lays the survivors.
///
/// This is the whole-transition entry point (a device removal); a single connector disconnect
/// inside a device rescan removes without reflowing, because the rescan reflows once at the end.
pub(super) fn apply_plan(state: &mut CrucibleState<UdevData>, plan: &OutputPlan<UdevOutputId>) {
    if plan.is_empty() {
        debug!("Output transition touches no mapped output");
    }
    for key in &plan.remove {
        drop_mapped_output(state, *key);
    }
    apply_positions(state, plan);
}

/// Re-lays every mapped output left-to-right and re-arranges each output's layer map.
///
/// Called after a device rescan so the outputs form a contiguous left-to-right strip with no gaps
/// or overlaps (anvil's `fixup_positions`); a mode change can resize an output under us, which
/// moves everything to its right. The placement rule itself lives in [`plan::plan_reflow`].
pub(super) fn reflow_outputs(state: &mut CrucibleState<UdevData>) {
    let plan = plan::plan_reflow(&output_strip(&state.space));
    apply_positions(state, &plan);
}

/// Maps every output in `plan.positions` at its planned origin and re-asserts the projection.
///
/// Every planned position is applied, not just the ones [`OutputPlan::survivor_reflow`] reports as
/// moved: re-mapping an output that stayed put is idempotent, and its layer map still has to be
/// re-arranged in case the output's *size* changed.
fn apply_positions(state: &mut CrucibleState<UdevData>, plan: &OutputPlan<UdevOutputId>) {
    if !plan.survivor_reflow.is_empty() {
        debug!(
            reflowed = plan.survivor_reflow.len(),
            "Output strip changed width; shifting the outputs to the right of it"
        );
    }
    for placement in &plan.positions {
        let Some(output) = output_for(&state.space, placement.key) else {
            continue;
        };
        state.space.map_output(&output, placement.position);
        output.change_current_state(None, None, None, Some(placement.position.into()));
        // Re-arrange the layer map and keep the model's stored rect origin/size in sync with the
        // new Space position.
        state.relayout_output(&output);
    }
    // Output positions/sizes changed: re-assert the model-driven projection (idempotent).
    state.apply_layout();
}

/// Tears down one mapped output: migrates its model workspaces onto a survivor, then unmaps it.
///
/// Every mapped output has either a [`SurfaceData`] or a [`PendingAdoption`] (an output whose DRM
/// output failed to initialize is unmapped at that point), so a plan's removal set and the
/// device's two maps agree. Dropping the record runs the output's [`OutputGlobal`] cleanup.
fn drop_mapped_output(state: &mut CrucibleState<UdevData>, key: UdevOutputId) {
    let Some(device) = state.backend_data.backends.get_mut(&key.device_id) else {
        return;
    };
    let Some(record) = take_mapped(&mut device.surfaces, &mut device.pending, &key.crtc) else {
        return;
    };
    let output = match &record {
        Mapped::Adopted(surface) => surface.output.clone(),
        Mapped::Pending(pending) => {
            info!(output = %pending.output.name(), "output removed before its adoption");
            pending.output.clone()
        }
    };
    state.night_light.cache.remove(&(key.device_id, key.crtc));
    // Migrate this output's model workspaces (and their windows) onto a survivor before
    // unmapping the Smithay output, so no window is destroyed on disconnect.
    state.unregister_output(&output);
    state.space.unmap_output(&output);
    state.space.refresh();
    drop(record);
    // A removed output leaves the presentation gate's armed set.
    state.present_gate_prune();
}

impl CrucibleState<UdevData> {
    /// Sets up a newly connected connector as a mapped [`Output`] and kicks off its first frame.
    pub(super) fn connector_connected(
        &mut self,
        node: DrmNode,
        connector: connector::Info,
        crtc: crtc::Handle,
    ) {
        let Some(device) = self.backend_data.backends.get(&node) else {
            return;
        };

        let output_name = format!(
            "{}-{}",
            connector.interface().as_str(),
            connector.interface_id()
        );
        info!(?crtc, "Setting up connector {}", output_name);
        info!(
            elapsed_ms = crate::startup::elapsed_ms(),
            output = %output_name,
            "startup: connector set up"
        );

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

        // Lay this output to the right of all existing ones. A connect always plans exactly one
        // addition, and appending never moves an existing output; an empty strip puts this first
        // output at the origin.
        let id = UdevOutputId {
            device_id: node,
            crtc,
        };
        let connect = plan::plan_connect(&output_strip(&self.space), id);
        let position = connect
            .add
            .first()
            .map_or((0, 0), |placement| placement.position);

        // The configured scale (by connector name, e.g. `DP-1`) is set before mapping, so the Space
        // geometry, the strip and the model rect are all logical from the start.
        let scale = crate::config::configured_scale(&self.config, &output.name());
        output.set_preferred(wl_mode);
        output.change_current_state(Some(wl_mode), None, Some(scale), Some(position.into()));
        self.space.map_output(&output, position);
        output.user_data().insert_if_missing(|| id);

        // From here on the global's cleanup is owned (dropped on every early return).
        let global = OutputGlobal {
            dh: self.display_handle.clone(),
            output: output.clone(),
            global: Some(global),
        };

        let planes = match drm_device.planes(&crtc) {
            Ok(planes) => planes,
            Err(err) => {
                warn!("Failed to query crtc planes: {}", err);
                self.space.unmap_output(&output);
                return;
            }
        };

        let pending = PendingAdoption {
            global,
            output: output.clone(),
            connector: connector.handle(),
            mode: drm_mode,
            planes,
        };

        // Register this connector with the model: the first compositor output adopts the existing
        // `World::new()` output; subsequent connectors get their own independent workspace stack
        // via `add_output`, seeded off the live and saved names. A start-up output is registered
        // before its DRM adoption, so the wallpaper client binds and arranges on it meanwhile.
        self.register_output(&output);

        // Start-up (plan 44): the first commit waits for this output's wallpaper buffer, or for
        // the adoption timeout. An output connected later adopts at once, as before.
        if self.backend_data.adoption_window_open {
            info!(
                elapsed_ms = crate::startup::elapsed_ms(),
                output = %output.name(),
                ?crtc,
                "startup: adoption deferred until the wallpaper"
            );
            if let Some(device) = self.backend_data.backends.get_mut(&node) {
                device.pending.insert(crtc, pending);
            }
            return;
        }
        self.adopt_output(node, crtc, pending, false);
    }

    /// Adopts a pending start-up output if it is still pending (a wallpaper buffer landed on it).
    pub(super) fn adopt_pending(&mut self, node: DrmNode, crtc: crtc::Handle) {
        let Some(pending) = self
            .backend_data
            .backends
            .get_mut(&node)
            .and_then(|device| device.pending.remove(&crtc))
        else {
            return;
        };
        self.adopt_output(node, crtc, pending, true);
    }

    /// Closes the start-up adoption window (the timeout): adopts every still-pending output, then
    /// disables the CRTCs CDE did not adopt (`DrmDevice::new(fd, false)` keeps them as found).
    pub(super) fn finish_startup_adoption(&mut self) {
        self.backend_data.adoption_window_open = false;
        let nodes: Vec<DrmNode> = self.backend_data.backends.keys().copied().collect();
        for node in &nodes {
            let pending: Vec<crtc::Handle> = self.backend_data.backends[node]
                .pending
                .keys()
                .copied()
                .collect();
            for crtc in crate::startup::ready_to_adopt(&pending, None, true) {
                self.adopt_pending(*node, crtc);
            }
        }
        for node in nodes {
            self.disable_unadopted_crtcs(node);
        }
    }

    /// The DRM adoption of one output: its `initialize_output` (the first commit), the surface
    /// insert and the first render.
    ///
    /// A `deferred` (start-up) adoption commits the real frame (the wallpaper) and counts as the
    /// output's locked presentation if a lock is pending (plan 44 invariant 5); a hot-plugged
    /// output commits the default (black) frame, as before. Should the commit fail from the state
    /// another DRM client left, that output's CRTCs are disabled and it retries once. If it cannot be
    /// initialized it is unregistered and unmapped (the dropped record removes its global).
    fn adopt_output(
        &mut self,
        node: DrmNode,
        crtc: crtc::Handle,
        pending: PendingAdoption,
        deferred: bool,
    ) {
        let Some(device) = self.backend_data.backends.get(&node) else {
            return;
        };
        let render_node = device.render_node.unwrap_or(self.backend_data.primary_gpu);
        let output = pending.output.clone();
        super::device::log_crtc_state(
            device.drm_output_manager.device(),
            "adoption commit",
            &[crtc],
        );
        info!(
            elapsed_ms = crate::startup::elapsed_ms(),
            output = %output.name(),
            deferred,
            "startup: adopting output"
        );

        let locked = self.session_lock.is_locked();
        let inputs = deferred.then(|| self.begin_frame(&output));
        let mut renderer = self
            .backend_data
            .gpus
            .single_renderer(&render_node)
            .unwrap();
        let mut render_elements = DrmOutputRenderElements::<_, OutputElements<'_>>::default();
        if let Some(inputs) = &inputs {
            let view = frame_view!(self, &output, inputs, self.dnd_icon.as_ref());
            let (elements, clear_color) =
                frame_elements(view, &mut self.header_text, &mut self.cursor, &mut renderer);
            render_elements.add_output(&crtc, clear_color, elements);
        }

        let device = self
            .backend_data
            .backends
            .get_mut(&node)
            .expect("looked up above");
        let mut attempt = |device: &mut super::device::BackendData| {
            device.drm_output_manager.lock().initialize_output(
                crtc,
                pending.mode,
                &[pending.connector],
                &output,
                Some(pending.planes.clone()),
                &mut renderer,
                &render_elements,
            )
        };
        let result = match attempt(device) {
            Ok(drm_output) => Ok(drm_output),
            Err(err) => {
                warn!(%err, ?crtc, "first commit failed from the inherited state; resetting it and retrying once");
                // Reset this output's inherited state: its target CRTC and the CRTC its connector
                // is currently bound to (another client may have driven it from a different one).
                let drm = device.drm_output_manager.device();
                let bound = drm
                    .get_connector(pending.connector, false)
                    .ok()
                    .and_then(|info| info.current_encoder())
                    .and_then(|encoder| drm.get_encoder(encoder).ok())
                    .and_then(|info| info.crtc());
                for c in std::iter::once(crtc).chain(bound.filter(|&b| b != crtc)) {
                    if let Err(err) = drm.set_crtc(c, None, (0, 0), &[], None) {
                        warn!(%err, crtc = ?c, "failed to disable the inherited crtc state");
                    }
                }
                attempt(device)
            }
        };
        let drm_output = match result {
            Ok(drm_output) => drm_output,
            Err(err) => {
                warn!("Failed to initialize drm output: {}", err);
                drop(renderer);
                self.unregister_output(&output);
                self.space.unmap_output(&output);
                self.space.refresh();
                self.present_gate_prune();
                return;
            }
        };
        drop(renderer);

        let device = self
            .backend_data
            .backends
            .get_mut(&node)
            .expect("looked up above");
        let dmabuf_feedback = drm_output.with_compositor(|compositor| {
            get_surface_dmabuf_feedback(
                self.backend_data.primary_gpu,
                device.render_node,
                node,
                &mut self.backend_data.gpus,
                compositor.surface(),
            )
        });

        let surface = SurfaceData {
            _global: pending.global,
            output: output.clone(),
            drm_output,
            dmabuf_feedback,
            power: PowerMark::default(),
            first_frame: Default::default(),
            first_flip: Default::default(),
        };
        device.surfaces.insert(crtc, surface);
        // A new CRTC has no night-light record: make sure it is tinted if the schedule says so.
        self.night_light.cache.remove(&(node, crtc));
        self.evaluate_night_light();

        // The adoption commit is synchronous and raises no VBlank: if it showed a locked frame,
        // it is this output's locked presentation (no later damage need follow).
        if deferred && crate::lock::sync_commit_presents_lock(locked, true) {
            self.lock_output_presented(&output);
        }
        // Plan 49: the same synchronous commit presents the gate's post-arm frame (it was
        // rendered just now). A non-deferred adoption committed nothing rendered: the kick-off
        // render below queues one, and its flip reports as usual.
        if deferred {
            self.present_gate_presented_now(&output);
        }

        // An output hot-plugged while the idle machine has the outputs blanked comes up powered
        // off and lights up on the next wake; if its power-off fails it renders as usual.
        if self.idle.machine.blanked() && self.backend_data.power_off_surface(node, crtc) {
            info!(?crtc, "idle: new output comes up powered off (blanked)");
            self.present_gate_prune();
            return;
        }

        // Kick off rendering for the new output.
        self.handle
            .insert_idle(move |state| state.render_surface(node, crtc));
    }

    /// Tears down the output for a disconnected connector.
    ///
    /// Only the removal half of the plan is executed here: this runs inside a device rescan, which
    /// reflows the surviving strip once after all of the scan's events have been applied.
    pub(super) fn connector_disconnected(
        &mut self,
        node: DrmNode,
        _connector: connector::Info,
        crtc: crtc::Handle,
    ) {
        let id = UdevOutputId {
            device_id: node,
            crtc,
        };
        let plan = plan::plan_disconnect(&output_strip(&self.space), &[id]);
        for key in &plan.remove {
            drop_mapped_output(self, *key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::backend::allocator::{Format, Fourcc, Modifier};

    #[test]
    fn disconnect_before_adoption_takes_the_pending_record() {
        let mut surfaces: HashMap<u8, &str> = HashMap::from([(1, "adopted")]);
        let mut pending: HashMap<u8, &str> = HashMap::from([(2, "pending")]);
        assert_eq!(
            take_mapped(&mut surfaces, &mut pending, &2),
            Some(Mapped::Pending("pending"))
        );
        assert!(pending.is_empty());
        assert_eq!(surfaces.len(), 1, "the adopted output is untouched");
        // A second disconnect event for the same CRTC finds nothing (no double cleanup).
        assert_eq!(take_mapped(&mut surfaces, &mut pending, &2), None);
    }

    #[test]
    fn device_removal_before_adoption_takes_every_record() {
        let mut surfaces: HashMap<u8, &str> = HashMap::from([(1, "adopted")]);
        let mut pending: HashMap<u8, &str> = HashMap::from([(2, "p2"), (3, "p3")]);
        let taken: Vec<_> = [1u8, 2, 3]
            .iter()
            .filter_map(|k| take_mapped(&mut surfaces, &mut pending, k))
            .collect();
        assert_eq!(
            taken,
            vec![
                Mapped::Adopted("adopted"),
                Mapped::Pending("p2"),
                Mapped::Pending("p3")
            ]
        );
        assert!(surfaces.is_empty() && pending.is_empty());
    }

    #[test]
    fn power_mark_off_drops_flips_and_wakes_once() {
        let mut mark = PowerMark::default();
        assert!(!mark.is_off());
        assert_eq!(mark.on_flip(true), FlipAction::Schedule);
        mark.powered_off();
        // A flip pending at clear() time completing while off: the handler drops it on `is_off`
        // before `frame_submitted`, so `on_flip` is never consulted.
        assert!(mark.is_off());
        assert!(mark.wake());
        assert!(!mark.is_off());
        // A second wake schedules nothing more (no double render).
        assert!(!mark.wake());
    }

    #[test]
    fn power_mark_stale_flip_after_wake_is_dropped_once() {
        let mut mark = PowerMark::default();
        mark.powered_off();
        assert!(mark.wake());
        // The stale flip arrives after the wake with no frame of its own: dropped.
        assert_eq!(mark.on_flip(false), FlipAction::Drop);
        // The wake frame's own flip then schedules normally.
        assert_eq!(mark.on_flip(true), FlipAction::Schedule);
        // Later frameless flips keep the pre-blanking behaviour (schedule).
        assert_eq!(mark.on_flip(false), FlipAction::Schedule);
    }

    #[test]
    fn power_mark_real_flip_after_wake_clears_the_stale_guard() {
        let mut mark = PowerMark::default();
        mark.powered_off();
        assert!(mark.wake());
        // No stale flip ever arrived; the wake frame's flip schedules and clears the guard.
        assert_eq!(mark.on_flip(true), FlipAction::Schedule);
        assert_eq!(mark.on_flip(false), FlipAction::Schedule);
    }

    fn fmt(code: Fourcc) -> Format {
        Format {
            code,
            modifier: Modifier::Linear,
        }
    }

    #[test]
    fn scanout_tranche_keeps_only_renderable_plane_formats() {
        let planes: FormatSet = [fmt(Fourcc::Argb8888), fmt(Fourcc::Nv12)]
            .into_iter()
            .collect();
        let render: FormatSet = [fmt(Fourcc::Argb8888), fmt(Fourcc::Abgr8888)]
            .into_iter()
            .collect();
        let tranche = scanout_tranche_formats(&planes, &render);
        assert_eq!(
            tranche.iter().copied().collect::<Vec<_>>(),
            vec![fmt(Fourcc::Argb8888)]
        );
    }

    #[test]
    fn scanout_tranche_is_empty_when_nothing_overlaps() {
        let planes: FormatSet = [fmt(Fourcc::Nv12)].into_iter().collect();
        let render: FormatSet = [fmt(Fourcc::Argb8888)].into_iter().collect();
        assert_eq!(scanout_tranche_formats(&planes, &render).iter().count(), 0);
    }
}
