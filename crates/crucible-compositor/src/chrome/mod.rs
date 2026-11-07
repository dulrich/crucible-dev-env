//! Compositor chrome: focus border, mark ring, per-pane stack indicator and overlay ring.
//!
//! Produces [`SolidColorRenderElement`]s drawn **above** the windows every frame (they ride in the
//! [`CustomRenderElements`](crate::render::CustomRenderElements) layer, which sits on top of the
//! `Space` window elements; see [`crate::render::frame_elements`]).
//!
//! ## Design
//! All planning is **pure** and Smithay-free, in [`plan`], working in layout [`Rect`]s (the
//! generic rect algebra lives on [`Rect`] in `crucible-layout`): [`stack_pips`], [`plan_tile`] and
//! [`plan_mark_ring`] produce the parts, [`paint_list`] orders them front to back with one overlay
//! rule per entry, and [`resolve`] applies that rule and the output translate. The entry point
//! [`build_chrome_elements`] reads `layout.world` (model stack state) and the projection last
//! applied to the `Space`, and converts each resolved rect to Smithay exactly once, through
//! [`PhysicalMap`] (at the output's fractional scale) and [`solid`]. No model mutation; no stale
//! cache — the elements are rebuilt each frame.
//!
//! ## Per-output correctness
//! The projection is derived across every output's **own** active workspace's visible windows (each
//! shifted to global Space coordinates).  Chrome reads those same rects, so it covers only
//! currently-visible windows across all outputs.  Render elements, however, are positioned in the
//! *output's* frame, so [`build_chrome_elements`] takes the rendered output's global origin and
//! translates every rect by it ([`Rect::translate`]); without that, a second output mirrors the
//! first output's chrome and never shows its own.
//!
//! ## Constants
//! The tile ring and gap come from `TilingConfig`; the fixed sizes ([`BORDER_THICKNESS`], the pip
//! sizes) live in [`plan`] and the colours in [`crate::palette`].

pub mod plan;

pub use plan::{
    BORDER_THICKNESS, ChromeParts, MARK_THICKNESS, OverlayChrome, PIP_GAP, PIP_MARGIN, PIP_SIZE,
    PaintEntry, PipKind, StripKind, TilePlan, UnderOverlay, overlay_border_color, paint_list,
    pip_margin, plan_mark_ring, plan_tile, resolve, stack_pips,
};

use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::utils::Scale;

use crucible_config::TilingConfig;
use crucible_layout::{Projection, Rect};

use crate::{
    layout::{LayoutState, WindowClass, window_rect},
    physical::{PhysicalMap, solid},
    state::{Backend, CrucibleState},
};

/// Builds the chrome render elements (class-aware focus ring, app-inset fill, terminal frames,
/// mark ring, stack badges and the overlay ring) for the current frame, in the frame of the output
/// whose global origin is `output_origin`.
///
/// Reads the focused identity and every visible tile from the applied [`Projection`] (tile rects)
/// and plans each tile with [`plan_tile`] by its class ([`LayoutState::window_class`]). Then
/// iterates every output's active workspace's panes; for panes with `> 1` window, builds pip
/// indicators via [`stack_pips`]. The parts are ordered by [`paint_list`] and resolved against the
/// overlay by [`resolve`].
///
/// Rects are converted to physical pixels at `scale` through [`PhysicalMap`]. The elements sit
/// above the windows and below the header and cursor ([`crate::render::frame_elements`] owns that
/// order).
pub fn build_chrome_elements(
    layout: &LayoutState,
    projection: &Projection,
    empty_focus_rect: Option<Rect>,
    overlay: Option<OverlayChrome>,
    output_origin: (i32, i32),
    tiling: &TilingConfig,
    scale: Scale<f64>,
) -> Vec<SolidColorRenderElement> {
    let world = &layout.world;
    let mut parts = ChromeParts::default();

    // ---- 1. Per visible tile: focus ring / frame, app-inset fill, mark ring ----
    let focus = projection.focus();
    let marked = world.marked();
    for &(id, tile) in projection.windows() {
        // A fullscreen window covers its output edge to edge: no ring, fill or mark ring.
        if projection.is_fullscreen(id) {
            continue;
        }
        let class = layout.window_class(id);
        if marked == Some(id) {
            parts.mark_ring = plan_mark_ring(tile, class, tiling);
        }
        let plan = plan_tile(tile, class, tiling, focus == Some(id));
        match plan.kind {
            StripKind::Ring => parts.ring.extend(plan.strips),
            StripKind::Frame => parts.frames.extend(plan.strips),
            StripKind::None => {}
        }
        parts.fills.extend(plan.fill);
    }
    // The focused empty pane (no window) gets the terminal ring on its tile.
    if focus.is_none()
        && let Some(tile) = empty_focus_rect
    {
        parts
            .ring
            .extend(plan_tile(tile, WindowClass::Terminal, tiling, true).strips);
    }

    // ---- 2. Per-pane stack-position indicators (global rects) ----
    // Iterate every output's active workspace panes; for any pane with > 1 window, draw pips
    // anchored at the top-right of that pane's tile, inside the ring/gap strip when it fits.
    for (_output_id, output) in world.outputs() {
        let ws_id = output.active_workspace();
        let Some(workspace) = world.workspace(ws_id) else {
            continue;
        };
        if world.fullscreen_of(ws_id).is_some() {
            continue;
        }
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
            let marked_index = marked.and_then(|m| pane.windows().iter().position(|w| *w == m));
            parts.pips.extend(stack_pips(
                pane_rect,
                pane.focused_index(),
                len,
                marked_index,
                margin,
            ));
        }
    }

    // Window rects (output-relative) whose edges the chrome must meet exactly (rule 2).
    let mut windows: Vec<Rect> = projection
        .windows()
        .iter()
        .map(|&(id, tile)| {
            if projection.is_fullscreen(id) {
                tile
            } else {
                window_rect(tile, layout.window_class(id), tiling)
            }
            .translate(-output_origin.0, -output_origin.1)
        })
        .collect();
    windows.extend(overlay.map(|o| o.rect.translate(-output_origin.0, -output_origin.1)));
    let map = PhysicalMap::new(scale, windows);

    let entries = paint_list(
        &parts,
        tiling.app.fill.to_rgba_f32(),
        tiling.terminal.frame.map(|f| f.to_rgba_f32()),
        overlay,
    );
    resolve(&entries, overlay, output_origin)
        .into_iter()
        .map(|(rect, color)| solid(map.rect(rect), color))
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

