//! The workspace header strip (plan 06 WP-6): one strip per output listing that output's workspace
//! stack, the active one highlighted; clickable, shown always / on a `toggle-header` / never.
//!
//! ## Layout
//! The decision logic is **pure** and Smithay-free (top of this module): [`segment_layout`] maps
//! name widths to segment rects, [`hit_test`] maps a point to a segment, [`ConsumedButtons`] pairs a consumed press with its release, and [`shown_on`] + [`strip_height`] /
//! [`Rect::shrink_top`] give the reflow while the strip is shown. The compositor glue (segments from the live
//! model) is below; press/drag/release decisions live in `drag`, render elements in `render`.
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
//! strip and segment rects go through [`PhysicalMap`](crate::physical::PhysicalMap) (edge rounding, so segments stay contiguous), the padding and
//! vertical centring scale with them, and the label text is rasterized at `size x scale` physical pixels
//! ([`TextRenderer::element`](crate::text::TextRenderer::element)), never upscaled. Hit-testing stays in logical pixels.

mod drag;
mod render;

pub use drag::{
    ConsumedButtons, DRAG_THRESHOLD, DragPreview, HeaderDrag, HeaderPress, ReleaseAction,
    StackOrders, StripHit, apply_release, drag_action,
};
pub use render::build_header_elements;

use std::collections::{BTreeMap, BTreeSet};

use crucible_config::{HeaderMode, HeaderStart, Rgb};
use crucible_layout::{OutputId, Rect, WorkspaceId};
use crucible_persist::session::HeaderPref;

use crate::{
    layout::model_output_id,
    state::{Backend, CrucibleState},
};

/// Vertical padding above and below the label, in pixels.
pub const PAD_Y: i32 = 5;
/// Horizontal padding on each side of a label, in pixels.
pub const PAD_X: i32 = 10;
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

