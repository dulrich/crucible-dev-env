//! The `World`: arena maps for the whole model plus every mutating operation.
//!
//! `World` is the single source of truth for focus, stacking, and workspace membership (decision #1
//! in `plans/02-tiling-model.md`). It holds arena-style maps keyed by typed ids and a per-workspace
//! [`SplitTree`] of panes. It is `Clone` and free of compositor types so it can be exhaustively
//! unit-tested.
//!
//! Geometry/rect math is deliberately absent — that is P2-WS-2. This module only tracks *which*
//! window is focused/visible, not *where* it is drawn.

use std::collections::BTreeMap;

use crate::geometry::{GapConfig, Rect, geometry_of_workspace};
use crate::ids::IdGen;
use crate::tree::SplitTree;
use crate::{Direction, OutputId, PaneId, WindowId, WorkspaceId};

/// A window: a thin model-side handle. The actual surface payload lives in the compositor, keyed by
/// [`WindowId`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// Stable identity of this window.
    pub id: WindowId,
}

/// A pane: a screen-geometry slot holding an ordered **stack** of windows, one focused/visible.
///
/// The stack and its focused index mirror an [`Output`]'s workspace stack exactly (decision #3 —
/// "stack" is one concept reused). [`Pane::cycle`] and [`Output::cycle`] share the same wraparound
/// logic via [`cycle_index`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    /// Stable identity of this pane.
    pub id: PaneId,
    /// Windows stacked in this pane, in stack order.
    pub windows: Vec<WindowId>,
    /// Index into `windows` of the focused (visible) window.
    pub focused: usize,
}

impl Pane {
    /// The currently focused/visible window, or `None` if the pane is (transiently) empty.
    #[must_use]
    pub fn focused_window(&self) -> Option<WindowId> {
        self.windows.get(self.focused).copied()
    }

    /// Rotates the stack focus by `delta` steps with wraparound.
    fn cycle(&mut self, delta: i64) {
        self.focused = cycle_index(self.focused, self.windows.len(), delta);
    }
}

/// A workspace: a named, stackable arrangement of panes owning a [`SplitTree`] of [`PaneId`]s.
#[derive(Debug, Clone, PartialEq)]
pub struct Workspace {
    /// Stable identity of this workspace.
    pub id: WorkspaceId,
    /// Human-facing name (used by the Phase-3 fuzzy picker).
    pub name: String,
    /// The pane currently focused within this workspace.
    pub focused_pane: PaneId,
    pub(crate) tree: SplitTree<PaneId>,
}

impl Workspace {
    /// The panes in this workspace, in left-to-right / top-to-bottom order.
    #[must_use]
    pub fn panes(&self) -> Vec<PaneId> {
        self.tree.leaves()
    }

    /// Number of panes in this workspace.
    #[must_use]
    pub fn pane_count(&self) -> usize {
        self.tree.leaf_count()
    }
}

/// An output (monitor): holds an ordered **stack** of workspaces, one active at a time.
///
/// Symmetric with [`Pane`]'s window stack (decision #3); switching is [`Output::cycle`] /
/// [`cycle_index`], the same primitive panes use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// Stable identity of this output.
    pub id: OutputId,
    /// Workspaces stacked on this output, in stack order.
    pub workspaces: Vec<WorkspaceId>,
    /// Index into `workspaces` of the active workspace.
    pub active: usize,
}

impl Output {
    /// The active workspace on this output.
    #[must_use]
    pub fn active_workspace(&self) -> WorkspaceId {
        self.workspaces[self.active]
    }

    /// Rotates the active-workspace index by `delta` steps with wraparound.
    fn cycle(&mut self, delta: i64) {
        self.active = cycle_index(self.active, self.workspaces.len(), delta);
    }
}

