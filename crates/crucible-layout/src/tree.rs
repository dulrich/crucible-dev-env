//! BSP-style split tree backing each workspace's pane arrangement.
//!
//! Internal arrangement is a binary split tree (decision #2 in `plans/02-tiling-model.md`): leaves
//! carry a payload (`PaneId` in the model, but the tree is generic so it can be unit-tested in
//! isolation), and internal nodes are horizontal or vertical splits carrying a ratio in `(0, 1)`.
//!
//! External verbs are **directional**. `focus`/`move` resolve a [`Direction`] *topologically*
//! against the tree — left/right within a horizontal split, up/down within a vertical split,
//! ascending to the nearest ancestor that permits the move. That ascent is an implementation detail
//! and is **never surfaced** to the user as a "focus parent" climb (decision #2, the i3 complaint).
//!
//! The tree is arena-based: nodes live in a `Vec<Option<Node<L>>>` indexed by [`NodeId`], so the
//! whole structure is `Clone` and free of `Rc`/`RefCell`.

use crate::Direction;
use crate::snapshot::LayoutNode;

/// Orientation of an internal split node.
///
/// `Horizontal` arranges its two children side by side (a *vertical* divider); `Vertical` stacks
/// them top and bottom (a *horizontal* divider). This matches `split-right` → horizontal and
/// `split-down` → vertical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Children are placed left | right.
    Horizontal,
    /// Children are placed top | bottom.
    Vertical,
}

impl Axis {
    /// The axis a direction acts on.
    fn of(dir: Direction) -> Self {
        if dir.is_horizontal() {
            Axis::Horizontal
        } else {
            Axis::Vertical
        }
    }
}

/// Index into the tree's node arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct NodeId(usize);

/// One slot in the tree: either a leaf carrying a payload or an internal split.
#[derive(Debug, Clone, PartialEq)]
enum Node<L> {
    /// A leaf carrying its payload and its parent (or `None` for the root).
    Leaf { value: L, parent: Option<NodeId> },
    /// An internal split with two children, a ratio, and its parent.
    Split {
        axis: Axis,
        /// Fraction of space given to the *first* child, in `(0, 1)`.
        ratio: f64,
        first: NodeId,
        second: NodeId,
        parent: Option<NodeId>,
    },
}

/// Which child of a split a node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    First,
    Second,
}

impl Side {
    fn opposite(self) -> Self {
        match self {
            Side::First => Side::Second,
            Side::Second => Side::First,
        }
    }
}

/// A binary space-partitioning tree whose leaves carry payloads of type `L`.
///
/// Invariants maintained by every operation:
/// - the tree is never empty (it always has at least one leaf);
/// - every internal node has exactly two children;
/// - `parent` links are consistent with `first`/`second` links;
/// - no `Split` has a single child (removal always collapses the parent).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SplitTree<L> {
    nodes: Vec<Option<Node<L>>>,
    root: NodeId,
    /// Free slots available for reuse (keeps the arena compact without aliasing payloads).
    free: Vec<usize>,
}

/// A view into a single tree node, returned by [`SplitTree::node_kind`].
///
/// Lets the geometry pass walk the tree without exposing the internal `Node` representation.
pub(crate) enum NodeKind<L> {
    /// A leaf carrying its payload.
    Leaf(L),
    /// An internal split with axis, ratio, and the ids of both children.
    Split {
        axis: Axis,
        ratio: f64,
        first: NodeId,
        second: NodeId,
    },
}

/// Default split ratio for a freshly created split.
pub(crate) const DEFAULT_RATIO: f64 = 0.5;
/// Clamp bound keeping ratios strictly inside `(0, 1)`.
const RATIO_EPS: f64 = 0.05;

impl<L: Copy + PartialEq> SplitTree<L> {
    /// Creates a tree consisting of a single leaf carrying `value`.
    pub(crate) fn new(value: L) -> Self {
        let nodes = vec![Some(Node::Leaf {
            value,
            parent: None,
        })];
        Self {
            nodes,
            root: NodeId(0),
            free: Vec::new(),
        }
    }

