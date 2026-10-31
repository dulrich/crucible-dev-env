//! The workspace header strip (plan 06 WP-6): one strip per output listing that output's workspace
//! stack, the active one highlighted; clickable, shown always / on a `toggle-header` / never.
//!
//! ## Layout
//! The decision logic is **pure** and Smithay-free (top of this module): [`segment_rects`] maps
//! name widths to segment rects, [`hit_test`] maps a point to a segment, [`ConsumedButtons`] pairs a consumed press with its release, and [`reserved_height`] /
//! [`Rect::shrink_top`] give the reflow while the strip is shown. The compositor glue (segments from the live
//! model, render elements) is below.
//!
//! ## Modes
//! - `always`: the strip's height is subtracted from the output rect handed to the projection
//!   ([`CrucibleState::output_bindings`]), so tiles, rings and X11 placement all agree.
//! - `toggle`: hidden until the `toggle-header` action (`Super+B`) shows it; while shown it reflows
//!   the tiles exactly like `always`. Every visibility change re-configures all windows
//!   ([`CrucibleState::set_header_visible`]). Clicking a label never hides it.
//! - `off`: no strip, no input effect.
//!
//! Strip geometry is in logical pixels. Like the rest of the chrome it is drawn at the output's fractional scale: the
//! strip and segment rects go through [`PhysicalMap`] (edge rounding, so segments stay contiguous), the padding and
//! vertical centring scale with them, and the label text is rasterized at `size x scale` physical pixels
//! ([`TextRenderer::element`]), never upscaled. Hit-testing stays in logical pixels.

use std::collections::BTreeSet;

use crucible_config::{HeaderConfig, HeaderMode, Rgb};
use crucible_layout::{OutputId, Rect, WorkspaceId};
use smithay::{
    backend::renderer::{ImportAll, ImportMem, Renderer},
    utils::{Physical, Point, Scale},
};

use crate::{
    chrome::{BORDER_COLOR, PhysicalMap, solid},
    layout::model_output_id,
    render::CustomRenderElements,
    state::{Backend, CrucibleState},
    text::TextRenderer,
};

/// Vertical padding above and below the label, in pixels.
pub const PAD_Y: i32 = 5;
/// Horizontal padding on each side of a label, in pixels.
pub const PAD_X: i32 = 10;
/// Strip background (a dark neutral, distinct from the ring accent).
pub const STRIP_BG: [f32; 4] = [0.09, 0.10, 0.12, 1.0];
/// Inactive label colour (straight RGBA).
pub const LABEL_RGBA: [u8; 4] = [205, 208, 214, 255];
/// Label colour on the highlighted (active) segment (straight RGBA).
pub const LABEL_ACTIVE_RGBA: [u8; 4] = [10, 14, 16, 255];
/// Width estimate per character when no font is loaded (so the strip still has segments).
const FALLBACK_CHAR_FRACTION: f32 = 0.6;

// ---- pure core -------------------------------------------------------------------------------

/// The strip's height for font `size`: the size plus padding top and bottom.
#[must_use]
pub fn strip_height(size: u32) -> i32 {
    i32::try_from(size).unwrap_or(i32::MAX / 4) + 2 * PAD_Y
}

/// The height reserved at the top of each output: the strip height while the strip is shown
/// (`always`, or `toggle` while `visible`), else `0`.
#[must_use]
pub fn reserved_height(config: &HeaderConfig, visible: bool) -> i32 {
    if is_shown(config.mode, visible) {
        strip_height(config.size)
    } else {
        0
    }
}

/// Whether the strip is drawn (and clickable) given the mode and the `toggle` state.
#[must_use]
pub fn is_shown(mode: HeaderMode, toggle_visible: bool) -> bool {
    match mode {
        HeaderMode::Off => false,
        HeaderMode::Always => true,
        HeaderMode::Toggle => toggle_visible,
    }
}

/// The strip rect across the top of `output` (global coordinates).
#[must_use]
pub fn strip_rect(output: Rect, height: i32) -> Rect {
    Rect::new(output.x, output.y, output.width, height.min(output.height))
}

/// One workspace label's clickable/drawn area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// Index into the output's workspace stack.
    pub index: usize,
    /// The segment rect (global coordinates), full strip height.
    pub rect: Rect,
}

