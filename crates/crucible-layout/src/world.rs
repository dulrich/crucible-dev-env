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
    /// The output's pixel geometry (position + size in the global/Space coordinate space).
    ///
    /// `World::new()` and `add_output` seed this; the compositor keeps it in sync via
    /// [`World::set_output_rect`] with the live pixel size of the corresponding physical monitor.
    pub rect: Rect,
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

/// The complete, derived picture of what the compositor must show: **the** projection.
///
/// A `Projection` is *derived* from a [`World`] snapshot ([`World::project`]); it is never mutated
/// directly and carries no change hints. It is the single source of visible geometry and focus —
/// nothing infers visibility from a mutation's return value or from the compositor's `Space`.
/// Comparing the previously applied projection with a freshly derived one
/// ([`Projection::diff`]) yields the minimal work the compositor must perform.
///
/// The window set is canonicalised (ascending [`WindowId`], one entry per id) so equality and diffs
/// are order-independent and deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Projection {
    /// Visible windows and their rects, ascending by id.
    windows: Vec<(WindowId, Rect)>,
    /// The window that should hold keyboard focus, if any.
    focus: Option<WindowId>,
}

impl Projection {
    /// Builds a projection from a visible `(WindowId, Rect)` set plus the focused identity.
    ///
    /// The set is sorted by [`WindowId`] and de-duplicated (first entry per id wins).
    #[must_use]
    pub fn new(
        windows: impl IntoIterator<Item = (WindowId, Rect)>,
        focus: Option<WindowId>,
    ) -> Self {
        let mut windows: Vec<(WindowId, Rect)> = windows.into_iter().collect();
        windows.sort_by_key(|(id, _)| *id);
        windows.dedup_by_key(|(id, _)| *id);
        Self { windows, focus }
    }

    /// The visible windows and their rects, ascending by id.
    #[must_use]
    pub fn windows(&self) -> &[(WindowId, Rect)] {
        &self.windows
    }

    /// The window that should hold keyboard focus, if any.
    #[must_use]
    pub fn focus(&self) -> Option<WindowId> {
        self.focus
    }

    /// The rect of `id` if it is visible in this projection.
    #[must_use]
    pub fn rect_of(&self, id: WindowId) -> Option<Rect> {
        self.windows
            .binary_search_by_key(&id, |(wid, _)| *wid)
            .ok()
            .map(|idx| self.windows[idx].1)
    }

    /// Whether `id` is visible in this projection.
    #[must_use]
    pub fn is_visible(&self, id: WindowId) -> bool {
        self.rect_of(id).is_some()
    }

    /// The minimal work turning `self` (the applied projection) into `next`.
    ///
    /// Guarantees:
    ///
    /// - a window visible in both with an identical rect yields **no** change (idempotence — the
    ///   property that prevents configure storms);
    /// - a window whose rect changed yields [`LayoutChange::Place`], with `resized` set iff its size
    ///   differs (so position-only moves skip the `configure`);
    /// - a newly visible window yields `Place { resized: true }`;
    /// - a window that stopped being visible yields [`LayoutChange::Hide`];
    /// - focus reports [`FocusChange::Unchanged`] unless the focused identity actually differs.
    ///
    /// Changes come out in ascending [`WindowId`] order (deterministic).
    pub fn diff(&self, next: &Projection) -> ProjectionDiff {
        let (prev, next_windows) = (&self.windows, &next.windows);
        let mut changes = Vec::new();
        let (mut i, mut j) = (0usize, 0usize);
        loop {
            match (prev.get(i), next_windows.get(j)) {
                (None, None) => break,
                (Some((pid, _)), None) => {
                    changes.push(LayoutChange::Hide { id: *pid });
                    i += 1;
                }
                (None, Some((nid, nrect))) => {
                    changes.push(LayoutChange::Place {
                        id: *nid,
                        rect: *nrect,
                        resized: true,
                    });
                    j += 1;
                }
                (Some((pid, prect)), Some((nid, nrect))) => {
                    if pid == nid {
                        if prect != nrect {
                            changes.push(LayoutChange::Place {
                                id: *nid,
                                rect: *nrect,
                                resized: prect.width != nrect.width || prect.height != nrect.height,
                            });
                        }
                        i += 1;
                        j += 1;
                    } else if pid < nid {
                        changes.push(LayoutChange::Hide { id: *pid });
                        i += 1;
                    } else {
                        changes.push(LayoutChange::Place {
                            id: *nid,
                            rect: *nrect,
                            resized: true,
                        });
                        j += 1;
                    }
                }
            }
        }

        let focus = if self.focus == next.focus {
            FocusChange::Unchanged
        } else {
            FocusChange::To(next.focus)
        };
        ProjectionDiff { changes, focus }
    }
}

