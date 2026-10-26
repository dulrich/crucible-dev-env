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

    /// Removes workspace `ws` together with its panes and windows from every arena, returning the
    /// removed window ids in tree order.
    ///
    /// Errors (mutating nothing) with [`LayoutError::UnknownWorkspace`] for an unknown id, or
    /// [`LayoutError::LastWorkspace`] if it is the only workspace on its output. The owning output's
    /// `active` keeps pointing at the same workspace when another was active; when the removed one
    /// was active, the workspace now at the same index (clamped to the end) becomes active. Focus
    /// follows the active workspace's focused pane.
    pub fn remove_workspace(&mut self, ws: WorkspaceId) -> Result<Vec<WindowId>, LayoutError> {
        let owner = self.owner_of(ws)?;
        self.check_not_last(owner)?;
        self.detach_workspace(owner, ws);
        let removed = self.workspaces.remove(&ws).expect("checked");
        let mut windows = Vec::new();
        for pane_id in removed.panes() {
            if let Some(pane) = self.panes.remove(&pane_id) {
                for w in pane.windows {
                    self.windows.remove(&w);
                    windows.push(w);
                }
            }
        }
        Ok(windows)
    }

    /// Moves workspace `ws` (with its panes and windows) to the end of `output`'s stack, makes it
    /// the active workspace there, and makes `output` the focused output.
    ///
    /// Errors (mutating nothing) with [`LayoutError::UnknownWorkspace`] / [`LayoutError::UnknownOutput`],
    /// [`LayoutError::SameOutput`] if it already lives on `output`, or [`LayoutError::LastWorkspace`]
    /// if it is the only workspace on its source output. The source output's `active` is fixed up
    /// exactly as in [`World::remove_workspace`].
    pub fn move_workspace_to_output(
        &mut self,
        ws: WorkspaceId,
        output: OutputId,
    ) -> Result<(), LayoutError> {
        let source = self.owner_of(ws)?;
        if !self.outputs.contains_key(&output) {
            return Err(LayoutError::UnknownOutput(output));
        }
        if source == output {
            return Err(LayoutError::SameOutput(output));
        }
        self.check_not_last(source)?;
        self.detach_workspace(source, ws);
        let target = self.outputs.get_mut(&output).expect("checked");
        target.workspaces.push(ws);
        target.active = target.workspaces.len() - 1;
        self.focused_output = output;
        Ok(())
    }

    fn owner_of(&self, ws: WorkspaceId) -> Result<OutputId, LayoutError> {
        self.outputs
            .values()
            .find(|o| o.workspaces.contains(&ws))
            .map(|o| o.id)
            .ok_or(LayoutError::UnknownWorkspace(ws))
    }

    fn check_not_last(&self, output: OutputId) -> Result<(), LayoutError> {
        if self.outputs[&output].workspaces.len() <= 1 {
            return Err(LayoutError::LastWorkspace(output));
        }
        Ok(())
    }

    /// Removes `ws` from `output`'s stack, fixing `active` (caller guarantees ≥2 workspaces).
    fn detach_workspace(&mut self, output: OutputId, ws: WorkspaceId) {
        let o = self.outputs.get_mut(&output).expect("owner");
        let idx = o.workspaces.iter().position(|w| *w == ws).expect("owned");
        o.workspaces.remove(idx);
        if idx < o.active {
            o.active -= 1;
        } else if o.active >= o.workspaces.len() {
            o.active = o.workspaces.len() - 1;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;

    /// Focused output with workspaces "1" (initial), plus "a" and "b"; "b" active.
    fn world3() -> (World, WorkspaceId, WorkspaceId, WorkspaceId) {
        let mut w = World::new();
        let first = w.active_workspace_id();
        let a = w.new_workspace("a");
        let b = w.new_workspace("b");
        (w, first, a, b)
    }

    #[test]
    fn remove_inactive_before_active_keeps_active_workspace() {
        let (mut w, first, _a, b) = world3();
        assert_eq!(w.remove_workspace(first).unwrap(), vec![]);
        assert_eq!(w.active_workspace_id(), b);
        w.check_invariants();
    }

    #[test]
    fn remove_inactive_after_active_keeps_active_workspace() {
        let (mut w, first, a, b) = world3();
        w.switch_workspace(first).unwrap();
        w.remove_workspace(b).unwrap();
        assert_eq!(w.active_workspace_id(), first);
        assert!(w.workspace(b).is_none());
        w.check_invariants();
        w.switch_workspace(a).unwrap();
        w.check_invariants();
    }

    #[test]
    fn remove_active_picks_same_index_then_clamps() {
        let (mut w, first, a, b) = world3();
        w.switch_workspace(a).unwrap();
        w.remove_workspace(a).unwrap();
        assert_eq!(w.active_workspace_id(), b, "same index");
        w.check_invariants();
        w.remove_workspace(b).unwrap();
        assert_eq!(w.active_workspace_id(), first, "clamped to the end");
        w.check_invariants();
    }

    #[test]
    fn remove_returns_windows_in_tree_order_and_purges_arenas() {
        let (mut w, _first, a, _b) = world3();
        w.switch_workspace(a).unwrap();
        let w1 = w.open_window();
        w.split(crate::Direction::Right);
        let w2 = w.open_window();
        let w3 = w.stack_window();
        assert_eq!(w.window_count(), 3);
        let removed = w.remove_workspace(a).unwrap();
        assert_eq!(removed, vec![w1, w2, w3]);
        assert_eq!(w.window_count(), 0);
        assert!(w.window(w1).is_none());
        w.check_invariants();
    }

    #[test]
    fn focus_lands_on_new_active_workspaces_focused_pane() {
        let (mut w, first, _a, b) = world3();
        w.switch_workspace(first).unwrap();
        let win = w.open_window();
        w.switch_workspace(b).unwrap();
        w.remove_workspace(b).unwrap();
        // "a" is now active; remove it so "first" takes over.
        let a_now = w.active_workspace_id();
        w.remove_workspace(a_now).unwrap();
        assert_eq!(w.active_workspace_id(), first);
        assert_eq!(w.focused_window(), Some(win));
        w.check_invariants();
    }

    #[test]
    fn remove_last_workspace_is_refused_without_mutation() {
        let mut w = World::new();
        let only = w.active_workspace_id();
        let before = w.clone();
        assert_eq!(
            w.remove_workspace(only),
            Err(LayoutError::LastWorkspace(w.focused_output()))
        );
        assert_eq!(w, before);
        assert_eq!(
            w.remove_workspace(WorkspaceId::from_raw(999)),
            Err(LayoutError::UnknownWorkspace(WorkspaceId::from_raw(999)))
        );
        assert_eq!(w, before);
        w.check_invariants();
    }

    fn two_outputs() -> (World, OutputId, OutputId) {
        let mut w = World::new();
        let o1 = w.focused_output();
        let o2 = w.add_output(Rect::new(10, 0, 10, 10));
        (w, o1, o2)
    }

    #[test]
    fn move_workspace_fixes_source_target_and_focus() {
        let (mut w, o1, o2) = two_outputs();
        let first = w.active_workspace_id();
        let a = w.new_workspace("a"); // o1: [first, a], a active
        w.move_workspace_to_output(a, o2).unwrap();
        assert_eq!(w.output(o1).unwrap().workspaces, vec![first]);
        assert_eq!(w.output(o1).unwrap().active_workspace(), first);
        let target = w.output(o2).unwrap();
        assert_eq!(target.workspaces.len(), 2);
        assert_eq!(target.active_workspace(), a);
        assert_eq!(w.focused_output(), o2);
        assert_eq!(w.active_workspace_id(), a);
        w.check_invariants();
    }

    #[test]
    fn move_inactive_workspace_keeps_source_active() {
        let (mut w, o1, o2) = two_outputs();
        let first = w.active_workspace_id();
        let a = w.new_workspace("a"); // a active
        w.move_workspace_to_output(first, o2).unwrap();
        assert_eq!(w.output(o1).unwrap().active_workspace(), a);
        assert_eq!(w.output(o2).unwrap().active_workspace(), first);
        w.check_invariants();
    }

    #[test]
    fn move_workspace_refusals_do_not_mutate() {
        let (mut w, o1, o2) = two_outputs();
        let first = w.active_workspace_id();
        let before = w.clone();
        assert_eq!(
            w.move_workspace_to_output(first, o1),
            Err(LayoutError::SameOutput(o1))
        );
        assert_eq!(
            w.move_workspace_to_output(first, o2),
            Err(LayoutError::LastWorkspace(o1))
        );
        let bad_ws = WorkspaceId::from_raw(999);
        assert_eq!(
            w.move_workspace_to_output(bad_ws, o2),
            Err(LayoutError::UnknownWorkspace(bad_ws))
        );
        let bad_out = OutputId::from_raw(999);
        assert_eq!(
            w.move_workspace_to_output(first, bad_out),
            Err(LayoutError::UnknownOutput(bad_out))
        );
        assert_eq!(w, before);
        w.check_invariants();
    }
}