/// Lays out one segment per label width, left to right from the strip's left edge, each
/// `width + 2 * PAD_X` wide, with no divider. See [`segment_layout`].
#[must_use]
pub fn segment_rects(widths: &[i32], strip: Rect) -> Vec<Segment> {
    segment_layout(widths, strip, 0).0
}

/// Like [`segment_rects`], with a `divider`-wide gap between adjacent segments (logical px).
/// Returns the segments and the divider rects (full strip height), one per adjacent pair of
/// laid-out segments: none before the first or after the last. Segment `i + 1` starts `divider`
/// px after segment `i` ends. Clipping is at the strip's right edge: a segment (and the divider
/// before it) that would start at or beyond it is dropped, the last visible segment is
/// truncated. With `divider == 0` the segments equal [`segment_rects`] and no dividers return.
#[must_use]
pub fn segment_layout(widths: &[i32], strip: Rect, divider: i32) -> (Vec<Segment>, Vec<Rect>) {
    let divider = divider.max(0);
    let mut segments: Vec<Segment> = Vec::new();
    let mut dividers = Vec::new();
    let mut x = strip.x;
    let right = strip.x + strip.width;
    for (index, w) in widths.iter().enumerate() {
        let start = if segments.is_empty() { x } else { x + divider };
        if start >= right {
            break;
        }
        if divider > 0 && !segments.is_empty() {
            dividers.push(Rect::new(x, strip.y, divider, strip.height));
        }
        let width = (w.max(&0) + 2 * PAD_X).min(right - start);
        segments.push(Segment {
            index,
            rect: Rect::new(start, strip.y, width, strip.height),
        });
        x = start + width;
    }
    (segments, dividers)
}

/// Trims `inset` logical px off the top and bottom of a divider rect; `None` when nothing is
/// left (height <= 0). A negative `inset` is treated as 0.
#[must_use]
pub fn inset_divider(div: Rect, inset: i32) -> Option<Rect> {
    let inset = inset.max(0);
    let height = div.height - 2 * inset;
    (height > 0).then(|| Rect::new(div.x, div.y + inset, div.width, height))
}

/// The segment index under `(x, y)`, if any.
#[must_use]
pub fn hit_test(segments: &[Segment], x: i32, y: i32) -> Option<usize> {
    segments
        .iter()
        .find(|s| s.rect.contains(x, y))
        .map(|s| s.index)
}

/// The visibility after a `toggle-header`: flips in `toggle` mode, unchanged otherwise.
#[must_use]
pub fn toggled_visible(mode: HeaderMode, visible: bool) -> bool {
    if mode == HeaderMode::Toggle {
        !visible
    } else {
        visible
    }
}

/// Physical button codes whose press the strip consumed; their paired release is swallowed
/// (audit E8: a release must never be forwarded for a press that was not).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ConsumedButtons(BTreeSet<u32>);

impl ConsumedButtons {
    /// Records `button`'s press as consumed.
    pub fn consume_press(&mut self, button: u32) {
        self.0.insert(button);
    }

    /// Whether `button`'s release is swallowed (its press was consumed); clears the record.
    pub fn consume_release(&mut self, button: u32) -> bool {
        self.0.remove(&button)
    }
}

// ---- compositor glue -------------------------------------------------------------------------

/// Per-session header state, held on [`CrucibleState`].
#[derive(Debug, Default)]
pub struct HeaderState {
    /// The `toggle`-mode state: whether the strip is currently shown.
    pub visible: bool,
    /// Buttons whose press the strip consumed.
    pub consumed: ConsumedButtons,
    /// The font spec the text renderer was last loaded for.
    pub font_loaded: Option<String>,
}

/// One output's strip as derived from the live model.
#[derive(Debug, Clone)]
pub struct HeaderLayout {
    /// The model output.
    pub output: OutputId,
    /// The output's global rect (unshrunk).
    pub output_rect: Rect,
    /// The strip rect (global).
    pub strip: Rect,
    /// Workspace names in stack order.
    pub names: Vec<String>,
    /// Workspace ids in stack order.
    pub workspaces: Vec<WorkspaceId>,
    /// Index of the active workspace.
    pub active: usize,
    /// Segment rects for `names`.
    pub segments: Vec<Segment>,
    /// Divider rects between adjacent segments (empty when the divider is 0).
    pub dividers: Vec<Rect>,
    /// Colour the dividers are drawn in (`header.divider_color`).
    pub divider_color: Rgb,
}

