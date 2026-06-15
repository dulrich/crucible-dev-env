//! The bridge between the pure [`crucible_layout`] model and the Smithay [`Space`].
//!
//! P2-WS-3 makes `crucible-layout` the single source of truth for geometry, focus, stacking, and
//! workspace membership (decision #1 in `plans/02-tiling-model.md`). This module owns:
//!
//! - the [`World`] (the model) plus a stable bidirectional [`WindowId`] ↔ [`WindowElement`] map
//!   ([`LayoutState`]);
//! - the **pure, unit-tested** diff helper [`diff_layout`], which turns a previous and a next set of
//!   window rects into the minimal list of [`LayoutChange`]s the compositor must apply (so we never
//!   issue a configure storm — only windows whose rect or visibility actually changed are touched);
//! - the [`CrucibleState::apply_layout`] step that drives the model's geometry/visibility/focus onto
//!   the `Space` after any model mutation.
//!
//! ## Visibility (decision #4)
//! Only the active workspace's panes, and only each pane's focused window, are mapped into the
//! `Space` and eligible for input/render. Non-visible windows are *unmapped* from the `Space`
//! (`Space::unmap_elem`) but **kept alive** in the id↔element map so re-showing them (stack cycle /
//! workspace switch) is just a re-`map_element` + configure — no surface teardown.
//!
//! ## Multi-output caveat (read this before WS-4)
//! [`World::new`] creates exactly **one** model output and there is no public API to add more, so all
//! real compositor outputs are currently driven from that single model `OutputId` (the focused
//! output): each physical output renders the model's active workspace, positioned at that output's
//! offset in the `Space`. This satisfies WS-3's exit criteria (winit single-output tiling; udev shows
//! the active workspace on every connector). True per-output independent workspaces need a model
//! `add_output`/`output_rect` API — flagged for WS-4 / a follow-up `crucible-layout` change.

use std::collections::HashMap;

use crucible_layout::{GapConfig, LayoutError, Reconcile, Rect, WindowId, World};
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::element::WindowElement;
use crate::focus::KeyboardFocusTarget;
use crate::state::{Backend, CrucibleState};

/// The model plus the stable [`WindowId`] ↔ [`WindowElement`] bidirectional map.
///
/// Lives in [`CrucibleState`]. The `World` is the authority; `by_id` lets the compositor resolve a
/// model id back to the concrete Smithay element it must position/configure/focus, and is the only
/// place a [`WindowElement`] for a model window is retained while it is hidden (not in the `Space`).
#[derive(Debug, Default)]
pub struct LayoutState {
    /// The pure layout model — single source of truth for geometry/focus/stacking/membership.
    pub world: World,
    /// Maps every model window to its concrete compositor element (visible or hidden).
    by_id: HashMap<WindowId, WindowElement>,
}

impl LayoutState {
    /// Creates a fresh layout state with the default one-output/one-workspace/one-pane `World`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            world: World::new(),
            by_id: HashMap::new(),
        }
    }

    /// Binds `element` to the model `id` (called on map). Also stamps the id onto the element.
    pub fn insert(&mut self, id: WindowId, element: WindowElement) {
        element.set_window_id(id);
        self.by_id.insert(id, element);
    }

    /// Removes the binding for `id` (called on unmap), returning the element if it was present.
    pub fn remove(&mut self, id: WindowId) -> Option<WindowElement> {
        self.by_id.remove(&id)
    }

    /// Resolves a model `id` to its compositor element.
    #[must_use]
    pub fn element(&self, id: WindowId) -> Option<&WindowElement> {
        self.by_id.get(&id)
    }

    /// Finds the first bound element matching `pred`, searching visible *and* hidden windows.
    ///
    /// Used by the unmap paths to locate a window's element even when it is hidden (not in the
    /// `Space`) — e.g. an X11 surface on an inactive workspace.
    #[must_use]
    pub fn find(&self, pred: impl Fn(&WindowElement) -> bool) -> Option<&WindowElement> {
        self.by_id.values().find(|e| pred(e))
    }
}