    /// Builds a tree shaped like `node`, allocating each leaf's payload from `next` in tree order.
    ///
    /// Returns the tree and the payloads in tree order. The caller validates ratios beforehand.
    pub(crate) fn from_layout(node: &LayoutNode, mut next: impl FnMut() -> L) -> (Self, Vec<L>) {
        let mut tree = Self {
            nodes: Vec::new(),
            root: NodeId(0),
            free: Vec::new(),
        };
        let mut leaves = Vec::new();
        let root = tree.build(node, None, &mut next, &mut leaves);
        tree.root = root;
        (tree, leaves)
    }

    fn build(
        &mut self,
        node: &LayoutNode,
        parent: Option<NodeId>,
        next: &mut impl FnMut() -> L,
        leaves: &mut Vec<L>,
    ) -> NodeId {
        match node {
            LayoutNode::Pane => {
                let value = next();
                leaves.push(value);
                self.alloc(Node::Leaf { value, parent })
            }
            LayoutNode::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                // Reserve the slot first so children can point back at it.
                self.nodes.push(None);
                let id = NodeId(self.nodes.len() - 1);
                let f = self.build(first, Some(id), next, leaves);
                let s = self.build(second, Some(id), next, leaves);
                self.nodes[id.0] = Some(Node::Split {
                    axis: *axis,
                    ratio: *ratio,
                    first: f,
                    second: s,
                    parent,
                });
                id
            }
        }
    }

    /// Snapshots the tree shape with leaves mapped to [`LayoutNode::Pane`].
    pub(crate) fn to_layout(&self) -> LayoutNode {
        self.layout_of(self.root)
    }

    fn layout_of(&self, id: NodeId) -> LayoutNode {
        match self.node(id) {
            Node::Leaf { .. } => LayoutNode::Pane,
            Node::Split {
                axis,
                ratio,
                first,
                second,
                ..
            } => LayoutNode::Split {
                axis: *axis,
                ratio: *ratio,
                first: Box::new(self.layout_of(*first)),
                second: Box::new(self.layout_of(*second)),
            },
        }
    }

    fn alloc(&mut self, node: Node<L>) -> NodeId {
        if let Some(idx) = self.free.pop() {
            self.nodes[idx] = Some(node);
            NodeId(idx)
        } else {
            self.nodes.push(Some(node));
            NodeId(self.nodes.len() - 1)
        }
    }

    fn free_node(&mut self, id: NodeId) {
        self.nodes[id.0] = None;
        self.free.push(id.0);
    }

    fn node(&self, id: NodeId) -> &Node<L> {
        self.nodes[id.0].as_ref().expect("live node id")
    }

    fn node_mut(&mut self, id: NodeId) -> &mut Node<L> {
        self.nodes[id.0].as_mut().expect("live node id")
    }

    fn parent_of(&self, id: NodeId) -> Option<NodeId> {
        match self.node(id) {
            Node::Leaf { parent, .. } | Node::Split { parent, .. } => *parent,
        }
    }

    fn set_parent(&mut self, id: NodeId, parent: Option<NodeId>) {
        match self.node_mut(id) {
            Node::Leaf { parent: p, .. } | Node::Split { parent: p, .. } => *p = parent,
        }
    }

    /// The side `child` occupies within its parent split.
    fn side_of(&self, parent: NodeId, child: NodeId) -> Side {
        match self.node(parent) {
            Node::Split { first, second, .. } => {
                if *first == child {
                    Side::First
                } else {
                    debug_assert_eq!(*second, child);
                    Side::Second
                }
            }
            Node::Leaf { .. } => unreachable!("parent must be a split"),
        }
    }

    fn child(&self, split: NodeId, side: Side) -> NodeId {
        match self.node(split) {
            Node::Split { first, second, .. } => match side {
                Side::First => *first,
                Side::Second => *second,
            },
            Node::Leaf { .. } => unreachable!("not a split"),
        }
    }

    /// Returns the leaf node id carrying `value`, if present.
    pub(crate) fn find_leaf(&self, value: L) -> Option<NodeId> {
        self.nodes.iter().enumerate().find_map(|(idx, slot)| {
            matches!(slot, Some(Node::Leaf { value: v, .. }) if *v == value).then_some(NodeId(idx))
        })
    }

    fn leaf_value(&self, id: NodeId) -> L {
        match self.node(id) {
            Node::Leaf { value, .. } => *value,
            Node::Split { .. } => unreachable!("not a leaf"),
        }
    }

    /// Number of leaves in the tree.
    pub(crate) fn leaf_count(&self) -> usize {
        self.nodes
            .iter()
            .filter(|slot| matches!(slot, Some(Node::Leaf { .. })))
            .count()
    }

    /// All leaf payloads, in left-to-right / top-to-bottom in-order traversal order.
    pub(crate) fn leaves(&self) -> Vec<L> {
        let mut out = Vec::new();
        self.collect_leaves(self.root, &mut out);
        out
    }

    fn collect_leaves(&self, id: NodeId, out: &mut Vec<L>) {
        match self.node(id) {
            Node::Leaf { value, .. } => out.push(*value),
            Node::Split { first, second, .. } => {
                self.collect_leaves(*first, out);
                self.collect_leaves(*second, out);
            }
        }
    }

    /// Splits the leaf carrying `target` along `dir`, inserting `new_value` as a new sibling leaf.
    ///
    /// The new leaf is placed on the side `dir` points toward: e.g. `split` toward [`Direction::Right`]
    /// puts the new leaf to the right (second child of a horizontal split). Returns `false` if
    /// `target` is not a leaf in this tree.
    pub(crate) fn split(&mut self, target: L, dir: Direction, new_value: L) -> bool {
        let Some(leaf) = self.find_leaf(target) else {
            return false;
        };
        let axis = Axis::of(dir);
        let new_leaf = self.alloc(Node::Leaf {
            value: new_value,
            parent: None,
        });

        // The existing leaf keeps its slot; we re-home it under a fresh split node. Order depends on
        // whether `dir` points "forward" (right/down → new leaf second) or "back" (left/up → first).
        let new_first_is_existing = matches!(dir, Direction::Right | Direction::Down);
        let (first, second) = if new_first_is_existing {
            (leaf, new_leaf)
        } else {
            (new_leaf, leaf)
        };

        let grandparent = self.parent_of(leaf);
        let split = self.alloc(Node::Split {
            axis,
            ratio: DEFAULT_RATIO,
            first,
            second,
            parent: grandparent,
        });

        self.set_parent(first, Some(split));
        self.set_parent(second, Some(split));

        match grandparent {
            None => self.root = split,
            Some(gp) => {
                let side = self.side_of(gp, leaf);
                match self.node_mut(gp) {
                    Node::Split { first, second, .. } => match side {
                        Side::First => *first = split,
                        Side::Second => *second = split,
                    },
                    Node::Leaf { .. } => unreachable!(),
                }
            }
        }
        true
    }

    /// Removes the leaf carrying `value`, collapsing its parent split and promoting the sibling.
    ///
    /// Returns `true` if the leaf was found and removed. The tree's last leaf is **never** removed
    /// (returns `false`) — the caller (`World`) guarantees a workspace always has ≥1 pane by
    /// removing the whole workspace instead.
    pub(crate) fn remove(&mut self, value: L) -> bool {
        let Some(leaf) = self.find_leaf(value) else {
            return false;
        };
        let Some(parent) = self.parent_of(leaf) else {
            // Removing the root leaf would empty the tree; disallowed.
            return false;
        };
        let side = self.side_of(parent, leaf);
        let sibling = self.child(parent, side.opposite());
        let grandparent = self.parent_of(parent);

        // Promote the sibling into the parent's position.
        self.set_parent(sibling, grandparent);
        match grandparent {
            None => self.root = sibling,
            Some(gp) => {
                let parent_side = self.side_of(gp, parent);
                match self.node_mut(gp) {
                    Node::Split { first, second, .. } => match parent_side {
                        Side::First => *first = sibling,
                        Side::Second => *second = sibling,
                    },
                    Node::Leaf { .. } => unreachable!(),
                }
            }
        }

        self.free_node(leaf);
        self.free_node(parent);
        true
    }

    /// Removes the leaf carrying `value` like [`remove`](Self::remove), returning the promoted
    /// sibling's leaf nearest the removed leaf's former position (the leaf that "took its place").
    ///
    /// Returns `None` (and changes nothing) when [`remove`](Self::remove) would refuse.
    pub(crate) fn remove_and_promote(&mut self, value: L) -> Option<L> {
        let leaf = self.find_leaf(value)?;
        let parent = self.parent_of(leaf)?;
        let side = self.side_of(parent, leaf);
        // Descend the sibling toward the shared boundary: if the removed leaf was the first child,
        // its nearest survivor is the sibling's first-most leaf, and vice versa.
        let mut node = self.child(parent, side.opposite());
        while let Node::Split { .. } = self.node(node) {
            node = self.child(node, side);
        }
        let survivor = self.leaf_value(node);
        self.remove(value).then_some(survivor)
    }

    /// Resolves the neighbouring leaf of `from` in direction `dir`, if any.
    ///
    /// Ascends to the nearest ancestor split on `dir`'s axis where `from`'s subtree sits on the side
    /// the move departs *from*, then descends into the opposite child picking the leaf nearest the
    /// shared boundary. Returns `None` when no such neighbour exists (the focused leaf is at the
    /// edge of the tree in that direction).
    pub(crate) fn neighbor(&self, from: L, dir: Direction) -> Option<L> {
        let leaf = self.find_leaf(from)?;
        let node = self.neighbor_node(leaf, dir)?;
        Some(self.leaf_value(node))
    }

    fn neighbor_node(&self, leaf: NodeId, dir: Direction) -> Option<NodeId> {
        let axis = Axis::of(dir);
        // The side we must depart *from* to move in `dir`: moving Right/Down means we currently sit
        // on the First child and cross into the Second; Left/Up is the mirror.
        let depart_side = match dir {
            Direction::Right | Direction::Down => Side::First,
            Direction::Left | Direction::Up => Side::Second,
        };

        let mut current = leaf;
        while let Some(parent) = self.parent_of(current) {
            let matches_axis =
                matches!(self.node(parent), Node::Split { axis: a, .. } if *a == axis);
            if matches_axis && self.side_of(parent, current) == depart_side {
                // Cross into the opposite subtree and descend to the boundary-nearest leaf.
                let neighbor_subtree = self.child(parent, depart_side.opposite());
                return Some(self.descend_to_boundary(neighbor_subtree, axis, depart_side));
            }
            current = parent;
        }
        None
    }

    /// Descends `subtree` to the leaf nearest the boundary we just crossed.
    ///
    /// `depart_side` is the side of the *origin*; when entering the neighbour we want the child on
    /// the same boundary, i.e. the `depart_side` child of any same-axis split along the way.
    fn descend_to_boundary(&self, subtree: NodeId, axis: Axis, depart_side: Side) -> NodeId {
        let mut current = subtree;
        loop {
            match self.node(current) {
                Node::Leaf { .. } => return current,
                Node::Split { axis: a, .. } => {
                    let side = if *a == axis {
                        // Same axis: pick the child touching the shared boundary.
                        depart_side
                    } else {
                        // Cross axis: choice is arbitrary; bias to the first child for determinism.
                        Side::First
                    };
                    current = self.child(current, side);
                }
            }
        }
    }

    /// Swaps the focused leaf with its directional neighbour leaf, reordering the arrangement.
    ///
    /// Returns the neighbour's payload (now where `value` used to be) on success, or `None` if there
    /// is no neighbour in that direction. Swapping payloads keeps the tree topology stable and is
    /// the predictable, zellij-style reorder the user chose.
    pub(crate) fn move_(&mut self, value: L, dir: Direction) -> Option<L> {
        let leaf = self.find_leaf(value)?;
        let neighbor = self.neighbor_node(leaf, dir)?;
        let neighbor_value = self.leaf_value(neighbor);
        // Swap the two leaves' payloads.
        match self.node_mut(leaf) {
            Node::Leaf { value: v, .. } => *v = neighbor_value,
            Node::Split { .. } => unreachable!(),
        }
        match self.node_mut(neighbor) {
            Node::Leaf { value: v, .. } => *v = value,
            Node::Split { .. } => unreachable!(),
        }
        Some(neighbor_value)
    }

    /// Adjusts the split ratio governing the leaf carrying `value` along `dir`'s axis by `delta`.
    ///
    /// Walks up to the nearest ancestor split on `dir`'s axis and nudges its ratio so the leaf's
    /// share grows when `dir` points "outward" from the leaf. Ratios are clamped to stay strictly
    /// inside `(0, 1)`. Returns `true` if a governing split was found and adjusted.
    pub(crate) fn resize(&mut self, value: L, dir: Direction, delta: f64) -> bool {
        let Some(leaf) = self.find_leaf(value) else {
            return false;
        };
        let axis = Axis::of(dir);
        let mut current = leaf;
        while let Some(parent) = self.parent_of(current) {
            let matches_axis =
                matches!(self.node(parent), Node::Split { axis: a, .. } if *a == axis);
            if matches_axis {
                // `ratio` is the first child's share, so it grows as the divider moves toward the
                // second child. Right/Down move the divider that way (increase ratio); Left/Up move
                // it back (decrease). This is divider-centric and independent of which child holds
                // the focused leaf — the predictable rule.
                let signed = if matches!(dir, Direction::Right | Direction::Down) {
                    delta
                } else {
                    -delta
                };
                if let Node::Split { ratio, .. } = self.node_mut(parent) {
                    *ratio = (*ratio + signed).clamp(RATIO_EPS, 1.0 - RATIO_EPS);
                }
                return true;
            }
            current = parent;
        }
        false
    }

    /// Returns the `NodeId` of the root node. Used by the geometry pass to start a tree walk.
    pub(crate) fn root_id(&self) -> NodeId {
        self.root
    }

    /// Returns a view of the node at `id` for use by the geometry pass.
    ///
    /// Panics if `id` does not refer to a live node (same contract as internal helpers).
    pub(crate) fn node_kind(&self, id: NodeId) -> NodeKind<L> {
        match self.node(id) {
            Node::Leaf { value, .. } => NodeKind::Leaf(*value),
            Node::Split {
                axis,
                ratio,
                first,
                second,
                ..
            } => NodeKind::Split {
                axis: *axis,
                ratio: *ratio,
                first: *first,
                second: *second,
            },
        }
    }

    /// Reads the ratio of the nearest ancestor split on `dir`'s axis above `value` (for tests).
    #[cfg(test)]
    pub(crate) fn ratio_along(&self, value: L, dir: Direction) -> Option<f64> {
        let leaf = self.find_leaf(value)?;
        let axis = Axis::of(dir);
        let mut current = leaf;
        while let Some(parent) = self.parent_of(current) {
            if let Node::Split { axis: a, ratio, .. } = self.node(parent) {
                if *a == axis {
                    return Some(*ratio);
                }
            }
            current = parent;
        }
        None
    }

    /// Validates the structural invariants. Used by the model's `check_invariants`; cheap enough to
    /// keep compiled (it is only ever called from test code).
    pub(crate) fn check_invariants(&self) {
        // Root has no parent.
        assert!(
            self.parent_of(self.root).is_none(),
            "root must be parentless"
        );
        let mut seen_leaves = 0usize;
        for (idx, slot) in self.nodes.iter().enumerate() {
            let Some(node) = slot else { continue };
            let id = NodeId(idx);
            match node {
                Node::Leaf { .. } => seen_leaves += 1,
                Node::Split { first, second, .. } => {
                    assert_ne!(first, second, "split children must differ");
                    assert_eq!(self.parent_of(*first), Some(id), "first parent link");
                    assert_eq!(self.parent_of(*second), Some(id), "second parent link");
                }
            }
            // Non-root nodes must have a parent that points back to them.
            if id != self.root {
                let p = self.parent_of(id).expect("non-root has parent");
                let s = self.side_of(p, id);
                assert_eq!(self.child(p, s), id, "parent points back to child");
            }
        }
        assert!(seen_leaves >= 1, "tree always has at least one leaf");
        assert_eq!(seen_leaves, self.leaf_count());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Direction::{Down, Left, Right, Up};

    /// Builds a tree over `u32` leaves for ergonomic tests.
    fn leaf(v: u32) -> SplitTree<u32> {
        SplitTree::new(v)
    }

    #[test]
    fn single_leaf_basics() {
        let t = leaf(1);
        assert_eq!(t.leaf_count(), 1);
        assert_eq!(t.leaves(), vec![1]);
        assert!(t.find_leaf(1).is_some());
        assert!(t.find_leaf(2).is_none());
        t.check_invariants();
    }

    #[test]
    fn split_right_places_new_leaf_second() {
        let mut t = leaf(1);
        assert!(t.split(1, Right, 2));
        assert_eq!(t.leaves(), vec![1, 2]);
        assert_eq!(t.leaf_count(), 2);
        t.check_invariants();
    }

    #[test]
    fn split_left_places_new_leaf_first() {
        let mut t = leaf(1);
        assert!(t.split(1, Left, 2));
        assert_eq!(t.leaves(), vec![2, 1]);
        t.check_invariants();
    }

    #[test]
    fn split_down_and_up_orient_vertically() {
        let mut t = leaf(1);
        t.split(1, Down, 2);
        assert_eq!(t.leaves(), vec![1, 2]);
        let mut u = leaf(1);
        u.split(1, Up, 2);
        assert_eq!(u.leaves(), vec![2, 1]);
    }

    #[test]
    fn split_unknown_leaf_is_noop() {
        let mut t = leaf(1);
        assert!(!t.split(99, Right, 2));
        assert_eq!(t.leaves(), vec![1]);
    }

    #[test]
    fn neighbor_within_horizontal_split() {
        let mut t = leaf(1);
        t.split(1, Right, 2); // [1 | 2]
        assert_eq!(t.neighbor(1, Right), Some(2));
        assert_eq!(t.neighbor(2, Left), Some(1));
        // No vertical neighbours in a purely horizontal split.
        assert_eq!(t.neighbor(1, Down), None);
        assert_eq!(t.neighbor(2, Up), None);
        // Edges.
        assert_eq!(t.neighbor(1, Left), None);
        assert_eq!(t.neighbor(2, Right), None);
    }

    #[test]
    fn neighbor_ascends_across_nested_splits() {
        // Build:  H( V(1,2) , 3 )  ->  left column has 1 over 2, right is 3.
        let mut t = leaf(1);
        t.split(1, Down, 2); // V(1,2)
        t.split(1, Right, 3); // splitting leaf 1 rightward; new node 3 to the right of leaf 1
        // Tree is V( H(1,3), 2 ): top row is [1|3], bottom row is the full-width 2.
        // Moving Right from 1 reaches its sibling 3.
        assert_eq!(t.neighbor(1, Right), Some(3));
        // Leaf 2 spans the full width below, so it has NO horizontal neighbour — the ascent finds
        // no matching-axis ancestor and correctly returns None (no surfaced "focus parent").
        assert_eq!(t.neighbor(2, Right), None);
        assert_eq!(t.neighbor(2, Left), None);
        // Down from the top row reaches 2 (ascend across the H split to the V split).
        assert_eq!(t.neighbor(1, Down), Some(2));
        assert_eq!(t.neighbor(3, Down), Some(2));
        // Up from 2 descends into the top row, biased to the boundary-nearest (first) leaf.
        assert_eq!(t.neighbor(2, Up), Some(1));
        t.check_invariants();
    }

    #[test]
    fn focus_and_opposite_returns_to_origin_horizontal() {
        let mut t = leaf(1);
        t.split(1, Right, 2);
        // focus(Right) then focus(Left) is the identity on neighbours.
        let r = t.neighbor(1, Right).unwrap();
        assert_eq!(t.neighbor(r, Left), Some(1));
    }

    #[test]
    fn move_swaps_payloads_keeping_topology() {
        let mut t = leaf(1);
        t.split(1, Right, 2); // [1 | 2]
        assert_eq!(t.move_(1, Right), Some(2));
        assert_eq!(t.leaves(), vec![2, 1]);
        // Topology unchanged (still one split, two leaves).
        assert_eq!(t.leaf_count(), 2);
        t.check_invariants();
    }

    #[test]
    fn move_at_edge_is_noop() {
        let mut t = leaf(1);
        t.split(1, Right, 2);
        assert_eq!(t.move_(1, Left), None);
        assert_eq!(t.leaves(), vec![1, 2]);
    }

    #[test]
    fn remove_collapses_parent_and_promotes_sibling() {
        let mut t = leaf(1);
        t.split(1, Right, 2);
        t.split(2, Right, 3); // [1 | (2 | 3)]
        assert_eq!(t.leaf_count(), 3);
        assert!(t.remove(2));
        assert_eq!(t.leaf_count(), 2);
        assert_eq!(t.leaves(), vec![1, 3]);
        t.check_invariants();
        // 1 and 3 are now siblings — neighbour works.
        assert_eq!(t.neighbor(1, Right), Some(3));
    }

    #[test]
    fn remove_last_leaf_is_disallowed() {
        let mut t = leaf(1);
        assert!(!t.remove(1));
        assert_eq!(t.leaf_count(), 1);
    }

    #[test]
    fn remove_unknown_is_noop() {
        let mut t = leaf(1);
        t.split(1, Right, 2);
        assert!(!t.remove(99));
        assert_eq!(t.leaf_count(), 2);
    }

    #[test]
    fn arena_slots_are_reused_without_aliasing() {
        let mut t = leaf(1);
        t.split(1, Right, 2);
        t.split(2, Right, 3);
        t.remove(3);
        // Reuse the freed slots by splitting again; invariants must hold.
        t.split(1, Down, 4);
        t.check_invariants();
        assert!(t.find_leaf(4).is_some());
        assert!(t.find_leaf(3).is_none());
    }

    #[test]
    fn resize_grows_focused_share_and_clamps() {
        let mut t = leaf(1);
        t.split(1, Right, 2); // horizontal split, ratio 0.5
        assert_eq!(t.ratio_along(1, Right), Some(DEFAULT_RATIO));
        // 1 is first child; growing it to the Right increases ratio.
        assert!(t.resize(1, Right, 0.2));
        assert!((t.ratio_along(1, Right).unwrap() - 0.7).abs() < 1e-9);
        // Clamp at the upper bound.
        assert!(t.resize(1, Right, 5.0));
        assert!(t.ratio_along(1, Right).unwrap() < 1.0);
        assert!(t.ratio_along(1, Right).unwrap() > 0.9);
        // Divider-centric: Left moves the divider left, decreasing the first child's ratio.
        let before = t.ratio_along(2, Right).unwrap();
        t.resize(2, Left, 0.1);
        assert!(t.ratio_along(2, Right).unwrap() < before);
    }

    #[test]
    fn resize_with_no_matching_axis_is_noop() {
        let mut t = leaf(1);
        t.split(1, Right, 2); // only a horizontal split
        assert!(!t.resize(1, Up, 0.1)); // no vertical ancestor
    }

    #[test]
    fn deep_nesting_neighbour_and_invariants() {
        let mut t = leaf(1);
        for v in 2..=8 {
            // Alternate split directions to build a deep, mixed-axis tree.
            let dir = if v % 2 == 0 { Right } else { Down };
            let target = v - 1;
            assert!(t.split(target, dir, v));
            t.check_invariants();
        }
        assert_eq!(t.leaf_count(), 8);
        // Every leaf is reachable; removing all but one keeps invariants.
        for v in 2..=8 {
            t.remove(v);
            t.check_invariants();
        }
        assert_eq!(t.leaf_count(), 1);
    }
}
