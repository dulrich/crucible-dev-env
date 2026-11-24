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
    startup::StartupEvent,
    state::CrucibleState,
};

use super::plan::{self, DesiredPositions, MappedOutput, OutputPlan};
use super::{OutputElements, UdevData};
mod adopt;
mod records;

pub(in crate::udev) use records::{
    FlipAction, Mapped, OutputGlobal, PendingAdoption, PowerMark, SurfaceData, UdevOutputId,
    take_mapped,
};

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
                name: output.name(),
                width: geometry.map(|geo| geo.size.w).unwrap_or(0),
                height: geometry.map(|geo| geo.size.h).unwrap_or(0),
                position: geometry.map(|geo| (geo.loc.x, geo.loc.y)).unwrap_or((0, 0)),
            })
        })
        .collect()
}

/// The desired origin of each output by connector name: the **only** source the planner is fed
/// from. Today that is the `output "<connector>" { position x y }` config; live overrides (the
/// `zwlr_output_manager_v1` server) extend exactly this function.
pub(super) fn desired_output_positions(state: &CrucibleState<UdevData>) -> DesiredPositions {
    state
        .config
        .outputs
        .iter()
        .filter_map(|(name, output)| Some((name.clone(), output.position?)))
        .collect()
}

/// The logical size Smithay's `Space` will report for `output` once mapped (transformed mode
/// size, divided by the fractional scale, rounded up), or `(0, 0)` without a mode.
pub(super) fn logical_size(output: &Output) -> (i32, i32) {
    output.current_mode().map_or((0, 0), |mode| {
        let size = output
            .current_transform()
            .transform_size(mode.size)
            .to_f64()
            .to_logical(output.current_scale().fractional_scale())
            .to_i32_ceil::<i32>();
        (size.w, size.h)
    })
}

/// Logs the warn-worthy outcomes of a plan: configured positions that fell back to the strip,
/// outputs the pointer cannot reach, and outputs left unplaced by overflowing arithmetic.
pub(super) fn warn_plan(plan: &OutputPlan<UdevOutputId>) {
    for name in &plan.fallbacks {
        warn!(output = %name, "configured output position overlaps another output (or overflows); placing it on the strip instead");
    }
    for name in &plan.unreachable {
        warn!(output = %name, "output shares no edge with any other output; the pointer cannot reach it");
    }
    for key in &plan.skipped {
        warn!(
            ?key,
            "output position arithmetic overflowed; leaving it unplaced"
        );
    }
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
    warn_plan(plan);
    apply_positions(state, plan);
}

/// Re-lays every mapped output per the placement rule and re-arranges each output's layer map.
///
/// Called after a device rescan so the unpositioned outputs form a contiguous left-to-right strip
/// with no gaps or overlaps (anvil's `fixup_positions`); a mode change can resize an output under us, which
/// moves everything to its right. The placement rule itself lives in [`plan::plan_reflow`]; it
/// also re-places outputs whose desired position changed.
pub(super) fn reflow_outputs(state: &mut CrucibleState<UdevData>) {
    let desired = desired_output_positions(state);
    let plan = plan::plan_reflow(&output_strip(&state.space), &desired);
    warn_plan(&plan);
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

/// Moves the existing outputs a connect plan displaced (`survivor_reflow`), leaving the addition
/// to the caller, which maps it itself.
pub(super) fn apply_survivor_reflow(
    state: &mut CrucibleState<UdevData>,
    plan: &OutputPlan<UdevOutputId>,
) {
    for placement in &plan.survivor_reflow {
        let Some(output) = output_for(&state.space, placement.key) else {
            continue;
        };
        state.space.map_output(&output, placement.position);
        output.change_current_state(None, None, None, Some(placement.position.into()));
        state.relayout_output(&output);
    }
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
    // A removed output leaves the presentation gate (a pending lock waiting on it re-evaluates).
    state.present_gate_prune();
}
