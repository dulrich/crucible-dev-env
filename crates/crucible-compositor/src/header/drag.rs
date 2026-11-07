//! Header press, drag and release decisions (pure), plus the compositor glue that feeds pointer
//! motion to a drag and commits a release.
//!
//! The drag preview runs the model's own operation: [`apply_release`] on a clone of the committed
//! [`World`], so preview and commit cannot diverge. The preview is cached on the [`HeaderDrag`] and
//! recomputed only when the drop target or the committed stack orders change.

use std::collections::BTreeMap;

use crucible_layout::{LayoutError, OutputId, WorkspaceId, World};

use super::HeaderState;
use crate::{
    layout::model_output_id,
    state::{Backend, CrucibleState},
};

/// A strip hit: the output whose strip is under a point and, on a label, its workspace (see
/// `CrucibleState::header_hit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StripHit {
    /// The output whose strip was hit.
    pub output: OutputId,
    /// The workspace label under the point (`None` on the strip outside every label).
    pub workspace: Option<WorkspaceId>,
}

/// Where a consumed header press landed: the strip hit under the press.
pub type HeaderPress = StripHit;

/// Physical button codes whose press the strip consumed, with where it landed; their paired
/// release is swallowed (audit E8: a release must never be forwarded for a press that was not)
/// and decides the click or reorder ([`drag_action`]).
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

/// Every output's workspace stack in order, as the committed model holds it.
pub type StackOrders = Vec<(OutputId, Vec<WorkspaceId>)>;

/// Pointer travel (logical px, Euclidean from the press point) past which a header press becomes
/// a drag (cvault's `THRESHOLD`).
pub const DRAG_THRESHOLD: f64 = 5.0;

