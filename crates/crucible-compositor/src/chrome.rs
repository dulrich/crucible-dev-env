//! Compositor chrome: focus border and per-pane stack indicator.
//!
//! Produces [`SolidColorRenderElement`]s drawn **above** the windows every frame (they ride in the
//! [`CustomRenderElements`](crate::render::CustomRenderElements) layer, which sits on top of the
//! `Space` window elements; see [`crate::render::frame_elements`]).
//!
//! ## Design
//! All planning is **pure** and works in layout [`Rect`]s (the generic rect algebra lives on
//! [`Rect`] in `crucible-layout`): [`stack_pips`], [`plan_tile`] and [`plan_chrome`]. The entry
//! point [`build_chrome_elements`] reads `layout.world` (model stack state) and the projection last
//! applied to the `Space`, and converts each planned rect to Smithay exactly once, through
//! [`solid`]. No model mutation; no stale cache — the elements are rebuilt each frame.
//!
//! ## Per-output correctness
//! The projection is derived across every output's **own** active workspace's visible windows (each
//! shifted to global Space coordinates).  Chrome reads those same rects, so it covers only
//! currently-visible windows across all outputs.  Render elements, however, are positioned in the
//! *output's* frame, so [`build_chrome_elements`] takes the rendered output's global origin and
//! translates every rect by it ([`Rect::translate`]); without that, a second output mirrors the
//! first output's chrome and never shows its own.
//!
//! ## Phase 3 seam
//! The constants below (`ENABLE_CHROME`, `BORDER_THICKNESS`, `BORDER_COLOR`, …) are hard-coded for
//! Phase 2.  Phase 3 should source them from `crucible-config` (KDL) and pass them in; the helpers
//! already accept all geometry parameters explicitly so they need no changes.

use smithay::backend::renderer::{
    element::{Id, Kind, solid::SolidColorRenderElement},
    utils::CommitCounter,
};

use crucible_config::{OverlayStyle, Rgb, TilingConfig};
use crucible_layout::{Projection, Rect};

use crate::{
    layout::{LayoutState, WindowClass, window_rect},
    state::{Backend, CrucibleState},
};

// ---------------------------------------------------------------------------
// Hardcoded chrome constants — Phase 3 replaces these with crucible-config values.
// ---------------------------------------------------------------------------

/// Set to `false` to disable all compositor chrome (focus border + stack badges).
pub const ENABLE_CHROME: bool = true;

/// Width of the focus-ring border, in logical pixels (applied at scale 1.0).
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
// The one layout-to-Smithay conversion.
// ---------------------------------------------------------------------------

/// A solid-colour render element covering `rect` (already in the output's frame).
#[must_use]
pub fn solid(rect: Rect, color: [f32; 4]) -> SolidColorRenderElement {
    SolidColorRenderElement::new(
        Id::new(),
        smithay::utils::Rectangle::<i32, smithay::utils::Physical>::new(
            (rect.x, rect.y).into(),
            (rect.width, rect.height).into(),
        ),
        CommitCounter::default(),
        color,
        Kind::Unspecified,
    )
}

// ---------------------------------------------------------------------------
// Pure planning helpers — unit-tested, no compositor types.
// ---------------------------------------------------------------------------

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
pub fn stack_pips(pane_rect: Rect, focused: usize, len: usize) -> Vec<(Rect, bool)> {
    stack_pips_with_margin(pane_rect, focused, len, PIP_MARGIN)
}

/// The pip inset from the tile's top-right corner for a ring/gap strip `strip` pixels thick: the
/// pip is centred in the strip when the strip is at least [`PIP_SIZE`], else [`PIP_MARGIN`].
#[must_use]
pub fn pip_margin(strip: i32) -> i32 {
    if strip >= PIP_SIZE {
        (strip - PIP_SIZE) / 2
    } else {
        PIP_MARGIN
    }
}

/// [`stack_pips`] with an explicit corner `margin` (see [`pip_margin`]).
#[must_use]
pub fn stack_pips_with_margin(
    pane_rect: Rect,
    focused: usize,
    len: usize,
    margin: i32,
) -> Vec<(Rect, bool)> {
    if len <= 1 {
        return Vec::new();
    }

    let focused = focused.min(len.saturating_sub(1));

    // Total width of the pip row: each pip is PIP_SIZE wide, separated by PIP_GAP.
    let row_width = len as i32 * PIP_SIZE + (len as i32 - 1) * PIP_GAP;

    // Anchor at top-right of pane, inset by PIP_MARGIN.
    let row_x = pane_rect.x + pane_rect.width - margin - row_width;
    let row_y = pane_rect.y + margin;

    (0..len)
        .map(|i| {
            let x = row_x + i as i32 * (PIP_SIZE + PIP_GAP);
            (Rect::new(x, row_y, PIP_SIZE, PIP_SIZE), i == focused)
        })
        .collect()
}

