//! Per-workspace fullscreen: at most one window per workspace covers its output's full rect.
//!
//! The state lives on [`Workspace`] (`fullscreen`) and is never serialized. It is entered by
//! [`World::set_fullscreen`] / [`World::toggle_fullscreen`] and left by an unset, a toggle, the
//! window closing or leaving the workspace, or any layout verb on that workspace that changes
//! focus, splits or moves (those verbs call [`World::exit_fullscreen`] first). Switching workspaces
//! away and back keeps it. [`World::project`] gives the fullscreen window its output's whole rect
//! and hides the workspace's other windows.

use super::*;

impl World {
    /// Enters (`on`) or leaves fullscreen for `window` on its own workspace.
    ///
    /// Entering replaces any other fullscreen window of that workspace and makes `window` the
    /// workspace's focused pane and its pane's focused window (the active workspace and focused
    /// output are left alone). Returns whether the state changed. Errors with
    /// [`LayoutError::UnknownWindow`] for a window the model does not hold.
    pub fn set_fullscreen(&mut self, window: WindowId, on: bool) -> Result<bool, LayoutError> {
        let location = self
            .locate_window(window)
            .ok_or(LayoutError::UnknownWindow(window))?;
        let ws = self
            .workspaces
            .get_mut(&location.workspace)
            .expect("located workspace");
        if !on {
            if ws.fullscreen == Some(window) {
                ws.fullscreen = None;
                return Ok(true);
            }
            return Ok(false);
        }
        if ws.fullscreen == Some(window) {
            return Ok(false);
        }
        ws.fullscreen = Some(window);
        ws.focused_pane = location.pane;
        let pane = self.panes.get_mut(&location.pane).expect("located pane");
        let found = pane.stack.focus_item(&window);
        debug_assert!(found, "window in its pane");
        Ok(true)
    }

    /// `toggle-fullscreen`: flips fullscreen for the focused window. A no-op (returning `false`)
    /// when the focused pane is empty.
    pub fn toggle_fullscreen(&mut self) -> bool {
        let Some(window) = self.focused_window() else {
            return false;
        };
        let on = !self.is_fullscreen(window);
        self.set_fullscreen(window, on).unwrap_or(false)
    }

    /// The fullscreen window of workspace `ws`, if any.
    #[must_use]
    pub fn fullscreen_of(&self, ws: WorkspaceId) -> Option<WindowId> {
        self.workspaces.get(&ws)?.fullscreen
    }

    /// The fullscreen window shown on `output` (its active workspace's), if any.
    #[must_use]
    pub fn output_fullscreen(&self, output: OutputId) -> Option<WindowId> {
        self.fullscreen_of(self.outputs.get(&output)?.active_workspace())
    }

    /// Whether `window` is its workspace's fullscreen window (shown or not).
    #[must_use]
    pub fn is_fullscreen(&self, window: WindowId) -> bool {
        self.workspaces
            .values()
            .any(|ws| ws.fullscreen == Some(window))
    }

    /// Every workspace's fullscreen window, ascending by id.
    #[must_use]
    pub fn fullscreen_windows(&self) -> Vec<WindowId> {
        let mut out: Vec<WindowId> = self
            .workspaces
            .values()
            .filter_map(|ws| ws.fullscreen)
            .collect();
        out.sort();
        out
    }

    /// Clears workspace `ws`'s fullscreen window (no-op when it has none or is unknown).
    pub(super) fn exit_fullscreen(&mut self, ws: WorkspaceId) {
        if let Some(w) = self.workspaces.get_mut(&ws) {
            w.fullscreen = None;
        }
    }

    /// Clears workspace `ws`'s fullscreen when `window` is it (the window closing).
    pub(super) fn exit_fullscreen_of(&mut self, ws: WorkspaceId, window: WindowId) {
        if self.fullscreen_of(ws) == Some(window) {
            self.exit_fullscreen(ws);
        }
    }

    /// Clears the fullscreen of the active workspace (before a focus/split/move verb on it).
    pub(super) fn exit_active_fullscreen(&mut self) {
        self.exit_fullscreen(self.active_workspace_id());
    }

    /// The pane `window` fullscreens over: its own pane while it is its workspace's fullscreen
    /// window.
    pub(super) fn fullscreen_pane(&self, ws: WorkspaceId) -> Option<(PaneId, WindowId)> {
        let window = self.fullscreen_of(ws)?;
        let loc = self.locate_window(window)?;
        (loc.workspace == ws).then_some((loc.pane, window))
    }
}
