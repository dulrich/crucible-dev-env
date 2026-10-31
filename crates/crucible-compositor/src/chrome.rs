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
//! [`PhysicalMap`] (at the output's fractional scale) and [`solid`]. No model mutation; no stale cache — the elements are rebuilt each frame.
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
//! The tile ring and gap come from `TilingConfig`; the constants below (`BORDER_THICKNESS`,
//! `BORDER_COLOR`, the pip sizes and colours) are the fixed overlay border and pip styling.

use smithay::backend::renderer::{
    element::{Id, Kind, solid::SolidColorRenderElement},
    utils::CommitCounter,
};

use smithay::utils::{Physical, Rectangle, Scale};

use crucible_config::{OverlayStyle, Rgb, TilingConfig};
use crucible_layout::{Projection, Rect};

use crate::{
    layout::{LayoutState, WindowClass, window_rect},
    state::{Backend, CrucibleState},
};

// ---------------------------------------------------------------------------
// Fixed chrome styling constants.
// ---------------------------------------------------------------------------

/// Set to `false` to disable all compositor chrome (focus border + stack badges).
pub const ENABLE_CHROME: bool = true;

/// Width of the focus-ring border, in logical pixels (applied at scale 1.0).
pub const BORDER_THICKNESS: i32 = 3;

/// Thickness of the mark ring, in logical pixels. Independent of the configured tile rings (app
/// tiles default to `ring=0`), so a marked window is always visible.
pub const MARK_THICKNESS: i32 = BORDER_THICKNESS;

/// Colour of the mark ring and of the stack pip of a marked, non-visible window. Amber, distinct
/// from the teal [`BORDER_COLOR`].
pub const MARK_COLOR: [f32; 4] = [0.95, 0.65, 0.15, 1.0];

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

/// A solid-colour render element covering the physical `rect` (in the output's frame). Build the
/// rect from a logical one with [`PhysicalMap::rect`].
#[must_use]
pub fn solid(rect: Rectangle<i32, Physical>, color: [f32; 4]) -> SolidColorRenderElement {
    SolidColorRenderElement::new(
        Id::new(),
        rect,
        CommitCounter::default(),
        color,
        Kind::Unspecified,
    )
}

/// The one logical-to-physical conversion for chrome rects (output-relative, logical) at an
/// output's fractional scale `s`.
///
/// ## Rounding rule
/// 1. **Edges, not sizes.** A rect's physical extent is `[round(x*s), round((x+w)*s))` per axis, so
///    two logical rects that share an edge share the physical edge: chrome stays seamless (no gap,
///    no overlap, area sum preserved).
/// 2. **Window edges follow smithay.** Smithay places a window at the rounded origin
///    `round(x*s)` and ends it at `origin + round(w*s)` (`space/mod.rs` rounds the location,
///    `element/surface.rs` sizes the surface relative to it), which can differ from
///    `round((x+w)*s)` by one pixel. For each registered window rect `(x, w)`, a chrome edge at
///    `x` lands on `round(x*s)` and an edge at `x + w` lands on `round(x*s) + round(w*s)`, so the
///    ring / fill bordering a window meets the client surface exactly. A window only snaps the
///    edges of rects that overlap it on the other axis (closed interval), and the first matching
///    window wins.
///
/// At scale 1 both rules reduce to the identity, so the output is unchanged.
#[derive(Debug, Clone)]
pub struct PhysicalMap {
    scale: Scale<f64>,
    windows: Vec<Rect>,
}

impl PhysicalMap {
    /// A map at `scale` whose rule 2 applies to `windows` (output-relative logical window rects).
    #[must_use]
    pub fn new(scale: Scale<f64>, windows: Vec<Rect>) -> Self {
        Self { scale, windows }
    }

    /// A map with no window edges to meet: plain edge rounding.
    #[must_use]
    pub fn plain(scale: Scale<f64>) -> Self {
        Self::new(scale, Vec::new())
    }

    /// The output scale this map converts at.
    #[must_use]
    pub fn scale(&self) -> Scale<f64> {
        self.scale
    }

