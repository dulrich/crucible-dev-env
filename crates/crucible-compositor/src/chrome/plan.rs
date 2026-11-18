//! Smithay-free chrome planning: pip geometry, tile rings/fills/frames, the mark ring, and the one
//! paint list with its overlay-rule [`resolve`]r. Everything here works in layout [`Rect`]s.

use crucible_config::{Rgb, TilingConfig};
use crucible_layout::Rect;

use crate::{
    layout::{WindowClass, window_rect},
    palette::{BORDER_COLOR, MARK_COLOR, PIP_ACTIVE_COLOR, PIP_INACTIVE_COLOR},
};

// ---------------------------------------------------------------------------
// Fixed chrome sizes (colours live in `crate::palette`).
// ---------------------------------------------------------------------------

/// Width of the focus-ring border, in logical pixels (applied at scale 1.0).
pub const BORDER_THICKNESS: i32 = 3;

/// Thickness of the mark ring, in logical pixels. Independent of the configured tile rings (app
/// tiles default to `ring=0`), so a marked window is always visible.
pub const MARK_THICKNESS: i32 = BORDER_THICKNESS;

/// Side length of a single stack-indicator pip, in logical pixels.
pub const PIP_SIZE: i32 = 8;

/// Gap between consecutive pips, in logical pixels.
pub const PIP_GAP: i32 = 3;

/// Margin from the top-right corner of the pane rect to the first pip, in logical pixels.
pub const PIP_MARGIN: i32 = 6;

// ---------------------------------------------------------------------------
// Pips.
// ---------------------------------------------------------------------------

/// What a stack pip stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipKind {
    /// The pane's focused (visible) window.
    Active,
    /// Any other window in the stack.
    Inactive,
    /// The marked window, when it is not the pane's focused window.
    Marked,
}

