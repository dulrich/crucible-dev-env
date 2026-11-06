//! The workspace header strip (plan 06 WP-6): one strip per output listing that output's workspace
//! stack, the active one highlighted; clickable, shown always / on a `toggle-header` / never.
//!
//! ## Layout
//! The decision logic is **pure** and Smithay-free (top of this module): [`segment_layout`] maps
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

use std::collections::{BTreeMap, BTreeSet};

use crucible_config::{HeaderConfig, HeaderMode, HeaderStart, Rgb};
use crucible_layout::{OutputId, Rect, WorkspaceId};
use smithay::{
    backend::renderer::{ImportAll, ImportMem, Renderer},
    utils::{Physical, Point, Scale},
};

use crate::{
    layout::model_output_id,
    palette::{BORDER_COLOR, STRIP_BG},
    physical::{PhysicalMap, solid},
    render::CustomRenderElements,
    state::{Backend, CrucibleState},
    text::TextRenderer,
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

/// Whether output's strip is drawn and hit-tested: never on a `fullscreen` output (whose stored
/// `toggle` state is left alone), else per [`is_shown`].
#[must_use]
pub fn shown_on(mode: HeaderMode, toggle_visible: bool, fullscreen: bool) -> bool {
    !fullscreen && is_shown(mode, toggle_visible)
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
pub fn seed_visible(pref: Option<bool>, start: HeaderStart) -> bool {
    pref.unwrap_or_else(|| start.is_shown())
}

/// Where a consumed header press landed: the output and, on a label, its workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaderPress {
    /// The output whose strip was pressed.
    pub output: OutputId,
    /// The workspace label under the press (`None` on the strip outside every label).
    pub workspace: Option<WorkspaceId>,
}

/// Physical button codes whose press the strip consumed, with where it landed; their paired
/// release is swallowed (audit E8: a release must never be forwarded for a press that was not)
/// and decides the click or reorder ([`release_decision`]).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ConsumedButtons(BTreeMap<u32, HeaderPress>);

impl ConsumedButtons {
    /// Records `button`'s press as consumed, at `press`.
    pub fn consume_press(&mut self, button: u32, press: HeaderPress) {
        self.0.insert(button, press);
    }

    /// The press recorded for `button` if its press was consumed (so its release is swallowed);
    /// clears the record.
    pub fn consume_release(&mut self, button: u32) -> Option<HeaderPress> {
        self.0.remove(&button)
    }
}

/// What a consumed header release does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseAction {
    /// Nothing (the release is still swallowed).
    None,
    /// The existing click: focus the output and switch to the workspace.
    Click(HeaderPress),
    /// Move `workspace` to stack index `to` on its output.
    Reorder {
        /// The pressed workspace.
        workspace: WorkspaceId,
        /// The released segment's index in the output's stack.
        to: usize,
    },
    /// Move `workspace` to another output at stack index `to` (`to` = the target stack's length
    /// appends).
    MoveToOutput {
        /// The pressed workspace.
        workspace: WorkspaceId,
        /// The output the release landed on.
        output: OutputId,
        /// The insertion index in that output's stack.
        to: usize,
    },
}

