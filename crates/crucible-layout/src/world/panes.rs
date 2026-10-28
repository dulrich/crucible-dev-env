//! Pane-level queries and focus: global pane rects, the pointer hit-test, and `focus_pane`.
//!
//! These mirror [`World::project`]'s inputs (`(OutputId, global Rect)` bindings plus a
//! [`GapConfig`]) so the compositor can resolve a pointer location or draw pane chrome from the
//! exact same geometry it projects windows with.

use super::*;
use crate::geometry::{GapConfig, geometry_of_workspace};

impl World {
    /// Every pane rect of each bound output's **active** workspace, in global coordinates.
    ///
    /// Laid out in the output's local space, then offset by the output origin; this is the one
    /// per-output iteration [`World::project`] is built on. Includes empty panes. Output ids the
    /// model does not know contribute nothing.
    #[must_use]
    pub fn pane_rects(&self, outputs: &[(OutputId, Rect)], gaps: GapConfig) -> Vec<(PaneId, Rect)> {
        let mut rects = Vec::new();
        for (output_id, global) in outputs {
            let Some(output) = self.outputs.get(output_id) else {
                continue;
            };
            let Some(workspace) = self.workspaces.get(&output.active_workspace()) else {
                continue;
            };
            let local = Rect::new(0, 0, global.width, global.height);
            rects.extend(
                geometry_of_workspace(workspace, local, gaps)
                    .into_iter()
                    .map(|(pane, rect)| (pane, rect.translate(global.x, global.y))),
            );
        }
        rects
    }

    /// The pane whose rect contains global point `(x, y)`, if any.
    ///
    /// Points in gaps (outer or inner) or outside every bound output return `None`.
    #[must_use]
    pub fn pane_at(
        &self,
        outputs: &[(OutputId, Rect)],
        gaps: GapConfig,
        x: i32,
        y: i32,
    ) -> Option<PaneId> {
        self.pane_rects(outputs, gaps)
            .into_iter()
            .find(|(_, r)| r.contains(x, y))
            .map(|(pane, _)| pane)
    }

    /// The global rect of `pane`, when it is on a bound output's active workspace.
    #[must_use]
    pub fn pane_rect(
        &self,
        outputs: &[(OutputId, Rect)],
        gaps: GapConfig,
        pane: PaneId,
    ) -> Option<Rect> {
        self.pane_rects(outputs, gaps)
            .into_iter()
            .find(|(p, _)| *p == pane)
            .map(|(_, r)| r)
    }

    /// The pane that holds `window`, if the model knows it.
    #[must_use]
    pub fn pane_of_window(&self, window: WindowId) -> Option<PaneId> {
        self.locate_window(window).map(|loc| loc.pane)
    }

    /// The focused pane when it holds no window (the target of the empty-pane focus ring).
    #[must_use]
    pub fn focused_empty_pane(&self) -> Option<PaneId> {
        let pane = self.focused_pane_id();
        self.panes
            .get(&pane)
            .filter(|p| p.windows().is_empty())
            .map(|_| pane)
    }

    /// Focuses `pane` wherever it lives: its workspace's owning output becomes the focused output,
    /// that workspace becomes the output's active workspace, and `pane` becomes its focused pane.
    /// The pane's stack index is left alone.
    ///
    /// Errors with [`LayoutError::UnknownPane`] (and mutates nothing) when `pane` is not in the
    /// model.
    pub fn focus_pane(&mut self, pane: PaneId) -> Result<(), LayoutError> {
        let workspace = self
            .workspaces
            .values()
            .find(|ws| ws.tree.leaves().contains(&pane))
            .map(|ws| ws.id)
            .ok_or(LayoutError::UnknownPane(pane))?;
        self.activate(workspace, Some(pane))
    }
}
