//! Data types (`Window`, `Pane`, `Workspace`, `Output`) and `World`'s read-only accessors.

use super::World;
use super::stack::Stack;
use crate::geometry::{GapConfig, Rect, geometry_of_workspace};
use crate::{OutputId, PaneId, WindowId, WorkspaceId, tree::SplitTree};

/// A window: a thin model-side handle. The actual surface payload lives in the compositor, keyed by
/// [`WindowId`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// Stable identity of this window.
    pub id: WindowId,
}

/// A pane: a screen-geometry slot holding an ordered **stack** of windows, one focused/visible.
///
/// The stack is the same [`Stack`] primitive an [`Output`]'s workspace stack uses (decision #3 —
/// "stack" is one concept reused).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    /// Stable identity of this pane.
    pub id: PaneId,
    /// Windows stacked in this pane, in stack order, with the focused (visible) one.
    pub(super) stack: Stack<WindowId>,
}

impl Pane {
    /// An empty pane.
    pub(super) fn empty(id: PaneId) -> Self {
        Self {
            id,
            stack: Stack::new(),
        }
    }

    /// The windows stacked in this pane, in stack order.
    #[must_use]
    pub fn windows(&self) -> &[WindowId] {
        self.stack.as_slice()
    }

    /// The currently focused/visible window, or `None` if the pane is (transiently) empty.
    #[must_use]
    pub fn focused_window(&self) -> Option<WindowId> {
        self.stack.focused().copied()
    }

    /// Index into [`Pane::windows`] of the focused (visible) window (0 when empty).
    #[must_use]
    pub fn focused_index(&self) -> usize {
        self.stack.focused_index()
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
    /// The window shown fullscreen on this workspace, if any (never serialized; see
    /// `World::set_fullscreen`).
    pub(super) fullscreen: Option<WindowId>,
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
/// Symmetric with [`Pane`]'s window stack (decision #3): both are a [`Stack`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// Stable identity of this output.
    pub id: OutputId,
    /// Workspaces stacked on this output, in stack order, with the active one focused.
    pub(super) stack: Stack<WorkspaceId>,
    /// The output's pixel geometry (position + size in the global/Space coordinate space).
    ///
    /// `add_output` seeds this (`World::new()` starts it as an empty `0x0` placeholder); the compositor keeps it in sync via
    /// [`World::set_output_rect`] with the live pixel size of the corresponding physical monitor.
    pub rect: Rect,
}

impl Output {
    /// The active workspace on this output.
    #[must_use]
    pub fn active_workspace(&self) -> WorkspaceId {
        *self.stack.focused().expect("output has ≥1 workspace")
    }

    /// The workspaces stacked on this output, in stack order.
    #[must_use]
    pub fn workspaces(&self) -> &[WorkspaceId] {
        self.stack.as_slice()
    }

    /// Index into [`Output::workspaces`] of the active workspace.
    #[must_use]
    pub fn active_index(&self) -> usize {
        self.stack.focused_index()
    }
}

/// The model focus path at one instant; see [`World::focus_path`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusPath {
    /// The focused output.
    pub output: OutputId,
    /// The active workspace on that output.
    pub workspace: WorkspaceId,
    /// That workspace's name.
    pub workspace_name: String,
    /// The focused pane on that workspace.
    pub pane: PaneId,
    /// The focused window of that pane, if the pane is not empty.
    pub window: Option<WindowId>,
}

impl std::fmt::Display for FocusPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "output={} workspace={}:{:?} pane={} window=",
            self.output.raw(),
            self.workspace.raw(),
            self.workspace_name,
            self.pane.raw()
        )?;
        match self.window {
            Some(id) => write!(f, "{}", id.raw()),
            None => f.write_str("-"),
        }
    }
}

impl World {
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
    /// Visibility geometry is `crate::geometry`'s job; this only reports *which* windows are eligible to be shown.
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
    /// The compositor passes zero gaps here so tiles abut; per-class gaps are applied to windows
    /// inside their tiles (`tiling` config).
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

    /// A snapshot of the full model focus path (output → workspace → pane → window).
    ///
    /// Cheap to take before and after a mutation; its [`Display`](std::fmt::Display) form is the
    /// compact one-line shape the compositor logs at info level so hardware logs show where
    /// splits, spawns and workspace verbs will land.
    #[must_use]
    pub fn focus_path(&self) -> FocusPath {
        let workspace = self.active_workspace_ref();
        FocusPath {
            output: self.focused_output,
            workspace: workspace.id,
            workspace_name: workspace.name.clone(),
            pane: workspace.focused_pane,
            window: self.focused_window(),
        }
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
}