/// Decides a consumed header release. `release` is the strip hit under the release point
/// (`None` off every strip or with the strip hidden); `normal` is whether the input mode is
/// still `Normal` (a lock or overlay opened since the press cancels); `order` is the pressed
/// output's workspace stack in order and `target_order` the released output's (the same slice
/// when both are one output). Same place as the press is a click; a different segment of the
/// same output is a reorder to that segment's index; the same output's strip outside every
/// label moves the workspace to the end (nothing if it already is last); another output's
/// segment moves it there at that segment's index and that output's strip outside every label
/// appends it; anything else does nothing. A label-less press never moves anything.
#[must_use]
pub fn release_decision(
    press: HeaderPress,
    release: Option<(OutputId, Option<WorkspaceId>)>,
    normal: bool,
    order: &[WorkspaceId],
    target_order: &[WorkspaceId],
) -> ReleaseAction {
    if !normal {
        return ReleaseAction::None;
    }
    let Some((output, workspace)) = release else {
        return ReleaseAction::None;
    };
    if output == press.output && workspace == press.workspace {
        return ReleaseAction::Click(press);
    }
    let Some(pressed) = press.workspace else {
        return ReleaseAction::None;
    };
    if output != press.output {
        let to = match workspace {
            Some(target) => target_order.iter().position(|w| *w == target),
            None => Some(target_order.len()),
        };
        return to.map_or(ReleaseAction::None, |to| ReleaseAction::MoveToOutput {
            workspace: pressed,
            output,
            to,
        });
    }
    match workspace {
        Some(target) => order
            .iter()
            .position(|w| *w == target)
            .map_or(ReleaseAction::None, |to| ReleaseAction::Reorder {
                workspace: pressed,
                to,
            }),
        None => match order.len().checked_sub(1) {
            Some(last) if order.last() != Some(&pressed) => ReleaseAction::Reorder {
                workspace: pressed,
                to: last,
            },
            _ => ReleaseAction::None,
        },
    }
}

/// A strip hit: the output and, on a label, its workspace (see `CrucibleState::header_hit`).
pub type StripHit = (OutputId, Option<WorkspaceId>);

/// Every output's workspace stack in order, as the committed model holds it.
pub type StackOrders = Vec<(OutputId, Vec<WorkspaceId>)>;

/// Pointer travel (logical px, Euclidean from the press point) past which a header press becomes
/// a drag (cvault's `THRESHOLD`).
pub const DRAG_THRESHOLD: f64 = 5.0;

/// A consumed header press that may become a drag: armed only by a press on a label, live once
/// the pointer travels past [`DRAG_THRESHOLD`]. While live it holds the current drop target,
/// computed against the **committed** model layout (never the previewed one, or the reflowed
/// segment would move under the pointer and the target would oscillate).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeaderDrag {
    /// The physical button that pressed.
    pub button: u32,
    /// The press.
    pub press: HeaderPress,
    origin: (f64, f64),
    live: bool,
    target: Option<StripHit>,
}

impl HeaderDrag {
    /// Arms a drag from a press at `at`; `None` for a label-less press (it never drags).
    #[must_use]
    pub fn arm(button: u32, press: HeaderPress, at: (f64, f64)) -> Option<Self> {
        press.workspace.map(|_| Self {
            button,
            press,
            origin: at,
            live: false,
            target: None,
        })
    }

    /// Whether the pointer has travelled past the threshold.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.live
    }

    /// The current drop target (`None` when not live, off every strip, or not in `Normal` mode).
    #[must_use]
    pub fn target(&self) -> Option<StripHit> {
        self.target
    }

    /// Feeds pointer motion to `at`; `hit` is the committed-layout strip hit under it and `normal`
    /// whether the input mode is `Normal`. Returns whether the drag's visible state changed (it
    /// went live or the target changed), i.e. whether the header must be redrawn.
    pub fn motion(&mut self, at: (f64, f64), hit: Option<StripHit>, normal: bool) -> bool {
        if !self.live {
            let (dx, dy) = (at.0 - self.origin.0, at.1 - self.origin.1);
            if dx.hypot(dy) <= DRAG_THRESHOLD {
                return false;
            }
            self.live = true;
        }
        let target = if normal { hit } else { None };
        let changed = target != self.target;
        self.target = target;
        changed
    }
}

