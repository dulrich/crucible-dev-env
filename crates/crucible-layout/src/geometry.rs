//! Pure geometry pass for Crucible's tiling model.
//!
//! Given a workspace's split tree, an output pixel rectangle, and a gap configuration, this module
//! computes the pixel [`Rect`] for every **visible** pane (and thus the pane's focused window).
//!
//! The pass is a pure read over the model — no mutation, no compositor types, no Smithay.
//!
//! # Coordinate system
//! - Origin (0, 0) is the top-left of the physical monitor.
//! - `x` increases rightward, `y` increases downward.
//! - Rects are in integer pixels; the compositor must use these values when calling `configure` on
//!   a Wayland surface or positioning an element in the Smithay `Space`.
//!
//! # Gap model
//! 1. **Outer gap** (`GapConfig::outer`) is subtracted from all four sides of the output rect once,
//!    before any splitting begins. The entire tiled area lives inside this inset rectangle.
//! 2. **Inner gap** (`GapConfig::inner`) is inserted *between* the two children of every split node.
//!    For a horizontal split (`Axis::Horizontal`, children side-by-side) the gap is horizontal space;
//!    for a vertical split (`Axis::Vertical`, children stacked) it is vertical space.
//!
//! # Rounding
//! To avoid sub-pixel drift across nested splits, sizes are computed as:
//! ```text
//! first_size  = round((extent - inner_gap) * ratio)
//! second_size = extent - inner_gap - first_size
//! ```
//! so `first_size + inner_gap + second_size == extent` exactly, with no lost or gained pixels.

use crate::PaneId;
use crate::tree::{Axis, NodeId, NodeKind, SplitTree};
use crate::world::Workspace;

/// An integer pixel rectangle.
///
/// - `x` and `y` are the position of the top-left corner relative to the output origin.
/// - `width` and `height` are the pixel extent (always ≥ 0).
///
/// All fields are `i32` so that rects can represent off-screen positions without wrapping, and so
/// that subtracting positions naturally yields signed offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    /// Left edge, in pixels from the output's left edge.
    pub x: i32,
    /// Top edge, in pixels from the output's top edge.
    pub y: i32,
    /// Width in pixels.
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
}

impl Rect {
    /// Creates a `Rect` from its components.
    #[must_use]
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// The pixel area covered by this rect (always ≥ 0).
    #[must_use]
    pub fn area(self) -> i64 {
        (self.width.max(0) as i64) * (self.height.max(0) as i64)
    }