/// A single window-placement change the compositor must apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutChange {
    /// The window is visible at `rect` and was newly shown, moved, or resized.
    Place {
        /// The window to place.
        id: WindowId,
        /// Its new rect, in the same coordinate space the projection was derived in.
        rect: Rect,
        /// Whether the size changed (a `configure` is required), vs. a position-only move.
        resized: bool,
    },
    /// The window is no longer visible and must be unmapped.
    Hide {
        /// The window to hide.
        id: WindowId,
    },
}

/// How keyboard focus differs between two projections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusChange {
    /// Both projections name the same focused identity: nothing to re-point.
    Unchanged,
    /// Focus must move to this window (`None` = no window should hold focus).
    To(Option<WindowId>),
}

/// The minimal work turning one [`Projection`] into another (see [`Projection::diff`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct ProjectionDiff {
    /// Geometry/visibility changes, ascending by [`WindowId`].
    pub changes: Vec<LayoutChange>,
    /// Whether (and where) keyboard focus must move.
    pub focus: FocusChange,
}

impl ProjectionDiff {
    /// `true` when the two projections were identical: applying this diff is a no-op.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.focus == FocusChange::Unchanged
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
    /// Attempted to remove the only/last output; at least one output must always exist.
    #[error("cannot remove the last output")]
    LastOutput,
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
            // Placeholder; the compositor calls `set_output_rect` with the real pixel size.
            rect: Rect::new(0, 0, 0, 0),
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

    /// Iterates over every output as `(OutputId, &Output)`, in ascending id order.
    pub fn outputs(&self) -> impl Iterator<Item = (OutputId, &Output)> {
        self.outputs.iter().map(|(id, o)| (*id, o))
    }