/// Whether an output's strip is drawn, hit-tested and reserved: never on a `fullscreen` output
/// (whose stored `toggle` state is left alone); else `always` shows, `off` hides and `toggle`
/// follows `toggle_visible`.
#[must_use]
pub fn shown_on(mode: HeaderMode, toggle_visible: bool, fullscreen: bool) -> bool {
    !fullscreen
        && match mode {
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
/// `width + 2 * PAD_X` wide, with a `divider`-wide gap between adjacent segments (logical px).
/// Returns the segments and the divider rects (full strip height), one per adjacent pair of
/// laid-out segments: none before the first or after the last. Segment `i + 1` starts `divider`
/// px after segment `i` ends. Clipping is at the strip's right edge: a segment (and the divider
/// before it) that would start at or beyond it is dropped, the last visible segment is
/// truncated. With `divider == 0` segments abut and no dividers return.
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

/// A newly known output's initial `toggle`-mode visibility: the saved preference, else `start`.
#[must_use]
pub fn seed_visible(pref: Option<HeaderPref>, start: HeaderStart) -> bool {
    pref.map_or_else(|| start.is_shown(), HeaderPref::is_shown)
}

// ---- compositor glue -------------------------------------------------------------------------

/// Per-session header state, held on [`CrucibleState`].
#[derive(Debug, Default)]
pub struct HeaderState {
    /// The `toggle`-mode state: the outputs whose strip is currently shown (absent = hidden).
    shown: BTreeSet<OutputId>,
    /// The saved preference per connector name, loaded from `session.kdl` at
    /// startup and updated on every toggle. Holds disconnected connectors too, so an unplug,
    /// a workspace-less output or `restore-session off` never loses it.
    pub prefs: BTreeMap<String, HeaderPref>,
    /// Buttons whose press the strip consumed.
    pub consumed: ConsumedButtons,
    /// The label press being tracked as a possible drag.
    pub drag: Option<HeaderDrag>,
    /// The font spec the text renderer was last loaded for.
    pub font_loaded: Option<String>,
}

impl HeaderState {
    /// Whether `id`'s `toggle`-mode strip is shown.
    #[must_use]
    pub fn is_visible(&self, id: OutputId) -> bool {
        self.shown.contains(&id)
    }

    /// Sets `id`'s `toggle`-mode visibility; returns whether it changed.
    pub fn set_visible(&mut self, id: OutputId, visible: bool) -> bool {
        if visible {
            self.shown.insert(id)
        } else {
            self.shown.remove(&id)
        }
    }

    /// Seeds `id` (connector `name`) from the saved preference, else from `start`. Does not
    /// record a preference: only a toggle does.
    pub fn seed(&mut self, id: OutputId, name: &str, start: HeaderStart) {
        let visible = seed_visible(self.prefs.get(name).copied(), start);
        self.set_visible(id, visible);
    }

    /// Records `name`'s preference; returns whether the stored map changed.
    pub fn remember(&mut self, name: &str, visible: bool) -> bool {
        let pref = HeaderPref::from_visible(visible);
        self.prefs.insert(name.to_string(), pref) != Some(pref)
    }
}

/// One output's strip as derived from the live model.
#[derive(Debug, Clone)]
pub struct HeaderLayout {
    /// The model output.
    pub output: OutputId,
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
    /// Whether output `id`'s strip is currently shown.
    pub(crate) fn header_shown(&self, id: OutputId) -> bool {
        shown_on(
            self.config.header.mode,
            self.header.is_visible(id),
            self.layout.world.output_fullscreen(id).is_some(),
        )
    }

    /// Every output's strip layout; empty while the strip is hidden.
    pub(crate) fn header_layouts(&self) -> Vec<HeaderLayout> {
        self.raw_output_rects()
            .into_iter()
            .filter_map(|(id, rect)| self.header_layout_for(id, rect))
            .collect()
    }

    /// The strip layout of model output `id` whose global rect is `rect`; `None` while the strip
    /// is hidden or when the model has no such output.
    pub(crate) fn header_layout_for(&self, id: OutputId, rect: Rect) -> Option<HeaderLayout> {
        self.header_layout_ordered(id, rect, None)
    }

    /// [`Self::header_layout_for`] drawn from `preview`'s order for this output, when given.
    pub(crate) fn header_layout_ordered(
        &self,
        id: OutputId,
        rect: Rect,
        preview: Option<&DragPreview>,
    ) -> Option<HeaderLayout> {
        if !self.header_shown(id) {
            return None;
        }
        let size = self.config.header.size;
        let height = strip_height(size);
        let divider = i32::try_from(self.config.header.divider).unwrap_or(0);
        let inset = i32::try_from(self.config.header.divider_inset).unwrap_or(0);
        let divider_color = self.config.header.divider_color;
        let badges = self.config.header.slots;
        let glyph = badges && self.header_text.has_glyph(SUPER_GLYPH);
        let output = self.layout.world.output(id)?;
        let strip = strip_rect(rect, height);
        let (mut names, mut workspaces) = (Vec::new(), Vec::new());
        let previewed = preview.and_then(|p| p.orders.iter().find(|(o, _)| *o == id));
        let order = previewed.map_or(output.workspaces(), |(_, v)| v.as_slice());
        for ws in order {
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
        let active = drag::active_index(
            output.active_workspace(),
            output.active_index(),
            &workspaces,
            preview,
        );
        Some(HeaderLayout {
            output: id,
            strip,
            names,
            workspaces,
            active,
            segments,
            dividers,
            divider_color,
        })
    }

    /// The strip hit under global `(x, y)`: the output and, on a label, its workspace.
    /// `None` when the strip is hidden or the point is outside every strip.
    pub(crate) fn header_hit(&self, x: i32, y: i32) -> Option<StripHit> {
        self.header_layouts()
            .into_iter()
            .find(|l| l.strip.contains(x, y))
            .map(|l| {
                let ws = hit_test(&l.segments, x, y).and_then(|i| l.workspaces.get(i).copied());
                StripHit {
                    output: l.output,
                    workspace: ws,
                }
            })
    }

    /// Sets output `id`'s `toggle`-mode visibility. On a change the applied projection is
    /// forgotten and the layout re-applied so every window is re-configured to the reflowed rects.
    pub(crate) fn set_header_visible(&mut self, id: OutputId, visible: bool) {
        let name = self
            .space
            .outputs()
            .find(|o| model_output_id(o) == Some(id))
            .map(|o| o.name());
        let remembered = name.is_some_and(|n| self.header.remember(&n, visible));
        if remembered {
            self.write_session();
        }
        if !self.header.set_visible(id, visible) {
            return;
        }
        self.invalidate_projection();
        self.apply_layout();
    }

    /// `toggle-header`: flips the focused output's strip in `toggle` mode; a no-op in `always`
    /// and `off`.
    pub(crate) fn toggle_header(&mut self) {
        let id = self.layout.world.focused_output();
        let next = toggled_visible(self.config.header.mode, self.header.is_visible(id));
        self.set_header_visible(id, next);
    }
}

/// Width estimate used when no font is loaded.
fn fallback_width(name: &str, size: u32) -> i32 {
    (name.chars().count() as f32 * size as f32 * FALLBACK_CHAR_FRACTION).ceil() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fullscreen_output_draws_and_hit_tests_no_strip_in_any_mode() {
        // `header_layout_for` (and so `header_hit` and render) gate on `shown_on`: a fullscreen
        // output yields no layout, hence a `None` hit, while its toggle state is not consulted.
        for mode in [HeaderMode::Off, HeaderMode::Always, HeaderMode::Toggle] {
            for visible in [false, true] {
                assert!(!shown_on(mode, visible, true), "{mode:?} {visible}");
            }
        }
    }

    /// The divider-free layout (`segment_layout(.., 0).0`).
    fn segment_rects(widths: &[i32], strip: Rect) -> Vec<Segment> {
        segment_layout(widths, strip, 0).0
    }

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
    fn reservation_by_mode_and_visibility() {
        let mut cfg = crucible_config::HeaderConfig::default();
        // What `output_bindings` reserves: the strip height while `shown_on`, else 0.
        let reserved = |cfg: &crucible_config::HeaderConfig, visible, fullscreen| {
            if shown_on(cfg.mode, visible, fullscreen) {
                strip_height(cfg.size)
            } else {
                0
            }
        };
        let out = Rect::new(0, 0, 1920, 1080);
        let h = strip_height(cfg.size);
        assert_eq!(h, 14 + 2 * PAD_Y);
        let shrunk = Rect::new(0, h, 1920, 1080 - h);
        for visible in [false, true] {
            cfg.mode = HeaderMode::Off;
            assert_eq!(out.shrink_top(reserved(&cfg, visible, false)), out);
            cfg.mode = HeaderMode::Always;
            assert_eq!(out.shrink_top(reserved(&cfg, visible, false)), shrunk);
        }
        cfg.mode = HeaderMode::Toggle;
        assert_eq!(out.shrink_top(reserved(&cfg, false, false)), out);
        assert_eq!(out.shrink_top(reserved(&cfg, true, false)), shrunk);
        // A fullscreen output reserves nothing in any mode.
        for mode in [HeaderMode::Off, HeaderMode::Always, HeaderMode::Toggle] {
            cfg.mode = mode;
            assert_eq!(out.shrink_top(reserved(&cfg, true, true)), out);
        }
        // Never swallows more than half the output.
        assert_eq!(Rect::new(0, 0, 10, 20).shrink_top(500).height, 10);
    }

    #[test]
    fn shown_by_mode() {
        assert!(!shown_on(HeaderMode::Off, true, false));
        assert!(shown_on(HeaderMode::Always, false, false));
        assert!(shown_on(HeaderMode::Toggle, true, false));
        assert!(!shown_on(HeaderMode::Toggle, false, false));
    }

    #[test]
    fn header_state_is_per_output() {
        let (a, b) = (OutputId::from_raw(1), OutputId::from_raw(2));
        let mut st = HeaderState::default();
        assert!(!st.is_visible(a));
        assert!(st.set_visible(a, true));
        assert!(!st.set_visible(a, true));
        assert!(st.is_visible(a) && !st.is_visible(b));
        assert!(!st.set_visible(b, false));
        assert!(st.set_visible(a, false));
        assert!(!st.is_visible(a));
    }

    #[test]
    fn seed_precedence_is_pref_then_start_then_hidden() {
        use HeaderStart::{Hidden, Shown};
        assert!(seed_visible(Some(HeaderPref::Shown), Hidden));
        assert!(!seed_visible(Some(HeaderPref::Hidden), Shown));
        assert!(seed_visible(None, Shown));
        assert!(!seed_visible(None, Hidden));
    }

    #[test]
    fn seed_uses_pref_and_does_not_record_one() {
        let (a, b) = (OutputId::from_raw(1), OutputId::from_raw(2));
        let mut st = HeaderState::default();
        st.prefs.insert("DP-1".into(), HeaderPref::Shown);
        st.seed(a, "DP-1", HeaderStart::Hidden);
        st.seed(b, "HDMI-A-1", HeaderStart::Shown);
        assert!(st.is_visible(a) && st.is_visible(b));
        assert_eq!(st.prefs.len(), 1);
        assert!(st.remember("HDMI-A-1", false));
        assert!(!st.remember("HDMI-A-1", false));
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
