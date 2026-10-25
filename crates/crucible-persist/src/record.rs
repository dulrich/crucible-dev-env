//! The saved shape of a workspace: topology plus per-pane window stacks.

use crucible_layout::{Axis, LayoutNode, PaneId, WindowId, WorkspaceId, WorkspaceLayout, World};
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

/// A node of the persisted split tree.
#[derive(Debug, Clone, PartialEq)]
pub enum RecordNode {
    /// A split of two sub-trees.
    Split {
        /// Orientation of the split.
        axis: Axis,
        /// Fraction of space given to `first`, strictly inside `(0, 1)`.
        ratio: f64,
        /// The first (left/top) child.
        first: Box<RecordNode>,
        /// The second (right/bottom) child.
        second: Box<RecordNode>,
    },
    /// A pane leaf.
    Pane(PaneRecord),
}

/// A whole workspace as persisted; one file holds one record.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceRecord {
    /// The workspace name (unique across the world).
    pub name: String,
    /// The folder the workspace was opened on, if any.
    pub root: Option<PathBuf>,
    /// Index of the focused pane in tree order.
    pub focused_pane: usize,
    /// The split tree with pane contents.
    pub tree: RecordNode,
}

impl RecordNode {
    fn to_layout(&self) -> LayoutNode {
        match self {
            RecordNode::Pane(_) => LayoutNode::Pane,
            RecordNode::Split {
                axis,
                ratio,
                first,
                second,
            } => LayoutNode::Split {
                axis: *axis,
                ratio: *ratio,
                first: Box::new(first.to_layout()),
                second: Box::new(second.to_layout()),
            },
        }
    }

    fn collect_panes<'a>(&'a self, out: &mut Vec<&'a PaneRecord>) {
        match self {
            RecordNode::Pane(p) => out.push(p),
            RecordNode::Split { first, second, .. } => {
                first.collect_panes(out);
                second.collect_panes(out);
            }
        }
    }
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
        let mut panes = workspace.panes().into_iter();
        let tree = Self::build(&layout.root, &mut panes, world, &window);
        Some(WorkspaceRecord {
            name: workspace.name.clone(),
            root,
            focused_pane: layout.focused_pane,
            tree,
        })
    }

    fn build(
        node: &LayoutNode,
        panes: &mut impl Iterator<Item = PaneId>,
        world: &World,
        window: &impl Fn(WindowId) -> WindowRecord,
    ) -> RecordNode {
        match node {
            LayoutNode::Pane => {
                let id = panes.next().expect("layout and pane order agree");
                let pane = world.pane(id).expect("workspace pane exists");
                RecordNode::Pane(PaneRecord {
                    focused: pane.focused,
                    windows: pane.windows.iter().map(|w| window(*w)).collect(),
                })
            }
            LayoutNode::Split {
                axis,
                ratio,
                first,
                second,
            } => RecordNode::Split {
                axis: *axis,
                ratio: *ratio,
                first: Box::new(Self::build(first, panes, world, window)),
                second: Box::new(Self::build(second, panes, world, window)),
            },
        }
    }

    /// The topology to hand to `World::insert_workspace`.
    #[must_use]
    pub fn layout(&self) -> WorkspaceLayout {
        WorkspaceLayout {
            root: self.tree.to_layout(),
            focused_pane: self.focused_pane,
        }
    }

    /// The panes in tree order — the same order `insert_workspace` returns pane ids in.
    #[must_use]
    pub fn panes(&self) -> Vec<&PaneRecord> {
        let mut out = Vec::new();
        self.tree.collect_panes(&mut out);
        out
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
        }
    }

    #[test]
    fn from_world_captures_shape_stacks_and_focus() {
        let mut w = World::new();
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
        assert_eq!(rec.layout(), w.workspace_layout(ws).unwrap());
    }

    #[test]
    fn from_world_unknown_workspace_is_none() {
        let w = World::new();
        assert!(
            WorkspaceRecord::from_world(&w, WorkspaceId::from_raw(77), None, |_| term(0)).is_none()
        );
    }
}
