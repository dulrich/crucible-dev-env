//! Output registration: the Smithay output to model [`OutputId`] binding and the output lifecycle.

use std::collections::HashSet;

use crucible_layout::{LayoutError, OutputId, Rect};
use smithay::output::Output;
use tracing::{debug, info, warn};

use crate::state::{Backend, CrucibleState};

/// Wrapper stored in a Smithay [`Output`]'s `user_data()` binding it to a model [`OutputId`].
///
/// Using the `UserDataMap` (rather than a parallel `HashMap` in state) keeps the Smithay↔model
/// mapping co-located with the output it describes, so there is nothing to keep in sync on add/remove.
#[derive(Debug, Clone, Copy)]
struct ModelOutputId(OutputId);

/// Returns the model [`OutputId`] bound to `output`, if one has been set.
#[must_use]
pub fn model_output_id(output: &Output) -> Option<OutputId> {
    output.user_data().get::<ModelOutputId>().map(|m| m.0)
}

/// Binds `id` to `output` in its `user_data()` (idempotent: the first binding wins).
pub fn set_model_output_id(output: &Output, id: OutputId) {
    output.user_data().insert_if_missing(|| ModelOutputId(id));
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Snapshots every mapped compositor output as `(model id, global rect)`.
    ///
    /// Outputs not yet bound to a model id, or with no geometry in the `Space`, are skipped — they
    /// contribute nothing to the projection.
    ///
    /// While the header strip is shown (`always`, or `toggle` while visible) each rect has its height removed from its top, so the
    /// projection, `pane_rect`, the empty-pane ring and X11 placement all agree.
    pub(crate) fn output_bindings(&self) -> Vec<(OutputId, Rect)> {
        let reserved = crate::header::reserved_height(&self.config.header, self.header.visible);
        self.raw_output_rects()
            .into_iter()
            .map(|(id, rect)| (id, rect.shrink_top(reserved)))
            .collect()
    }

    /// Every bound output's full global rect, before any header reservation.
    pub(crate) fn raw_output_rects(&self) -> Vec<(OutputId, Rect)> {
        self.space
            .outputs()
            .filter_map(|output| {
                let id = model_output_id(output)?;
                let geo = self.space.output_geometry(output)?;
                Some((id, Rect::new(geo.loc.x, geo.loc.y, geo.size.w, geo.size.h)))
            })
            .collect()
    }

    // ---- output lifecycle --------------------------------------------------------------------

    /// Registers a Smithay `output` with the model: adopt-or-add, then bind + size it.
    ///
    /// The **first** compositor output adopts the model's pre-existing `World::new()` output (which
    /// is otherwise unbound); every subsequent output calls [`World::add_output`] to get its own
    /// independent workspace stack, its seed named by `new_workspace_name` off the live and saved
    /// names. Either way the chosen model [`OutputId`] is stashed in the
    /// Smithay output's `user_data()` and its stored rect is set to the output's live `Space`
    /// geometry (logical, global Space coordinates). Returns the model [`OutputId`]. Idempotent:
    /// re-registering an already-bound output only refreshes its rect.
    ///
    /// The output must already be mapped into the `Space` (with its scale set before mapping);
    /// registering an unmapped output is a programming error: it is logged and skipped (`None`).
    pub fn register_output(&mut self, output: &Output) -> Option<OutputId> {
        let Some(rect) = self.output_rect(output) else {
            warn!(output = %output.name(), "register_output before map_output; ignoring");
            return None;
        };
        if let Some(existing) = model_output_id(output) {
            self.set_model_output_rect(existing, rect);
            return Some(existing);
        }

        // The binding decision (adopt the initial model output vs. allocate) is the model half of
        // registration and lives on `LayoutState` so it is testable without a `Space`.
        let bound: HashSet<OutputId> = self.space.outputs().filter_map(model_output_id).collect();
        // A freshly allocated output's seed is named off the saved names so it never shadows one.
        let saved = self.persist.saved_names();
        let model_id = self.layout.bind_output(&bound, rect, &saved);
        set_model_output_id(output, model_id);
        self.apply_layout();
        Some(model_id)
    }

    /// The model rect of a mapped `output`: its `Space` geometry, or `None` while unmapped.
    fn output_rect(&self, output: &Output) -> Option<Rect> {
        let geo = self.space.output_geometry(output)?;
        Some(Rect::new(geo.loc.x, geo.loc.y, geo.size.w, geo.size.h))
    }

    /// Re-arranges `output`'s layer map in place, then re-syncs its model rect from the `Space`.
    ///
    /// The per-output step shared by [`Backend::outputs_changed`] and the udev reflow; the caller
    /// re-applies the layout once after its loop.
    pub fn relayout_output(&mut self, output: &Output) {
        smithay::desktop::layer_map_for_output(output).arrange();
        self.sync_output_rect(output);
    }

    /// Updates the stored model rect for `output` from its current Space geometry.
    ///
    /// Called after an output is (re)mapped/resized so the projection sees the live pixel size.
    /// No-op for an output not yet bound to a model id.
    pub fn sync_output_rect(&mut self, output: &Output) {
        let Some(model_id) = model_output_id(output) else {
            return;
        };
        let Some(rect) = self.output_rect(output) else {
            return;
        };
        self.set_model_output_rect(model_id, rect);
        self.lock_output_resized(output);
    }

    /// Writes `rect` onto model output `id`, reporting (and skipping) a stale binding.
    ///
    /// A Smithay output can outlive its model output (the connector is gone but a frame is still in
    /// flight). That is recoverable: the geometry update is dropped and the surviving outputs keep
    /// their own rects.
    fn set_model_output_rect(&mut self, id: OutputId, rect: Rect) {
        match self.layout.update_output_rect(id, rect) {
            Ok(()) => self.apply_layout(),
            Err(err) => warn!(%err, ?id, "stale output binding; dropping geometry update"),
        }
    }

    /// Removes the Smithay `output`'s model output, migrating its workspaces to a survivor.
    ///
    /// Calls [`World::remove_output`], which preserves every window by appending the removed
    /// output's workspaces onto a surviving output, then applies the projection. The **last** model
    /// output is never removed ([`LayoutError::LastOutput`]) — the model always keeps one — and an
    /// unbound output is nothing to remove.
    pub fn unregister_output(&mut self, output: &Output) {
        let Some(model_id) = model_output_id(output) else {
            debug!("unregister_output for an unbound output; ignoring");
            return;
        };
        match self.layout.release_output(model_id) {
            Ok(()) => self.apply_layout(),
            Err(LayoutError::LastOutput) => {
                debug!(
                    ?model_id,
                    "keeping the last model output (last-output policy)"
                );
            }
            Err(err) => warn!(%err, ?model_id, "remove_output failed; keeping model output"),
        }
    }

    /// Routes the focused model output to the one backing `output`, if bound.
    ///
    /// Call when input/pointer focus moves onto a monitor (or a window maps there) so new windows
    /// and workspace verbs act on that monitor. No-op if unbound or already focused.
    pub fn focus_output(&mut self, output: &Output) {
        let Some(model_id) = model_output_id(output) else {
            return;
        };
        if self.layout.world.focused_output() == model_id {
            return;
        }
        match self.with_model(|world| world.set_focused_output(model_id)) {
            Ok(()) => info!(
                output = %output.name(),
                focus = %self.layout.world.focus_path(),
                "output focus"
            ),
            Err(err) => warn!(%err, ?model_id, "stale output binding; focus stays where it is"),
        }
    }
}
