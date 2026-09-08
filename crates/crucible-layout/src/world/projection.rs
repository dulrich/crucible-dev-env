//! The derived [`Projection`] (visible geometry + focus) and its [`Projection::diff`].

use super::World;
use crate::geometry::{GapConfig, Rect};
use crate::{OutputId, WindowId};

/// The complete, derived picture of what the compositor must show: **the** projection.
///
/// A `Projection` is *derived* from a [`World`] snapshot ([`World::project`]); it is never mutated
/// directly and carries no change hints. It is the single source of visible geometry and focus —
/// nothing infers visibility from a mutation's return value or from the compositor's `Space`.
/// Comparing the previously applied projection with a freshly derived one
/// ([`Projection::diff`]) yields the minimal work the compositor must perform.
///
/// The window set is canonicalised (ascending [`WindowId`], one entry per id) so equality and diffs
/// are order-independent and deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Projection {
    /// Visible windows and their rects, ascending by id.
    windows: Vec<(WindowId, Rect)>,
    /// The window that should hold keyboard focus, if any.
    focus: Option<WindowId>,
}

impl Projection {
    /// Builds a projection from a visible `(WindowId, Rect)` set plus the focused identity.
    ///
    /// The set is sorted by [`WindowId`] and de-duplicated (first entry per id wins).
    #[must_use]
    pub fn new(
        windows: impl IntoIterator<Item = (WindowId, Rect)>,
        focus: Option<WindowId>,
    ) -> Self {
        let mut windows: Vec<(WindowId, Rect)> = windows.into_iter().collect();
        windows.sort_by_key(|(id, _)| *id);
        windows.dedup_by_key(|(id, _)| *id);
        Self { windows, focus }
    }

    /// The visible windows and their rects, ascending by id.
    #[must_use]
    pub fn windows(&self) -> &[(WindowId, Rect)] {
        &self.windows
    }

    /// The window that should hold keyboard focus, if any.
    #[must_use]
    pub fn focus(&self) -> Option<WindowId> {
        self.focus
    }

    /// The rect of `id` if it is visible in this projection.
    #[must_use]
    pub fn rect_of(&self, id: WindowId) -> Option<Rect> {
        self.windows
            .binary_search_by_key(&id, |(wid, _)| *wid)
            .ok()
            .map(|idx| self.windows[idx].1)
    }

    /// Whether `id` is visible in this projection.
    #[must_use]
    pub fn is_visible(&self, id: WindowId) -> bool {
        self.rect_of(id).is_some()
    }

    /// The minimal work turning `self` (the applied projection) into `next`.
    ///
    /// Guarantees:
    ///
    /// - a window visible in both with an identical rect yields **no** change (idempotence — the
    ///   property that prevents configure storms);
    /// - a window whose rect changed yields [`LayoutChange::Place`], with `resized` set iff its size
    ///   differs (so position-only moves skip the `configure`);
    /// - a newly visible window yields `Place { resized: true }`;
    /// - a window that stopped being visible yields [`LayoutChange::Hide`];
    /// - focus reports [`FocusChange::Unchanged`] unless the focused identity actually differs.
    ///
    /// Changes come out in ascending [`WindowId`] order (deterministic).
    pub fn diff(&self, next: &Projection) -> ProjectionDiff {
        let (prev, next_windows) = (&self.windows, &next.windows);
        let mut changes = Vec::new();
        let (mut i, mut j) = (0usize, 0usize);
        loop {
            match (prev.get(i), next_windows.get(j)) {
                (None, None) => break,
                (Some((pid, _)), None) => {
                    changes.push(LayoutChange::Hide { id: *pid });
                    i += 1;
                }
                (None, Some((nid, nrect))) => {
                    changes.push(LayoutChange::Place {
                        id: *nid,
                        rect: *nrect,
                        resized: true,
                    });
                    j += 1;
                }
                (Some((pid, prect)), Some((nid, nrect))) => {
                    if pid == nid {
                        if prect != nrect {
                            changes.push(LayoutChange::Place {
                                id: *nid,
                                rect: *nrect,
                                resized: prect.width != nrect.width || prect.height != nrect.height,
                            });
                        }
                        i += 1;
                        j += 1;
                    } else if pid < nid {
                        changes.push(LayoutChange::Hide { id: *pid });
                        i += 1;
                    } else {
                        changes.push(LayoutChange::Place {
                            id: *nid,
                            rect: *nrect,
                            resized: true,
                        });
                        j += 1;
                    }
                }
            }
        }

        let focus = if self.focus == next.focus {
            FocusChange::Unchanged
        } else {
            FocusChange::To(next.focus)
        };
        ProjectionDiff { changes, focus }
    }
}