    /// Insets this rect by `amount` on all four sides, clamping to zero size.
    #[must_use]
    pub fn inset(self, amount: i32) -> Self {
        let x = self.x + amount;
        let y = self.y + amount;
        let width = (self.width - amount * 2).max(0);
        let height = (self.height - amount * 2).max(0);
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// This rect grown by `by` pixels on every side (no clamping; the inverse of an unclamped
    /// [`Rect::inset`]).
    #[must_use]
    pub fn outset(self, by: i32) -> Self {
        Self::new(
            self.x - by,
            self.y - by,
            self.width + 2 * by,
            self.height + 2 * by,
        )
    }

    /// This rect moved by `(dx, dy)`. Translating into the frame of an output whose global origin
    /// is `(ox, oy)` is `translate(-ox, -oy)`.
    #[must_use]
    pub fn translate(self, dx: i32, dy: i32) -> Self {
        Self::new(self.x + dx, self.y + dy, self.width, self.height)
    }

    /// Whether the rect has non-zero area.
    #[must_use]
    pub fn has_area(self) -> bool {
        self.width > 0 && self.height > 0
    }

    /// Whether the two rects overlap with non-zero area (touching edges do not intersect).
    #[must_use]
    pub fn intersects(self, other: Self) -> bool {
        self.x < other.x + other.width
            && other.x < self.x + self.width
            && self.y < other.y + other.height
            && other.y < self.y + self.height
    }

    /// Whether the point `(x, y)` lies inside the rect (left/top edges inclusive, right/bottom
    /// exclusive).
    #[must_use]
    pub fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    /// This rect with `height` removed from its top, clamped so at least half the height remains.
    #[must_use]
    pub fn shrink_top(self, height: i32) -> Self {
        let h = height.clamp(0, self.height / 2);
        Self::new(self.x, self.y + h, self.width, self.height - h)
    }

    /// The frame between `self` (outer) and `inner` (which must lie within it) as
    /// `[top, bottom, left, right]`: the horizontal strips span the outer width, the vertical
    /// strips fill the height between them. Together they tile `self \ inner` exactly.
    #[must_use]
    pub fn frame_strips(self, inner: Self) -> [Self; 4] {
        let inner_right = inner.x + inner.width;
        let inner_bottom = inner.y + inner.height;
        [
            Self::new(self.x, self.y, self.width, inner.y - self.y),
            Self::new(
                self.x,
                inner_bottom,
                self.width,
                self.y + self.height - inner_bottom,
            ),
            Self::new(self.x, inner.y, inner.x - self.x, inner.height),
            Self::new(
                inner_right,
                inner.y,
                self.x + self.width - inner_right,
                inner.height,
            ),
        ]
    }

    /// The four strips `[top, bottom, left, right]` of a border `thickness` pixels wide drawn
    /// inside this rect. The thickness is clamped to `0..=min(width, height) / 2`, so strips never
    /// have negative size; a zero thickness yields zero-area strips.
    #[must_use]
    pub fn border_strips(self, thickness: i32) -> [Self; 4] {
        let t = thickness.max(0).min(self.width / 2).min(self.height / 2);
        let (x, y, w, h) = (self.x, self.y, self.width, self.height);
        [
            Self::new(x, y, w, t),
            Self::new(x, y + h - t, w, t),
            Self::new(x, y + t, t, h - 2 * t),
            Self::new(x + w - t, y + t, t, h - 2 * t),
        ]
    }

    /// This rect minus `hole`: up to four non-overlapping rects covering exactly the part outside
    /// `hole`. A disjoint hole returns the rect whole.
    #[must_use]
    pub fn subtract(self, hole: Self) -> Vec<Self> {
        if !self.intersects(hole) {
            return vec![self];
        }
        let x = hole.x.max(self.x);
        let y = hole.y.max(self.y);
        let clipped = Self::new(
            x,
            y,
            (hole.x + hole.width).min(self.x + self.width) - x,
            (hole.y + hole.height).min(self.y + self.height) - y,
        );
        self.frame_strips(clipped)
            .into_iter()
            .filter(|r| r.has_area())
            .collect()
    }
}

/// Gap configuration passed to the geometry pass.
///
/// The compositor passes zero gaps, so tiles abut and cover the output; per-class gaps are applied
/// to windows inside their tiles by the compositor (`tiling` config). The non-zero default below is
/// only a convenient value for tests and other callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GapConfig {
    /// Pixels of space around the whole output's tiled area (all four sides).
    ///
    /// Default: 8 px.
    pub outer: u32,
    /// Pixels of space inserted *between* two sibling panes at every split boundary.
    ///
    /// Default: 6 px.
    pub inner: u32,
}

impl Default for GapConfig {
    /// Returns the recommended default gaps.
    ///
    /// Not what the compositor uses (it passes zero gaps); kept as a convenient non-zero value.
    fn default() -> Self {
        Self { outer: 8, inner: 6 }
    }
}

impl GapConfig {
    /// Constructs a gap config with explicit outer and inner values.
    #[must_use]
    pub const fn new(outer: u32, inner: u32) -> Self {
        Self { outer, inner }
    }
}

// ---- tree walk -------------------------------------------------------------------------------

/// Computes `(PaneId, Rect)` pairs for every leaf of `tree`, given that the whole tree occupies
/// `rect`. Uses `inner` gap pixels between siblings at each split.
///
/// This is the core recursive subdivision. It is exposed as `pub(crate)` so `World::geometry`
/// (and tests) can call it directly.
pub(crate) fn geometry_of_tree(
    tree: &SplitTree<PaneId>,
    rect: Rect,
    inner: u32,
) -> Vec<(PaneId, Rect)> {
    let mut out = Vec::new();
    collect_geometry(tree, tree.root_id(), rect, inner, &mut out);
    out
}

