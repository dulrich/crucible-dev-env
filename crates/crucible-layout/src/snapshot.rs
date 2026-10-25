//! Topology-only workspace snapshot: the data [`World::workspace_layout`](crate::World) returns and
//! [`World::insert_workspace`](crate::World) consumes.
//!
//! A snapshot carries the split tree (axis + ratio per split) and the focused pane's tree-order
//! index. It deliberately carries **no** window ids, stack lengths or stack focus — those belong
//! to the persistence record, which replays them with `open_window_at` / `set_stack_focus`.

use crate::tree::Axis;

/// A node of a [`WorkspaceLayout`] tree: a pane leaf or a two-child split.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutNode {
    /// A pane (a leaf of the split tree).
    Pane,
    /// A split of two sub-trees.
    Split {
        /// Orientation of the split.
        axis: Axis,
        /// Fraction of space given to `first`; must lie strictly inside `(0, 1)`.
        ratio: f64,
        /// The first (left/top) child.
        first: Box<LayoutNode>,
        /// The second (right/bottom) child.
        second: Box<LayoutNode>,
    },
}

impl LayoutNode {
    /// Number of pane leaves under this node.
    #[must_use]
    pub fn pane_count(&self) -> usize {
        match self {
            LayoutNode::Pane => 1,
            LayoutNode::Split { first, second, .. } => first.pane_count() + second.pane_count(),
        }
    }

    /// `true` when every split ratio is finite and strictly inside `(0, 1)`.
    #[must_use]
    pub fn ratios_valid(&self) -> bool {
        match self {
            LayoutNode::Pane => true,
            LayoutNode::Split {
                ratio,
                first,
                second,
                ..
            } => {
                ratio.is_finite()
                    && *ratio > 0.0
                    && *ratio < 1.0
                    && first.ratios_valid()
                    && second.ratios_valid()
            }
        }
    }
}

/// The topology of one workspace: its split tree and which pane (tree order) is focused.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceLayout {
    /// The split tree; leaves are panes in tree (left-to-right / top-to-bottom) order.
    pub root: LayoutNode,
    /// Index of the focused pane in tree order (`Workspace::panes()` order).
    pub focused_pane: usize,
}
