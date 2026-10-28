//! Topology workspace snapshot: the data [`World::workspace_layout`](crate::World) returns and
//! [`World::insert_workspace`](crate::World) consumes.
//!
//! A snapshot carries the split tree (axis + ratio per split), a caller-chosen payload `P` on each
//! pane leaf, and the focused pane's tree-order index. The model returns `PaneId` leaves; the
//! persistence record carries its own per-pane record as the leaf, so one tree type serves both.

use crate::tree::Axis;

/// A node of a [`WorkspaceLayout`] tree: a pane leaf carrying `P`, or a two-child split.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutNode<P> {
    /// A pane (a leaf of the split tree) with its payload.
    Pane(P),
    /// A split of two sub-trees.
    Split {
        /// Orientation of the split.
        axis: Axis,
        /// Fraction of space given to `first`; must lie strictly inside `(0, 1)`.
        ratio: f64,
        /// The first (left/top) child.
        first: Box<LayoutNode<P>>,
        /// The second (right/bottom) child.
        second: Box<LayoutNode<P>>,
    },
}

impl<P> LayoutNode<P> {
    /// Number of pane leaves under this node.
    #[must_use]
    pub fn pane_count(&self) -> usize {
        match self {
            LayoutNode::Pane(_) => 1,
            LayoutNode::Split { first, second, .. } => first.pane_count() + second.pane_count(),
        }
    }

    /// `true` when every split ratio is finite and strictly inside `(0, 1)`.
    #[must_use]
    pub fn ratios_valid(&self) -> bool {
        match self {
            LayoutNode::Pane(_) => true,
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

    /// The leaf payloads in tree (left-to-right / top-to-bottom) order.
    #[must_use]
    pub fn leaves(&self) -> Vec<&P> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect<'a>(&'a self, out: &mut Vec<&'a P>) {
        match self {
            LayoutNode::Pane(p) => out.push(p),
            LayoutNode::Split { first, second, .. } => {
                first.collect(out);
                second.collect(out);
            }
        }
    }

    /// Consumes the tree, mapping every leaf payload with `f` in tree order; the shape is kept.
    #[must_use]
    pub fn map<Q>(self, mut f: impl FnMut(P) -> Q) -> LayoutNode<Q> {
        self.map_with(&mut f)
    }

    fn map_with<Q>(self, f: &mut impl FnMut(P) -> Q) -> LayoutNode<Q> {
        match self {
            LayoutNode::Pane(p) => LayoutNode::Pane(f(p)),
            LayoutNode::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let first = Box::new(first.map_with(f));
                let second = Box::new(second.map_with(f));
                LayoutNode::Split {
                    axis,
                    ratio,
                    first,
                    second,
                }
            }
        }
    }

    /// Borrowing [`LayoutNode::map`]: maps every `&P` with `f` in tree order.
    #[must_use]
    pub fn map_ref<Q>(&self, mut f: impl FnMut(&P) -> Q) -> LayoutNode<Q> {
        self.map_ref_with(&mut f)
    }

    fn map_ref_with<Q>(&self, f: &mut impl FnMut(&P) -> Q) -> LayoutNode<Q> {
        match self {
            LayoutNode::Pane(p) => LayoutNode::Pane(f(p)),
            LayoutNode::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let first = Box::new(first.map_ref_with(f));
                let second = Box::new(second.map_ref_with(f));
                LayoutNode::Split {
                    axis: *axis,
                    ratio: *ratio,
                    first,
                    second,
                }
            }
        }
    }
}

/// The topology of one workspace: its split tree and which pane (tree order) is focused.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceLayout<P> {
    /// The split tree; leaves are panes in tree (left-to-right / top-to-bottom) order.
    pub root: LayoutNode<P>,
    /// Index of the focused pane in tree order (`Workspace::panes()` order).
    pub focused_pane: usize,
}

impl<P> WorkspaceLayout<P> {
    /// A single-pane layout whose one leaf carries `pane`.
    #[must_use]
    pub fn single(pane: P) -> Self {
        Self {
            root: LayoutNode::Pane(pane),
            focused_pane: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> LayoutNode<u32> {
        LayoutNode::Split {
            axis: Axis::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutNode::Pane(1)),
            second: Box::new(LayoutNode::Split {
                axis: Axis::Vertical,
                ratio: 0.25,
                first: Box::new(LayoutNode::Pane(2)),
                second: Box::new(LayoutNode::Pane(3)),
            }),
        }
    }

    #[test]
    fn leaves_are_in_tree_order() {
        assert_eq!(sample().leaves(), vec![&1, &2, &3]);
        assert_eq!(sample().pane_count(), 3);
    }

    #[test]
    fn map_visits_leaves_in_tree_order_and_keeps_shape() {
        let mut seen = Vec::new();
        let mapped = sample().map(|p| {
            seen.push(p);
            format!("p{p}")
        });
        assert_eq!(seen, vec![1, 2, 3]);
        assert_eq!(mapped.leaves(), vec!["p1", "p2", "p3"]);
        // Shape (axes and ratios) survives a round of mapping.
        assert_eq!(mapped.map_ref(|_| ()), sample().map(|_| ()));
    }
}
