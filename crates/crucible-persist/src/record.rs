//! The saved shape of a workspace: topology plus per-pane window stacks.

use crucible_layout::{LayoutNode, WindowId, WorkspaceId, WorkspaceLayout, World};
use std::path::PathBuf;

/// One window as persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowRecord {
    /// An external terminal: its working directory and, when something other than the idle shell
    /// is in the foreground, the command (one shell-quoted string).
    Terminal {
        /// Working directory to restore into.
        cwd: PathBuf,
        /// Foreground command, if any.
        command: Option<String>,
        /// The CDE-minted `CRUCIBLE_TERM_SESSION` key (32 lowercase hex chars) of a `cterm`
        /// terminal, re-exported on restore so the daemon can reattach the live shell.
        session: Option<String>,
    },
    /// A non-terminal application.
    App {
        /// The application id, if known.
        app_id: Option<String>,
        /// The command line to relaunch it, if known.
        cmdline: Option<String>,
    },
}

/// One pane as persisted: its window stack in order and the focused stack index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRecord {
    /// Index of the focused window in `windows` (0 for an empty pane).
    pub focused: usize,
    /// Windows in stack order.
    pub windows: Vec<WindowRecord>,
}

/// A node of the persisted split tree: the layout tree with a [`PaneRecord`] on every leaf.
pub type RecordNode = LayoutNode<PaneRecord>;

/// A whole workspace as persisted; one file holds one record.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceRecord {
    /// The workspace name (unique across the world).
    pub name: String,
    /// The folder the workspace was opened on, if any.
    pub root: Option<PathBuf>,
    /// Index of the focused pane in tree order.
    pub focused_pane: usize,
    /// Connector name of the output the workspace was last viewed on, if known. A restore returns
    /// the workspace there when that output is connected.
    pub output: Option<String>,
    /// The split tree with pane contents.
    pub tree: RecordNode,
}

impl WorkspaceRecord {
    /// Captures workspace `ws` of `world`, mapping each window through `window`. Returns `None`
    /// if the workspace id is unknown. `root` is stored as given.
    pub fn from_world(
        world: &World,
        ws: WorkspaceId,
        root: Option<PathBuf>,
        window: impl Fn(WindowId) -> WindowRecord,
    ) -> Option<WorkspaceRecord> {
        let workspace = world.workspace(ws)?;
        let layout = world.workspace_layout(ws)?;
        let tree = layout.root.map(|id| {
            let pane = world.pane(id).expect("workspace pane exists");
            PaneRecord {
                focused: pane.focused_index(),
                windows: pane.windows().iter().map(|w| window(*w)).collect(),
            }
        });
        Some(WorkspaceRecord {
            name: workspace.name.clone(),
            root,
            focused_pane: layout.focused_pane,
            output: None,
            tree,
        })
    }

    /// The topology to hand to `World::insert_workspace`.
    #[must_use]
    pub fn layout(&self) -> WorkspaceLayout<()> {
        WorkspaceLayout {
            root: self.tree.map_ref(|_| ()),
            focused_pane: self.focused_pane,
        }
    }

    /// The panes in tree order — the same order `insert_workspace` returns pane ids in.
    #[must_use]
    pub fn panes(&self) -> Vec<&PaneRecord> {
        self.tree.leaves()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_layout::Direction::{Down, Right};

    fn term(n: usize) -> WindowRecord {
        WindowRecord::Terminal {
            cwd: PathBuf::from(format!("/w/{n}")),
            command: None,
            session: None,
        }
    }

    #[test]
    fn from_world_captures_shape_stacks_and_focus() {
        let mut w = World::new("1");
        w.split(Right);
        w.split(Down);
        let ws = w.active_workspace_id();
        let panes = w.workspace(ws).unwrap().panes();
        let a = w.open_window_at(panes[0], 0).unwrap();
        let b = w.open_window_at(panes[0], 1).unwrap();
        w.set_stack_focus(panes[0], 1).unwrap();
        let c = w.open_window_at(panes[2], 0).unwrap();
        let tag = |id: WindowId| {
            term(if id == a {
                1
            } else if id == b {
                2
            } else if id == c {
                3
            } else {
                9
            })
        };
        let rec = WorkspaceRecord::from_world(&w, ws, Some("/r".into()), tag).unwrap();
        assert_eq!(rec.name, "1");
        assert_eq!(rec.root, Some(PathBuf::from("/r")));
        assert_eq!(rec.focused_pane, 2);
        let recs = rec.panes();
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0].windows, vec![term(1), term(2)]);
        assert_eq!(recs[0].focused, 1);
        assert!(recs[1].windows.is_empty());
        assert_eq!(recs[2].windows, vec![term(3)]);
        // layout() agrees with the world's own snapshot.
        let snap = w.workspace_layout(ws).unwrap();
        assert_eq!(rec.layout().root, snap.root.map_ref(|_| ()));
        assert_eq!(rec.layout().focused_pane, snap.focused_pane);
    }

    #[test]
    fn from_world_unknown_workspace_is_none() {
        let w = World::new("1");
        assert!(
            WorkspaceRecord::from_world(&w, WorkspaceId::from_raw(77), None, |_| term(0)).is_none()
        );
    }
}
