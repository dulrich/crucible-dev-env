//! Window lifecycle (open/close), pane split/stack, and focus/move/cycle/resize operations.

use super::*;
use crate::{Direction, PaneId, WindowId, WorkspaceId};

/// Where a window currently lives.
pub(super) struct WindowLocation {
    pub(super) workspace: WorkspaceId,
    pub(super) pane: PaneId,
}

impl World {
    /// Opens a new window into the focused pane of the active workspace and focuses it.
    ///
    /// Returns the new [`WindowId`]. The new window becomes the focused (visible) window of its
    /// pane; the caller re-derives the [`super::Projection`] to learn what that changed.
    pub fn open_window(&mut self) -> WindowId {
        let id = WindowId::from_raw(self.window_ids.alloc());
        self.windows.insert(id, Window { id });

        let pane_id = self.focused_pane_id();
        let pane = self.panes.get_mut(&pane_id).expect("focused pane valid");
        pane.stack.push(id);

        id
    }

    /// Opens a new window into `pane`'s stack at `min(index, len)` without changing the workspace's
    /// focused pane, the active workspace, or which window the pane has focused.
    ///
    /// Inserting before the focused window shifts the focused index so the same window stays
    /// focused; an empty pane's focus is 0. Used to replay a saved stack order in any map order.
    /// Errors with [`LayoutError::UnknownPane`] if `pane` is not in the model.
    pub fn open_window_at(&mut self, pane: PaneId, index: usize) -> Result<WindowId, LayoutError> {
        let p = self
            .panes
            .get_mut(&pane)
            .ok_or(LayoutError::UnknownPane(pane))?;
        let id = WindowId::from_raw(self.window_ids.alloc());
        p.stack.insert(index, id);
        self.windows.insert(id, Window { id });
        Ok(id)
    }

    /// Sets `pane`'s focused stack index. Errors with [`LayoutError::UnknownPane`] or, for an index
    /// not below the stack length, [`LayoutError::StackIndexOutOfRange`] (an empty pane accepts
    /// only 0).
    pub fn set_stack_focus(&mut self, pane: PaneId, index: usize) -> Result<(), LayoutError> {
        let p = self
            .panes
            .get_mut(&pane)
            .ok_or(LayoutError::UnknownPane(pane))?;
        p.stack
            .set_focus(index)
            .map_err(|e| LayoutError::StackIndexOutOfRange {
                pane,
                index,
                len: e.len,
            })
    }

    /// Closes `window`, removing it from whatever pane holds it.
    ///
    /// The pane persists even if this empties it: panes are only removed by the explicit
    /// [`close_pane`](World::close_pane) verb. Pane focus is unchanged; within the pane's stack the
    /// focused window stays focused (closing the focused one shows the next at the same index).
    pub fn close_window(&mut self, window: WindowId) -> Result<(), LayoutError> {
        let location = self
            .locate_window(window)
            .ok_or(LayoutError::UnknownWindow(window))?;
        self.windows.remove(&window);

        let pane = self.panes.get_mut(&location.pane).expect("located pane");
        let removed = pane.stack.remove_item(&window);
        debug_assert!(removed, "window in its pane");

        Ok(())
    }

    /// Closes the focused pane of the active workspace (the `close-pane` verb).
    ///
    /// Only an empty pane that is not the workspace's last pane is removed; the split tree
    /// re-balances (its parent split collapses, promoting the sibling) and focus moves to the leaf
    /// of the promoted sibling that took the removed pane's place. Otherwise nothing changes and
    /// [`LayoutError::PaneNotEmpty`] or [`LayoutError::LastPane`] is returned.
    pub fn close_pane(&mut self) -> Result<(), LayoutError> {
        let workspace = self.active_workspace_id();
        let pane = self.focused_pane_id();
        if !self
            .panes
            .get(&pane)
            .expect("focused pane valid")
            .stack
            .is_empty()
        {
            return Err(LayoutError::PaneNotEmpty);
        }
        let ws = self.workspaces.get_mut(&workspace).expect("workspace");
        let survivor = ws
            .tree
            .remove_and_promote(pane)
            .ok_or(LayoutError::LastPane)?;
        ws.focused_pane = survivor;
        self.panes.remove(&pane);
        Ok(())
    }

