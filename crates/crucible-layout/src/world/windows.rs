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
        if self.marked == Some(window) {
            self.marked = None;
        }

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

    /// The window currently marked by [`mark_window`](World::mark_window), if any.
    #[must_use]
    pub fn marked(&self) -> Option<WindowId> {
        self.marked
    }

    /// Moves `window` into `pane`'s stack, wherever either lives.
    ///
    /// The window leaves its source stack (the source pane persists, possibly empty) and is pushed
    /// onto the target stack as its focused window. The target pane, its workspace (made active on
    /// its output) and that output become focused. Errors with [`LayoutError::UnknownWindow`],
    /// [`LayoutError::UnknownPane`] or [`LayoutError::SamePane`], mutating nothing.
    pub fn move_window_to_pane(
        &mut self,
        window: WindowId,
        pane: PaneId,
    ) -> Result<(), LayoutError> {
        let location = self
            .locate_window(window)
            .ok_or(LayoutError::UnknownWindow(window))?;
        if !self.panes.contains_key(&pane) {
            return Err(LayoutError::UnknownPane(pane));
        }
        if location.pane == pane {
            return Err(LayoutError::SamePane);
        }
        // A pane in the arena but in no workspace tree cannot be focused; check before mutating.
        let target_pane_known = self
            .workspaces
            .values()
            .any(|ws| ws.tree.leaves().contains(&pane));
        if !target_pane_known {
            return Err(LayoutError::UnknownPane(pane));
        }

        let source = self.panes.get_mut(&location.pane).expect("source pane");
        let removed = source.stack.remove_item(&window);
        debug_assert!(removed, "window in its pane");
        self.panes
            .get_mut(&pane)
            .expect("target pane")
            .stack
            .push(window);
        self.focus_pane(pane)
            .expect("target pane is in a workspace tree");
        Ok(())
    }

    /// The `mark-window` toggle.
    ///
    /// With no mark, marks the focused window (a no-op when there is none). With a mark whose
    /// window lives in the focused pane, clears it. With a mark elsewhere, moves the marked window
    /// into the focused pane and clears the mark.
    pub fn mark_window(&mut self) {
        let Some(marked) = self.marked else {
            self.marked = self.focused_window();
            return;
        };
        let target = self.focused_pane_id();
        // A same-pane press (or a stale mark) just clears; otherwise place the window.
        let _ = self.move_window_to_pane(marked, target);
        self.marked = None;
    }

    /// Swaps two windows that live in different panes, wherever those panes live.
    ///
    /// `a` takes `b`'s index in `b`'s pane stack and `b` takes `a`'s index in `a`'s stack; each pane
    /// keeps its focused index (so a window newly at a pane's focused index becomes visible there).
    /// Focus is untouched. Errors with [`LayoutError::UnknownWindow`] or [`LayoutError::SamePane`],
    /// mutating nothing.
    pub fn swap_windows(&mut self, a: WindowId, b: WindowId) -> Result<(), LayoutError> {
        let loc_a = self.locate_window(a).ok_or(LayoutError::UnknownWindow(a))?;
        let loc_b = self.locate_window(b).ok_or(LayoutError::UnknownWindow(b))?;
        if loc_a.pane == loc_b.pane {
            return Err(LayoutError::SamePane);
        }
        for (pane_id, out, incoming) in [(loc_a.pane, a, b), (loc_b.pane, b, a)] {
            let stack = &mut self.panes.get_mut(&pane_id).expect("located pane").stack;
            let focus = stack.focused_index();
            let index = stack.position(&out).expect("window in its pane");
            stack.remove(index);
            stack.insert(index, incoming);
            stack
                .set_focus(focus)
                .expect("stack length is unchanged by a swap");
        }
        Ok(())
    }

    /// The `swap-marked-window` toggle.
    ///
    /// Like [`mark_window`](World::mark_window), but swaps. With no mark, marks the focused window
    /// (a no-op when there is none). With a mark whose window lives in the focused pane (or a
    /// stale mark), clears it. With the focused pane empty, moves the marked window in (as
    /// `mark_window` does) and clears. Otherwise swaps the marked window with the focused pane's
    /// active window: the marked window takes that window's place (so it is shown in the focused
    /// pane, which stays focused) and that window takes the marked one's, then clears the mark.
    pub fn swap_marked_window(&mut self) {
        let Some(marked) = self.marked else {
            self.marked = self.focused_window();
            return;
        };
        let target = self.focused_pane_id();
        let same_or_stale = self
            .locate_window(marked)
            .is_none_or(|loc| loc.pane == target);
        if same_or_stale {
            self.marked = None;
        } else if let Some(active) = self.focused_window() {
            if self.swap_windows(marked, active).is_ok() {
                let _ = self.focus_pane(target);
            }
            self.marked = None;
        } else {
            self.mark_window();
        }
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

#[cfg(test)]
mod swap_tests {
    use crate::{Direction::*, LayoutError, Rect, WindowId, World};

    /// Two side-by-side panes: left holds `a`, right holds `b` (focused).
    fn two_panes() -> (World, WindowId, WindowId) {
        let mut w = World::new("1");
        let a = w.open_window();
        w.split(Right);
        let b = w.open_window();
        (w, a, b)
    }

    #[test]
    fn swaps_within_the_workspace() {
        let (mut w, a, b) = two_panes();
        let right = w.focused_pane_id();
        let left = w.pane_of_window(a).unwrap();
        w.focus(Left);
        w.swap_marked_window(); // marks a
        assert_eq!(w.marked(), Some(a));
        w.focus(Right);
        w.swap_marked_window();
        w.check_invariants();
        assert_eq!(w.marked(), None);
        assert_eq!(w.pane_of_window(a), Some(right));
        assert_eq!(w.pane_of_window(b), Some(left));
        assert_eq!(w.focused_pane_id(), right);
        assert_eq!(w.focused_window(), Some(a));
    }

    #[test]
    fn swaps_across_workspaces() {
        let mut w = World::new("1");
        let a = w.open_window();
        let ws1 = w.active_workspace_id();
        w.swap_marked_window();
        let ws2 = w.new_workspace("two").unwrap();
        let b = w.open_window();
        w.swap_marked_window();
        w.check_invariants();
        assert_eq!(w.active_workspace_id(), ws2);
        assert_eq!(w.focused_window(), Some(a));
        w.switch_workspace(ws1).unwrap();
        assert_eq!(w.focused_window(), Some(b));
        w.check_invariants();
    }

    #[test]
    fn swaps_across_outputs() {
        let mut w = World::new("1");
        let first = w.focused_output();
        w.set_output_rect(first, Rect::new(0, 0, 1920, 1080))
            .unwrap();
        let a = w.open_window();
        let src = w.focused_pane_id();
        w.swap_marked_window();
        let second = w.add_output(Rect::new(1920, 0, 1280, 1024), "o2").unwrap();
        w.set_focused_output(second).unwrap();
        let b = w.open_window();
        let dst = w.focused_pane_id();
        w.swap_marked_window();
        w.check_invariants();
        assert_eq!(w.focused_output(), second);
        assert_eq!(w.pane_of_window(a), Some(dst));
        assert_eq!(w.pane_of_window(b), Some(src));
        assert_eq!(w.focused_window(), Some(a));
    }

    #[test]
    fn visible_marked_window_leaves_displaced_window_visible_in_source() {
        let (mut w, a, b) = two_panes();
        let left = w.pane_of_window(a).unwrap();
        w.focus(Left);
        w.swap_marked_window(); // a is visible in left
        w.focus(Right);
        w.swap_marked_window();
        w.check_invariants();
        assert_eq!(w.pane(left).unwrap().focused_window(), Some(b));
    }

    #[test]
    fn stacked_marked_window_leaves_source_visible_window_alone() {
        let mut w = World::new("1");
        let a = w.open_window();
        let a2 = w.open_window(); // a2 visible, a stacked beneath
        w.split(Right);
        let b = w.open_window();
        let left = w.pane_of_window(a).unwrap();
        w.focus(Left);
        w.cycle_stack(1); // a visible
        w.swap_marked_window(); // marks a
        w.cycle_stack(1); // a2 visible again, a stacked, still marked
        w.focus(Right);
        w.swap_marked_window();
        w.check_invariants();
        assert_eq!(w.pane(left).unwrap().focused_window(), Some(a2));
        assert_eq!(w.pane(left).unwrap().windows(), &[b, a2]);
        assert_eq!(w.focused_window(), Some(a));
    }

    #[test]
    fn empty_destination_moves_the_marked_window() {
        let mut w = World::new("1");
        let a = w.open_window();
        w.split(Right);
        let empty = w.focused_pane_id();
        w.focus(Left);
        w.swap_marked_window();
        w.focus(Right);
        w.swap_marked_window();
        w.check_invariants();
        assert_eq!(w.marked(), None);
        assert_eq!(w.pane_of_window(a), Some(empty));
        assert_eq!(w.focused_window(), Some(a));
    }

    #[test]
    fn same_pane_press_clears_and_changes_nothing_else() {
        let mut w = World::new("1");
        let a = w.open_window();
        let pane = w.focused_pane_id();
        w.swap_marked_window();
        w.swap_marked_window();
        w.check_invariants();
        assert_eq!(w.marked(), None);
        assert_eq!(w.pane_of_window(a), Some(pane));
        assert_eq!(w.focused_window(), Some(a));
    }

    #[test]
    fn no_mark_marks_the_focused_window() {
        let mut w = World::new("1");
        let a = w.open_window();
        w.swap_marked_window();
        w.check_invariants();
        assert_eq!(w.marked(), Some(a));
    }

    #[test]
    fn swap_windows_errors_mutate_nothing() {
        let (mut w, a, b) = two_panes();
        let b2 = w.open_window(); // same pane as b
        let before = w.clone();
        assert_eq!(w.swap_windows(b, b2), Err(LayoutError::SamePane));
        assert_eq!(w, before);
        let ghost = WindowId::from_raw(u64::MAX);
        assert_eq!(
            w.swap_windows(a, ghost),
            Err(LayoutError::UnknownWindow(ghost))
        );
        assert_eq!(w, before);
        assert_eq!(
            w.swap_windows(ghost, a),
            Err(LayoutError::UnknownWindow(ghost))
        );
        assert_eq!(w, before);
        w.check_invariants();
    }
}