impl PipKind {
    /// The pip's colour.
    #[must_use]
    pub fn color(self) -> [f32; 4] {
        match self {
            Self::Active => PIP_ACTIVE_COLOR,
            Self::Inactive => PIP_INACTIVE_COLOR,
            Self::Marked => MARK_COLOR,
        }
    }
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

/// Computes pip rectangles for a pane-stack indicator.
///
/// Returns `len` small squares placed in a row at the **top-right** corner of `pane_rect`,
/// inset by `margin` (see [`pip_margin`]) and spaced by [`PIP_GAP`]. The pip at `focused` is
/// [`PipKind::Active`]; the pip at `marked` is [`PipKind::Marked`] unless it is the focused one;
/// the rest are [`PipKind::Inactive`].
///
/// Returns an empty `Vec` when `len <= 1` (single-window pane needs no indicator).
///
/// # Guarantees
/// - Returns exactly `len` pips when `len > 1`, zero when `len <= 1`.
/// - Exactly one pip is `Active`, at position `focused` (clamped to `0..len`).
/// - All pips are contained within `pane_rect` (when `pane_rect` is large enough for them).
#[must_use]
pub fn stack_pips(
    pane_rect: Rect,
    focused: usize,
    len: usize,
    marked: Option<usize>,
    margin: i32,
) -> Vec<(Rect, PipKind)> {
    if len <= 1 {
        return Vec::new();
    }

    let focused = focused.min(len.saturating_sub(1));

    // Total width of the pip row: each pip is PIP_SIZE wide, separated by PIP_GAP.
    let row_width = len as i32 * PIP_SIZE + (len as i32 - 1) * PIP_GAP;

    // Anchor at top-right of pane, inset by the margin.
    let row_x = pane_rect.x + pane_rect.width - margin - row_width;
    let row_y = pane_rect.y + margin;

    (0..len)
        .map(|i| {
            let x = row_x + i as i32 * (PIP_SIZE + PIP_GAP);
            let kind = if i == focused {
                PipKind::Active
            } else if marked == Some(i) {
                PipKind::Marked
            } else {
                PipKind::Inactive
            };
            (Rect::new(x, row_y, PIP_SIZE, PIP_SIZE), kind)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Overlay.
// ---------------------------------------------------------------------------

/// The mapped overlay's global rect plus its border settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayChrome {
    /// The overlay's rect in global Space coordinates.
    pub rect: Rect,
    /// Whether to draw the border ring.
    pub border: bool,
    /// The ring colour, resolved from the overlay style and accent
    /// ([`crucible_config::surface_accent`] for [`crucible_config::Surface::OverlayRing`]).
    pub ring: Rgb,
}

// ---------------------------------------------------------------------------
// Tiles and the mark ring.
// ---------------------------------------------------------------------------

/// The strips of a `thickness` border inside `rect` that have area.
pub(crate) fn ring_strips(rect: Rect, thickness: i32) -> Vec<Rect> {
    rect.border_strips(thickness)
        .into_iter()
        .filter(|r| r.has_area())
        .collect()
}

/// What a tile's [`TilePlan::strips`] are drawn as.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StripKind {
    /// No strips.
    #[default]
    None,
    /// The focus ring (in [`BORDER_COLOR`]; hidden while an overlay is mapped).
    Ring,
    /// The frame of an unfocused terminal whose `frame` colour is configured (clipped around an
    /// overlay, drawn behind every other chrome).
    Frame,
}

/// The chrome rects of one visible tile (global coordinates).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TilePlan {
    /// Ring or frame strips (see [`TilePlan::kind`]); empty exactly when `kind` is `None`.
    pub strips: Vec<Rect>,
    /// What [`TilePlan::strips`] are: the focus ring of a focused tile with a non-zero ring, the
    /// frame of an unfocused framed terminal, or nothing.
    pub kind: StripKind,
    /// Opaque fill rects (apps only): the part of the app inset the ring does not cover.
    pub fill: Vec<Rect>,
}

impl TilePlan {
    fn strips(strips: Vec<Rect>, kind: StripKind, fill: Vec<Rect>) -> Self {
        let kind = if strips.is_empty() {
            StripKind::None
        } else {
            kind
        };
        Self { strips, kind, fill }
    }
}

/// Plans the ring and fill of the window of `class` in `tile`. **Pure.**
///
/// - Terminal: the ring (`tiling.terminal.ring`) is drawn **inside** the tile, in the terminal's
///   own padding; there is no fill. An unfocused terminal with a configured `frame` colour carries
///   the same strips as a [`StripKind::Frame`].
/// - App: the window is [`window_rect`]; the focused ring (`tiling.app.ring`, clamped to the inset)
///   is drawn **outside** the window, in the inset; the rest of the inset up to the tile edge is
///   `fill`, so the ring band and the fill tile the inset exactly and no background shows.
#[must_use]
pub fn plan_tile(tile: Rect, class: WindowClass, tiling: &TilingConfig, focused: bool) -> TilePlan {
    match class {
        WindowClass::Terminal => {
            let kind = if focused {
                StripKind::Ring
            } else if tiling.terminal.frame.is_some() {
                StripKind::Frame
            } else {
                return TilePlan::default();
            };
            let strips = ring_strips(
                terminal_ring_rect(tile, tiling),
                i32::try_from(tiling.terminal.ring).unwrap_or(i32::MAX),
            );
            TilePlan::strips(strips, kind, Vec::new())
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
            let strips = if ring_t > 0 {
                ring_strips(ring_outer, ring_t)
            } else {
                Vec::new()
            };
            let fill = tile
                .frame_strips(ring_outer)
                .into_iter()
                .filter(|r| r.has_area())
                .collect();
            TilePlan::strips(strips, StripKind::Ring, fill)
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

// ---------------------------------------------------------------------------
// The paint list and its overlay rule.
// ---------------------------------------------------------------------------

/// What happens to a [`PaintEntry`] while an overlay is mapped (the overlay window sits below
/// chrome, so chrome must stay off it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnderOverlay {
    /// Hidden whenever any overlay is mapped (the focus and mark rings).
    Hide,
    /// Dropped when the rect intersects the overlay rect (pips).
    DropIfHit,
    /// The overlay rect is subtracted from the rect (fills, frames; the overlay ring lies outside
    /// the overlay rect, so this keeps it whole).
    Clip,
}

/// One coloured chrome rect (global coordinates) and its overlay rule.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaintEntry {
    /// The rect, in global Space coordinates.
    pub rect: Rect,
    /// Its `[R, G, B, A]` colour.
    pub color: [f32; 4],
    /// What happens to it while an overlay is mapped.
    pub under_overlay: UnderOverlay,
}

/// The planned chrome of one frame (global coordinates), before the overlay rule.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChromeParts {
    /// The mark ring strips of the visible marked window.
    pub mark_ring: Vec<Rect>,
    /// Every pane's pips, the marked window's included.
    pub pips: Vec<(Rect, PipKind)>,
    /// The focus-ring strips (the focused tile, or the focused empty pane).
    pub ring: Vec<Rect>,
    /// The app fills.
    pub fills: Vec<Rect>,
    /// The terminal frame strips.
    pub frames: Vec<Rect>,
}

/// The paint list, front to back (earlier entries are drawn in front): mark ring → overlay ring
/// (with `overlay` mapped and its `border` on, [`BORDER_THICKNESS`] **outside** the overlay) →
/// pips → focus ring → fills in `fill` → frames, the last only when a `frame` colour is
/// configured. **Pure.**
#[must_use]
pub fn paint_list(
    parts: &ChromeParts,
    fill: [f32; 4],
    frame: Option<[f32; 4]>,
    overlay: Option<OverlayChrome>,
) -> Vec<PaintEntry> {
    let entry = |rect, color, under_overlay| PaintEntry {
        rect,
        color,
        under_overlay,
    };
    let mut out: Vec<PaintEntry> = parts
        .mark_ring
        .iter()
        .map(|&r| entry(r, MARK_COLOR, UnderOverlay::Hide))
        .collect();
    if let Some(o) = overlay
        && o.border
    {
        let t = BORDER_THICKNESS;
        let color = o.ring.to_rgba_f32();
        out.extend(
            o.rect
                .outset(t)
                .border_strips(t)
                .into_iter()
                .map(|r| entry(r, color, UnderOverlay::Clip)),
        );
    }
    out.extend(
        parts
            .pips
            .iter()
            .map(|&(r, kind)| entry(r, kind.color(), UnderOverlay::DropIfHit)),
    );
    out.extend(
        parts
            .ring
            .iter()
            .map(|&r| entry(r, BORDER_COLOR, UnderOverlay::Hide)),
    );
    out.extend(
        parts
            .fills
            .iter()
            .map(|&r| entry(r, fill, UnderOverlay::Clip)),
    );
    if let Some(color) = frame {
        out.extend(
            parts
                .frames
                .iter()
                .map(|&r| entry(r, color, UnderOverlay::Clip)),
        );
    }
    out
}

/// Applies the overlay rule to every entry and translates the survivors into the frame of the
/// output whose global origin is `output_origin`, keeping the order. **Pure.**
#[must_use]
pub fn resolve(
    entries: &[PaintEntry],
    overlay: Option<OverlayChrome>,
    output_origin: (i32, i32),
) -> Vec<(Rect, [f32; 4])> {
    let local = |r: Rect| r.translate(-output_origin.0, -output_origin.1);
    let mut out = Vec::new();
    for e in entries {
        let Some(o) = overlay else {
            out.push((local(e.rect), e.color));
            continue;
        };
        match e.under_overlay {
            UnderOverlay::Hide => {}
            UnderOverlay::DropIfHit => {
                if !e.rect.intersects(o.rect) {
                    out.push((local(e.rect), e.color));
                }
            }
            UnderOverlay::Clip => out.extend(
                e.rect
                    .subtract(o.rect)
                    .into_iter()
                    .map(|r| (local(r), e.color)),
            ),
        }
    }
    out
}

#[cfg(test)]
mod tests;