/// A single window-placement change the compositor must apply to the `Space`.
///
/// Produced by the pure [`diff_layout`] helper from a previous and next rect set, so the apply step
/// touches only what actually changed (idempotent — no configure storms).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutChange {
    /// The window is now visible at `rect` and was either newly visible or moved/resized.
    ///
    /// `resized` is `true` when the size changed (so the apply step must send a `configure`); a pure
    /// move (same size, new position) sets it `false` and only re-`map_element`s.
    Place {
        /// The window to place.
        id: WindowId,
        /// Its new output-relative rect.
        rect: Rect,
        /// Whether the size changed (a `configure` is required), vs. a position-only move.
        resized: bool,
    },
    /// The window is no longer visible and must be unmapped from the `Space`.
    Hide {
        /// The window to hide.
        id: WindowId,
    },
}

/// Computes the minimal set of [`LayoutChange`]s to turn the `previous` rect set into `next`.
///
/// **Pure** (no compositor types) so the diff logic is unit-tested without a running compositor.
/// Both inputs are `(WindowId, Rect)` lists — the visible windows + their output-relative rects from
/// `World::geometry` before and after a model mutation. Guarantees:
///
/// - a window present in `next` but absent/changed in `previous` yields a [`LayoutChange::Place`]
///   (with `resized` set iff its size differs, so position-only moves skip the `configure`);
/// - a window present in `previous` but absent from `next` yields a [`LayoutChange::Hide`];
/// - a window whose rect is identical in both yields **no** change (idempotence — the key property
///   that prevents configure storms).
#[must_use]
pub fn diff_layout(previous: &[(WindowId, Rect)], next: &[(WindowId, Rect)]) -> Vec<LayoutChange> {
    let prev_map: HashMap<WindowId, Rect> = previous.iter().copied().collect();
    let next_map: HashMap<WindowId, Rect> = next.iter().copied().collect();

    let mut changes = Vec::new();

    // Placements / moves / resizes for everything visible in `next`.
    for (id, rect) in next {
        match prev_map.get(id) {
            Some(old) if old == rect => {
                // Unchanged — emit nothing (idempotent).
            }
            Some(old) => {
                let resized = old.width != rect.width || old.height != rect.height;
                changes.push(LayoutChange::Place {
                    id: *id,
                    rect: *rect,
                    resized,
                });
            }
            None => {
                // Newly visible — always needs a configure to take its computed size.
                changes.push(LayoutChange::Place {
                    id: *id,
                    rect: *rect,
                    resized: true,
                });
            }
        }
    }

    // Anything visible before but gone now must be hidden.
    for (id, _) in previous {
        if !next_map.contains_key(id) {
            changes.push(LayoutChange::Hide { id: *id });
        }
    }

    changes
}

