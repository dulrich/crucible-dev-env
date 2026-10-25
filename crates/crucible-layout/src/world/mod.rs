//! The `World`: arena maps for the whole model plus every mutating operation, split by concern
//! across `types`, `projection`, `windows`, `workspaces`, and `outputs`.
//!
//! `World` is the single source of truth for focus, stacking, and workspace membership (decision #1
//! in `plans/02-tiling-model.md`). It holds arena-style maps keyed by typed ids and a per-workspace
//! [`SplitTree`] of panes. It is `Clone` and free of compositor types so it can be exhaustively
//! unit-tested. Rect math lives in `crate::geometry`; [`World::geometry`] runs it over the model.

use crate::{OutputId, PaneId, WindowId, WorkspaceId, geometry::Rect, ids::IdGen, tree::SplitTree};
use std::collections::BTreeMap;

mod outputs;
mod panes;
mod projection;
mod types;
mod windows;
mod workspaces;

pub use projection::{FocusChange, LayoutChange, Projection, ProjectionDiff};
pub use types::{FocusPath, Output, Pane, Window, Workspace};

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
    /// The referenced pane id is not in the model.
    #[error("unknown pane {0:?}")]
    UnknownPane(PaneId),
    /// The requested move/switch target is the same workspace the window already lives on.
    #[error("window already on target workspace")]
    SameWorkspace,
    /// Attempted to remove the only/last output; at least one output must always exist.
    #[error("cannot remove the last output")]
    LastOutput,
    /// close-pane refused: the focused pane still holds windows (close-pane never closes windows).
    #[error("pane is not empty")]
    PaneNotEmpty,
    /// close-pane refused: the focused pane is the workspace's last pane.
    #[error("cannot close the last pane of a workspace")]
    LastPane,
    /// Another workspace already holds this name (workspace names are unique across the `World`).
    #[error("workspace name {0:?} is already in use")]
    DuplicateName(String),
    /// A stack index was out of range for the pane's window stack.
    #[error("stack index {index} out of range for pane {pane:?} (len {len})")]
    StackIndexOutOfRange {
        /// The pane whose stack was addressed.
        pane: PaneId,
        /// The offending index.
        index: usize,
        /// The pane's current stack length.
        len: usize,
    },
    /// A snapshot split ratio was not finite or not strictly inside `(0, 1)`.
    #[error("split ratio must be finite and strictly between 0 and 1")]
    InvalidRatio,
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

    /// Asserts the model's core invariants. Exposed (hidden) so integration tests can call it; it is
    /// pure assertions with no observable effect in release builds beyond the checks themselves.
    #[doc(hidden)]
    pub fn check_invariants(&self) {
        let output = self
            .outputs
            .get(&self.focused_output)
            .expect("focused output valid");
        assert!(output.active < output.workspaces.len(), "active in range");
        let ws_id = output.active_workspace();
        let ws = self.workspaces.get(&ws_id).expect("active workspace valid");
        assert!(
            ws.panes().contains(&ws.focused_pane),
            "focused pane is in the active workspace"
        );
        ws.tree.check_invariants();
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
        for w in self.workspaces.keys() {
            assert!(
                ws_owner.contains_key(w),
                "workspace {w:?} owned by an output"
            );
        }
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