    /// The physical rect of the logical `r`.
    #[must_use]
    pub fn rect(&self, r: Rect) -> Rectangle<i32, Physical> {
        let near =
            |lo: i32, len: i32, w_lo: i32, w_len: i32| lo <= w_lo + w_len && w_lo <= lo + len;
        let xs: Vec<(i32, i32)> = self
            .windows
            .iter()
            .filter(|w| near(r.y, r.height, w.y, w.height))
            .map(|w| (w.x, w.width))
            .collect();
        let ys: Vec<(i32, i32)> = self
            .windows
            .iter()
            .filter(|w| near(r.x, r.width, w.x, w.width))
            .map(|w| (w.y, w.height))
            .collect();
        let (x0, x1) = (
            snap(r.x, &xs, self.scale.x),
            snap(r.x + r.width, &xs, self.scale.x),
        );
        let (y0, y1) = (
            snap(r.y, &ys, self.scale.y),
            snap(r.y + r.height, &ys, self.scale.y),
        );
        Rectangle::new((x0, y0).into(), ((x1 - x0).max(0), (y1 - y0).max(0)).into())
    }
}

/// Rounds a logical coordinate to physical, honouring window `(start, len)` spans (rule 2).
fn snap(coord: i32, spans: &[(i32, i32)], s: f64) -> i32 {
    let round = |v: i32| (f64::from(v) * s).round() as i32;
    for &(start, len) in spans {
        if coord == start {
            return round(start);
        }
        if coord == start + len {
            return round(start) + round(len);
        }
    }
    round(coord)
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
    /// Frame strips (terminals only): the ring geometry of an **unfocused** terminal tile whose
    /// `frame` colour is configured; empty otherwise.
    pub frame: Vec<Rect>,
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
        WindowClass::Terminal => {
            let strips = ring_strips(
                terminal_ring_rect(tile, tiling),
                i32::try_from(tiling.terminal.ring).unwrap_or(i32::MAX),
            );
            let framed = !focused && tiling.terminal.frame.is_some();
            TilePlan {
                ring: if focused { strips.clone() } else { Vec::new() },
                fill: Vec::new(),
                frame: if framed { strips } else { Vec::new() },
            }
        }
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
                frame: Vec::new(),
            }
        }
    }
}

/// The rect a terminal's ring (and frame) is drawn on: the tile inset by `ring_inset`.
fn terminal_ring_rect(tile: Rect, tiling: &TilingConfig) -> Rect {
    tile.inset(i32::try_from(tiling.terminal.ring_inset).unwrap_or(i32::MAX))
}

/// Plans the mark ring of the marked window of `class` in `tile`. **Pure.**
///
/// Always [`MARK_THICKNESS`] thick, whatever the configured ring: a terminal's ring sits inside the
/// tile (at the terminal ring's `ring_inset`, so it stays clear of the text); an app's sits outside the window, in the inset (clamped to it). With no inset (gap 0) it
/// falls back to the inside of the tile.
#[must_use]
pub fn plan_mark_ring(tile: Rect, class: WindowClass, tiling: &TilingConfig) -> Vec<Rect> {
    if class == WindowClass::App {
        let window = window_rect(tile, class, tiling);
        let t = MARK_THICKNESS.min(window.x - tile.x);
        if t > 0 {
            return ring_strips(window.outset(t), t);
        }
    }
    let base = if class == WindowClass::Terminal {
        terminal_ring_rect(tile, tiling)
    } else {
        tile
    };
    ring_strips(base, MARK_THICKNESS)
}

/// The pip of the marked window at stack `index` of a pane of `len` windows, or `None` when the
/// pane shows no pips (`len <= 1`) or `index` is out of range. **Pure.**
#[must_use]
pub fn mark_pip(
    pane_rect: Rect,
    focused: usize,
    len: usize,
    index: usize,
    margin: i32,
) -> Option<Rect> {
    stack_pips_with_margin(pane_rect, focused, len, margin)
        .get(index)
        .map(|&(r, _)| r)
}

