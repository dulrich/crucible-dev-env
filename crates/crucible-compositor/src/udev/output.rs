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

use smithay::{
    backend::{
        allocator::{format::FormatSet, gbm::GbmAllocator},
        drm::{
            DrmDeviceFd, DrmNode, DrmSurface,
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
        drm::control::{ModeTypeFlags, connector, crtc},
        wayland_protocols::wp::linux_dmabuf::zv1::server::zwp_linux_dmabuf_feedback_v1::TrancheFlags,
        wayland_server::{DisplayHandle, backend::GlobalId},
    },
    wayland::dmabuf::{DmabufFeedback, DmabufFeedbackBuilder},
};
use smithay_drm_extras::display_info;
use tracing::{debug, info, warn};

use crate::{element::WindowElement, state::CrucibleState};

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
    /// Display handle, used to remove the `wl_output` global on drop.
    dh: DisplayHandle,
    /// The Smithay output mapped into the shared [`Space`].
    output: Output,
    /// The `wl_output` global advertised to clients (removed on drop).
    global: Option<GlobalId>,
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

impl Drop for SurfaceData {
    fn drop(&mut self) {
        self.output.leave_all();
        if let Some(global) = self.global.take() {
            self.dh.remove_global::<CrucibleState<UdevData>>(global);
        }
    }
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
/// A [`SurfaceData`] only ever exists for an output that is mapped in the [`Space`], so a plan's
/// removal set and the device's surface map agree; an output whose surface never came up (its DRM
/// output failed to initialize) is left alone here, exactly as before.
fn drop_mapped_output(state: &mut CrucibleState<UdevData>, key: UdevOutputId) {
    let Some(device) = state.backend_data.backends.get_mut(&key.device_id) else {
        return;
    };
    let Some(surface) = device.surfaces.remove(&key.crtc) else {
        return;
    };
    // Migrate this output's model workspaces (and their windows) onto a survivor before
    // unmapping the Smithay output, so no window is destroyed on disconnect.
    state.unregister_output(&surface.output);
    state.space.unmap_output(&surface.output);
    state.space.refresh();
}

impl CrucibleState<UdevData> {
    /// Sets up a newly connected connector as a mapped [`Output`] and kicks off its first frame.
    pub(super) fn connector_connected(
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

        // Snapshot the data needed to register this connector with the model once the `device`
        // borrow is released (the registration must touch `self`, but `device` borrows `self`).
        let register_output = output.clone();

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
            dh: self.display_handle.clone(),
            output,
            global: Some(global),
            drm_output,
            dmabuf_feedback,
            power: PowerMark::default(),
        };
        device.surfaces.insert(crtc, surface);

        // Register this connector with the model now that the `device` borrow is released: the first
        // compositor output adopts the existing `World::new()` output; subsequent connectors get
        // their own independent workspace stack via `add_output`, seeded off the live and saved names.
        self.register_output(&register_output);

        // An output hot-plugged while the idle machine has the outputs blanked comes up powered
        // off and lights up on the next wake; if its power-off fails it renders as usual.
        if self.idle.machine.blanked() && self.backend_data.power_off_surface(node, crtc) {
            info!(?crtc, "idle: new output comes up powered off (blanked)");
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