/// Border colour for an overlay of `style`: the N1 accent, or [`BORDER_COLOR`] for `terminal`.
#[must_use]
pub fn overlay_border_color(style: OverlayStyle) -> [f32; 4] {
    match style {
        OverlayStyle::Terminal => BORDER_COLOR,
        // #ff8a3d
        OverlayStyle::N1Dark | OverlayStyle::N1Med => [1.0, 138.0 / 255.0, 61.0 / 255.0, 1.0],
        // #f07714
        OverlayStyle::N1Light => [240.0 / 255.0, 119.0 / 255.0, 20.0 / 255.0, 1.0],
    }
}

/// The mapped overlay's global rect plus its border settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayChrome {
    /// The overlay's rect in global Space coordinates.
    pub rect: Rect,
    /// Whether to draw the border ring.
    pub border: bool,
    /// The overlay style (selects the ring colour).
    pub style: OverlayStyle,
}

/// The strips of a `thickness` border inside `rect` that have area.
fn ring_strips(rect: Rect, thickness: i32) -> Vec<Rect> {
    rect.border_strips(thickness)
        .into_iter()
        .filter(|r| r.has_area())
        .collect()
}

/// The chrome rects of one visible tile (global coordinates).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TilePlan {
    /// Focus-ring strips (empty unless the tile's window is focused and its ring is non-zero).
    pub ring: Vec<Rect>,
    /// Opaque fill rects (apps only): the part of the app inset the ring does not cover.
    pub fill: Vec<Rect>,
}

/// Plans the ring and fill of the window of `class` in `tile`. **Pure.**
///
/// - Terminal: the ring (`tiling.terminal.ring`) is drawn **inside** the tile, in the terminal's
///   own padding; there is no fill.
/// - App: the window is [`window_rect`]; the focused ring (`tiling.app.ring`, clamped to the inset)
///   is drawn **outside** the window, in the inset; the rest of the inset up to the tile edge is
///   `fill`, so the ring band and the fill tile the inset exactly and no background shows.
#[must_use]
pub fn plan_tile(tile: Rect, class: WindowClass, tiling: &TilingConfig, focused: bool) -> TilePlan {
    match class {
        WindowClass::Terminal => TilePlan {
            ring: if focused {
                ring_strips(
                    tile,
                    i32::try_from(tiling.terminal.ring).unwrap_or(i32::MAX),
                )
            } else {
                Vec::new()
            },
            fill: Vec::new(),
        },
        WindowClass::App => {
            let window = window_rect(tile, class, tiling);
            let inset = window.x - tile.x;
            let ring_t = if focused {
                i32::try_from(tiling.app.ring)
                    .unwrap_or(i32::MAX)
                    .min(inset)
            } else {
                0
            };
            let ring_outer = window.outset(ring_t);
            TilePlan {
                ring: if ring_t > 0 {
                    ring_strips(ring_outer, ring_t)
                } else {
                    Vec::new()
                },
                fill: tile
                    .frame_strips(ring_outer)
                    .into_iter()
                    .filter(|r| r.has_area())
                    .collect(),
            }
        }
    }
}

/// The opaque colour for a configured fill, as `[R, G, B, A]`.
#[must_use]
pub fn fill_color(fill: Rgb) -> [f32; 4] {
    [
        f32::from(fill.0) / 255.0,
        f32::from(fill.1) / 255.0,
        f32::from(fill.2) / 255.0,
        1.0,
    ]
}