    /// Splits the focused pane along `dir`, creating a new empty pane that becomes focused.
    ///
    /// `split-right`/`split-down` map to [`Direction::Right`]/[`Direction::Down`]. The new pane has
    /// no windows yet; the next [`open_window`](World::open_window) lands in it.
    pub fn split(&mut self, dir: Direction) -> PaneId {
        let new_pane_id = PaneId::from_raw(self.pane_ids.alloc());
        let workspace_id = self.active_workspace_id();
        let focused_pane = self.focused_pane_id();

        let ws = self.workspaces.get_mut(&workspace_id).expect("workspace");
        ws.tree.split(focused_pane, dir, new_pane_id);
        ws.focused_pane = new_pane_id;

        self.panes.insert(new_pane_id, Pane::empty(new_pane_id));

        new_pane_id
    }

    /// Opens a new window onto the focused pane's **stack** (does not create a pane).
    ///
    /// Symmetric to [`open_window`](World::open_window) but named to mirror the `stack-window` verb;
    /// the window is pushed onto the current pane and focused.
    pub fn stack_window(&mut self) -> WindowId {
        // Same behaviour as opening a window: push onto the focused pane and focus it. Kept as a
        // distinct verb so callers/keybinds read clearly; both honour the stack model symmetrically.
        self.open_window()
    }

    /// Moves pane focus in `dir` within the active workspace (topological neighbour).
    ///
    /// No-op when there is no neighbour in that direction.
    pub fn focus(&mut self, dir: Direction) {
        let workspace_id = self.active_workspace_id();
        let focused_pane = self.focused_pane_id();
        let ws = self.workspaces.get_mut(&workspace_id).expect("workspace");
        if let Some(neighbor) = ws.tree.neighbor(focused_pane, dir) {
            ws.focused_pane = neighbor;
        }
    }

    /// Focuses `window` wherever it lives: the output owning its workspace becomes the focused
    /// output, that workspace becomes the output's active workspace, the window's pane becomes the
    /// workspace's focused pane, and the pane's stack index points at the window.
    ///
    /// Errors with [`LayoutError::UnknownWindow`] (and mutates nothing) when `window` is not in the
    /// model.
    pub fn focus_window(&mut self, window: WindowId) -> Result<(), LayoutError> {
        let location = self
            .locate_window(window)
            .ok_or(LayoutError::UnknownWindow(window))?;
        self.activate(location.workspace, Some(location.pane))?;
        let pane = self.panes.get_mut(&location.pane).expect("located pane");
        let found = pane.stack.focus_item(&window);
        debug_assert!(found, "window in its pane");
        Ok(())
    }

    /// Moves the focused pane in `dir`, swapping it with its directional neighbour.
    ///
    /// Focus follows the moved pane. No-op when there is no neighbour in that direction.
    pub fn move_(&mut self, dir: Direction) {
        let workspace_id = self.active_workspace_id();
        let focused_pane = self.focused_pane_id();
        let ws = self.workspaces.get_mut(&workspace_id).expect("workspace");
        // After a swap the focused pane's *payload* is now where the neighbour was; focus follows
        // the pane id, which the tree keeps pointing at the moved pane's new slot automatically
        // because we swap payloads — so focused_pane stays correct and we just recompute visibility.
        ws.tree.move_(focused_pane, dir);
    }

    /// Cycles the focused pane's window stack by `delta` steps (wraparound), changing visibility.
    pub fn cycle_stack(&mut self, delta: i64) {
        let pane_id = self.focused_pane_id();
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            pane.stack.cycle(delta);
        }
    }

    /// Adjusts the split ratio governing the focused pane along `dir` by `delta` (clamped).
    ///
    /// This changes pane geometry only (the ratio is read back by [`super::World::project`]); focus
    /// is untouched.
    pub fn resize(&mut self, dir: Direction, delta: f64) {
        let workspace_id = self.active_workspace_id();
        let focused_pane = self.focused_pane_id();
        let ws = self.workspaces.get_mut(&workspace_id).expect("workspace");
        ws.tree.resize(focused_pane, dir, delta);
    }

    /// Finds which workspace + pane currently holds `window`.
    pub(super) fn locate_window(&self, window: WindowId) -> Option<WindowLocation> {
        for ws in self.workspaces.values() {
            for pane_id in ws.tree.leaves() {
                if let Some(pane) = self.panes.get(&pane_id) {
                    if pane.windows().contains(&window) {
                        return Some(WindowLocation {
                            workspace: ws.id,
                            pane: pane_id,
                        });
                    }
                }
            }
        }
        None
    }
}