/// What a consumed header release does, given the committed stack `orders`. `dragged` is whether
/// the press went past the threshold: such a release over its own slot is nothing (never a
/// click). Otherwise [`release_decision`] unchanged, except that a move of an output's last
/// workspace to another output is refused here (the model refuses it too), so the preview and
/// the commit agree.
#[must_use]
pub fn drag_action(
    press: HeaderPress,
    release: Option<StripHit>,
    normal: bool,
    dragged: bool,
    orders: &[(OutputId, Vec<WorkspaceId>)],
) -> ReleaseAction {
    let order_of = |o: OutputId| -> &[WorkspaceId] {
        orders
            .iter()
            .find(|(id, _)| *id == o)
            .map_or(&[][..], |(_, v)| v.as_slice())
    };
    let target_output = release.map_or(press.output, |(o, _)| o);
    let action = release_decision(
        press,
        release,
        normal,
        order_of(press.output),
        order_of(target_output),
    );
    match action {
        ReleaseAction::Click(_) if dragged => ReleaseAction::None,
        ReleaseAction::MoveToOutput { .. } if order_of(press.output).len() <= 1 => {
            ReleaseAction::None
        }
        other => other,
    }
}

/// The strip orders to draw while a drag is live, and the dragged workspace (which keeps the
/// active highlight on whichever strip it is previewed on).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DragPreview {
    /// Per output, the order to draw.
    pub orders: StackOrders,
    /// The dragged workspace.
    pub dragged: WorkspaceId,
}

/// The preview of `action` over the committed `orders`: exactly the stack orders the model holds
/// after committing `action` (`Reorder` moves within the output, `MoveToOutput` removes from the
/// source and inserts, index clamped). `None` for an action that moves nothing.
#[must_use]
pub fn preview_orders(
    action: ReleaseAction,
    orders: &[(OutputId, Vec<WorkspaceId>)],
) -> Option<DragPreview> {
    let mut next = orders.to_vec();
    let (workspace, output, to) = match action {
        ReleaseAction::Reorder { workspace, to } => (workspace, None, to),
        ReleaseAction::MoveToOutput {
            workspace,
            output,
            to,
        } => (workspace, Some(output), to),
        ReleaseAction::None | ReleaseAction::Click(_) => return None,
    };
    let source = next.iter().position(|(_, v)| v.contains(&workspace))?;
    let from = next[source].1.iter().position(|w| *w == workspace)?;
    next[source].1.remove(from);
    let dest = match output {
        Some(o) => next.iter().position(|(id, _)| *id == o)?,
        None => source,
    };
    let stack = &mut next[dest].1;
    stack.insert(to.min(stack.len()), workspace);
    Some(DragPreview {
        orders: next,
        dragged: workspace,
    })
}

/// The live drag's preview (`None` when not live or the target moves nothing): derived from the
/// same [`drag_action`] the release commits, so preview and commit cannot diverge.
#[must_use]
pub fn drag_preview(
    drag: &HeaderDrag,
    normal: bool,
    orders: &[(OutputId, Vec<WorkspaceId>)],
) -> Option<DragPreview> {
    if !drag.is_live() {
        return None;
    }
    preview_orders(
        drag_action(drag.press, drag.target(), normal, true, orders),
        orders,
    )
}

// ---- compositor glue -------------------------------------------------------------------------

/// Per-session header state, held on [`CrucibleState`].
#[derive(Debug, Default)]
pub struct HeaderState {
    /// The `toggle`-mode state: the outputs whose strip is currently shown (absent = hidden).
    shown: BTreeSet<OutputId>,
    /// The saved preference per connector name (shown = `true`), loaded from `session.kdl` at
    /// startup and updated on every toggle. Holds disconnected connectors too, so an unplug,
    /// a workspace-less output or `restore-session off` never loses it.
    pub prefs: BTreeMap<String, bool>,
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

    /// Records a consumed press of `button` at `at`, arming a drag when it landed on a label.
    pub fn press(&mut self, button: u32, press: HeaderPress, at: (f64, f64)) {
        self.consumed.consume_press(button, press);
        if self.drag.is_none() {
            self.drag = HeaderDrag::arm(button, press, at);
        }
    }