/// Decides a consumed header release. `release` is the strip hit under the release point
/// (`None` off every strip or with the strip hidden); `normal` is whether the input mode is
/// still `Normal` (a lock or overlay opened since the press cancels); `dragged` is whether the
/// press went past the threshold; `orders` is the committed stack order of every output. Same
/// place as the press is a click (nothing once dragged); a different segment of the same output
/// is a reorder to that segment's index; the same output's strip outside every label moves the
/// workspace to the end (nothing if it already is last); another output's segment moves it there
/// at that segment's index and that output's strip outside every label appends it; anything else
/// does nothing. A label-less press never moves anything. The model's own refusals (the last
/// workspace never leaves its output) are left to [`apply_release`].
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
    let Some(hit) = release.filter(|_| normal) else {
        return ReleaseAction::None;
    };
    if hit == press {
        return if dragged {
            ReleaseAction::None
        } else {
            ReleaseAction::Click(press)
        };
    }
    let Some(pressed) = press.workspace else {
        return ReleaseAction::None;
    };
    if hit.output != press.output {
        let target_order = order_of(hit.output);
        let to = match hit.workspace {
            Some(target) => target_order.iter().position(|w| *w == target),
            None => Some(target_order.len()),
        };
        return to.map_or(ReleaseAction::None, |to| ReleaseAction::MoveToOutput {
            workspace: pressed,
            output: hit.output,
            to,
        });
    }
    let order = order_of(press.output);
    match hit.workspace {
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

/// Commits a release's move to `world` through the model's own verbs: `Reorder` is
/// [`World::reorder_workspace`], `MoveToOutput` is [`World::move_workspace_to_output_at`] (which
/// refuses an output's last workspace with [`LayoutError::LastWorkspace`], mutating nothing).
/// `None` and `Click` change nothing.
///
/// # Errors
/// The model verb's error; the world is then unchanged.
pub fn apply_release(world: &mut World, action: ReleaseAction) -> Result<(), LayoutError> {
    match action {
        ReleaseAction::Reorder { workspace, to } => {
            world.reorder_workspace(workspace, to).map(drop)
        }
        ReleaseAction::MoveToOutput {
            workspace,
            output,
            to,
        } => world.move_workspace_to_output_at(workspace, output, to),
        ReleaseAction::None | ReleaseAction::Click(_) => Ok(()),
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

/// The preview of dropping `press` on `target`: a clone of the committed `world` with the
/// release's [`drag_action`] applied by [`apply_release`], read back per output of `base` (the
/// committed orders). `None` when the action moves nothing, the model refuses it, or the orders
/// come out unchanged.
#[must_use]
pub fn preview_of(
    world: &World,
    base: &StackOrders,
    press: HeaderPress,
    target: Option<StripHit>,
) -> Option<DragPreview> {
    let action = drag_action(press, target, true, true, base);
    let (ReleaseAction::Reorder { workspace, .. } | ReleaseAction::MoveToOutput { workspace, .. }) =
        action
    else {
        return None;
    };
    let mut next = world.clone();
    apply_release(&mut next, action).ok()?;
    let orders: StackOrders = base
        .iter()
        .map(|(id, _)| {
            let order = next
                .output(*id)
                .map_or_else(Vec::new, |o| o.workspaces().to_vec());
            (*id, order)
        })
        .collect();
    (orders != *base).then_some(DragPreview {
        orders,
        dragged: workspace,
    })
}

/// A preview computed for one drop target over one set of committed orders.
#[derive(Debug, Clone, PartialEq)]
struct PreviewCache {
    base: StackOrders,
    preview: Option<DragPreview>,
}

/// A consumed header press that may become a drag: armed only by a press on a label, live once
/// the pointer travels past [`DRAG_THRESHOLD`]. While live it holds the current drop target,
/// computed against the **committed** model layout (never the previewed one, or the reflowed
/// segment would move under the pointer and the target would oscillate), and the cached preview.
#[derive(Debug, Clone, PartialEq)]
pub struct HeaderDrag {
    /// The physical button that pressed.
    pub button: u32,
    /// The press.
    pub press: HeaderPress,
    origin: (f64, f64),
    live: bool,
    target: Option<StripHit>,
    cache: Option<PreviewCache>,
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
            cache: None,
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
    /// went live or the target changed), i.e. whether the header must be redrawn; a change drops
    /// the cached preview.
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
        if changed {
            self.cache = None;
        }
        changed
    }

    /// Recomputes the cached preview ([`preview_of`]) when the target changed since the last
    /// refresh or the committed `base` orders differ from the cached ones; returns whether it
    /// recomputed. A drag that is not live caches nothing.
    pub fn refresh_preview(&mut self, world: &World, base: StackOrders) -> bool {
        if !self.live || self.cache.as_ref().is_some_and(|c| c.base == base) {
            return false;
        }
        let preview = preview_of(world, &base, self.press, self.target);
        self.cache = Some(PreviewCache { base, preview });
        true
    }

    /// The cached preview, only while live and computed over exactly the committed `base`.
    #[must_use]
    pub fn preview(&self, base: &StackOrders) -> Option<&DragPreview> {
        let cache = self
            .cache
            .as_ref()
            .filter(|c| self.live && c.base == *base)?;
        cache.preview.as_ref()
    }
}

/// The highlighted index in the drawn `order`: the dragged workspace when it is on this strip,
/// else the committed active workspace's new place (or its old index, clamped, if it left).
pub(super) fn active_index(
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

impl HeaderState {
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
        let dragged = self
            .drag
            .take_if(|d| d.button == button)
            .is_some_and(|d| d.is_live());
        Some((press, dragged))
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Every output's committed workspace stack order.
    pub(crate) fn header_orders(&self) -> StackOrders {
        self.raw_output_rects()
            .into_iter()
            .filter_map(|(id, _)| Some((id, self.layout.world.output(id)?.workspaces().to_vec())))
            .collect()
    }

    /// The live drag's preview, if any: refreshes the drag's cache against the committed model
    /// (recomputing only on a target or committed-order change), then reads it. Hidden while the
    /// input mode is not `Normal`. Only drawing consults it; hit-testing never does.
    pub(crate) fn header_drag_preview(&mut self) -> Option<DragPreview> {
        self.header.drag.as_ref()?;
        let base = self.header_orders();
        let drag = self.header.drag.as_mut()?;
        drag.refresh_preview(&self.layout.world, base.clone());
        if !self.input_is_normal() {
            return None;
        }
        self.header.drag.as_ref()?.preview(&base).cloned()
    }

    /// Feeds pointer motion to the armed drag (if any): recomputes the drop target against the
    /// committed layout. The header redraws through the per-frame element diff (the drawn layout
    /// is built from the preview), so a changed target needs no further damage call.
    pub(crate) fn header_drag_motion(&mut self) {
        if self.header.drag.is_none() {
            return;
        }
        let at = self.pointer.current_location();
        let hit = self.header_hit(at.x.floor() as i32, at.y.floor() as i32);
        let normal = self.input_is_normal();
        if let Some(drag) = self.header.drag.as_mut() {
            drag.motion((at.x, at.y), hit, normal);
        }
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

    /// Applies a consumed header press at its release: a click, a reorder, a move or nothing,
    /// per [`drag_action`], with the move committed by [`apply_release`]. Re-checks the input
    /// mode (a lock or overlay may have opened since the press).
    pub(crate) fn finish_header_press(&mut self, press: HeaderPress, dragged: bool) {
        let at = self.pointer.current_location();
        let release = self.header_hit(at.x.floor() as i32, at.y.floor() as i32);
        let normal = self.input_is_normal();
        let orders = self.header_orders();
        match drag_action(press, release, normal, dragged, &orders) {
            ReleaseAction::None => {}
            ReleaseAction::Click(p) => self.header_click(p.output, p.workspace),
            action => match self.with_model(|world| apply_release(world, action)) {
                Ok(()) => {}
                Err(err @ LayoutError::LastWorkspace(_)) => {
                    tracing::debug!(%err, "header drag: the last workspace stays on its output");
                }
                Err(err) if matches!(action, ReleaseAction::Reorder { .. }) => {
                    tracing::warn!(%err, "header drag: workspace reorder failed");
                }
                Err(err) => tracing::warn!(%err, "header drag: workspace move failed"),
            },
        }
    }
}

#[cfg(test)]
mod tests;