/// A single window-placement change the compositor must apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutChange {
    /// The window is visible at `rect` and was newly shown, moved, or resized.
    Place {
        /// The window to place.
        id: WindowId,
        /// Its new rect, in the same coordinate space the projection was derived in.
        rect: Rect,
        /// Whether the size changed (a `configure` is required), vs. a position-only move.
        resized: bool,
    },
    /// The window is no longer visible and must be unmapped.
    Hide {
        /// The window to hide.
        id: WindowId,
    },
}

/// How keyboard focus differs between two projections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusChange {
    /// Both projections name the same focused identity: nothing to re-point.
    Unchanged,
    /// Focus must move to this window (`None` = no window should hold focus).
    To(Option<WindowId>),
}

/// The minimal work turning one [`Projection`] into another (see [`Projection::diff`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct ProjectionDiff {
    /// Geometry/visibility changes, ascending by [`WindowId`].
    pub changes: Vec<LayoutChange>,
    /// Whether (and where) keyboard focus must move.
    pub focus: FocusChange,
}

impl ProjectionDiff {
    /// `true` when the two projections were identical: applying this diff is a no-op.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.focus == FocusChange::Unchanged
    }
}

impl World {
    /// Derives the [`Projection`]: every visible window's rect plus the focused identity.
    ///
    /// `outputs` pairs each model [`OutputId`] with its **global** rect (origin + pixel size, as the
    /// compositor knows it). Each output contributes *its own* active workspace's visible windows,
    /// laid out inside that rect and offset into the global coordinate space. Output ids the model
    /// does not know contribute nothing.
    ///
    /// This is the sole source of visible geometry and focus; mutating ops return no hints.
    #[must_use]
    pub fn project(&self, outputs: &[(OutputId, Rect)], gaps: GapConfig) -> Projection {
        let mut windows: Vec<(WindowId, Rect)> = Vec::new();
        for (output_id, global) in outputs {
            let local = Rect::new(0, 0, global.width, global.height);
            for (window, rect) in self.geometry(*output_id, local, gaps) {
                windows.push((
                    window,
                    Rect::new(
                        rect.x + global.x,
                        rect.y + global.y,
                        rect.width,
                        rect.height,
                    ),
                ));
            }
        }
        Projection::new(windows, self.focused_window())
    }

    /// Like [`World::project`] but using every output's stored `Output::rect`.
    ///
    /// Convenient for callers (and tests) that keep the model's rects up to date via
    /// [`World::set_output_rect`].
    #[must_use]
    pub fn project_stored(&self, gaps: GapConfig) -> Projection {
        let outputs: Vec<(OutputId, Rect)> =
            self.outputs.iter().map(|(id, o)| (*id, o.rect)).collect();
        self.project(&outputs, gaps)
    }
}

#[cfg(test)]
mod projection_tests {
    use super::*;

    const fn r(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect::new(x, y, w, h)
    }

    fn wid(n: u64) -> WindowId {
        WindowId::from_raw(n)
    }

    fn proj(windows: &[(WindowId, Rect)], focus: Option<WindowId>) -> Projection {
        Projection::new(windows.iter().copied(), focus)
    }

    #[test]
    fn construction_canonicalises_order_and_duplicates() {
        let a = proj(
            &[(wid(2), r(0, 0, 10, 10)), (wid(1), r(10, 0, 10, 10))],
            None,
        );
        let b = proj(
            &[(wid(1), r(10, 0, 10, 10)), (wid(2), r(0, 0, 10, 10))],
            None,
        );
        assert_eq!(a, b);
        assert_eq!(a.windows()[0].0, wid(1));

        // A duplicate id keeps the first entry only.
        let dup = proj(&[(wid(1), r(0, 0, 1, 1)), (wid(1), r(5, 5, 5, 5))], None);
        assert_eq!(dup.windows().len(), 1);
    }

    #[test]
    fn unchanged_projection_diffs_to_nothing() {
        let set = [(wid(0), r(0, 0, 100, 100)), (wid(1), r(100, 0, 100, 100))];
        let p = proj(&set, Some(wid(1)));
        let diff = p.diff(&p.clone());
        assert!(diff.is_empty());
        assert_eq!(diff.focus, FocusChange::Unchanged);
        assert!(diff.changes.is_empty());
    }

    #[test]
    fn newly_visible_window_is_placed_with_configure() {
        let diff = Projection::default().diff(&proj(&[(wid(0), r(0, 0, 800, 600))], Some(wid(0))));
        assert_eq!(
            diff.changes,
            vec![LayoutChange::Place {
                id: wid(0),
                rect: r(0, 0, 800, 600),
                resized: true,
            }]
        );
        assert_eq!(diff.focus, FocusChange::To(Some(wid(0))));
    }

