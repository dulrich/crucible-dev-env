//! Workspace lifecycle (create, rename, switch, cycle) and moving windows between workspaces.

use super::*;
use crate::{OutputId, PaneId, WindowId, WorkspaceId, WorkspaceLayout, tree::SplitTree};

impl World {
    /// Creates a new empty workspace on the focused output, pushes it on the stack, and switches to
    /// it. Returns the new workspace id.
    ///
    /// Does **not** check name uniqueness (the compositor passes unique names); use
    /// [`World::insert_workspace`] / [`World::workspace_by_name`] where uniqueness matters.
    pub fn new_workspace(&mut self, name: impl Into<String>) -> WorkspaceId {
        let workspace_id = self.build_workspace(name);

        let output = self.outputs.get_mut(&self.focused_output).expect("output");
        output.workspaces.push(workspace_id);
        output.active = output.workspaces.len() - 1;

        workspace_id
    }

    /// Renames a workspace. Errors if the id is unknown, or if a *different* workspace already
    /// holds `name` ([`LayoutError::DuplicateName`]); renaming to its own current name is fine.
    pub fn rename_workspace(
        &mut self,
        id: WorkspaceId,
        name: impl Into<String>,
    ) -> Result<(), LayoutError> {
        let name = name.into();
        if !self.workspaces.contains_key(&id) {
            return Err(LayoutError::UnknownWorkspace(id));
        }
        if self.workspace_by_name(&name).is_some_and(|o| o != id) {
            return Err(LayoutError::DuplicateName(name));
        }
        self.workspaces.get_mut(&id).expect("checked").name = name;
        Ok(())
    }

    /// Finds the workspace named `name`, if any.
    #[must_use]
    pub fn workspace_by_name(&self, name: &str) -> Option<WorkspaceId> {
        self.workspaces
            .values()
            .find(|w| w.name == name)
            .map(|w| w.id)
    }

    /// Snapshots the topology of workspace `id` (split tree + focused pane index), or `None` if the
    /// id is unknown. Stack contents and stack focus are not part of the snapshot.
    #[must_use]
    pub fn workspace_layout(&self, id: WorkspaceId) -> Option<WorkspaceLayout> {
        let ws = self.workspaces.get(&id)?;
        let focused_pane = ws
            .tree
            .leaves()
            .iter()
            .position(|p| *p == ws.focused_pane)
            .unwrap_or(0);
        Some(WorkspaceLayout {
            root: ws.tree.to_layout(),
            focused_pane,
        })
    }

    /// Builds a workspace named `name` shaped like `layout`, with **empty** panes, and appends it
    /// to `output`'s workspace stack without switching to it (use [`World::switch_workspace`]).
    ///
    /// Returns the workspace id and its pane ids in tree order. A `layout.focused_pane` that is out
    /// of range is clamped to pane 0. Errors (and mutates nothing) on an unknown output, a name
    /// held by another workspace ([`LayoutError::DuplicateName`]), or a split ratio that is not
    /// finite and strictly inside `(0, 1)` ([`LayoutError::InvalidRatio`]).
    pub fn insert_workspace(
        &mut self,
        output: OutputId,
        name: impl Into<String>,
        layout: &WorkspaceLayout,
    ) -> Result<(WorkspaceId, Vec<PaneId>), LayoutError> {
        let name = name.into();
        if !self.outputs.contains_key(&output) {
            return Err(LayoutError::UnknownOutput(output));
        }
        if self.workspace_by_name(&name).is_some() {
            return Err(LayoutError::DuplicateName(name));
        }
        if !layout.root.ratios_valid() {
            return Err(LayoutError::InvalidRatio);
        }
        let pane_ids = &mut self.pane_ids;
        let (tree, panes) =
            SplitTree::from_layout(&layout.root, || PaneId::from_raw(pane_ids.alloc()));
        for id in &panes {
            self.panes.insert(
                *id,
                Pane {
                    id: *id,
                    windows: Vec::new(),
                    focused: 0,
                },
            );
        }
        let workspace_id = WorkspaceId::from_raw(self.workspace_ids.alloc());
        let focused = panes.get(layout.focused_pane).copied().unwrap_or(panes[0]);
        self.workspaces.insert(
            workspace_id,
            Workspace {
                id: workspace_id,
                name,
                focused_pane: focused,
                tree,
            },
        );
        self.outputs
            .get_mut(&output)
            .expect("checked")
            .workspaces
            .push(workspace_id);
        Ok((workspace_id, panes))
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