/// Pure plan of the mark indicator (output-local, drawn in front of everything else): the ring
/// strips of a visible marked window and/or the marked window's pip, both in [`MARK_COLOR`]. The
/// ring is suppressed, and a pip intersecting the overlay dropped, while an overlay is mapped.
#[must_use]
pub fn plan_mark_chrome(
    ring: &[Rect],
    pip: Option<Rect>,
    overlay: Option<OverlayChrome>,
    output_origin: (i32, i32),
) -> Vec<(Rect, [f32; 4])> {
    let local = |r: Rect| r.translate(-output_origin.0, -output_origin.1);
    let mut out = Vec::new();
    if let Some(p) = pip
        && !overlay.is_some_and(|o| p.intersects(o.rect))
    {
        out.push((local(p), MARK_COLOR));
    }
    if overlay.is_none() {
        out.extend(ring.iter().map(|&r| (local(r), MARK_COLOR)));
    }
    out
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

/// The frame strips (output-local) in `color`, clipped around a mapped `overlay` like the fills.
/// Drawn behind every other chrome. Pure.
#[must_use]
pub fn plan_frames(
    frames: &[Rect],
    color: [f32; 4],
    overlay: Option<OverlayChrome>,
    output_origin: (i32, i32),
) -> Vec<(Rect, [f32; 4])> {
    let local = |r: Rect| r.translate(-output_origin.0, -output_origin.1);
    let mut out = Vec::new();
    for &rect in frames {
        match overlay {
            Some(o) => out.extend(rect.subtract(o.rect).into_iter().map(|r| (local(r), color))),
            None => out.push((local(rect), color)),
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
/// Rects are converted to physical pixels at `scale` through [`PhysicalMap`].
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
    scale: Scale<f64>,
) -> Vec<SolidColorRenderElement> {
    if !ENABLE_CHROME {
        return Vec::new();
    }
    let world = &layout.world;

    // ---- 1. Per visible tile: focus ring (focused window) and app-inset fill (every app) ----
    let focus = projection.focus();
    let mut ring: Vec<Rect> = Vec::new();
    let mut fills: Vec<Rect> = Vec::new();
    let mut frames: Vec<Rect> = Vec::new();
    let marked = world.marked();
    let mut mark_ring: Vec<Rect> = Vec::new();
    let mut mark_pip_rect: Option<Rect> = None;
    for &(id, tile) in projection.windows() {
        if marked == Some(id) {
            mark_ring = plan_mark_ring(tile, layout.window_class(id), tiling);
        }
        let plan = plan_tile(tile, layout.window_class(id), tiling, focus == Some(id));
        ring.extend(plan.ring);
        fills.extend(plan.fill);
        frames.extend(plan.frame);
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
            let mut pane_pips =
                stack_pips_with_margin(pane_rect, pane.focused_index(), len, margin);
            // The marked window's pip is drawn by the mark plan, in the mark colour.
            if let Some(index) = marked.and_then(|m| pane.windows().iter().position(|w| *w == m))
                && index != pane.focused_index()
                && let Some(pip) = pane_pips.get(index).map(|&(r, _)| r)
            {
                mark_pip_rect = Some(pip);
                pane_pips.remove(index);
            }
            pips.extend(pane_pips);
        }
    }

    // Window rects (output-relative) whose edges the chrome must meet exactly (rule 2).
    let mut windows: Vec<Rect> = projection
        .windows()
        .iter()
        .map(|&(id, tile)| {
            window_rect(tile, layout.window_class(id), tiling)
                .translate(-output_origin.0, -output_origin.1)
        })
        .collect();
    windows.extend(overlay.map(|o| o.rect.translate(-output_origin.0, -output_origin.1)));
    let map = PhysicalMap::new(scale, windows);

    let mut plan = plan_mark_chrome(&mark_ring, mark_pip_rect, overlay, output_origin);
    plan.extend(plan_chrome(
        &ring,
        &fills,
        fill_color(tiling.app.fill),
        &pips,
        overlay,
        output_origin,
    ));
    if let Some(frame) = tiling.terminal.frame {
        plan.extend(plan_frames(
            &frames,
            fill_color(frame),
            overlay,
            output_origin,
        ));
    }
    plan.into_iter()
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

    // ---- mark indicator ----

    #[test]
    fn mark_ring_is_amber_and_at_least_mark_thickness_even_with_ring_zero() {
        let tile = rect(0, 0, 400, 300);
        let t = tiling(10, 0); // app gap 10, app ring 0 (the default)
        assert!(plan_tile(tile, WindowClass::App, &t, true).ring.is_empty());
        let strips = plan_mark_ring(tile, WindowClass::App, &t);
        assert_eq!(strips.len(), 4);
        let window = window_rect(tile, WindowClass::App, &t);
        for r in &strips {
            assert!(!r.intersects(window), "mark ring over content: {r:?}");
            assert!(inside(*r, tile));
        }
        let out = plan_mark_chrome(&strips, None, None, (0, 0));
        assert_eq!(out.len(), 4);
        assert!(out.iter().all(|(_, c)| *c == MARK_COLOR));
        assert_ne!(MARK_COLOR, BORDER_COLOR);
        assert_eq!(
            area_sum(&strips),
            i64::from((400 - 20 + 6) * (300 - 20 + 6) - (400 - 20) * (300 - 20))
        );
    }

    #[test]
    fn mark_ring_falls_back_inside_the_tile_without_an_inset_and_for_terminals() {
        let tile = rect(0, 0, 400, 300);
        let flush = plan_mark_ring(tile, WindowClass::App, &tiling(0, 0));
        assert_eq!(area_sum(&flush), 400 * 300 - 394 * 294);
        let term = plan_mark_ring(tile, WindowClass::Terminal, &tiling(10, 0));
        assert_eq!(area_sum(&term), 400 * 300 - 394 * 294);
        assert!(term.iter().all(|r| inside(*r, tile)));
    }

    #[test]
    fn mark_ring_clamps_to_a_thin_inset() {
        let tile = rect(0, 0, 400, 300);
        let strips = plan_mark_ring(tile, WindowClass::App, &tiling(1, 0));
        assert_eq!(strips[0].y, 0, "ring clamped to the 1px inset");
        assert!(strips.iter().all(|r| inside(*r, tile)));
    }

    #[test]
    fn mark_pip_is_the_marked_stack_index() {
        let pane = rect(0, 0, 800, 600);
        let all = stack_pips_with_margin(pane, 0, 3, PIP_MARGIN);
        for (i, (want, _)) in all.iter().enumerate() {
            assert_eq!(
                mark_pip(pane, 0, 3, i, PIP_MARGIN),
                Some(*want),
                "index {i}"
            );
        }
        assert_eq!(mark_pip(pane, 0, 3, 3, PIP_MARGIN), None);
        assert_eq!(mark_pip(pane, 0, 1, 0, PIP_MARGIN), None);
        let out = plan_mark_chrome(&[], Some(all[2].0), None, (0, 0));
        assert_eq!(out, vec![(all[2].0, MARK_COLOR)]);
    }

    #[test]
    fn mark_chrome_respects_overlay_and_output_origin() {
        let ring = rect(100, 100, 50, 50).border_strips(3);
        let pip = rect(120, 105, 8, 8);
        let over = overlay(true, OverlayStyle::Terminal).map(|mut o| {
            o.rect = rect(110, 100, 100, 100);
            o
        });
        // Overlay drops the ring and the intersecting pip.
        assert!(plan_mark_chrome(&ring, Some(pip), over, (0, 0)).is_empty());
        // Without an overlay everything is translated into the output frame.
        let out = plan_mark_chrome(&ring, Some(pip), None, (100, 100));
        assert_eq!(out.len(), 5);
        assert_eq!(out[0].0, rect(20, 5, 8, 8));
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
    fn terminal_ring_inset_and_unfocused_frame() {
        let tile = rect(100, 50, 400, 300);
        let mut t = TilingConfig::default();
        t.terminal.gap = 9;
        t.terminal.ring = 1;
        t.terminal.ring_inset = 4;
        // No frame colour: unfocused draws nothing, focused ring sits 4px inside the tile.
        assert_eq!(
            plan_tile(tile, WindowClass::Terminal, &t, false),
            TilePlan::default()
        );
        let focused = plan_tile(tile, WindowClass::Terminal, &t, true);
        assert!(focused.frame.is_empty());
        assert_eq!(area_sum(&focused.ring), 392 * 292 - 390 * 290);
        assert!(focused.ring.iter().all(|r| inside(*r, tile.inset(4))));
        // With a frame colour the unfocused tile carries the same strips as a frame.
        t.terminal.frame = Some(Rgb(0x3a, 0x3a, 0x3a));
        let idle = plan_tile(tile, WindowClass::Terminal, &t, false);
        assert!(idle.ring.is_empty());
        assert_eq!(idle.frame, focused.ring);
        assert!(
            plan_tile(tile, WindowClass::Terminal, &t, true)
                .frame
                .is_empty()
        );
        // The mark ring follows the same inset.
        let mark = plan_mark_ring(tile, WindowClass::Terminal, &t);
        assert!(mark.iter().all(|r| inside(*r, tile.inset(4))));
        // App tiles never carry a frame.
        assert!(
            plan_tile(tile, WindowClass::App, &t, false)
                .frame
                .is_empty()
        );
        // Frames clip around an overlay.
        let overlay = OverlayChrome {
            rect: rect(0, 0, 100000, 100000),
            border: false,
            style: OverlayStyle::N1Med,
        };
        assert!(plan_frames(&idle.frame, [0.0; 4], Some(overlay), (0, 0)).is_empty());
        assert_eq!(plan_frames(&idle.frame, [0.0; 4], None, (100, 50)).len(), 4);
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

#[cfg(test)]
mod scale_tests {
    use super::*;

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
            let pieces: Vec<_> = plan
                .ring
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