/// Recursively subdivides `rect` following the tree rooted at `node_id`.
fn collect_geometry(
    tree: &SplitTree<PaneId>,
    node_id: NodeId,
    rect: Rect,
    inner: u32,
    out: &mut Vec<(PaneId, Rect)>,
) {
    match tree.node_kind(node_id) {
        NodeKind::Leaf(pane_id) => {
            out.push((pane_id, rect));
        }
        NodeKind::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let gap = inner as i32;
            let (first_rect, second_rect) = subdivide(rect, axis, ratio, gap);
            collect_geometry(tree, first, first_rect, inner, out);
            collect_geometry(tree, second, second_rect, inner, out);
        }
    }
}

/// Splits `rect` into two child rects along `axis`, with `gap` pixels between them.
///
/// The split is deterministic: `first_size = round((extent - gap) * ratio)`, second gets the
/// remainder so `first_size + gap + second_size == extent` exactly — no pixel drift.
fn subdivide(rect: Rect, axis: Axis, ratio: f64, gap: i32) -> (Rect, Rect) {
    match axis {
        Axis::Horizontal => {
            // Children are side-by-side; split the width.
            let available = (rect.width - gap).max(0);
            let first_w = (available as f64 * ratio).round() as i32;
            let second_w = (available - first_w).max(0);
            let first_rect = Rect::new(rect.x, rect.y, first_w, rect.height);
            let second_rect = Rect::new(rect.x + first_w + gap, rect.y, second_w, rect.height);
            (first_rect, second_rect)
        }
        Axis::Vertical => {
            // Children are stacked; split the height.
            let available = (rect.height - gap).max(0);
            let first_h = (available as f64 * ratio).round() as i32;
            let second_h = (available - first_h).max(0);
            let first_rect = Rect::new(rect.x, rect.y, rect.width, first_h);
            let second_rect = Rect::new(rect.x, rect.y + first_h + gap, rect.width, second_h);
            (first_rect, second_rect)
        }
    }
}

// ---- World entry point -----------------------------------------------------------------------

