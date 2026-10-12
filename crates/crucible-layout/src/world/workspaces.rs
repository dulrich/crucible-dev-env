//! Workspace lifecycle (create, rename, switch, cycle) and moving windows between workspaces.

use super::*;
use crate::{PaneId, WindowId, WorkspaceId, tree::SplitTree};

impl World {
    /// Creates a new empty workspace on the focused output, pushes it on the stack, and switches to
    /// it. Returns the new workspace id.
    pub fn new_workspace(&mut self, name: impl Into<String>) -> WorkspaceId {
        let workspace_id = self.build_workspace(name);

        let output = self.outputs.get_mut(&self.focused_output).expect("output");
        output.workspaces.push(workspace_id);
        output.active = output.workspaces.len() - 1;

        workspace_id
    }

    /// Renames a workspace. Returns an error if the id is unknown.
    pub fn rename_workspace(
        &mut self,
        id: WorkspaceId,
        name: impl Into<String>,
    ) -> Result<(), LayoutError> {
        let ws = self
            .workspaces
            .get_mut(&id)
            .ok_or(LayoutError::UnknownWorkspace(id))?;
        ws.name = name.into();
        Ok(())
    }

    /// Switches the focused output to make `target` its active workspace.
    ///
    /// `target` must be a workspace stacked on the focused output. Changes which windows are visible
    /// and where focus lands.
    pub fn switch_workspace(&mut self, target: WorkspaceId) -> Result<(), LayoutError> {
        if !self.workspaces.contains_key(&target) {
            return Err(LayoutError::UnknownWorkspace(target));
        }
        let output = self.outputs.get_mut(&self.focused_output).expect("output");
        let pos = output
            .workspaces
            .iter()
            .position(|w| *w == target)
            .ok_or(LayoutError::UnknownWorkspace(target))?;
        output.active = pos;
        Ok(())
    }

    /// Switches the focused output's active workspace by `delta` (wraparound).
    pub fn cycle_workspace(&mut self, delta: i64) {
        let output = self.outputs.get_mut(&self.focused_output).expect("output");
        output.cycle(delta);
    }

    /// Moves `window` from its current pane to the focused pane of `target` workspace.
    ///
    /// The window is removed from its source pane (which persists, possibly empty) and
    /// pushed onto and focused in the target workspace's focused pane. Errors if the window or target
    /// is unknown, or the window already lives on `target`.
    pub fn move_window_to_workspace(
        &mut self,
        window: WindowId,
        target: WorkspaceId,
    ) -> Result<(), LayoutError> {
        let location = self
            .locate_window(window)
            .ok_or(LayoutError::UnknownWindow(window))?;
        if !self.workspaces.contains_key(&target) {
            return Err(LayoutError::UnknownWorkspace(target));
        }
        if location.workspace == target {
            return Err(LayoutError::SameWorkspace);
        }

        // Detach from the source pane.
        let pane = self.panes.get_mut(&location.pane).expect("source pane");
        let idx = pane
            .windows
            .iter()
            .position(|w| *w == window)
            .expect("window present");
        pane.windows.remove(idx);
        if pane.windows.is_empty() {
            pane.focused = 0;
        } else if pane.focused >= pane.windows.len() {
            pane.focused = pane.windows.len() - 1;
        }

        // Attach to the target workspace's focused pane.
        let target_pane_id = self.workspaces.get(&target).expect("target").focused_pane;
        let target_pane = self.panes.get_mut(&target_pane_id).expect("target pane");
        target_pane.windows.push(window);
        target_pane.focused = target_pane.windows.len() - 1;

        Ok(())
    }

    /// Creates a fresh empty workspace (one empty pane) and inserts both into the arena maps.
    ///
    /// Shared by [`super::World::new`]-style seeding, [`super::World::add_output`], and
    /// [`World::new_workspace`]. Returns the new workspace id. Does **not** attach the workspace to
    /// any output.
    pub(super) fn build_workspace(&mut self, name: impl Into<String>) -> WorkspaceId {
        let workspace_id = WorkspaceId::from_raw(self.workspace_ids.alloc());
        let pane_id = PaneId::from_raw(self.pane_ids.alloc());
        self.panes.insert(
            pane_id,
            Pane {
                id: pane_id,
                windows: Vec::new(),
                focused: 0,
            },
        );
        self.workspaces.insert(
            workspace_id,
            Workspace {
                id: workspace_id,
                name: name.into(),
                focused_pane: pane_id,
                tree: SplitTree::new(pane_id),
            },
        );
        workspace_id
    }
}