    /// Iterates over every output id, in ascending order.
    pub fn output_ids(&self) -> impl Iterator<Item = OutputId> + '_ {
        self.outputs.keys().copied()
    }

    /// The number of outputs currently in the model (always ≥ 1).
    #[must_use]
    pub fn output_count(&self) -> usize {
        self.outputs.len()
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

    /// Like [`World::geometry`] but uses the output's **stored** [`Output::rect`] origin/size.
    ///
    /// Convenience for callers that have kept the model's rect up to date via
    /// [`World::set_output_rect`]; the returned rects are in the output's own coordinate space
    /// (origin at `rect.x`/`rect.y`). Returns an empty `Vec` if `output_id` is unknown.
    #[must_use]
    pub fn geometry_for(&self, output_id: OutputId, gaps: GapConfig) -> Vec<(WindowId, Rect)> {
        let Some(output) = self.outputs.get(&output_id) else {
            return Vec::new();
        };
        self.geometry(output_id, output.rect, gaps)
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

    // ---- projection ---------------------------------------------------------------------------

    /// Derives the [`Projection`]: every visible window's rect plus the focused identity.
    ///
    /// `outputs` pairs each model [`OutputId`] with its **global** rect (origin + pixel size, as the
    /// compositor knows it). Each output contributes *its own* active workspace's visible windows,
    /// laid out inside that rect and offset into the global coordinate space. Output ids the model
    /// does not know contribute nothing.
    ///
    /// This is the sole source of visible geometry and focus; mutating ops return no hints.
    #[must_use]
    pub fn project(&self, outputs: &[(OutputId, Rect)], gaps: GapConfig) -> Projection {
        let mut windows: Vec<(WindowId, Rect)> = Vec::new();
        for (output_id, global) in outputs {
            let local = Rect::new(0, 0, global.width, global.height);
            for (window, rect) in self.geometry(*output_id, local, gaps) {
                windows.push((
                    window,
                    Rect::new(
                        rect.x + global.x,
                        rect.y + global.y,
                        rect.width,
                        rect.height,
                    ),
                ));
            }
        }
        Projection::new(windows, self.focused_window())
    }

    /// Like [`World::project`] but using every output's **stored** [`Output::rect`].
    ///
    /// Convenient for callers (and tests) that keep the model's rects up to date via
    /// [`World::set_output_rect`].
    #[must_use]
    pub fn project_stored(&self, gaps: GapConfig) -> Projection {
        let outputs: Vec<(OutputId, Rect)> =
            self.outputs.iter().map(|(id, o)| (*id, o.rect)).collect();
        self.project(&outputs, gaps)
    }

    // ---- window lifecycle ---------------------------------------------------------------------

    /// Opens a new window into the focused pane of the active workspace and focuses it.
    ///
    /// Returns the new [`WindowId`]. The new window becomes the focused (visible) window of its
    /// pane; the caller re-derives the [`Projection`] to learn what that changed.
    pub fn open_window(&mut self) -> WindowId {
        let id = WindowId::from_raw(self.window_ids.alloc());
        self.windows.insert(id, Window { id });

        let pane_id = self.focused_pane_id();
        let pane = self.panes.get_mut(&pane_id).expect("focused pane valid");
        pane.windows.push(id);
        pane.focused = pane.windows.len() - 1;

        id
    }

    /// Closes `window`, removing it from whatever pane holds it.
    ///
    /// If that empties a pane and the pane is not the workspace's last pane, the pane is removed and
    /// the split tree re-balances (its parent split collapses, promoting the sibling). The last pane
    /// of a workspace is kept (empty) so the invariant "a focused pane always exists" holds.
    pub fn close_window(&mut self, window: WindowId) -> Result<(), LayoutError> {
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

        Ok(())
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
    pub fn split(&mut self, dir: Direction) -> PaneId {
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

    // ---- focus / move / cycle / resize --------------------------------------------------------

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
            pane.cycle(delta);
        }
    }

    /// Adjusts the split ratio governing the focused pane along `dir` by `delta` (clamped).
    ///
    /// This changes pane geometry only (the ratio is read back by [`World::project`]); focus is
    /// untouched.
    pub fn resize(&mut self, dir: Direction, delta: f64) {
        let workspace_id = self.active_workspace_id();
        let focused_pane = self.focused_pane_id();
        let ws = self.workspaces.get_mut(&workspace_id).expect("workspace");
        ws.tree.resize(focused_pane, dir, delta);
    }

    // ---- workspaces ---------------------------------------------------------------------------

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
    /// The window is removed from its source pane (collapsing it if that empties a non-last pane) and
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
        if pane.windows.is_empty() {
            self.collapse_empty_pane(location.workspace, location.pane);
        }

        // Attach to the target workspace's focused pane.
        let target_pane_id = self.workspaces.get(&target).expect("target").focused_pane;
        let target_pane = self.panes.get_mut(&target_pane_id).expect("target pane");
        target_pane.windows.push(window);
        target_pane.focused = target_pane.windows.len() - 1;

        Ok(())
    }

    // ---- outputs ------------------------------------------------------------------------------

    /// Creates a fresh empty workspace (one empty pane) and inserts both into the arena maps.
    ///
    /// Shared by [`World::new`]-style seeding, [`World::add_output`], and [`World::new_workspace`].
    /// Returns the new workspace id. Does **not** attach the workspace to any output.
    fn build_workspace(&mut self, name: impl Into<String>) -> WorkspaceId {
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

    /// Adds a new output with `rect`, giving it its own independent initial workspace + empty pane.
    ///
    /// Mirrors what [`World::new`] does for the first output. The new output starts with no windows
    /// and is **not** made the focused output, so nothing currently visible changes. Returns the new
    /// [`OutputId`].
    pub fn add_output(&mut self, rect: Rect) -> OutputId {
        let output_id = OutputId::from_raw(self.output_ids.alloc());
        let workspace_id = self.build_workspace("1");
        self.outputs.insert(
            output_id,
            Output {
                id: output_id,
                workspaces: vec![workspace_id],
                active: 0,
                rect,
            },
        );
        // New output is empty and not focused: nothing visible changes.
        output_id
    }

    /// Removes an output **without destroying any windows**.
    ///
    /// The removed output's entire workspace stack (and thus its panes and windows, which live in
    /// the arena maps and are never touched here) is **appended onto a surviving output's**
    /// workspace stack. The survivor is the focused output if it survives, else the lowest remaining
    /// [`OutputId`]. The migrated workspaces become inactive on the survivor, so their windows are
    /// hidden, not closed.
    ///
    /// If `id` was the focused output, focus repoints to the survivor. Returns
    /// [`LayoutError::LastOutput`] when asked to remove the only output, or
    /// [`LayoutError::UnknownOutput`] for an unknown id.
    pub fn remove_output(&mut self, id: OutputId) -> Result<(), LayoutError> {
        if !self.outputs.contains_key(&id) {
            return Err(LayoutError::UnknownOutput(id));
        }
        if self.outputs.len() == 1 {
            return Err(LayoutError::LastOutput);
        }

        // Pick the survivor: the focused output if it isn't the one being removed, else the lowest
        // remaining output id.
        let survivor = if self.focused_output != id {
            self.focused_output
        } else {
            self.outputs
                .keys()
                .copied()
                .find(|oid| *oid != id)
                .expect("≥1 other output remains")
        };

        let removed = self.outputs.remove(&id).expect("output present");
        // Migrate the removed output's workspaces (with their panes/windows) onto the survivor.
        // They become inactive (the survivor keeps its own active index), so windows are hidden.
        let survivor_output = self.outputs.get_mut(&survivor).expect("survivor present");
        survivor_output.workspaces.extend(removed.workspaces);

        // Repoint focus if we just removed the focused output.
        if self.focused_output == id {
            self.focused_output = survivor;
        }

        // Windows from the migrated (now inactive) workspaces are hidden, not removed.
        Ok(())
    }

    /// Sets the stored pixel geometry of `id`. Errors if the output is unknown.
    pub fn set_output_rect(&mut self, id: OutputId, rect: Rect) -> Result<(), LayoutError> {
        let output = self
            .outputs
            .get_mut(&id)
            .ok_or(LayoutError::UnknownOutput(id))?;
        output.rect = rect;
        Ok(())
    }

    /// Routes focus to `id` so subsequent window/workspace ops act on that output.
    ///
    /// The next derived [`Projection`] reports the newly focused output's focus. Errors if `id` is
    /// unknown.
    pub fn set_focused_output(&mut self, id: OutputId) -> Result<(), LayoutError> {
        if !self.outputs.contains_key(&id) {
            return Err(LayoutError::UnknownOutput(id));
        }
        self.focused_output = id;
        Ok(())
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
        // At least one output always exists.
        assert!(!self.outputs.is_empty(), "≥1 output always");
        // Every output's active index is valid, has ≥1 workspace, and every listed workspace exists.
        // Also assert each workspace belongs to exactly one output (no id aliasing after migrate).
        let mut ws_owner: BTreeMap<WorkspaceId, OutputId> = BTreeMap::new();
        for o in self.outputs.values() {
            assert!(!o.workspaces.is_empty(), "every output has ≥1 workspace");
            assert!(o.active < o.workspaces.len());
            for w in &o.workspaces {
                assert!(self.workspaces.contains_key(w), "listed workspace exists");
                let prev = ws_owner.insert(*w, o.id);
                assert!(
                    prev.is_none(),
                    "workspace {w:?} belongs to exactly one output"
                );
            }
        }
        // Every workspace is owned by some output (no orphaned workspaces).
        for w in self.workspaces.keys() {
            assert!(
                ws_owner.contains_key(w),
                "workspace {w:?} owned by an output"
            );
        }
        // Every workspace's focused pane exists in its own tree.
        for w in self.workspaces.values() {
            assert!(
                w.panes().contains(&w.focused_pane),
                "workspace focused pane is in its tree"
            );
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

#[cfg(test)]
mod multi_output_tests {
    use super::*;
    use crate::Direction;

    const GAPS: GapConfig = GapConfig::new(0, 0);
    const R1: Rect = Rect::new(0, 0, 1920, 1080);
    const R2: Rect = Rect::new(1920, 0, 1280, 1024);

    #[test]
    fn new_world_has_one_output_with_placeholder_rect() {
        let w = World::new();
        assert_eq!(w.output_count(), 1);
        let id = w.focused_output();
        assert_eq!(w.output(id).unwrap().rect, Rect::new(0, 0, 0, 0));
        w.check_invariants();
    }

    #[test]
    fn add_output_creates_an_independent_stack() {
        let mut w = World::new();
        let first = w.focused_output();
        let before = w.project_stored(GAPS);
        let second = w.add_output(R2);
        // New output starts empty and does not become focused; no visible change.
        assert!(before.diff(&w.project_stored(GAPS)).is_empty());
        assert_ne!(first, second);
        assert_eq!(w.focused_output(), first);
        assert_eq!(w.output_count(), 2);

        // The second output owns its own single workspace, distinct from the first's.
        let ws_first = w.output(first).unwrap().active_workspace();
        let ws_second = w.output(second).unwrap().active_workspace();
        assert_ne!(ws_first, ws_second);
        assert_eq!(w.output(second).unwrap().rect, R2);
        w.check_invariants();
    }

    #[test]
    fn ops_on_focused_output_do_not_disturb_other_outputs() {
        let mut w = World::new();
        let first = w.focused_output();
        let second = w.add_output(R2);

        let second_before = w.output(second).unwrap().clone();
        // All of these act on `first` (the focused output).
        w.open_window();
        w.split(Direction::Right);
        w.new_workspace("scratch");

        // The second output is byte-for-byte unchanged.
        assert_eq!(w.output(second).unwrap(), &second_before);
        // ...but the first output gained a workspace.
        assert_eq!(w.output(first).unwrap().workspaces.len(), 2);
        w.check_invariants();
    }

    #[test]
    fn switching_focused_output_then_ops_affect_only_that_output() {
        let mut w = World::new();
        let first = w.focused_output();
        let second = w.add_output(R2);

        let first_before = w.output(first).unwrap().clone();
        w.set_focused_output(second).unwrap();
        assert_eq!(w.focused_output(), second);

        let win = w.open_window();
        w.new_workspace("2");

        // First output untouched.
        assert_eq!(w.output(first).unwrap(), &first_before);
        // Second output now has two workspaces, and the window landed on it.
        assert_eq!(w.output(second).unwrap().workspaces.len(), 2);
        assert!(w.locate_window(win).is_some());
        w.check_invariants();
    }

    #[test]
    fn per_output_geometry_is_independent() {
        let mut w = World::new();
        let first = w.focused_output();
        w.set_output_rect(first, R1).unwrap();
        let second = w.add_output(R2);

        // A window on the first output.
        let win1 = w.open_window();
        // Switch focus and add a window on the second output.
        w.set_focused_output(second).unwrap();
        let win2 = w.open_window();

        let g1 = w.geometry(first, R1, GAPS);
        assert_eq!(g1.len(), 1);
        assert_eq!(g1[0], (win1, R1));

        let g2 = w.geometry(second, R2, GAPS);
        assert_eq!(g2.len(), 1);
        // geometry uses an output-relative rect; we pass R2's own size at origin in real use, but
        // here we pass R2 directly and expect the window to fill it.
        assert_eq!(g2[0].0, win2);

        // geometry_for uses the stored rect (R2, offset at x=1920).
        let g2_stored = w.geometry_for(second, GAPS);
        assert_eq!(g2_stored.len(), 1);
        assert_eq!(g2_stored[0], (win2, R2));
        w.check_invariants();
    }

    #[test]
    fn remove_output_migrates_workspaces_and_windows_without_loss() {
        let mut w = World::new();
        let first = w.focused_output();
        let second = w.add_output(R2);

        // Put a window on the second output (on its own workspace).
        w.set_focused_output(second).unwrap();
        let win = w.open_window();
        w.new_workspace("extra"); // second now has 2 workspaces
        let win2 = w.open_window();
        assert_eq!(w.window_count(), 2);

        // Refocus first, then remove the second output.
        w.set_focused_output(first).unwrap();
        let first_ws_before = w.output(first).unwrap().workspaces.len();
        w.remove_output(second).unwrap();

        // No windows destroyed; both still locatable.
        assert_eq!(w.window_count(), 2);
        assert!(w.locate_window(win).is_some());
        assert!(w.locate_window(win2).is_some());
        // The migrated workspaces are inactive on the survivor, so their windows are hidden, not
        // closed: neither appears in the projection.
        let projection = w.project_stored(GAPS);
        assert!(!projection.is_visible(win));
        assert!(!projection.is_visible(win2));

        // Second output's 2 workspaces appended onto the first.
        assert_eq!(
            w.output(first).unwrap().workspaces.len(),
            first_ws_before + 2
        );
        assert_eq!(w.output_count(), 1);
        w.check_invariants();
    }

    #[test]
    fn remove_focused_output_repoints_focus_to_survivor() {
        let mut w = World::new();
        let first = w.focused_output();
        let second = w.add_output(R2);

        w.set_focused_output(second).unwrap();
        assert_eq!(w.focused_output(), second);

        w.remove_output(second).unwrap();
        assert_eq!(w.focused_output(), first);
        assert_eq!(w.project_stored(GAPS).focus(), w.focused_window());
        w.check_invariants();
    }

    #[test]
    fn remove_last_output_errors() {
        let mut w = World::new();
        let only = w.focused_output();
        assert_eq!(w.remove_output(only), Err(LayoutError::LastOutput));
        w.check_invariants();
    }

    #[test]
    fn remove_unknown_output_errors() {
        let mut w = World::new();
        let bogus = OutputId::from_raw(999);
        assert_eq!(
            w.remove_output(bogus),
            Err(LayoutError::UnknownOutput(bogus))
        );
    }

    #[test]
    fn set_output_rect_and_set_focused_errors_on_unknown() {
        let mut w = World::new();
        let bogus = OutputId::from_raw(999);
        assert_eq!(
            w.set_output_rect(bogus, R1),
            Err(LayoutError::UnknownOutput(bogus))
        );
        assert_eq!(
            w.set_focused_output(bogus),
            Err(LayoutError::UnknownOutput(bogus))
        );
    }

    #[test]
    fn remove_survivor_is_lowest_id_when_focused_is_removed() {
        // Three outputs; focus the highest, remove it -> survivor should be the focused-if-survives
        // rule; but when focused is removed, survivor is the lowest remaining id.
        let mut w = World::new();
        let first = w.focused_output();
        let second = w.add_output(R2);
        let third = w.add_output(R2);

        w.set_focused_output(third).unwrap();
        w.remove_output(third).unwrap();
        // Lowest remaining id is `first`.
        assert_eq!(w.focused_output(), first);
        assert!(w.output(first).is_some());
        assert!(w.output(second).is_some());
        w.check_invariants();
    }
}

#[cfg(test)]
mod projection_tests {
    use super::*;

    const fn r(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect::new(x, y, w, h)
    }

    fn wid(n: u64) -> WindowId {
        WindowId::from_raw(n)
    }

    fn proj(windows: &[(WindowId, Rect)], focus: Option<WindowId>) -> Projection {
        Projection::new(windows.iter().copied(), focus)
    }

    #[test]
    fn construction_canonicalises_order_and_duplicates() {
        let a = proj(
            &[(wid(2), r(0, 0, 10, 10)), (wid(1), r(10, 0, 10, 10))],
            None,
        );
        let b = proj(
            &[(wid(1), r(10, 0, 10, 10)), (wid(2), r(0, 0, 10, 10))],
            None,
        );
        assert_eq!(a, b);
        assert_eq!(a.windows()[0].0, wid(1));

        // A duplicate id keeps the first entry only.
        let dup = proj(&[(wid(1), r(0, 0, 1, 1)), (wid(1), r(5, 5, 5, 5))], None);
        assert_eq!(dup.windows().len(), 1);
    }

    #[test]
    fn unchanged_projection_diffs_to_nothing() {
        let set = [(wid(0), r(0, 0, 100, 100)), (wid(1), r(100, 0, 100, 100))];
        let p = proj(&set, Some(wid(1)));
        let diff = p.diff(&p.clone());
        assert!(diff.is_empty());
        assert_eq!(diff.focus, FocusChange::Unchanged);
        assert!(diff.changes.is_empty());
    }

    #[test]
    fn newly_visible_window_is_placed_with_configure() {
        let diff = Projection::default().diff(&proj(&[(wid(0), r(0, 0, 800, 600))], Some(wid(0))));
        assert_eq!(
            diff.changes,
            vec![LayoutChange::Place {
                id: wid(0),
                rect: r(0, 0, 800, 600),
                resized: true,
            }]
        );
        assert_eq!(diff.focus, FocusChange::To(Some(wid(0))));
    }

    #[test]
    fn window_that_stopped_being_visible_is_hidden() {
        let prev = proj(&[(wid(0), r(0, 0, 800, 600))], Some(wid(0)));
        let diff = prev.diff(&Projection::default());
        assert_eq!(diff.changes, vec![LayoutChange::Hide { id: wid(0) }]);
        assert_eq!(diff.focus, FocusChange::To(None));
    }

    #[test]
    fn position_only_move_does_not_set_resized() {
        let prev = proj(&[(wid(0), r(0, 0, 400, 600))], Some(wid(0)));
        let next = proj(&[(wid(0), r(410, 0, 400, 600))], Some(wid(0)));
        assert_eq!(
            prev.diff(&next).changes,
            vec![LayoutChange::Place {
                id: wid(0),
                rect: r(410, 0, 400, 600),
                resized: false,
            }]
        );
    }

    #[test]
    fn resize_sets_resized_flag() {
        let prev = proj(&[(wid(0), r(0, 0, 800, 600))], Some(wid(0)));
        let next = proj(&[(wid(0), r(0, 0, 400, 600))], Some(wid(0)));
        assert_eq!(
            prev.diff(&next).changes,
            vec![LayoutChange::Place {
                id: wid(0),
                rect: r(0, 0, 400, 600),
                resized: true,
            }]
        );
    }

    #[test]
    fn focus_only_change_moves_focus_without_geometry_work() {
        let set = [(wid(0), r(0, 0, 400, 600)), (wid(1), r(400, 0, 400, 600))];
        let prev = proj(&set, Some(wid(0)));
        let next = proj(&set, Some(wid(1)));
        let diff = prev.diff(&next);
        assert!(diff.changes.is_empty());
        assert_eq!(diff.focus, FocusChange::To(Some(wid(1))));
        assert!(!diff.is_empty());
    }

    #[test]
    fn split_re_tiles_only_the_two_affected_panes() {
        // One full-width window, then a second appears taking the right half: the first shrinks to
        // the left half (resized) and the second is newly placed. Nothing is spuriously hidden.
        let prev = proj(&[(wid(0), r(0, 0, 800, 600))], Some(wid(0)));
        let next = proj(
            &[(wid(0), r(0, 0, 400, 600)), (wid(1), r(400, 0, 400, 600))],
            Some(wid(1)),
        );
        assert_eq!(
            prev.diff(&next).changes,
            vec![
                LayoutChange::Place {
                    id: wid(0),
                    rect: r(0, 0, 400, 600),
                    resized: true,
                },
                LayoutChange::Place {
                    id: wid(1),
                    rect: r(400, 0, 400, 600),
                    resized: true,
                },
            ]
        );
    }

    #[test]
    fn stack_cycle_hides_one_shows_another_in_place() {
        // A stacked pane: window 0 visible, then cycle makes window 1 visible in the SAME rect.
        let rect = r(0, 0, 800, 600);
        let prev = proj(&[(wid(0), rect)], Some(wid(0)));
        let next = proj(&[(wid(1), rect)], Some(wid(1)));
        assert_eq!(
            prev.diff(&next).changes,
            vec![
                LayoutChange::Hide { id: wid(0) },
                LayoutChange::Place {
                    id: wid(1),
                    rect,
                    resized: true,
                },
            ]
        );
    }

    #[test]
    fn workspace_switch_hides_all_old_shows_all_new() {
        let prev = proj(
            &[(wid(0), r(0, 0, 400, 600)), (wid(1), r(400, 0, 400, 600))],
            Some(wid(1)),
        );
        let next = proj(&[(wid(2), r(0, 0, 800, 600))], Some(wid(2)));
        let changes = prev.diff(&next).changes;
        assert_eq!(changes.len(), 3);
        assert!(changes.contains(&LayoutChange::Hide { id: wid(0) }));
        assert!(changes.contains(&LayoutChange::Hide { id: wid(1) }));
        assert!(changes.contains(&LayoutChange::Place {
            id: wid(2),
            rect: r(0, 0, 800, 600),
            resized: true,
        }));
    }

    #[test]
    fn rect_lookups_answer_visibility() {
        let p = proj(&[(wid(3), r(1, 2, 3, 4))], Some(wid(3)));
        assert_eq!(p.rect_of(wid(3)), Some(r(1, 2, 3, 4)));
        assert!(p.is_visible(wid(3)));
        assert_eq!(p.rect_of(wid(9)), None);
        assert!(!p.is_visible(wid(9)));
    }
}