/// Converts a model [`Rect`] (output-relative, top-left origin) into a Smithay logical rectangle.
#[must_use]
fn to_logical(rect: Rect) -> Rectangle<i32, Logical> {
    Rectangle::new(
        Point::from((rect.x, rect.y)),
        Size::from((rect.width.max(0), rect.height.max(0))),
    )
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Applies the current model state to the `Space`: geometry, visibility, and keyboard focus.
    ///
    /// Call after **every** model mutation, passing the [`Reconcile`] the op returned. The flow:
    ///
    /// 1. drop any `reconcile.removed` windows from the id↔element map (they were already removed
    ///    from the `Space` at unmap time, but the map binding is cleared here);
    /// 2. recompute `World::geometry` for **every** mapped compositor output (each output is offset
    ///    by its position in the `Space`) and diff it against the last applied set with
    ///    [`diff_layout`], applying only the changes (position via `map_element`, size via the
    ///    toplevel/X11 `configure`), and unmapping windows that became invisible;
    /// 3. if `reconcile.focused_window` is set (or focus otherwise changed), re-point keyboard focus
    ///    at that window's element (X11 focus is driven through the same seat path).
    ///
    /// Idempotent: re-applying an unchanged model issues no configures.
    pub fn apply_layout(&mut self, reconcile: &Reconcile) {
        // (1) Forget removed windows.
        for id in &reconcile.removed {
            self.layout.remove(*id);
        }

        // (2) Recompute + apply geometry/visibility for every output.
        self.apply_geometry();

        // (3) Follow the model's focused window with keyboard (and X11) focus.
        if reconcile.visibility_changed || reconcile.focused_window.is_some() {
            self.apply_focus(reconcile.focused_window);
        }
    }

    /// Recomputes and applies geometry + visibility for all mapped outputs (no focus change).
    ///
    /// Exposed separately so the per-frame backends can re-assert geometry cheaply (it is idempotent)
    /// without touching focus. Each compositor output is mapped to the single model `OutputId` (see
    /// the module-level multi-output caveat) and positioned at its `Space` offset.
    pub fn apply_geometry(&mut self) {
        let output_id = self.layout.world.focused_output();
        let gaps = GapConfig::default();

        // Snapshot each output's global geometry so we don't hold a borrow on the Space while mutating
        // it below. The size drives the model's per-output `Rect`; the location is the offset into the
        // global Space coordinate space.
        let output_geos: Vec<Rectangle<i32, Logical>> = self
            .space
            .outputs()
            .filter_map(|o| self.space.output_geometry(o))
            .collect();

        // Build the desired visible set across all outputs (output-relative rects from the model,
        // shifted by each output's offset into global Space coordinates).
        let mut next: Vec<(WindowId, Rect)> = Vec::new();
        for geo in &output_geos {
            let output_rect = Rect {
                x: 0,
                y: 0,
                width: geo.size.w,
                height: geo.size.h,
            };
            for (id, rect) in self.layout.world.geometry(output_id, output_rect, gaps) {
                next.push((
                    id,
                    Rect {
                        x: rect.x + geo.loc.x,
                        y: rect.y + geo.loc.y,
                        width: rect.width,
                        height: rect.height,
                    },
                ));
            }
        }

        let changes = diff_layout(&self.last_layout, &next);
        for change in &changes {
            match change {
                LayoutChange::Place { id, rect, resized } => {
                    let Some(element) = self.layout.element(*id).cloned() else {
                        continue;
                    };
                    let logical = to_logical(*rect);
                    // Size first (configure), then position via map_element so the element is mapped
                    // (or re-mapped, for a window returning from hidden) at the right place.
                    if *resized {
                        configure_size(&element, logical.size);
                    }
                    self.space.map_element(element, logical.loc, false);
                }
                LayoutChange::Hide { id } => {
                    if let Some(element) = self.layout.element(*id).cloned() {
                        self.space.unmap_elem(&element);
                    }
                }
            }
        }

        self.last_layout = next;
    }

    /// Re-points keyboard focus (and X11 focus) at the model's focused window, if any.
    fn apply_focus(&mut self, focused: Option<WindowId>) {
        let Some(focused) = focused else {
            // No focused window (e.g. the active workspace is empty): clear focus.
            self.set_keyboard_focus(None);
            return;
        };
        let Some(element) = self.layout.element(focused).cloned() else {
            return;
        };
        // Raise + X11-activate via the existing focus path, then assign keyboard focus.
        self.space.raise_element(&element, true);
        if let Some(surface) = element.x11_surface() {
            if let Some(xwm) = self.xwm.as_mut() {
                let _ = xwm.raise_window(surface);
            }
        }
        self.set_keyboard_focus(Some(KeyboardFocusTarget::Window(element)));
    }

    /// Runs a fallible model mutation, applying its reconcile only on success.
    ///
    /// Convenience for the (temporary) debug hook and any future caller that drives a `Result`-typed
    /// op; errors are returned to the caller untouched.
    pub fn with_model<F>(&mut self, op: F) -> Result<(), LayoutError>
    where
        F: FnOnce(&mut World) -> Result<Reconcile, LayoutError>,
    {
        let reconcile = op(&mut self.layout.world)?;
        self.apply_layout(&reconcile);
        Ok(())
    }
}