    /// The paired release of `button`: its press and whether it had become a drag. Ends the drag.
    pub fn release(&mut self, button: u32) -> Option<(HeaderPress, bool)> {
        let press = self.consumed.consume_release(button)?;
        let dragged = match self.drag {
            Some(d) if d.button == button => {
                self.drag = None;
                d.is_live()
            }
            _ => false,
        };
        Some((press, dragged))
    }

    /// Records `name`'s preference; returns whether the stored map changed.
    pub fn remember(&mut self, name: &str, visible: bool) -> bool {
        self.prefs.insert(name.to_string(), visible) != Some(visible)
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

    /// Every output's committed workspace stack order.
    pub(crate) fn header_orders(&self) -> StackOrders {
        self.raw_output_rects()
            .into_iter()
            .filter_map(|(id, _)| Some((id, self.layout.world.output(id)?.workspaces().to_vec())))
            .collect()
    }

    /// The live drag's preview, if any. Only drawing consults it; hit-testing never does.
    pub(crate) fn header_drag_preview(&self) -> Option<DragPreview> {
        let drag = self.header.drag.as_ref()?;
        let normal = self.input_is_normal();
        drag_preview(drag, normal, &self.header_orders())
    }

    /// Feeds pointer motion to the armed drag (if any): recomputes the drop target against the
    /// committed layout. The header redraws through the per-frame element diff (the drawn layout
    /// is built from the preview), so a changed target needs no further damage call.
    pub(crate) fn header_drag_motion(&mut self) {
        let Some(mut drag) = self.header.drag else {
            return;
        };
        let at = self.pointer.current_location();
        let hit = self.header_hit(at.x.floor() as i32, at.y.floor() as i32);
        let normal = self.input_is_normal();
        drag.motion((at.x, at.y), hit, normal);
        self.header.drag = Some(drag);
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
        let active = active_index(
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
    pub(crate) fn header_hit(&self, x: i32, y: i32) -> Option<(OutputId, Option<WorkspaceId>)> {
        self.header_layouts()
            .into_iter()
            .find(|l| l.strip.contains(x, y))
            .map(|l| {
                let ws = hit_test(&l.segments, x, y).and_then(|i| l.workspaces.get(i).copied());
                (l.output, ws)
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

/// The highlighted index in the drawn `order`: the dragged workspace when it is on this strip,
/// else the committed active workspace's new place (or its old index, clamped, if it left).
fn active_index(
    active: WorkspaceId,
    committed: usize,
    order: &[WorkspaceId],
    preview: Option<&DragPreview>,
) -> usize {
    let find = |w: WorkspaceId| order.iter().position(|x| *x == w);
    preview
        .and_then(|p| find(p.dragged))
        .or_else(|| find(active))
        .unwrap_or_else(|| committed.min(order.len().saturating_sub(1)))
}

/// Width estimate used when no font is loaded.
fn fallback_width(name: &str, size: u32) -> i32 {
    (name.chars().count() as f32 * size as f32 * FALLBACK_CHAR_FRACTION).ceil() as i32
}

/// Builds the strip's render elements (front-to-back: labels, then highlight and background
/// solids) for one output's `layout`, positioned in the output's frame at
/// `origin`. Without a loaded font only the backgrounds and highlight are produced.
pub fn build_header_elements<R>(
    layout: &HeaderLayout,
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
    let l = layout;
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
        let loc =
            Point::<f64, Physical>::from((f64::from(seg_rect.loc.x + pad_x), f64::from(text_y)));
        if let Some(el) = text.element(renderer, loc, &l.names[seg.index], size, rgba, scale) {
            labels.push(CustomRenderElements::Text(el));
        }
    }
    let divider = l.divider_color.to_rgba_f32();
    for div in &l.dividers {
        solids.push(solid(map.rect(to_origin(*div)), divider));
    }
    solids.push(solid(strip, STRIP_BG));
    labels.extend(solids.into_iter().map(CustomRenderElements::SolidColor));
    labels
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
                assert_eq!(shown_on(mode, visible, false), is_shown(mode, visible));
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
    fn consumed_press_swallows_exactly_its_release() {
        const LEFT: u32 = 0x110;
        const RIGHT: u32 = 0x111;
        let press = HeaderPress {
            output: OutputId::from_raw(1),
            workspace: None,
        };
        let mut c = ConsumedButtons::default();
        // A release whose press was forwarded is forwarded.
        assert!(c.consume_release(RIGHT).is_none());
        c.consume_press(LEFT, press);
        assert!(c.consume_release(RIGHT).is_none()); // unrelated button forwarded
        // Swallowed even if the strip was hidden in between (the record is independent of it).
        assert_eq!(c.consume_release(LEFT), Some(press));
        assert!(c.consume_release(LEFT).is_none()); // cleared
    }

    fn ws(n: u64) -> WorkspaceId {
        WorkspaceId::from_raw(n)
    }

    #[test]
    fn release_decision_table() {
        let out = OutputId::from_raw(1);
        let other = OutputId::from_raw(2);
        let order = [ws(1), ws(2), ws(3)];
        let press = HeaderPress {
            output: out,
            workspace: Some(ws(1)),
        };
        let other_order = [ws(7), ws(8)];
        let decide = |release, normal| {
            let target: &[WorkspaceId] = match release {
                Some((o, _)) if o == other => &other_order,
                _ => &order,
            };
            release_decision(press, release, normal, &order, target)
        };
        // Same segment: the existing click.
        assert_eq!(
            decide(Some((out, Some(ws(1)))), true),
            ReleaseAction::Click(press)
        );
        // A different segment of the same output: reorder to that segment's index.
        assert_eq!(
            decide(Some((out, Some(ws(3)))), true),
            ReleaseAction::Reorder {
                workspace: ws(1),
                to: 2
            }
        );
        // The same output's strip outside every label: move to the end.
        assert_eq!(
            decide(Some((out, None)), true),
            ReleaseAction::Reorder {
                workspace: ws(1),
                to: 2
            }
        );
        // Another output's label: move there at that label's index.
        assert_eq!(
            decide(Some((other, Some(ws(8)))), true),
            ReleaseAction::MoveToOutput {
                workspace: ws(1),
                output: other,
                to: 1
            }
        );
        // Another output's strip outside every label: append there.
        assert_eq!(
            decide(Some((other, None)), true),
            ReleaseAction::MoveToOutput {
                workspace: ws(1),
                output: other,
                to: 2
            }
        );
        // A label the target stack does not hold, off every strip, or a cancelled mode: nothing.
        assert_eq!(
            decide(Some((other, Some(ws(2)))), true),
            ReleaseAction::None
        );
        assert_eq!(decide(None, true), ReleaseAction::None);
        assert_eq!(decide(Some((other, None)), false), ReleaseAction::None);
        // Releasing the last workspace on its own strip's empty area is already at the end.
        let last = HeaderPress {
            output: out,
            workspace: Some(ws(3)),
        };
        assert_eq!(
            release_decision(last, Some((out, None)), true, &order, &order),
            ReleaseAction::None
        );
        // A target missing from the stack order: nothing.
        assert_eq!(decide(Some((out, Some(ws(9)))), true), ReleaseAction::None);
    }

    #[test]
    fn release_decision_label_less_press_clicks_in_place_only() {
        let out = OutputId::from_raw(1);
        let order = [ws(1), ws(2)];
        let press = HeaderPress {
            output: out,
            workspace: None,
        };
        assert_eq!(
            release_decision(press, Some((out, None)), true, &order, &order),
            ReleaseAction::Click(press)
        );
        assert_eq!(
            release_decision(press, Some((out, Some(ws(2)))), true, &order, &order),
            ReleaseAction::None
        );
    }

    #[test]
    fn a_mode_change_between_press_and_release_cancels_but_the_release_is_consumed() {
        let out = OutputId::from_raw(1);
        let order = [ws(1), ws(2)];
        let press = HeaderPress {
            output: out,
            workspace: Some(ws(1)),
        };
        let mut c = ConsumedButtons::default();
        c.consume_press(0x110, press);
        // The mode is no longer Normal at release (a lock or overlay opened): no action...
        let recorded = c.consume_release(0x110).expect("release is consumed");
        for hit in [Some((out, Some(ws(1)))), Some((out, Some(ws(2))))] {
            assert_eq!(
                release_decision(recorded, hit, false, &order, &order),
                ReleaseAction::None
            );
        }
        // ...and the record is cleared, so it is not consumed twice.
        assert!(c.consume_release(0x110).is_none());
    }

    fn press_on(output: OutputId, w: u64) -> HeaderPress {
        HeaderPress {
            output,
            workspace: Some(ws(w)),
        }
    }

    fn two_outputs() -> StackOrders {
        vec![
            (OutputId::from_raw(1), vec![ws(1), ws(2), ws(3)]),
            (OutputId::from_raw(2), vec![ws(7), ws(8)]),
        ]
    }

    #[test]
    fn drag_goes_live_only_past_the_threshold_and_a_label_less_press_never_arms() {
        let out = OutputId::from_raw(1);
        let hit = Some((out, Some(ws(2))));
        let mut d = HeaderDrag::arm(1, press_on(out, 1), (100.0, 10.0)).expect("label press arms");
        assert!(!d.motion((104.9, 10.0), hit, true) && !d.is_live());
        assert_eq!(d.target(), None);
        assert!(d.motion((105.1, 10.0), hit, true) && d.is_live());
        assert_eq!(d.target(), hit);
        // Same target again: no change (no redraw); travelling back keeps it live.
        assert!(!d.motion((101.0, 10.0), hit, true) && d.is_live());
        let none = HeaderPress {
            output: out,
            workspace: None,
        };
        assert!(HeaderDrag::arm(1, none, (0.0, 0.0)).is_none());
    }

    #[test]
    fn preview_reorders_within_and_across_outputs() {
        let (a, b) = (OutputId::from_raw(1), OutputId::from_raw(2));
        let orders = two_outputs();
        let orders_after = |press, target| {
            let act = drag_action(press, Some(target), true, true, &orders);
            preview_orders(act, &orders).map(|p| p.orders)
        };
        let get = |o: &StackOrders, id| o.iter().find(|(i, _)| *i == id).unwrap().1.clone();
        // Forward and backward within one output; the empty area appends.
        let fwd = orders_after(press_on(a, 1), (a, Some(ws(3)))).unwrap();
        assert_eq!(get(&fwd, a), [ws(2), ws(3), ws(1)]);
        let back = orders_after(press_on(a, 3), (a, Some(ws(1)))).unwrap();
        assert_eq!(get(&back, a), [ws(3), ws(1), ws(2)]);
        let end = orders_after(press_on(a, 1), (a, None)).unwrap();
        assert_eq!(get(&end, a), [ws(2), ws(3), ws(1)]);
        // Across outputs: at a label's index, and at the end.
        let at = orders_after(press_on(a, 2), (b, Some(ws(8)))).unwrap();
        assert_eq!(get(&at, a), [ws(1), ws(3)]);
        assert_eq!(get(&at, b), [ws(7), ws(2), ws(8)]);
        let app = orders_after(press_on(a, 2), (b, None)).unwrap();
        assert_eq!(get(&app, b), [ws(7), ws(8), ws(2)]);
    }

    #[test]
    fn preview_refuses_what_the_release_refuses() {
        let (a, b) = (OutputId::from_raw(1), OutputId::from_raw(2));
        let mut orders = two_outputs();
        orders[1].1 = vec![ws(7)]; // output 2 holds a single workspace
        let preview = |press, target, normal| {
            let mut d = HeaderDrag::arm(1, press, (0.0, 0.0)).unwrap();
            d.motion((50.0, 0.0), target, normal);
            drag_preview(&d, normal, &orders)
        };
        let act =
            |press, target, normal, dragged| drag_action(press, target, normal, dragged, &orders);
        // The last workspace never leaves its output: no preview, no move.
        assert_eq!(preview(press_on(b, 7), Some((a, None)), true), None);
        assert_eq!(
            act(press_on(b, 7), Some((a, None)), true, true),
            ReleaseAction::None
        );
        // Own slot, off every strip, a non-Normal mode: original orders and a no-op release.
        assert_eq!(preview(press_on(a, 2), Some((a, Some(ws(2)))), true), None);
        assert_eq!(
            act(press_on(a, 2), Some((a, Some(ws(2)))), true, true),
            ReleaseAction::None
        );
        assert_eq!(preview(press_on(a, 2), None, true), None);
        assert_eq!(preview(press_on(a, 2), Some((a, Some(ws(3)))), false), None);
        assert_eq!(
            act(press_on(a, 2), Some((a, Some(ws(3)))), false, true),
            ReleaseAction::None
        );
        // Before the threshold the existing click path is unchanged.
        assert_eq!(
            act(press_on(a, 2), Some((a, Some(ws(2)))), true, false),
            ReleaseAction::Click(press_on(a, 2))
        );
    }

    #[test]
    fn preview_equals_commit_for_a_table_of_targets() {
        let (a, b) = (OutputId::from_raw(1), OutputId::from_raw(2));
        let orders = two_outputs();
        // Applies a release action to the orders the way the model ops do.
        let commit = |act: ReleaseAction| -> StackOrders {
            let mut o = orders.clone();
            match act {
                ReleaseAction::Reorder { workspace, to } => {
                    let v = &mut o[0].1;
                    v.retain(|w| *w != workspace);
                    v.insert(to.min(v.len()), workspace);
                }
                ReleaseAction::MoveToOutput {
                    workspace,
                    output,
                    to,
                } => {
                    o[0].1.retain(|w| *w != workspace);
                    let i = o.iter().position(|(id, _)| *id == output).unwrap();
                    let v = &mut o[i].1;
                    v.insert(to.min(v.len()), workspace);
                }
                ReleaseAction::None | ReleaseAction::Click(_) => {}
            }
            o
        };
        let targets = [
            None,
            Some((a, None)),
            Some((a, Some(ws(1)))),
            Some((a, Some(ws(3)))),
            Some((b, None)),
            Some((b, Some(ws(7)))),
            Some((b, Some(ws(8)))),
        ];
        for w in [1, 2, 3] {
            for t in targets {
                let act = drag_action(press_on(a, w), t, true, true, &orders);
                let shown =
                    preview_orders(act, &orders).map_or_else(|| orders.clone(), |p| p.orders);
                assert_eq!(shown, commit(act), "ws {w} -> {t:?}");
            }
        }
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
        assert!(seed_visible(Some(true), Hidden));
        assert!(!seed_visible(Some(false), Shown));
        assert!(seed_visible(None, Shown));
        assert!(!seed_visible(None, Hidden));
    }

    #[test]
    fn seed_uses_pref_and_does_not_record_one() {
        let (a, b) = (OutputId::from_raw(1), OutputId::from_raw(2));
        let mut st = HeaderState::default();
        st.prefs.insert("DP-1".into(), true);
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

#[cfg(test)]
mod scale_tests {
    use super::*;

    #[test]
    fn scaled_strip_and_segments_stay_contiguous() {
        let s = Scale::from(170.0 / 120.0);
        let map = PhysicalMap::plain(s);
        let strip = Rect::new(100, 0, 300, 24);
        let segs = segment_layout(&[20, 30], strip, 0).0;
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