/// Super-key glyph (place of interest sign) used in slot badges.
const SUPER_GLYPH: char = '\u{2318}';

/// The label for a workspace: `name`, prefixed with its slot badge when pinned to `slot`.
/// The badge is the Super glyph plus the digit (`⌘3 name`), or `S-3 name` when the header font
/// has no such glyph. `None` slot leaves the name unchanged.
fn badge_label(name: &str, slot: Option<u8>, glyph: bool) -> String {
    match slot {
        None => name.to_string(),
        Some(n) if glyph => format!("{SUPER_GLYPH}{n} {name}"),
        Some(n) => format!("S-{n} {name}"),
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Whether the strip is currently shown.
    pub(crate) fn header_shown(&self) -> bool {
        is_shown(self.config.header.mode, self.header.visible)
    }

    /// Every output's strip layout; empty while the strip is hidden.
    pub(crate) fn header_layouts(&self) -> Vec<HeaderLayout> {
        if !self.header_shown() {
            return Vec::new();
        }
        let size = self.config.header.size;
        let height = strip_height(size);
        let divider = i32::try_from(self.config.header.divider).unwrap_or(0);
        let inset = i32::try_from(self.config.header.divider_inset).unwrap_or(0);
        let divider_color = self.config.header.divider_color;
        let badges = self.config.header.slots;
        let glyph = badges && self.header_text.has_glyph(SUPER_GLYPH);
        self.raw_output_rects()
            .into_iter()
            .filter_map(|(id, rect)| {
                let output = self.layout.world.output(id)?;
                let strip = strip_rect(rect, height);
                let (mut names, mut workspaces) = (Vec::new(), Vec::new());
                for ws in output.workspaces() {
                    let name = &self.layout.world.workspace(*ws)?.name;
                    let slot = if badges {
                        self.persist.slots.slot_of(name)
                    } else {
                        None
                    };
                    names.push(badge_label(name, slot, glyph));
                    workspaces.push(*ws);
                }
                let widths: Vec<i32> = names
                    .iter()
                    .map(|n| {
                        self.header_text
                            .measure(n, size)
                            .unwrap_or_else(|| fallback_width(n, size))
                    })
                    .collect();
                let (segments, dividers) = segment_layout(&widths, strip, divider);
                let dividers = dividers
                    .into_iter()
                    .filter_map(|d| inset_divider(d, inset))
                    .collect();
                Some(HeaderLayout {
                    output: id,
                    output_rect: rect,
                    strip,
                    names,
                    workspaces,
                    active: output.active_index(),
                    segments,
                    dividers,
                    divider_color,
                })
            })
            .collect()
    }

    /// The strip hit under global `(x, y)`: the output and, on a label, its workspace.
    /// `None` when the strip is hidden or the point is outside every strip.
    pub(crate) fn header_hit(&self, x: i32, y: i32) -> Option<(OutputId, Option<WorkspaceId>)> {
        self.header_layouts()
            .into_iter()
            .find(|l| l.strip.contains(x, y))
            .map(|l| {
                let ws = hit_test(&l.segments, x, y).and_then(|i| l.workspaces.get(i).copied());
                (l.output, ws)
            })
    }

    /// Sets the `toggle`-mode visibility. On a change the applied projection is forgotten and the
    /// layout re-applied so every window is re-configured to the reflowed rects.
    pub(crate) fn set_header_visible(&mut self, visible: bool) {
        if self.header.visible == visible {
            return;
        }
        self.header.visible = visible;
        self.invalidate_projection();
        self.apply_layout();
    }

    /// `toggle-header`: flips the strip in `toggle` mode; a no-op in `always` and `off`.
    pub(crate) fn toggle_header(&mut self) {
        let next = toggled_visible(self.config.header.mode, self.header.visible);
        self.set_header_visible(next);
    }

    /// Handles a consumed press on the strip: focus the output and switch to the clicked
    /// workspace. The strip stays open.
    pub(crate) fn header_click(&mut self, output: OutputId, workspace: Option<WorkspaceId>) {
        if let Some(ws) = workspace {
            let target = self
                .space
                .outputs()
                .find(|o| model_output_id(o) == Some(output))
                .cloned();
            if let Some(target) = target {
                self.focus_output(&target);
            }
            if let Err(err) = self.with_model(|world| world.switch_workspace(ws)) {
                tracing::warn!(%err, "header click: workspace switch failed");
            }
        }
    }
}

/// Width estimate used when no font is loaded.
fn fallback_width(name: &str, size: u32) -> i32 {
    (name.chars().count() as f32 * size as f32 * FALLBACK_CHAR_FRACTION).ceil() as i32
}

/// Builds the strip's render elements (front-to-back: labels, then highlight and background
/// solids) for `output` only, positioned in the output's frame at
/// `origin`. Without a loaded font only the backgrounds and highlight are produced.
pub fn build_header_elements<R>(
    layouts: &[HeaderLayout],
    output: OutputId,
    origin: (i32, i32),
    size: u32,
    text: &mut TextRenderer,
    renderer: &mut R,
    scale: Scale<f64>,
) -> Vec<CustomRenderElements<R>>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Send + Clone + 'static,
{
    let map = PhysicalMap::plain(scale);
    let to_origin = |r: Rect| r.translate(-origin.0, -origin.1);
    let mut labels = Vec::new();
    let mut solids = Vec::new();
    for l in layouts.iter().filter(|l| l.output == output) {
        let strip = map.rect(to_origin(l.strip));
        let line = (f64::from(size) * 1.2 * scale.y).round() as i32;
        let text_y = strip.loc.y + (strip.size.h - line) / 2;
        let pad_x = (f64::from(PAD_X) * scale.x).round() as i32;
        for seg in &l.segments {
            let active = seg.index == l.active;
            let seg_rect = map.rect(to_origin(seg.rect));
            if active {
                solids.push(solid(seg_rect, BORDER_COLOR));
            }
            let rgba = if active {
                LABEL_ACTIVE_RGBA
            } else {
                LABEL_RGBA
            };
            let loc = Point::<f64, Physical>::from((
                f64::from(seg_rect.loc.x + pad_x),
                f64::from(text_y),
            ));
            if let Some(el) = text.element(renderer, loc, &l.names[seg.index], size, rgba, scale) {
                labels.push(CustomRenderElements::Text(el));
            }
        }
        let Rgb(dr, dg, db) = l.divider_color;
        for div in &l.dividers {
            let [r, g, b] = [dr, dg, db].map(|c| f32::from(c) / 255.0);
            solids.push(solid(map.rect(to_origin(*div)), [r, g, b, 1.0]));
        }
        solids.push(solid(strip, STRIP_BG));
    }
    labels.extend(solids.into_iter().map(CustomRenderElements::SolidColor));
    labels
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip() -> Rect {
        Rect::new(100, 0, 300, 24)
    }

    #[test]
    fn badge_labels() {
        assert_eq!(badge_label("work", None, true), "work");
        assert_eq!(badge_label("work", None, false), "work");
        assert_eq!(badge_label("work", Some(3), true), "\u{2318}3 work");
        assert_eq!(badge_label("work", Some(3), false), "S-3 work");
    }

    #[test]
    fn segments_are_laid_out_left_to_right_and_clipped() {
        let segs = segment_rects(&[20, 30], strip());
        assert_eq!(segs[0].rect, Rect::new(100, 0, 40, 24));
        assert_eq!(segs[1].rect, Rect::new(140, 0, 50, 24));
        // Overflowing: the last visible is truncated, later ones dropped.
        let segs = segment_rects(&[200, 200, 200], strip());
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[1].rect.width, 300 - 220);
        assert!(segment_rects(&[], strip()).is_empty());
    }

    #[test]
    fn hit_test_maps_points_to_segments() {
        let segs = segment_rects(&[20, 30], strip());
        assert_eq!(hit_test(&segs, 100, 0), Some(0));
        assert_eq!(hit_test(&segs, 139, 23), Some(0));
        assert_eq!(hit_test(&segs, 140, 5), Some(1));
        assert_eq!(hit_test(&segs, 190, 5), None); // past the last label
        assert_eq!(hit_test(&segs, 120, 24), None); // below the strip
    }

    #[test]
    fn divider_gaps_sit_between_segments_only() {
        let (segs, divs) = segment_layout(&[20, 30, 10], strip(), 2);
        assert_eq!(segs[0].rect, Rect::new(100, 0, 40, 24));
        assert_eq!(segs[1].rect, Rect::new(142, 0, 50, 24));
        assert_eq!(segs[2].rect, Rect::new(194, 0, 30, 24));
        // One per adjacent pair; none before the first or after the last.
        assert_eq!(divs, [Rect::new(140, 0, 2, 24), Rect::new(192, 0, 2, 24)]);
        let (one, none) = segment_layout(&[20], strip(), 2);
        assert_eq!(one.len(), 1);
        assert!(none.is_empty());
    }

    #[test]
    fn divider_clips_at_right_edge() {
        // Strip is x 100..400. Segment 0 = 100..320; the divider at 320..322 fits, segment 1 is
        // truncated to 322..400.
        let (segs, divs) = segment_layout(&[200, 200, 200], strip(), 2);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[1].rect, Rect::new(322, 0, 78, 24));
        assert_eq!(divs, [Rect::new(320, 0, 2, 24)]);
        // A divider starting at the right edge is dropped with the segment after it.
        let (segs, divs) = segment_layout(&[280], strip(), 2);
        assert_eq!((segs.len(), divs.len()), (1, 0));
        let (segs, divs) = segment_layout(&[280, 10], strip(), 2);
        assert_eq!((segs.len(), divs.len()), (1, 0));
        assert_eq!(segs[0].rect.width, 300);
    }

    #[test]
    fn hit_test_on_a_divider_hits_no_segment() {
        let (segs, divs) = segment_layout(&[20, 30], strip(), 2);
        assert_eq!(hit_test(&segs, divs[0].x, 5), None);
        assert_eq!(hit_test(&segs, divs[0].x + 1, 5), None);
        assert_eq!(hit_test(&segs, divs[0].x - 1, 5), Some(0));
        assert_eq!(hit_test(&segs, divs[0].x + 2, 5), Some(1));
    }

    #[test]
    fn inset_trims_divider_rect_top_and_bottom() {
        let (_, divs) = segment_layout(&[20, 30], strip(), 2);
        assert_eq!(inset_divider(divs[0], 0), Some(divs[0]));
        assert_eq!(inset_divider(divs[0], 6), Some(Rect::new(140, 6, 2, 12)));
    }

    #[test]
    fn inset_of_half_the_strip_drops_the_divider() {
        let (_, divs) = segment_layout(&[20, 30], strip(), 2);
        assert_eq!(inset_divider(divs[0], 11), Some(Rect::new(140, 11, 2, 2)));
        assert_eq!(inset_divider(divs[0], 12), None);
        assert_eq!(inset_divider(divs[0], 32), None);
    }

    #[test]
    fn inset_leaves_segment_rects_unchanged() {
        // Segments come from segment_layout alone; the inset only post-trims dividers.
        let w = [20, 30, 10];
        let (segs, divs) = segment_layout(&w, strip(), 2);
        let trimmed: Vec<_> = divs.iter().filter_map(|d| inset_divider(*d, 6)).collect();
        assert_eq!(trimmed.len(), divs.len());
        assert_eq!(segs, segment_layout(&w, strip(), 2).0);
        assert_eq!(hit_test(&segs, 140, 5), None);
    }

    #[test]
    fn zero_divider_is_unchanged() {
        let w = [20, 30, 200, 200];
        let (segs, divs) = segment_layout(&w, strip(), 0);
        assert_eq!(segs, segment_rects(&w, strip()));
        assert!(divs.is_empty());
        assert_eq!(segs[1].rect, Rect::new(140, 0, 50, 24));
    }

    #[test]
    fn consumed_press_swallows_exactly_its_release() {
        const LEFT: u32 = 0x110;
        const RIGHT: u32 = 0x111;
        let mut c = ConsumedButtons::default();
        // A release whose press was forwarded is forwarded.
        assert!(!c.consume_release(RIGHT));
        c.consume_press(LEFT);
        assert!(!c.consume_release(RIGHT)); // unrelated button forwarded
        // Swallowed even if the strip was hidden in between (the record is independent of it).
        assert!(c.consume_release(LEFT));
        assert!(!c.consume_release(LEFT)); // cleared
    }

    #[test]
    fn reservation_by_mode_and_visibility() {
        let mut cfg = HeaderConfig::default();
        let out = Rect::new(0, 0, 1920, 1080);
        let h = strip_height(cfg.size);
        assert_eq!(h, 14 + 2 * PAD_Y);
        let shrunk = Rect::new(0, h, 1920, 1080 - h);
        for visible in [false, true] {
            cfg.mode = HeaderMode::Off;
            assert_eq!(out.shrink_top(reserved_height(&cfg, visible)), out);
            cfg.mode = HeaderMode::Always;
            assert_eq!(out.shrink_top(reserved_height(&cfg, visible)), shrunk);
        }
        cfg.mode = HeaderMode::Toggle;
        assert_eq!(out.shrink_top(reserved_height(&cfg, false)), out);
        assert_eq!(out.shrink_top(reserved_height(&cfg, true)), shrunk);
        // Never swallows more than half the output.
        assert_eq!(Rect::new(0, 0, 10, 20).shrink_top(500).height, 10);
    }

    #[test]
    fn shown_by_mode() {
        assert!(!is_shown(HeaderMode::Off, true));
        assert!(is_shown(HeaderMode::Always, false));
        assert!(is_shown(HeaderMode::Toggle, true));
        assert!(!is_shown(HeaderMode::Toggle, false));
    }

    #[test]
    fn toggle_header_flips_only_in_toggle_mode() {
        assert!(toggled_visible(HeaderMode::Toggle, false));
        assert!(!toggled_visible(HeaderMode::Toggle, true));
        for mode in [HeaderMode::Off, HeaderMode::Always] {
            assert!(!toggled_visible(mode, false));
            assert!(toggled_visible(mode, true));
        }
    }
}