/// Sends a `configure` carrying `size` to whichever surface kind backs `element`.
///
/// Wayland toplevels get a pending-state size + `send_configure`; X11 windows get a direct
/// `configure` at their current location with the new size. Kept free of `self` so it does not
/// borrow the whole state.
fn configure_size(element: &WindowElement, size: Size<i32, Logical>) {
    if let Some(toplevel) = element.toplevel() {
        toplevel.with_pending_state(|state| {
            state.size = Some(size);
        });
        toplevel.send_configure();
    } else if let Some(surface) = element.x11_surface() {
        let geo = surface.geometry();
        let rect = Rectangle::new(geo.loc, size);
        let _ = surface.configure(Some(rect));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    fn wid(n: u64) -> WindowId {
        WindowId::from_raw(n)
    }

    #[test]
    fn unchanged_layout_yields_no_changes() {
        let set = vec![(wid(0), r(0, 0, 100, 100)), (wid(1), r(100, 0, 100, 100))];
        assert!(diff_layout(&set, &set).is_empty());
    }

    #[test]
    fn newly_visible_window_is_placed_with_configure() {
        let prev = vec![];
        let next = vec![(wid(0), r(0, 0, 800, 600))];
        let changes = diff_layout(&prev, &next);
        assert_eq!(
            changes,
            vec![LayoutChange::Place {
                id: wid(0),
                rect: r(0, 0, 800, 600),
                resized: true,
            }]
        );
    }

    #[test]
    fn removed_window_is_hidden() {
        let prev = vec![(wid(0), r(0, 0, 800, 600))];
        let next = vec![];
        assert_eq!(
            diff_layout(&prev, &next),
            vec![LayoutChange::Hide { id: wid(0) }]
        );
    }

    #[test]
    fn position_only_move_does_not_set_resized() {
        // Same size, different origin -> a move that must NOT trigger a configure.
        let prev = vec![(wid(0), r(0, 0, 400, 600))];
        let next = vec![(wid(0), r(410, 0, 400, 600))];
        assert_eq!(
            diff_layout(&prev, &next),
            vec![LayoutChange::Place {
                id: wid(0),
                rect: r(410, 0, 400, 600),
                resized: false,
            }]
        );
    }

    #[test]
    fn resize_sets_resized_flag() {
        let prev = vec![(wid(0), r(0, 0, 800, 600))];
        let next = vec![(wid(0), r(0, 0, 400, 600))];
        assert_eq!(
            diff_layout(&prev, &next),
            vec![LayoutChange::Place {
                id: wid(0),
                rect: r(0, 0, 400, 600),
                resized: true,
            }]
        );
    }

    #[test]
    fn split_re_tiles_only_the_two_affected_panes() {
        // One full-width window, then a second appears taking the right half: the first shrinks to
        // the left half (resized) and the second is newly placed. Nothing is spuriously hidden.
        let prev = vec![(wid(0), r(0, 0, 800, 600))];
        let next = vec![(wid(0), r(0, 0, 400, 600)), (wid(1), r(400, 0, 400, 600))];
        let changes = diff_layout(&prev, &next);
        assert_eq!(changes.len(), 2);
        assert!(changes.contains(&LayoutChange::Place {
            id: wid(0),
            rect: r(0, 0, 400, 600),
            resized: true,
        }));
        assert!(changes.contains(&LayoutChange::Place {
            id: wid(1),
            rect: r(400, 0, 400, 600),
            resized: true,
        }));
    }

    #[test]
    fn stack_cycle_hides_one_shows_another_in_place() {
        // A stacked pane: window 0 visible, then cycle makes window 1 visible in the SAME rect.
        // 0 is hidden, 1 is placed (newly visible -> configure).
        let rect = r(0, 0, 800, 600);
        let prev = vec![(wid(0), rect)];
        let next = vec![(wid(1), rect)];
        let changes = diff_layout(&prev, &next);
        assert_eq!(changes.len(), 2);
        assert!(changes.contains(&LayoutChange::Hide { id: wid(0) }));
        assert!(changes.contains(&LayoutChange::Place {
            id: wid(1),
            rect,
            resized: true,
        }));
    }

    #[test]
    fn workspace_switch_hides_all_old_shows_all_new() {
        // Switching workspaces replaces the whole visible set.
        let prev = vec![(wid(0), r(0, 0, 400, 600)), (wid(1), r(400, 0, 400, 600))];
        let next = vec![(wid(2), r(0, 0, 800, 600))];
        let changes = diff_layout(&prev, &next);
        assert_eq!(changes.len(), 3);
        assert!(changes.contains(&LayoutChange::Hide { id: wid(0) }));
        assert!(changes.contains(&LayoutChange::Hide { id: wid(1) }));
        assert!(changes.contains(&LayoutChange::Place {
            id: wid(2),
            rect: r(0, 0, 800, 600),
            resized: true,
        }));
    }
}