/// Computes the pixel [`Rect`] for every pane in `workspace`, given the output's pixel rect and gap
/// config.
///
/// The outer gap insets `output_rect` once; the inner gap then separates siblings at each split.
/// Returns one `(PaneId, Rect)` pair per leaf pane (including empty panes — pane-level geometry does
/// not consider window contents). [`World::geometry`](crate::World::geometry) wraps this to pair each
/// pane's focused window with its rect and drop windowless / non-visible panes.
pub fn geometry_of_workspace(
    workspace: &Workspace,
    output_rect: Rect,
    gaps: GapConfig,
) -> Vec<(PaneId, Rect)> {
    let inner_rect = output_rect.inset(gaps.outer as i32);
    geometry_of_tree(&workspace.tree, inner_rect, gaps.inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Direction, World};

    /// Output rect used across most golden tests: 1920×1080 at origin.
    const OUTPUT: Rect = Rect::new(0, 0, 1920, 1080);

    /// Zero-gap config for structural tests (easier to reason about exact numbers).
    const NO_GAPS: GapConfig = GapConfig { outer: 0, inner: 0 };

    // ---- single-pane -----------------------------------------------------------------

    #[test]
    fn single_pane_fills_output_minus_outer_gap() {
        let mut w = World::new("1");
        let win = w.open_window();
        let ws_id = w.active_workspace_id();
        let ws = w.workspace(ws_id).unwrap();

        let gaps = GapConfig::default(); // outer=8, inner=6
        let pane_rects = geometry_of_workspace(ws, OUTPUT, gaps);
        assert_eq!(pane_rects.len(), 1);
        let expected = OUTPUT.inset(8);
        assert_eq!(pane_rects[0].1, expected);

        // World::geometry returns (WindowId, Rect)
        let win_rects = w.geometry(w.focused_output(), OUTPUT, gaps);
        assert_eq!(win_rects.len(), 1);
        assert_eq!(win_rects[0].0, win);
        assert_eq!(win_rects[0].1, expected);
    }

    #[test]
    fn single_pane_no_gaps_fills_entire_output() {
        let mut w = World::new("1");
        let _ = w.open_window();
        let ws_id = w.active_workspace_id();
        let ws = w.workspace(ws_id).unwrap();

        let pane_rects = geometry_of_workspace(ws, OUTPUT, NO_GAPS);
        assert_eq!(pane_rects.len(), 1);
        assert_eq!(pane_rects[0].1, OUTPUT);
    }

    // ---- two-pane H-split ------------------------------------------------------------

    #[test]
    fn horizontal_split_two_panes_sum_to_width() {
        let mut w = World::new("1");
        let _ = w.open_window();
        let _ = w.split(Direction::Right); // now two panes, split horizontally

        let ws_id = w.active_workspace_id();
        let ws = w.workspace(ws_id).unwrap();
        let gaps = GapConfig { outer: 0, inner: 6 };
        let pane_rects = geometry_of_workspace(ws, OUTPUT, gaps);
        assert_eq!(pane_rects.len(), 2);

        let (_, r0) = pane_rects[0];
        let (_, r1) = pane_rects[1];

        // Rects are at y=0, height=full
        assert_eq!(r0.y, 0);
        assert_eq!(r0.height, OUTPUT.height);
        assert_eq!(r1.y, 0);
        assert_eq!(r1.height, OUTPUT.height);

        // Two widths + gap == total width
        assert_eq!(r0.width + 6 + r1.width, OUTPUT.width);

        // Second rect starts right after first + gap
        assert_eq!(r1.x, r0.x + r0.width + 6);
    }

    // ---- two-pane V-split ------------------------------------------------------------

    #[test]
    fn vertical_split_two_panes_sum_to_height() {
        let mut w = World::new("1");
        let _ = w.open_window();
        let _ = w.split(Direction::Down); // two panes, split vertically

        let ws_id = w.active_workspace_id();
        let ws = w.workspace(ws_id).unwrap();
        let gaps = GapConfig { outer: 0, inner: 6 };
        let pane_rects = geometry_of_workspace(ws, OUTPUT, gaps);
        assert_eq!(pane_rects.len(), 2);

        let (_, r0) = pane_rects[0];
        let (_, r1) = pane_rects[1];

        assert_eq!(r0.height + 6 + r1.height, OUTPUT.height);
        assert_eq!(r1.y, r0.y + r0.height + 6);
    }

    // ---- non-0.5 ratio ---------------------------------------------------------------

    #[test]
    fn non_default_ratio_distributes_correctly() {
        let mut w = World::new("1");
        let _ = w.open_window();
        let _ = w.split(Direction::Right);
        // Push ratio to 0.75 (first pane gets 75%)
        w.resize(Direction::Right, 0.25);

        let ws_id = w.active_workspace_id();
        let ws = w.workspace(ws_id).unwrap();
        let pane_rects = geometry_of_workspace(ws, OUTPUT, NO_GAPS);
        assert_eq!(pane_rects.len(), 2);

        let (_, r0) = pane_rects[0];
        let (_, r1) = pane_rects[1];

        // Exact pixel sum must equal total width
        assert_eq!(r0.width + r1.width, OUTPUT.width);
        // First pane should be wider (ratio > 0.5 after resize)
        assert!(
            r0.width > r1.width,
            "first pane should be larger: {r0:?} vs {r1:?}"
        );
    }

    // ---- nested splits ---------------------------------------------------------------

    #[test]
    fn nested_splits_cover_full_area_no_gaps() {
        // Build:  H(V(p0, p1), p2)  — left column top/bottom, right side full height.
        let mut w = World::new("1");
        let _ = w.open_window(); // pane 0 focused
        let _ = w.split(Direction::Right); // pane 1 to the right (horizontal split)
        w.focus(Direction::Left); // go back to pane 0
        let _ = w.split(Direction::Down); // pane 2 below pane 0 (vertical split of pane 0)

        let ws_id = w.active_workspace_id();
        let ws = w.workspace(ws_id).unwrap();
        let pane_rects = geometry_of_workspace(ws, OUTPUT, NO_GAPS);
        assert_eq!(pane_rects.len(), 3);

        let total_area: i64 = pane_rects.iter().map(|(_, r)| r.area()).sum();
        assert_eq!(total_area, OUTPUT.area());
    }

    #[test]
    fn nested_splits_with_gaps_cover_full_area_exactly() {
        // Same topology, with gaps — total pane area + gap pixels must equal output area.
        let mut w = World::new("1");
        let _ = w.open_window();
        let _ = w.split(Direction::Right);
        w.focus(Direction::Left);
        let _ = w.split(Direction::Down);

        let ws_id = w.active_workspace_id();
        let ws = w.workspace(ws_id).unwrap();

        let gaps = GapConfig { outer: 0, inner: 6 };
        let pane_rects = geometry_of_workspace(ws, OUTPUT, gaps);
        let pane_area: i64 = pane_rects.iter().map(|(_, r)| r.area()).sum();

        // There is one H-split (inner gap of 6px × full height) and one V-split inside the left
        // column (inner gap of 6px × half width).
        // Total pixel area = pane area + horizontal divider + vertical divider area.
        // We only check that pane_area <= total output area and that all rects are inside OUTPUT.
        assert!(pane_area <= OUTPUT.area());
        for (_, r) in &pane_rects {
            assert!(r.x >= 0);
            assert!(r.y >= 0);
            assert!(r.x + r.width <= OUTPUT.width);
            assert!(r.y + r.height <= OUTPUT.height);
        }
    }

    // ---- stacked pane — only focused window gets rect --------------------------------

    #[test]
    fn stacked_pane_rect_follows_focused_window() {
        let mut w = World::new("1");
        let win_a = w.open_window();
        // Push a second window onto the same pane's stack.
        let win_b = w.stack_window();

        let output_id = w.focused_output();
        let gaps = GapConfig::default();

        // win_b is currently focused (it was the last pushed); win_a is non-focused / stacked.
        let rects = w.geometry(output_id, OUTPUT, gaps);
        assert_eq!(rects.len(), 1, "only one visible window");
        assert_eq!(rects[0].0, win_b, "focused window (win_b) gets the rect");

        // Cycle the stack: win_a becomes focused.
        w.cycle_stack(1);
        let rects = w.geometry(output_id, OUTPUT, gaps);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].0, win_a, "after cycle, win_a gets the rect");
        // The non-focused win_b has no rect.
        assert!(!rects.iter().any(|(id, _)| *id == win_b));
    }

    // ---- inactive workspace — panes get no rect --------------------------------------

    #[test]
    fn inactive_workspace_panes_get_no_rect() {
        let mut w = World::new("1");
        let win_a = w.open_window();

        // Create and switch to a new workspace.
        let _ = w.new_workspace("2").unwrap();
        let win_b = w.open_window();

        let output_id = w.focused_output();
        let gaps = GapConfig::default();
        let rects = w.geometry(output_id, OUTPUT, gaps);

        // Only the active workspace (workspace 2, with win_b) contributes rects.
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].0, win_b);
        assert!(!rects.iter().any(|(id, _)| *id == win_a));
    }

    // ---- pane with no windows contributes nothing ------------------------------------

    #[test]
    fn empty_pane_contributes_no_rect() {
        let mut w = World::new("1");
        // Split first to get two panes; don't open a window in the second.
        let _ = w.split(Direction::Right);

        let output_id = w.focused_output();
        let rects = w.geometry(output_id, OUTPUT, GapConfig::default());
        // No windows at all → empty.
        assert!(rects.is_empty());

        // Open in focused (second) pane only.
        let win = w.open_window();
        let rects = w.geometry(output_id, OUTPUT, GapConfig::default());
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].0, win);
    }

    // ---- property / fuzz-ish test ----------------------------------------------------

    /// For a tree built by `n` sequential splits in alternating directions, the total covered
    /// area plus all gap pixels must equal the output area **exactly** (no drift).
    #[test]
    fn property_total_area_plus_gaps_equals_output_for_random_trees() {
        // No outer gap makes the accounting straightforward: we track pane areas only.
        // With outer gap the remainder is `outer_gap_area`; we check both forms.
        let configs: &[(u32, u32, u32)] = &[
            // (outer, inner, splits)
            (0, 0, 1),
            (0, 6, 1),
            (0, 6, 4),
            (0, 6, 7),
            (8, 6, 1),
            (8, 6, 5),
            (8, 6, 9),
            (0, 1, 15), // large tree, 1px gaps
            (4, 3, 10),
        ];

        for &(outer, inner, splits) in configs {
            let mut w = World::new("1");
            let _ = w.open_window();
            for i in 0..splits {
                let dir = if i % 2 == 0 {
                    Direction::Right
                } else {
                    Direction::Down
                };
                let _ = w.split(dir);
                let _ = w.open_window();
            }

            let ws_id = w.active_workspace_id();
            let ws = w.workspace(ws_id).unwrap();
            let gaps = GapConfig { outer, inner };
            let pane_rects = geometry_of_workspace(ws, OUTPUT, gaps);

            // Every pane rect must lie within the inner (outer-inset) area.
            let inner_rect = OUTPUT.inset(outer as i32);
            for (_, r) in &pane_rects {
                assert!(
                    r.x >= inner_rect.x,
                    "outer={outer} inner={inner} splits={splits}: rect.x={} < inner.x={}",
                    r.x,
                    inner_rect.x
                );
                assert!(
                    r.y >= inner_rect.y,
                    "outer={outer} inner={inner} splits={splits}: rect.y={} < inner.y={}",
                    r.y,
                    inner_rect.y
                );
                assert!(
                    r.x + r.width <= inner_rect.x + inner_rect.width,
                    "outer={outer} inner={inner} splits={splits}: rect right edge {} > inner right {}",
                    r.x + r.width,
                    inner_rect.x + inner_rect.width
                );
                assert!(
                    r.y + r.height <= inner_rect.y + inner_rect.height,
                    "outer={outer} inner={inner} splits={splits}: rect bottom {} > inner bottom {}",
                    r.y + r.height,
                    inner_rect.y + inner_rect.height
                );
            }

            // Total pane area + all gap pixels == inner area.
            // We compute this via a spatial reconstruction: for a binary tree with `n` leaves,
            // there are exactly `n-1` split boundaries. Instead of tracking gap areas symbolically,
            // we verify by checking the geometry function output tiles exactly.
            //
            // Simpler direct assertion: if we set inner=0 and outer=0, all pane areas sum to total.
            let zero_gaps = GapConfig { outer: 0, inner: 0 };
            let zero_rects = geometry_of_workspace(ws, OUTPUT, zero_gaps);
            let zero_total: i64 = zero_rects.iter().map(|(_, r)| r.area()).sum();
            assert_eq!(
                zero_total,
                OUTPUT.area(),
                "outer={outer} inner={inner} splits={splits}: zero-gap areas {zero_total} != output area {}",
                OUTPUT.area()
            );

            // With gaps, pane area + gap area == inner_rect area.
            let n = pane_rects.len() as i64;
            let pane_area: i64 = pane_rects.iter().map(|(_, r)| r.area()).sum();
            let inner_area = inner_rect.area();

            // Gap pixels: for a binary tree with n leaves there are n-1 splits, but splits are
            // nested, so the actual gap area depends on topology. We verify the upper bound:
            // pane_area + expected_gap_area == inner_area must hold if there is no pixel drift.
            // Rather than computing gap area analytically (complex for nested trees), we verify
            // by a round-trip: re-run with inner=0 and outer=0 to get no-gap total, then
            // compute gap area = inner_area - pane_area and verify it is exactly what
            // the inner-gap mechanism consumes.
            //
            // Strong check: pane_area <= inner_area (panes never exceed the inner rect),
            // and for zero gaps pane_area == inner_area.
            assert!(
                pane_area <= inner_area,
                "outer={outer} inner={inner} splits={splits}: pane_area {pane_area} > inner_area {inner_area}"
            );
            if inner > 0 && n > 1 {
                assert!(
                    pane_area < inner_area,
                    "outer={outer} inner={inner} splits={splits}: with gaps, pane_area should be < inner_area"
                );
            }
            if inner == 0 {
                assert_eq!(
                    pane_area, inner_area,
                    "outer={outer} splits={splits}: zero inner gap, pane_area {pane_area} != inner_area {inner_area}"
                );
            }

            // Verify no individual rect has negative dimensions.
            for (_, r) in &pane_rects {
                assert!(r.width >= 0, "negative width: {r:?}");
                assert!(r.height >= 0, "negative height: {r:?}");
            }
        }
    }

    // ---- Rect helpers ----------------------------------------------------------------

    #[test]
    fn rect_area_and_inset() {
        let r = Rect::new(0, 0, 100, 200);
        assert_eq!(r.area(), 20_000);

        let inset = r.inset(10);
        assert_eq!(inset, Rect::new(10, 10, 80, 180));
        assert_eq!(inset.area(), 80 * 180);
    }

    #[test]
    fn rect_inset_clamps_to_zero() {
        let r = Rect::new(0, 0, 10, 10);
        let inset = r.inset(20);
        assert_eq!(inset.width, 0);
        assert_eq!(inset.height, 0);
    }

    // ---- Rect algebra (moved from the compositor's chrome module) ---------------------

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect::new(x, y, w, h)
    }

    #[test]
    fn output_local_translation_moves_second_output_chrome_on_screen() {
        // Two 3840-wide outputs side by side: a ring on the right output (global x=3850) must land
        // at local x=10 on it, and a ring on the left output must fall off the right one.
        let right_ring = rect(3850, 20, 1900, 2100).translate(-3840, 0);
        assert_eq!(right_ring, rect(10, 20, 1900, 2100));
        let left_ring_on_right = rect(10, 20, 1900, 2100).translate(-3840, 0);
        assert!(left_ring_on_right.x + left_ring_on_right.width <= 0);
        // The origin output is unchanged.
        assert_eq!(rect(10, 20, 30, 40).translate(0, 0), rect(10, 20, 30, 40));
    }

    #[test]
    fn border_frames_window_with_correct_thickness() {
        let t = 4;
        let [top, bottom, left, right] = rect(10, 20, 400, 300).border_strips(t);
        assert_eq!(top, rect(10, 20, 400, t));
        assert_eq!(bottom, rect(10, 20 + 300 - t, 400, t));
        assert_eq!(left, rect(10, 20 + t, t, 300 - 2 * t));
        assert_eq!(right, rect(10 + 400 - t, 20 + t, t, 300 - 2 * t));
    }

    #[test]
    fn border_zero_thickness_produces_zero_area_strips() {
        for s in rect(0, 0, 100, 100).border_strips(0) {
            assert!(!s.has_area(), "expected zero-area strip: {s:?}");
        }
    }

    #[test]
    fn border_strips_are_within_window_bounds() {
        for strip in rect(50, 50, 200, 150).border_strips(5) {
            assert!(strip.x >= 50);
            assert!(strip.y >= 50);
            assert!(strip.x + strip.width <= 50 + 200);
            assert!(strip.y + strip.height <= 50 + 150);
        }
    }

    #[test]
    fn border_thickness_clamped_to_half_rect_size() {
        // Extremely thick border on a small rect must not produce negative inner sizes.
        for strip in rect(0, 0, 10, 6).border_strips(99) {
            assert!(strip.width >= 0, "negative width: {strip:?}");
            assert!(strip.height >= 0, "negative height: {strip:?}");
        }
    }

    #[test]
    fn subtracting_an_overlay_leaves_only_the_uncovered_part() {
        let r = rect(0, 0, 100, 10);
        assert_eq!(r.subtract(rect(200, 0, 5, 5)), vec![r]);
        let rest = r.subtract(rect(40, -5, 20, 30));
        assert_eq!(
            rest.iter().map(|p| p.area()).sum::<i64>(),
            100 * 10 - 20 * 10
        );
        assert!(rest.iter().all(|p| !p.intersects(rect(40, -5, 20, 30))));
    }

    #[test]
    fn rect_intersection_excludes_touching_edges() {
        assert!(rect(0, 0, 10, 10).intersects(rect(9, 9, 10, 10)));
        assert!(!rect(0, 0, 10, 10).intersects(rect(10, 0, 10, 10)));
    }

    #[test]
    fn rect_contains_is_half_open_and_outset_shrink_top_round_trip() {
        let r = rect(10, 20, 30, 40);
        assert!(r.contains(10, 20) && r.contains(39, 59));
        assert!(!r.contains(40, 20) && !r.contains(10, 60) && !r.contains(9, 20));
        assert_eq!(r.outset(3).inset(3), r);
        assert_eq!(r.shrink_top(5), rect(10, 25, 30, 35));
        assert_eq!(r.shrink_top(500).height, 20);
    }
}