#[cfg(test)]
mod scale_tests {
    use super::*;
    use crate::chrome::plan::ring_strips;
    use smithay::utils::{Physical, Rectangle};

    fn s170() -> Scale<f64> {
        Scale::from(170.0 / 120.0)
    }

    fn area(r: Rectangle<i32, Physical>) -> i64 {
        i64::from(r.size.w) * i64::from(r.size.h)
    }

    #[test]
    fn scale_one_reproduces_the_unscaled_rects() {
        let window = Rect::new(13, 10, 380, 280);
        let map = PhysicalMap::new(Scale::from(1.0), vec![window]);
        for r in [
            Rect::new(0, 0, 400, 300),
            Rect::new(13, 10, 380, 280),
            Rect::new(393, 6, 4, 288),
            Rect::new(9, 6, 388, 4),
        ] {
            let p = map.rect(r);
            assert_eq!(
                (p.loc.x, p.loc.y, p.size.w, p.size.h),
                (r.x, r.y, r.width, r.height)
            );
        }
    }

    #[test]
    fn scaled_app_ring_and_fill_tile_the_inset_without_gaps() {
        let tile = Rect::new(7, 9, 401, 299);
        for (gap, ring, focused) in [(10, 4, true), (10, 10, true), (10, 0, true), (3, 3, false)] {
            let mut t = TilingConfig::default();
            t.app.gap = gap;
            t.app.ring = ring;
            let window = window_rect(tile, WindowClass::App, &t);
            let map = PhysicalMap::new(s170(), vec![window]);
            let plan = plan_tile(tile, WindowClass::App, &t, focused);
            // App strips are always the focus ring.
            let pieces: Vec<_> = plan
                .strips
                .iter()
                .chain(&plan.fill)
                .map(|&r| map.rect(r))
                .collect();
            let (ptile, pwin) = (map.rect(tile), map.rect(window));
            assert_eq!(
                pieces.iter().map(|&p| area(p)).sum::<i64>(),
                area(ptile) - area(pwin),
                "gap={gap} ring={ring}"
            );
            for (i, a) in pieces.iter().enumerate() {
                assert!(ptile.contains_rect(*a), "{a:?} leaves {ptile:?}");
                assert!(!a.overlaps(pwin), "{a:?} over window {pwin:?}");
                for b in &pieces[i + 1..] {
                    assert!(!a.overlaps(*b), "overlap {a:?} {b:?}");
                }
            }
        }
    }

    #[test]
    fn chrome_meets_the_window_at_smithays_edge() {
        // x=13, w=380 at 170/120: smithay places the window at round(18.42)=18 and ends it at
        // 18 + round(538.33) = 556, one pixel short of plain edge rounding (round(556.75) = 557).
        let s = s170();
        let window = Rect::new(13, 10, 380, 280);
        let map = PhysicalMap::new(s, vec![window]);
        let smithay_end = (13.0_f64 * s.x).round() as i32 + (380.0_f64 * s.x).round() as i32;
        assert_eq!(smithay_end, 556);
        let plain = PhysicalMap::plain(s).rect(Rect::new(393, 10, 4, 280));
        assert_eq!(plain.loc.x, 557);
        let pwin = map.rect(window);
        assert_eq!((pwin.loc.x, pwin.loc.x + pwin.size.w), (18, 556));
        let right = map.rect(Rect::new(393, 10, 4, 280));
        let left = map.rect(Rect::new(9, 10, 4, 280));
        assert_eq!(right.loc.x, 556, "no gap or overlap on the right");
        assert_eq!(
            left.loc.x + left.size.w,
            18,
            "no gap or overlap on the left"
        );
        // Vertically the same rule: y=10, h=280.
        let top = map.rect(Rect::new(13, 6, 380, 4));
        let bottom = map.rect(Rect::new(13, 290, 380, 4));
        assert_eq!(top.loc.y + top.size.h, pwin.loc.y);
        assert_eq!(bottom.loc.y, pwin.loc.y + pwin.size.h);
        // A terminal ring drawn inside a tile that is the window ends at the same edge.
        let right_edge = ring_strips(window, 3)
            .iter()
            .map(|&r| map.rect(r))
            .map(|p| p.loc.x + p.size.w)
            .max();
        assert_eq!(right_edge, Some(556));
    }
}