    #[test]
    fn window_that_stopped_being_visible_is_hidden() {
        let prev = proj(&[(wid(0), r(0, 0, 800, 600))], Some(wid(0)));
        let diff = prev.diff(&Projection::default());
        assert_eq!(diff.changes, vec![LayoutChange::Hide { id: wid(0) }]);
        assert_eq!(diff.focus, FocusChange::To(None));
    }

    #[test]
    fn position_only_move_does_not_set_resized() {
        let prev = proj(&[(wid(0), r(0, 0, 400, 600))], Some(wid(0)));
        let next = proj(&[(wid(0), r(410, 0, 400, 600))], Some(wid(0)));
        assert_eq!(
            prev.diff(&next).changes,
            vec![LayoutChange::Place {
                id: wid(0),
                rect: r(410, 0, 400, 600),
                resized: false,
            }]
        );
    }

    #[test]
    fn resize_sets_resized_flag() {
        let prev = proj(&[(wid(0), r(0, 0, 800, 600))], Some(wid(0)));
        let next = proj(&[(wid(0), r(0, 0, 400, 600))], Some(wid(0)));
        assert_eq!(
            prev.diff(&next).changes,
            vec![LayoutChange::Place {
                id: wid(0),
                rect: r(0, 0, 400, 600),
                resized: true,
            }]
        );
    }

    #[test]
    fn focus_only_change_moves_focus_without_geometry_work() {
        let set = [(wid(0), r(0, 0, 400, 600)), (wid(1), r(400, 0, 400, 600))];
        let prev = proj(&set, Some(wid(0)));
        let next = proj(&set, Some(wid(1)));
        let diff = prev.diff(&next);
        assert!(diff.changes.is_empty());
        assert_eq!(diff.focus, FocusChange::To(Some(wid(1))));
        assert!(!diff.is_empty());
    }

    #[test]
    fn split_re_tiles_only_the_two_affected_panes() {
        // One full-width window, then a second appears taking the right half: the first shrinks to
        // the left half (resized) and the second is newly placed. Nothing is spuriously hidden.
        let prev = proj(&[(wid(0), r(0, 0, 800, 600))], Some(wid(0)));
        let next = proj(
            &[(wid(0), r(0, 0, 400, 600)), (wid(1), r(400, 0, 400, 600))],
            Some(wid(1)),
        );
        assert_eq!(
            prev.diff(&next).changes,
            vec![
                LayoutChange::Place {
                    id: wid(0),
                    rect: r(0, 0, 400, 600),
                    resized: true,
                },
                LayoutChange::Place {
                    id: wid(1),
                    rect: r(400, 0, 400, 600),
                    resized: true,
                },
            ]
        );
    }

    #[test]
    fn stack_cycle_hides_one_shows_another_in_place() {
        // A stacked pane: window 0 visible, then cycle makes window 1 visible in the SAME rect.
        let rect = r(0, 0, 800, 600);
        let prev = proj(&[(wid(0), rect)], Some(wid(0)));
        let next = proj(&[(wid(1), rect)], Some(wid(1)));
        assert_eq!(
            prev.diff(&next).changes,
            vec![
                LayoutChange::Hide { id: wid(0) },
                LayoutChange::Place {
                    id: wid(1),
                    rect,
                    resized: true,
                },
            ]
        );
    }

    #[test]
    fn workspace_switch_hides_all_old_shows_all_new() {
        let prev = proj(
            &[(wid(0), r(0, 0, 400, 600)), (wid(1), r(400, 0, 400, 600))],
            Some(wid(1)),
        );
        let next = proj(&[(wid(2), r(0, 0, 800, 600))], Some(wid(2)));
        let changes = prev.diff(&next).changes;
        assert_eq!(changes.len(), 3);
        assert!(changes.contains(&LayoutChange::Hide { id: wid(0) }));
        assert!(changes.contains(&LayoutChange::Hide { id: wid(1) }));
        assert!(changes.contains(&LayoutChange::Place {
            id: wid(2),
            rect: r(0, 0, 800, 600),
            resized: true,
        }));
    }

    #[test]
    fn rect_lookups_answer_visibility() {
        let p = proj(&[(wid(3), r(1, 2, 3, 4))], Some(wid(3)));
        assert_eq!(p.rect_of(wid(3)), Some(r(1, 2, 3, 4)));
        assert!(p.is_visible(wid(3)));
        assert_eq!(p.rect_of(wid(9)), None);
        assert!(!p.is_visible(wid(9)));
    }
}