/// Shared stack-rotation primitive for both pane window-stacks and output workspace-stacks.
///
/// Rotates `current` by `delta` within `[0, len)` with wraparound. Returns `0` for an empty stack.
fn cycle_index(current: usize, len: usize, delta: i64) -> usize {
    if len == 0 {
        return 0;
    }
    let len_i = len as i64;
    let next = (current as i64 + delta).rem_euclid(len_i);
    next as usize
}

/// Describes what the compositor must reconcile after a mutating op (decision: minimal diffs).
///
/// P2-WS-3 applies these to the Smithay `Space`: drop the `removed` windows, re-evaluate visibility
/// when `visibility_changed`, and re-point keyboard focus at `focused_window`. A field being unset
/// means "no change of that kind".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[must_use]
pub struct Reconcile {
    /// Windows that no longer exist in the model and must be unmapped/dropped.
    pub removed: Vec<WindowId>,
    /// The window that should now hold keyboard focus (the focused window of the focused pane on the
    /// active workspace), if any.
    pub focused_window: Option<WindowId>,
    /// Set when the set of *visible* windows may have changed (stack cycle, workspace switch, split,
    /// removal) and the compositor should recompute the render/geometry set.
    pub visibility_changed: bool,
}

impl Reconcile {
    fn removed(removed: Vec<WindowId>) -> Self {
        Self {
            removed,
            visibility_changed: true,
            ..Default::default()
        }
    }
}

/// Errors returned by fallible `World` operations.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LayoutError {
    /// The referenced output id is not in the model.
    #[error("unknown output {0:?}")]
    UnknownOutput(OutputId),
    /// The referenced workspace id is not in the model.
    #[error("unknown workspace {0:?}")]
    UnknownWorkspace(WorkspaceId),
    /// The referenced window id is not in the model.
    #[error("unknown window {0:?}")]
    UnknownWindow(WindowId),
    /// The requested move/switch target is the same workspace the window already lives on.
    #[error("window already on target workspace")]
    SameWorkspace,
}

/// The whole layout model: arena maps + the focused output.
#[derive(Debug, Clone, PartialEq)]
pub struct World {
    outputs: BTreeMap<OutputId, Output>,
    workspaces: BTreeMap<WorkspaceId, Workspace>,
    panes: BTreeMap<PaneId, Pane>,
    windows: BTreeMap<WindowId, Window>,
    focused_output: OutputId,

    output_ids: IdGen,
    workspace_ids: IdGen,
    pane_ids: IdGen,
    window_ids: IdGen,
}

impl World {
    /// Creates a `World` with one output bearing one empty default workspace with one empty pane.
    ///
    /// This guarantees the core invariants from the start: a focused output exists, its active
    /// workspace exists, and that workspace has a focused pane.
    #[must_use]
    pub fn new() -> Self {
        let mut output_ids = IdGen::new();
        let mut workspace_ids = IdGen::new();
        let mut pane_ids = IdGen::new();

        let output_id = OutputId::from_raw(output_ids.alloc());
        let workspace_id = WorkspaceId::from_raw(workspace_ids.alloc());
        let pane_id = PaneId::from_raw(pane_ids.alloc());

        let pane = Pane {
            id: pane_id,
            windows: Vec::new(),
            focused: 0,
        };
        let workspace = Workspace {
            id: workspace_id,
            name: "1".to_string(),
            focused_pane: pane_id,
            tree: SplitTree::new(pane_id),
        };
        let output = Output {
            id: output_id,
            workspaces: vec![workspace_id],
            active: 0,
        };

        let mut outputs = BTreeMap::new();
        outputs.insert(output_id, output);
        let mut workspaces = BTreeMap::new();
        workspaces.insert(workspace_id, workspace);
        let mut panes = BTreeMap::new();
        panes.insert(pane_id, pane);

        Self {
            outputs,
            workspaces,
            panes,
            windows: BTreeMap::new(),
            focused_output: output_id,
            output_ids,
            workspace_ids,
            pane_ids,
            window_ids: IdGen::new(),
        }
    }

    // ---- read-only accessors ------------------------------------------------------------------