#[cfg(test)]
mod scale_tests {
    use super::*;

    #[test]
    fn scaled_strip_and_segments_stay_contiguous() {
        let s = Scale::from(170.0 / 120.0);
        let map = PhysicalMap::plain(s);
        let strip = Rect::new(100, 0, 300, 24);
        let segs = segment_rects(&[20, 30], strip);
        let (ps, p0, p1) = (
            map.rect(strip),
            map.rect(segs[0].rect),
            map.rect(segs[1].rect),
        );
        // Strip: round(100*s)=142 .. round(400*s)=567, height round(24*s)=34.
        assert_eq!(
            (ps.loc.x, ps.size.w, ps.loc.y, ps.size.h),
            (142, 425, 0, 34)
        );
        // Segments abut (shared edge) and start at the strip's left edge, full strip height.
        assert_eq!(p0.loc.x, ps.loc.x);
        assert_eq!(p0.loc.x + p0.size.w, p1.loc.x);
        assert_eq!((p0.loc.y, p0.size.h), (ps.loc.y, ps.size.h));
        assert!(p1.loc.x + p1.size.w <= ps.loc.x + ps.size.w);
        // At scale 1 the strip is unchanged.
        let one = PhysicalMap::plain(Scale::from(1.0)).rect(strip);
        assert_eq!((one.loc.x, one.size.w, one.size.h), (100, 300, 24));
    }

    #[test]
    fn half_scale_divider_never_overlaps_segments() {
        let map = PhysicalMap::plain(Scale::from(0.5));
        // Odd divider origin: segment 0 = 101..141, divider 141..142, segment 1 = 142..192.
        let strip = Rect::new(101, 0, 300, 24);
        let (segs, divs) = segment_layout(&[20, 30], strip, 1);
        let (p0, pd, p1) = (
            map.rect(segs[0].rect),
            map.rect(divs[0]),
            map.rect(segs[1].rect),
        );
        let end = |r: smithay::utils::Rectangle<i32, Physical>| r.loc.x + r.size.w;
        assert!(end(p0) <= pd.loc.x);
        assert!(end(pd) <= p1.loc.x);
        assert!(pd.size.w >= 0);
    }
}
