//! Compositor chrome: focus border and per-pane stack indicator.
//!
//! Produces [`SolidColorRenderElement`]s drawn **above** the windows every frame (they ride in the
//! [`CustomRenderElements`](crate::render::CustomRenderElements) layer, which sits on top of the
//! `Space` window elements in `output_elements`).
//!
//! ## Design
//! All geometry math is factored into **pure, unit-tested helpers** at the bottom of this module
//! ([`border_rects`] and [`stack_pips`]).  The compositor-side entry point
//! [`CrucibleState::chrome_elements`] calls those helpers with data read from
//! `self.layout.world` (model focus/stack state) and `self.last_layout` (global-coord rects that
//! were last applied to the `Space`).  No model mutation; no stale cache — the elements are rebuilt
//! from live model state each frame.
//!
//! ## Per-output correctness
//! `last_layout` is built by `apply_geometry` as the union of every output's **own** active
//! workspace's visible windows (each shifted to global Space coordinates).  Chrome reads those same
//! rects, so it automatically covers only currently-visible windows across all outputs without any
//! additional per-output filtering.
//!
//! ## Phase 3 seam
//! The constants below (`ENABLE_CHROME`, `BORDER_THICKNESS`, `BORDER_COLOR`, …) are hard-coded for
//! Phase 2.  Phase 3 should source them from `crucible-config` (KDL) and pass them in; the helpers
//! (`border_rects`, `stack_pips`) already accept all geometry parameters explicitly so they need no
//! changes.

use smithay::{
    backend::renderer::{
        element::{Id, Kind, solid::SolidColorRenderElement},
        utils::CommitCounter,
    },
    utils::{Physical, Rectangle},
};

use crucible_layout::{Rect, WindowId};

use crate::{
    render::CustomRenderElements,
    state::{Backend, CrucibleState},
};

// ---------------------------------------------------------------------------
// Phase-2 chrome constants — Phase 3 replaces these with crucible-config values.
// ---------------------------------------------------------------------------

/// Set to `false` to disable all compositor chrome (focus border + stack badges).
pub const ENABLE_CHROME: bool = true;

/// Width of the focus-ring border, in logical pixels (applied at scale 1.0 for Phase 2).
pub const BORDER_THICKNESS: i32 = 3;

/// Focus-ring colour as `[R, G, B, A]` premultiplied floats.  Bright teal accent.
pub const BORDER_COLOR: [f32; 4] = [0.20, 0.80, 0.80, 1.0];

/// Side length of a single stack-indicator pip, in logical pixels.
pub const PIP_SIZE: i32 = 8;

/// Gap between consecutive pips, in logical pixels.
pub const PIP_GAP: i32 = 3;

/// Margin from the top-right corner of the pane rect to the first pip, in logical pixels.
pub const PIP_MARGIN: i32 = 6;

/// Colour of the **active** (focused) pip: bright accent matches the border.
pub const PIP_ACTIVE_COLOR: [f32; 4] = [0.20, 0.80, 0.80, 1.0];

/// Colour of **inactive** pips: muted grey so the position is still legible.
pub const PIP_INACTIVE_COLOR: [f32; 4] = [0.45, 0.45, 0.45, 0.85];

// ---------------------------------------------------------------------------
// Pure geometry helpers — unit-tested, no compositor types.
// ---------------------------------------------------------------------------

/// Computes the four border-strip rects that frame `window` with a border of `thickness` pixels.
///
/// The strips are ordered `[top, bottom, left, right]` and together form a complete rectangular
/// frame with no gaps or overlaps at the corners (the horizontal strips span the full width;
/// the vertical strips fill the remaining height between them).
///
/// # Guarantees
/// - Each strip is strictly within or touching the outer boundary of `window`.
/// - The union of the four strips covers exactly the frame pixels.
/// - For `thickness == 0` or a zero-size rect all strips have zero area.
/// - `x`/`y` values are never negative relative to the window origin.
#[must_use]
pub fn border_rects(window: Rect, thickness: i32) -> [Rectangle<i32, Physical>; 4] {
    let t = thickness
        .max(0)
        .min(window.width / 2)
        .min(window.height / 2);
    let x = window.x;
    let y = window.y;
    let w = window.width;
    let h = window.height;

    let top = Rectangle::new((x, y).into(), (w, t).into());
    let bottom = Rectangle::new((x, y + h - t).into(), (w, t).into());
    let left = Rectangle::new((x, y + t).into(), (t, h - 2 * t).into());
    let right = Rectangle::new((x + w - t, y + t).into(), (t, h - 2 * t).into());

    [top, bottom, left, right]
}