/// Pure chrome plan: the coloured rects (output-local) for the app fills, the focus ring, the pips
/// and, while an overlay is mapped, the overlay ring. Earlier entries are drawn in front.
///
/// While `overlay` is `Some` the pane focus ring is suppressed, no pip intersecting the overlay is
/// emitted and the fills are clipped around it (the overlay window sits below chrome). With
/// `border` on, a ring of [`BORDER_THICKNESS`] is drawn **outside** the overlay.
#[must_use]
pub fn plan_chrome(
    ring: &[Rect],
    fills: &[Rect],
    fill: [f32; 4],
    pips: &[(Rect, bool)],
    overlay: Option<OverlayChrome>,
    output_origin: (i32, i32),
) -> Vec<(Rect, [f32; 4])> {
    let local = |r: Rect| r.translate(-output_origin.0, -output_origin.1);
    let mut out = Vec::new();
    if let Some(o) = overlay
        && o.border
    {
        let t = BORDER_THICKNESS;
        let color = overlay_border_color(o.style);
        for strip in local(o.rect.outset(t)).border_strips(t) {
            out.push((strip, color));
        }
    }
    for &(pip, is_focused) in pips {
        if overlay.is_some_and(|o| pip.intersects(o.rect)) {
            continue;
        }
        let color = if is_focused {
            PIP_ACTIVE_COLOR
        } else {
            PIP_INACTIVE_COLOR
        };
        out.push((local(pip), color));
    }
    if overlay.is_none() {
        for &strip in ring {
            out.push((local(strip), BORDER_COLOR));
        }
    }
    for &rect in fills {
        match overlay {
            Some(o) => out.extend(rect.subtract(o.rect).into_iter().map(|r| (local(r), fill))),
            None => out.push((local(rect), fill)),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Compositor entry point — reads model state, produces SolidColorRenderElements.
// ---------------------------------------------------------------------------

/// Builds the chrome render elements (class-aware focus ring, app-inset fill, stack badges and the
/// overlay ring) for the current frame, in the frame of the output whose global origin is
/// `output_origin`.
///
/// Reads the focused identity and every visible tile from the applied [`Projection`] (tile rects)
/// and plans each tile with [`plan_tile`] by its class ([`LayoutState::window_class`]). Then
/// iterates every output's active workspace's panes; for panes with `> 1` window, builds pip
/// indicators via [`stack_pips_with_margin`].
///
/// Returns an empty vec when [`ENABLE_CHROME`] is `false`. The elements sit above the windows and
/// below the header and cursor ([`crate::render::frame_elements`] owns that order).
pub fn build_chrome_elements(
    layout: &LayoutState,
    projection: &Projection,
    empty_focus_rect: Option<Rect>,
    overlay: Option<OverlayChrome>,
    output_origin: (i32, i32),
    tiling: &TilingConfig,
) -> Vec<SolidColorRenderElement> {
    if !ENABLE_CHROME {
        return Vec::new();
    }
    let world = &layout.world;

    // ---- 1. Per visible tile: focus ring (focused window) and app-inset fill (every app) ----
    let focus = projection.focus();
    let mut ring: Vec<Rect> = Vec::new();
    let mut fills: Vec<Rect> = Vec::new();
    for &(id, tile) in projection.windows() {
        let plan = plan_tile(tile, layout.window_class(id), tiling, focus == Some(id));
        ring.extend(plan.ring);
        fills.extend(plan.fill);
    }
    // The focused empty pane (no window) gets the terminal ring on its tile.
    if focus.is_none()
        && let Some(tile) = empty_focus_rect
    {
        ring.extend(plan_tile(tile, WindowClass::Terminal, tiling, true).ring);
    }

    // ---- 2. Per-pane stack-position indicators (global rects) ----
    let mut pips: Vec<(Rect, bool)> = Vec::new();
    // Iterate every output's active workspace panes; for any pane with > 1 window, draw pips
    // anchored at the top-right of that pane's tile, inside the ring/gap strip when it fits.
    for (_output_id, output) in world.outputs() {
        let ws_id = output.active_workspace();
        let Some(workspace) = world.workspace(ws_id) else {
            continue;
        };
        for pane_id in workspace.panes() {
            let Some(pane) = world.pane(pane_id) else {
                continue;
            };
            let len = pane.windows().len();
            if len <= 1 {
                continue;
            }
            // The pane's "rect" in the projection is its focused window's tile.
            let Some(focused_win) = pane.focused_window() else {
                continue;
            };
            let Some(pane_rect) = projection.rect_of(focused_win) else {
                continue;
            };
            let strip = match layout.window_class(focused_win) {
                WindowClass::Terminal => tiling.terminal.gap,
                WindowClass::App => tiling.app.gap,
            };
            let margin = pip_margin(i32::try_from(strip).unwrap_or(i32::MAX));
            pips.extend(stack_pips_with_margin(
                pane_rect,
                pane.focused_index(),
                len,
                margin,
            ));
        }
    }

    plan_chrome(
        &ring,
        &fills,
        fill_color(tiling.app.fill),
        &pips,
        overlay,
        output_origin,
    )
    .into_iter()
    .map(|(rect, color)| solid(rect, color))
    .collect()
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// The mapped overlay's global rect and border settings; `None` while no overlay is mapped.
    pub(crate) fn overlay_chrome(&self) -> Option<OverlayChrome> {
        Some(OverlayChrome {
            rect: self.mapped_overlay_rect()?,
            border: self.config.overlay.border,
            style: self.config.overlay.style,
        })
    }
}

// ---------------------------------------------------------------------------
// Unit tests for the pure planning helpers (the rect algebra is tested in crucible-layout).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect::new(x, y, w, h)
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
            assert!(r.x >= pane.x, "pip left of pane: {r:?}");
            assert!(r.y >= pane.y, "pip above pane: {r:?}");
            assert!(
                r.x + r.width <= pane.x + pane.width,
                "pip right of pane: {r:?}"
            );
            assert!(
                r.y + r.height <= pane.y + pane.height,
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

    // ---- overlay chrome ----

    fn overlay(border: bool, style: OverlayStyle) -> Option<OverlayChrome> {
        Some(OverlayChrome {
            rect: rect(100, 100, 400, 300),
            border,
            style,
        })
    }

    #[test]
    fn no_overlay_draws_pane_ring_and_all_pips() {
        let pips = [(rect(100, 100, 8, 8), true), (rect(900, 10, 8, 8), false)];
        let ring = rect(0, 0, 1000, 800).border_strips(BORDER_THICKNESS);
        let out = plan_chrome(&ring, &[], APP_FILL, &pips, None, (0, 0));
        // 2 pips (in front), then 4 ring strips.
        assert_eq!(out.len(), 6);
        assert!(out[2..].iter().all(|(_, c)| *c == BORDER_COLOR));
    }

    const APP_FILL: [f32; 4] = [0.1, 0.2, 0.3, 1.0];

    #[test]
    fn overlay_suppresses_pane_ring() {
        let ring = rect(0, 0, 1000, 800).border_strips(BORDER_THICKNESS);
        let out = plan_chrome(
            &ring,
            &[],
            APP_FILL,
            &[],
            overlay(false, OverlayStyle::N1Med),
            (0, 0),
        );
        assert!(out.is_empty(), "no ring and no overlay border: {out:?}");
    }

    #[test]
    fn overlay_drops_intersecting_pips_only() {
        let pips = [(rect(120, 110, 8, 8), true), (rect(900, 10, 8, 8), false)];
        let out = plan_chrome(
            &[],
            &[],
            APP_FILL,
            &pips,
            overlay(false, OverlayStyle::N1Med),
            (0, 0),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0.x, 900);
    }

    #[test]
    fn overlay_ring_sits_outside_the_rect() {
        let out = plan_chrome(
            &[],
            &[],
            APP_FILL,
            &[],
            overlay(true, OverlayStyle::N1Med),
            (0, 0),
        );
        assert_eq!(out.len(), 4);
        let t = BORDER_THICKNESS;
        let inner = rect(100, 100, 400, 300);
        for (strip, color) in &out {
            assert_eq!(*color, overlay_border_color(OverlayStyle::N1Med));
            assert!(!strip.intersects(inner), "strip inside: {strip:?}");
            assert!(strip.x >= 100 - t && strip.y >= 100 - t);
            assert!(strip.x + strip.width <= 500 + t);
            assert!(strip.y + strip.height <= 400 + t);
        }
        // Top strip spans the expanded width and sits directly above the rect.
        assert_eq!(out[0].0.y, 100 - t);
        assert_eq!(out[0].0.width, 400 + 2 * t);
    }

    #[test]
    fn overlay_ring_is_translated_per_output() {
        let out = plan_chrome(
            &[],
            &[],
            APP_FILL,
            &[],
            overlay(true, OverlayStyle::Terminal),
            (50, 20),
        );
        assert_eq!(out[0].0.x, 100 - BORDER_THICKNESS - 50);
        assert_eq!(out[0].0.y, 100 - BORDER_THICKNESS - 20);
        assert_eq!(out[0].1, BORDER_COLOR);
    }

    #[test]
    fn overlay_border_colours_per_style() {
        assert_eq!(overlay_border_color(OverlayStyle::Terminal), BORDER_COLOR);
        assert_eq!(
            overlay_border_color(OverlayStyle::N1Dark),
            overlay_border_color(OverlayStyle::N1Med)
        );
        assert_ne!(
            overlay_border_color(OverlayStyle::N1Light),
            overlay_border_color(OverlayStyle::N1Med)
        );
    }

    // ---- per-class tile planning ----

    fn tiling(app_gap: u32, app_ring: u32) -> TilingConfig {
        let mut t = TilingConfig::default();
        t.app.gap = app_gap;
        t.app.ring = app_ring;
        t
    }

    fn area_sum(rects: &[Rect]) -> i64 {
        rects.iter().map(|r| r.area()).sum()
    }

    fn inside(inner: Rect, outer: Rect) -> bool {
        inner.x >= outer.x
            && inner.y >= outer.y
            && inner.x + inner.width <= outer.x + outer.width
            && inner.y + inner.height <= outer.y + outer.height
    }

    #[test]
    fn terminal_ring_is_inside_the_tile_and_has_no_fill() {
        let tile = rect(100, 50, 400, 300);
        let plan = plan_tile(tile, WindowClass::Terminal, &TilingConfig::default(), true);
        assert_eq!(plan.ring.len(), 4);
        assert!(plan.ring.iter().all(|r| inside(*r, tile)));
        // ring 3 around the tile edge: area = w*h - (w-6)*(h-6).
        assert_eq!(area_sum(&plan.ring), 400 * 300 - 394 * 294);
        assert!(plan.fill.is_empty());
        // An unfocused terminal draws nothing.
        let idle = plan_tile(tile, WindowClass::Terminal, &TilingConfig::default(), false);
        assert_eq!(idle, TilePlan::default());
    }

    #[test]
    fn app_ring_is_outside_the_window_and_inside_the_tile() {
        let tile = rect(0, 0, 400, 300);
        let t = tiling(10, 4);
        let window = window_rect(tile, WindowClass::App, &t);
        assert_eq!(window, rect(10, 10, 380, 280));
        let plan = plan_tile(tile, WindowClass::App, &t, true);
        for r in &plan.ring {
            assert!(inside(*r, tile), "ring leaves the tile: {r:?}");
            assert!(!r.intersects(window), "ring over content: {r:?}");
        }
        // The ring hugs the window: the top strip starts ring-thickness above it.
        assert_eq!(plan.ring[0].y, 10 - 4);
        assert_eq!(area_sum(&plan.ring), 388 * 288 - 380 * 280);
    }

    #[test]
    fn app_fill_and_ring_tile_the_inset_exactly() {
        let tile = rect(7, 9, 401, 299);
        for (gap, ring) in [(10, 4), (10, 10), (10, 0), (3, 3)] {
            let t = tiling(gap, ring);
            let window = window_rect(tile, WindowClass::App, &t);
            let inset_area = tile.area() - window.area();
            let focused = plan_tile(tile, WindowClass::App, &t, true);
            let all: Vec<Rect> = focused.ring.iter().chain(&focused.fill).copied().collect();
            assert_eq!(area_sum(&all), inset_area, "gap={gap} ring={ring}");
            for (i, a) in all.iter().enumerate() {
                assert!(inside(*a, tile) && !a.intersects(window));
                for b in &all[i + 1..] {
                    assert!(!a.intersects(*b), "overlap {a:?} {b:?}");
                }
            }
            // Unfocused: the fill alone covers the whole inset.
            let idle = plan_tile(tile, WindowClass::App, &t, false);
            assert!(idle.ring.is_empty());
            assert_eq!(area_sum(&idle.fill), inset_area);
        }
    }

    #[test]
    fn app_gap_zero_draws_nothing_and_ring_clamps_to_the_inset() {
        let tile = rect(0, 0, 100, 100);
        assert_eq!(
            plan_tile(tile, WindowClass::App, &tiling(0, 0), true),
            TilePlan::default()
        );
        // A ring wider than the (clamped) inset never leaves the tile.
        let plan = plan_tile(rect(0, 0, 10, 10), WindowClass::App, &tiling(50, 50), true);
        assert!(
            plan.ring
                .iter()
                .chain(&plan.fill)
                .all(|r| inside(*r, rect(0, 0, 10, 10)))
        );
    }

    #[test]
    fn pips_centre_in_a_strip_that_fits_them() {
        assert_eq!(pip_margin(6), PIP_MARGIN);
        assert_eq!(pip_margin(8), 0);
        assert_eq!(pip_margin(12), 2);
        let pips = stack_pips_with_margin(rect(0, 0, 400, 300), 0, 2, pip_margin(12));
        assert_eq!(pips[1].0.y, 2);
        assert_eq!(pips[1].0.x + PIP_SIZE, 400 - 2);
    }
}