    /// The id of the focused output.
    #[must_use]
    pub fn focused_output(&self) -> OutputId {
        self.focused_output
    }

    /// Looks up an output.
    #[must_use]
    pub fn output(&self, id: OutputId) -> Option<&Output> {
        self.outputs.get(&id)
    }

    /// Looks up a workspace.
    #[must_use]
    pub fn workspace(&self, id: WorkspaceId) -> Option<&Workspace> {
        self.workspaces.get(&id)
    }

    /// Looks up a pane.
    #[must_use]
    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.get(&id)
    }

    /// Looks up a window.
    #[must_use]
    pub fn window(&self, id: WindowId) -> Option<&Window> {
        self.windows.get(&id)
    }

    /// Total number of windows in the model.
    #[must_use]
    pub fn window_count(&self) -> usize {
        self.windows.len()
    }

    /// The active workspace id on the focused output.
    #[must_use]
    pub fn active_workspace_id(&self) -> WorkspaceId {
        self.focused_output_ref().active_workspace()
    }

    /// The focused pane id on the active workspace.
    #[must_use]
    pub fn focused_pane_id(&self) -> PaneId {
        self.active_workspace_ref().focused_pane
    }

    /// The focused window (focused window of the focused pane on the active workspace), if any.
    #[must_use]
    pub fn focused_window(&self) -> Option<WindowId> {
        self.panes
            .get(&self.focused_pane_id())
            .and_then(Pane::focused_window)
    }

    /// All windows currently visible: the focused window of each pane in the active workspace.
    ///
    /// Visibility geometry is P2-WS-2; this only reports *which* windows are eligible to be shown.
    #[must_use]
    pub fn visible_windows(&self) -> Vec<WindowId> {
        self.active_workspace_ref()
            .panes()
            .into_iter()
            .filter_map(|pid| self.panes.get(&pid).and_then(Pane::focused_window))
            .collect()
    }

    /// Computes pixel rects for all **visible** windows on the given output.
    ///
    /// For each pane in the output's **active** workspace, the pane's focused window (if any) is
    /// paired with the pixel [`Rect`] computed by subdividing `output_rect` according to the
    /// workspace's split tree and the provided [`GapConfig`].
    ///
    /// - Panes with no windows contribute nothing.
    /// - Stacked (non-focused) windows get no rect.
    /// - Panes on inactive workspaces get no rect.
    ///
    /// If `output_id` is unknown, returns an empty `Vec`.
    ///
    /// See [`GapConfig::default`] for the recommended temporary defaults (Phase 3 will source these
    /// from `crucible-config`).
    #[must_use]
    pub fn geometry(
        &self,
        output_id: OutputId,
        output_rect: Rect,
        gaps: GapConfig,
    ) -> Vec<(WindowId, Rect)> {
        let Some(output) = self.outputs.get(&output_id) else {
            return Vec::new();
        };
        let ws_id = output.active_workspace();
        let Some(workspace) = self.workspaces.get(&ws_id) else {
            return Vec::new();
        };
        let pane_rects = geometry_of_workspace(workspace, output_rect, gaps);
        pane_rects
            .into_iter()
            .filter_map(|(pane_id, rect)| {
                let pane = self.panes.get(&pane_id)?;
                let window_id = pane.focused_window()?;
                Some((window_id, rect))
            })
            .collect()
    }

    fn focused_output_ref(&self) -> &Output {
        self.outputs
            .get(&self.focused_output)
            .expect("focused output always valid")
    }

    fn active_workspace_ref(&self) -> &Workspace {
        let ws = self.focused_output_ref().active_workspace();
        self.workspaces.get(&ws).expect("active workspace valid")
    }

    /// Reconcile reflecting only a focus/visibility recompute (no removals).
    fn focus_reconcile(&self) -> Reconcile {
        Reconcile {
            removed: Vec::new(),
            focused_window: self.focused_window(),
            visibility_changed: true,
        }
    }

    // ---- window lifecycle ---------------------------------------------------------------------

    /// Opens a new window into the focused pane of the active workspace and focuses it.
    ///
    /// Returns the new [`WindowId`] and the reconcile result. The new window becomes the focused
    /// (visible) window of its pane.
    pub fn open_window(&mut self) -> (WindowId, Reconcile) {
        let id = WindowId::from_raw(self.window_ids.alloc());
        self.windows.insert(id, Window { id });

        let pane_id = self.focused_pane_id();
        let pane = self.panes.get_mut(&pane_id).expect("focused pane valid");
        pane.windows.push(id);
        pane.focused = pane.windows.len() - 1;

        (id, self.focus_reconcile())
    }

    /// Closes `window`, removing it from whatever pane holds it.
    ///
    /// If that empties a pane and the pane is not the workspace's last pane, the pane is removed and
    /// the split tree re-balances (its parent split collapses, promoting the sibling). The last pane
    /// of a workspace is kept (empty) so the invariant "a focused pane always exists" holds.
    pub fn close_window(&mut self, window: WindowId) -> Result<Reconcile, LayoutError> {
        let location = self
            .locate_window(window)
            .ok_or(LayoutError::UnknownWindow(window))?;
        self.windows.remove(&window);

        let pane = self.panes.get_mut(&location.pane).expect("located pane");
        let idx = pane
            .windows
            .iter()
            .position(|w| *w == window)
            .expect("window in its pane");
        pane.windows.remove(idx);
        if pane.focused >= pane.windows.len() && !pane.windows.is_empty() {
            pane.focused = pane.windows.len() - 1;
        } else if pane.windows.is_empty() {
            pane.focused = 0;
        }

        if pane.windows.is_empty() {
            self.collapse_empty_pane(location.workspace, location.pane);
        }

        // Closing a window only perturbs focus on the active workspace; either way report the
        // current focused window so WS-3 keeps keyboard focus correct.
        let mut rec = Reconcile::removed(vec![window]);
        rec.focused_window = self.focused_window();
        Ok(rec)
    }

    /// Removes an empty pane from its workspace and re-balances the tree.
    ///
    /// Does nothing if it is the workspace's last pane (kept empty to preserve the focused-pane
    /// invariant). Re-points `focused_pane` to a surviving pane when the focused pane was removed.
    fn collapse_empty_pane(&mut self, workspace: WorkspaceId, pane: PaneId) {
        let ws = self.workspaces.get_mut(&workspace).expect("workspace");
        if ws.tree.leaf_count() <= 1 {
            return; // keep the last pane (empty) — invariant: a focused pane always exists.
        }
        let was_focused = ws.focused_pane == pane;
        if !ws.tree.remove(pane) {
            return;
        }
        self.panes.remove(&pane);

        let ws = self.workspaces.get_mut(&workspace).expect("workspace");
        if was_focused {
            // Promote the first surviving leaf as the new focused pane.
            ws.focused_pane = ws.tree.leaves().first().copied().expect("≥1 pane remains");
        }
    }

    // ---- split & stack ------------------------------------------------------------------------

    /// Splits the focused pane along `dir`, creating a new empty pane that becomes focused.
    ///
    /// `split-right`/`split-down` map to [`Direction::Right`]/[`Direction::Down`]. The new pane has
    /// no windows yet; the next [`open_window`](World::open_window) lands in it.
    pub fn split(&mut self, dir: Direction) -> (PaneId, Reconcile) {
        let new_pane_id = PaneId::from_raw(self.pane_ids.alloc());
        let workspace_id = self.active_workspace_id();
        let focused_pane = self.focused_pane_id();

        let ws = self.workspaces.get_mut(&workspace_id).expect("workspace");
        ws.tree.split(focused_pane, dir, new_pane_id);
        ws.focused_pane = new_pane_id;

        self.panes.insert(
            new_pane_id,
            Pane {
                id: new_pane_id,
                windows: Vec::new(),
                focused: 0,
            },
        );

        (new_pane_id, self.focus_reconcile())
    }

    /// Opens a new window onto the focused pane's **stack** (does not create a pane).
    ///
    /// Symmetric to [`open_window`](World::open_window) but named to mirror the `stack-window` verb;
    /// the window is pushed onto the current pane and focused.
    pub fn stack_window(&mut self) -> (WindowId, Reconcile) {
        // Same behaviour as opening a window: push onto the focused pane and focus it. Kept as a
        // distinct verb so callers/keybinds read clearly; both honour the stack model symmetrically.
        self.open_window()
    }

    // ---- focus / move / cycle / resize --------------------------------------------------------

    /// Moves pane focus in `dir` within the active workspace (topological neighbour).
    ///
    /// No-op (returns a reconcile with unchanged focus) when there is no neighbour in that direction.
    pub fn focus(&mut self, dir: Direction) -> Reconcile {
        let workspace_id = self.active_workspace_id();
        let focused_pane = self.focused_pane_id();
        let ws = self.workspaces.get_mut(&workspace_id).expect("workspace");
        if let Some(neighbor) = ws.tree.neighbor(focused_pane, dir) {
            ws.focused_pane = neighbor;
        }
        self.focus_reconcile()
    }

    /// Moves the focused pane in `dir`, swapping it with its directional neighbour.
    ///
    /// Focus follows the moved pane. No-op when there is no neighbour in that direction.
    pub fn move_(&mut self, dir: Direction) -> Reconcile {
        let workspace_id = self.active_workspace_id();
        let focused_pane = self.focused_pane_id();
        let ws = self.workspaces.get_mut(&workspace_id).expect("workspace");
        // After a swap the focused pane's *payload* is now where the neighbour was; focus follows
        // the pane id, which the tree keeps pointing at the moved pane's new slot automatically
        // because we swap payloads — so focused_pane stays correct and we just recompute visibility.
        ws.tree.move_(focused_pane, dir);
        self.focus_reconcile()
    }

    /// Cycles the focused pane's window stack by `delta` steps (wraparound), changing visibility.
    pub fn cycle_stack(&mut self, delta: i64) -> Reconcile {
        let pane_id = self.focused_pane_id();
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            pane.cycle(delta);
        }
        self.focus_reconcile()
    }

    /// Adjusts the split ratio governing the focused pane along `dir` by `delta` (clamped).
    ///
    /// Pure focus/visibility-wise this changes geometry only (P2-WS-2 reads the ratio); the
    /// reconcile signals a visibility recompute without changing focus.
    pub fn resize(&mut self, dir: Direction, delta: f64) -> Reconcile {
        let workspace_id = self.active_workspace_id();
        let focused_pane = self.focused_pane_id();
        let ws = self.workspaces.get_mut(&workspace_id).expect("workspace");
        ws.tree.resize(focused_pane, dir, delta);
        Reconcile {
            removed: Vec::new(),
            focused_window: self.focused_window(),
            visibility_changed: true,
        }
    }

    // ---- workspaces ---------------------------------------------------------------------------

    /// Creates a new empty workspace on the focused output, pushes it on the stack, and switches to
    /// it. Returns the new workspace id.
    pub fn new_workspace(&mut self, name: impl Into<String>) -> (WorkspaceId, Reconcile) {
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

        let output = self.outputs.get_mut(&self.focused_output).expect("output");
        output.workspaces.push(workspace_id);
        output.active = output.workspaces.len() - 1;

        (workspace_id, self.focus_reconcile())
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
    pub fn switch_workspace(&mut self, target: WorkspaceId) -> Result<Reconcile, LayoutError> {
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
        Ok(self.focus_reconcile())
    }

    /// Switches the focused output's active workspace by `delta` (wraparound).
    pub fn cycle_workspace(&mut self, delta: i64) -> Reconcile {
        let output = self.outputs.get_mut(&self.focused_output).expect("output");
        output.cycle(delta);
        self.focus_reconcile()
    }

    /// Moves `window` from its current pane to the focused pane of `target` workspace.
    ///
    /// The window is removed from its source pane (collapsing it if that empties a non-last pane) and
    /// pushed onto and focused in the target workspace's focused pane. Errors if the window or target
    /// is unknown, or the window already lives on `target`.
    pub fn move_window_to_workspace(
        &mut self,
        window: WindowId,
        target: WorkspaceId,
    ) -> Result<Reconcile, LayoutError> {
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
        if pane.windows.is_empty() {
            self.collapse_empty_pane(location.workspace, location.pane);
        }

        // Attach to the target workspace's focused pane.
        let target_pane_id = self.workspaces.get(&target).expect("target").focused_pane;
        let target_pane = self.panes.get_mut(&target_pane_id).expect("target pane");
        target_pane.windows.push(window);
        target_pane.focused = target_pane.windows.len() - 1;

        Ok(self.focus_reconcile())
    }

    // ---- helpers ------------------------------------------------------------------------------

    /// Finds which workspace + pane currently holds `window`.
    fn locate_window(&self, window: WindowId) -> Option<WindowLocation> {
        for ws in self.workspaces.values() {
            for pane_id in ws.tree.leaves() {
                if let Some(pane) = self.panes.get(&pane_id) {
                    if pane.windows.contains(&window) {
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

    /// Asserts the model's core invariants. Exposed (hidden) so integration tests can call it; it is
    /// pure assertions with no observable effect in release builds beyond the checks themselves.
    #[doc(hidden)]
    pub fn check_invariants(&self) {
        // Focused output is valid.
        let output = self
            .outputs
            .get(&self.focused_output)
            .expect("focused output valid");
        // Active workspace index is valid and refers to a known workspace.
        assert!(output.active < output.workspaces.len(), "active in range");
        let ws_id = output.active_workspace();
        let ws = self.workspaces.get(&ws_id).expect("active workspace valid");
        // Focused pane exists in the active workspace's tree.
        assert!(
            ws.panes().contains(&ws.focused_pane),
            "focused pane is in the active workspace"
        );
        ws.tree.check_invariants();
        // Every output's active index is valid and every listed workspace exists.
        for o in self.outputs.values() {
            assert!(o.active < o.workspaces.len());
            for w in &o.workspaces {
                assert!(self.workspaces.contains_key(w), "listed workspace exists");
            }
        }
        // Every pane referenced by every workspace tree exists, and pane focus is in range.
        for w in self.workspaces.values() {
            w.tree.check_invariants();
            for pid in w.panes() {
                let pane = self.panes.get(&pid).expect("tree pane exists in arena");
                if pane.windows.is_empty() {
                    assert_eq!(pane.focused, 0);
                } else {
                    assert!(pane.focused < pane.windows.len(), "pane focus in range");
                }
                for win in &pane.windows {
                    assert!(self.windows.contains_key(win), "pane window exists");
                }
            }
        }
        // Every window lives in exactly one pane.
        let mut counts: BTreeMap<WindowId, usize> = BTreeMap::new();
        for pane in self.panes.values() {
            for w in &pane.windows {
                *counts.entry(*w).or_default() += 1;
            }
        }
        for win in self.windows.keys() {
            assert_eq!(
                counts.get(win).copied().unwrap_or(0),
                1,
                "window in one pane"
            );
        }
    }
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

/// Where a window currently lives.
struct WindowLocation {
    workspace: WorkspaceId,
    pane: PaneId,
}

#[cfg(test)]
mod cycle_tests {
    use super::cycle_index;

    #[test]
    fn cycle_wraps_forward_and_back() {
        assert_eq!(cycle_index(0, 3, 1), 1);
        assert_eq!(cycle_index(2, 3, 1), 0);
        assert_eq!(cycle_index(0, 3, -1), 2);
        assert_eq!(cycle_index(0, 3, 4), 1);
        assert_eq!(cycle_index(0, 0, 1), 0);
        assert_eq!(cycle_index(0, 1, 1), 0);
    }
}