/// Computes pip rectangles for a pane-stack indicator.
///
/// Returns `len` small squares placed in a row at the **top-right** corner of `pane_rect`,
/// inset by `PIP_MARGIN` and spaced by `PIP_GAP`.  Each tuple is `(rect, is_focused)`;
/// `is_focused` is `true` for the pip at index `focused`.
///
/// Returns an empty `Vec` when `len <= 1` (single-window pane needs no indicator).
///
/// # Guarantees
/// - Returns exactly `len` pips when `len > 1`, zero when `len <= 1`.
/// - Exactly one pip has `is_focused == true`, at position `focused` (clamped to `0..len`).
/// - All pips are contained within `pane_rect` (when `pane_rect` is large enough for them).
#[must_use]
pub fn stack_pips(
    pane_rect: Rect,
    focused: usize,
    len: usize,
) -> Vec<(Rectangle<i32, Physical>, bool)> {
    if len <= 1 {
        return Vec::new();
    }

    let focused = focused.min(len.saturating_sub(1));

    // Total width of the pip row: each pip is PIP_SIZE wide, separated by PIP_GAP.
    let row_width = len as i32 * PIP_SIZE + (len as i32 - 1) * PIP_GAP;

    // Anchor at top-right of pane, inset by PIP_MARGIN.
    let row_x = pane_rect.x + pane_rect.width - PIP_MARGIN - row_width;
    let row_y = pane_rect.y + PIP_MARGIN;

    (0..len)
        .map(|i| {
            let x = row_x + i as i32 * (PIP_SIZE + PIP_GAP);
            let rect = Rectangle::new((x, row_y).into(), (PIP_SIZE, PIP_SIZE).into());
            (rect, i == focused)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Compositor entry point — reads model state, produces SolidColorRenderElements.
// ---------------------------------------------------------------------------

/// Builds the chrome render elements (focus border + stack badges) for the current frame.
///
/// Reads `focused_window` from the model's world, looks it up in `last_layout` to get its rect,
/// then constructs border strips via [`border_rects`].  Then iterates every output's active
/// workspace's panes; for panes with `> 1` window, builds pip indicators via [`stack_pips`].
///
/// Returns an empty vec when [`ENABLE_CHROME`] is `false`.
///
/// Caller note: the returned elements should be prepended to (or chained before) the cursor
/// elements so the stacking order from front to back is: cursor → chrome → windows.
pub fn build_chrome_elements(
    world: &crucible_layout::World,
    last_layout: &[(WindowId, Rect)],
) -> Vec<SolidColorRenderElement> {
    if !ENABLE_CHROME {
        return Vec::new();
    }

    // Build a fast rect-lookup from the visible window set.
    let rect_of: std::collections::HashMap<WindowId, Rect> = last_layout.iter().copied().collect();

    let mut elements: Vec<SolidColorRenderElement> = Vec::new();

    // ---- 1. Focus-ring border around the focused window ----
    if let Some(focused_id) = world.focused_window() {
        if let Some(&win_rect) = rect_of.get(&focused_id) {
            for strip in border_rects(win_rect, BORDER_THICKNESS) {
                elements.push(SolidColorRenderElement::new(
                    Id::new(),
                    strip,
                    CommitCounter::default(),
                    BORDER_COLOR,
                    Kind::Unspecified,
                ));
            }
        }
    }

    // ---- 2. Per-pane stack-position indicators ----
    // Iterate every output's active workspace panes; for any pane with > 1 window, draw pips
    // anchored at the top-right of that pane's focused-window rect in last_layout.
    for (_output_id, output) in world.outputs() {
        let ws_id = output.active_workspace();
        let Some(workspace) = world.workspace(ws_id) else {
            continue;
        };
        for pane_id in workspace.panes() {
            let Some(pane) = world.pane(pane_id) else {
                continue;
            };
            let len = pane.windows.len();
            if len <= 1 {
                continue;
            }
            // The pane's "rect" in last_layout is its focused window's rect.
            let Some(focused_win) = pane.focused_window() else {
                continue;
            };
            let Some(&pane_rect) = rect_of.get(&focused_win) else {
                continue;
            };
            for (rect, is_focused) in stack_pips(pane_rect, pane.focused, len) {
                let color = if is_focused {
                    PIP_ACTIVE_COLOR
                } else {
                    PIP_INACTIVE_COLOR
                };
                elements.push(SolidColorRenderElement::new(
                    Id::new(),
                    rect,
                    CommitCounter::default(),
                    color,
                    Kind::Unspecified,
                ));
            }
        }
    }

    elements
}

// ---------------------------------------------------------------------------
// Compositor entry point on CrucibleState.
// ---------------------------------------------------------------------------

impl<B: Backend + 'static> CrucibleState<B> {
    /// Returns the chrome render elements for the current frame.
    ///
    /// Reads live model state (`self.layout.world`) and `self.last_layout` (which holds the
    /// global-Space rects that were last applied to the `Space`) to produce:
    /// - A focus-ring border (4 strips) around the focused window.
    /// - Per-pane stack-position pips for any pane with `> 1` window.
    ///
    /// These elements must be placed **above the windows but below the cursor** in the front-to-back
    /// element list.  The winit and udev render paths chain them in before calling `output_elements`.
    ///
    /// No renderer reference is needed: `SolidColorRenderElement` is renderer-agnostic.  The generic
    /// `R` parameter exists only so the return type matches `CustomRenderElements<R>` at the call
    /// site, allowing direct concatenation with the cursor elements without extra conversions.
    pub fn chrome_elements<R>(&self) -> Vec<CustomRenderElements<R>>
    where
        R: smithay::backend::renderer::Renderer
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem,
    {
        build_chrome_elements(&self.layout.world, &self.last_layout)
            .into_iter()
            .map(CustomRenderElements::SolidColor)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Unit tests for pure geometry helpers.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    // ---- border_rects tests ----

    #[test]
    fn border_frames_window_with_correct_thickness() {
        let window = rect(10, 20, 400, 300);
        let t = 4;
        let [top, bottom, left, right] = border_rects(window, t);

        // Top strip: full width, `t` tall, at window origin.
        assert_eq!(top.loc.x, 10);
        assert_eq!(top.loc.y, 20);
        assert_eq!(top.size.w, 400);
        assert_eq!(top.size.h, t);

        // Bottom strip: full width, `t` tall, at the bottom edge.
        assert_eq!(bottom.loc.x, 10);
        assert_eq!(bottom.loc.y, 20 + 300 - t);
        assert_eq!(bottom.size.w, 400);
        assert_eq!(bottom.size.h, t);

        // Left strip: `t` wide, fills the gap between top and bottom.
        assert_eq!(left.loc.x, 10);
        assert_eq!(left.loc.y, 20 + t);
        assert_eq!(left.size.w, t);
        assert_eq!(left.size.h, 300 - 2 * t);

        // Right strip: mirrors left on the right edge.
        assert_eq!(right.loc.x, 10 + 400 - t);
        assert_eq!(right.loc.y, 20 + t);
        assert_eq!(right.size.w, t);
        assert_eq!(right.size.h, 300 - 2 * t);
    }

    #[test]
    fn border_zero_thickness_produces_zero_area_strips() {
        let window = rect(0, 0, 100, 100);
        let strips = border_rects(window, 0);
        for s in strips {
            assert!(
                s.size.w == 0 || s.size.h == 0,
                "expected zero-area strip: {s:?}"
            );
        }
    }

    #[test]
    fn border_strips_are_within_window_bounds() {
        let window = rect(50, 50, 200, 150);
        let [top, bottom, left, right] = border_rects(window, 5);
        for strip in [top, bottom, left, right] {
            assert!(strip.loc.x >= 50);
            assert!(strip.loc.y >= 50);
            assert!(strip.loc.x + strip.size.w <= 50 + 200);
            assert!(strip.loc.y + strip.size.h <= 50 + 150);
        }
    }

    #[test]
    fn border_thickness_clamped_to_half_rect_size() {
        // Extremely thick border on a small rect must not produce negative inner sizes.
        let window = rect(0, 0, 10, 6);
        let [top, bottom, left, right] = border_rects(window, 99);
        // Thickness is clamped to min(width/2, height/2) = 3; left/right inner height = 0.
        for strip in [top, bottom, left, right] {
            assert!(strip.size.w >= 0, "negative width: {strip:?}");
            assert!(strip.size.h >= 0, "negative height: {strip:?}");
        }
    }

    // ---- stack_pips tests ----

    #[test]
    fn single_window_pane_produces_no_pips() {
        let pips = stack_pips(rect(0, 0, 800, 600), 0, 1);
        assert!(pips.is_empty());
    }

    #[test]
    fn zero_window_pane_produces_no_pips() {
        let pips = stack_pips(rect(0, 0, 800, 600), 0, 0);
        assert!(pips.is_empty());
    }

    #[test]
    fn pip_count_equals_len() {
        for len in 2..=6 {
            let pips = stack_pips(rect(0, 0, 800, 600), 0, len);
            assert_eq!(pips.len(), len, "expected {len} pips, got {}", pips.len());
        }
    }

    #[test]
    fn exactly_one_focused_pip() {
        for len in 2..=5 {
            for focused in 0..len {
                let pips = stack_pips(rect(0, 0, 800, 600), focused, len);
                let active_count = pips.iter().filter(|(_, is_f)| *is_f).count();
                assert_eq!(
                    active_count, 1,
                    "len={len} focused={focused}: expected 1 active pip, got {active_count}"
                );
                // The focused pip must be at the `focused` index.
                assert!(pips[focused].1, "pip at focused={focused} must be active");
            }
        }
    }

    #[test]
    fn all_pips_contained_within_pane_rect() {
        let pane = rect(100, 50, 400, 300);
        let pips = stack_pips(pane, 1, 3);
        for (r, _) in &pips {
            assert!(r.loc.x >= pane.x, "pip left of pane: {r:?}");
            assert!(r.loc.y >= pane.y, "pip above pane: {r:?}");
            assert!(
                r.loc.x + r.size.w <= pane.x + pane.width,
                "pip right of pane: {r:?}"
            );
            assert!(
                r.loc.y + r.size.h <= pane.y + pane.height,
                "pip below pane: {r:?}"
            );
        }
    }

    #[test]
    fn focused_clamp_does_not_panic_on_out_of_bounds() {
        // focused >= len: should not panic, should clamp.
        let pips = stack_pips(rect(0, 0, 800, 600), 99, 3);
        assert_eq!(pips.len(), 3);
        let active_count = pips.iter().filter(|(_, f)| *f).count();
        assert_eq!(active_count, 1);
    }
}
