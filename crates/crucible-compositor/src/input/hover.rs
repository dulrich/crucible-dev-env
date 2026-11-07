//! Focus-follows-mouse hover helpers: the pointer-enter identity and its pure detection.

/// What the pointer hovers, for focus-follows-mouse enter detection.
///
/// Keyed at **pane** level: a managed window and the area of its pane it does not cover share
/// one identity, so moving between them is not an enter. The window under the pointer is kept
/// separately by the caller for routing.
#[derive(Debug, Clone, PartialEq)]
pub enum HoverTarget {
    /// A pane (via its managed window, or its uncovered area).
    Pane(crucible_layout::PaneId),
    /// Gap / empty space on this output, outside every pane.
    Output(smithay::output::Output),
}

/// The pane-level hover key for a pointer location, before the output fallback.
///
/// `managed_window` is the managed window under the pointer (if any); `pane_under` is the pane
/// whose rect contains the pointer. A managed window keys by the pane holding it; otherwise the
/// pane under the pointer. `None` means the caller falls back to the output.
#[must_use]
pub fn hover_pane_key(
    world: &crucible_layout::World,
    managed_window: Option<crucible_layout::WindowId>,
    pane_under: Option<crucible_layout::PaneId>,
) -> Option<crucible_layout::PaneId> {
    match managed_window {
        Some(id) => world.pane_of_window(id),
        None => pane_under,
    }
}

/// Records `current` as the hover target and reports whether it is a new one (an *enter*).
///
/// `None` (an unmanaged element, or nothing resolvable) leaves `stored` unchanged and is never an
/// enter.
pub fn hover_entered<T: PartialEq>(stored: &mut Option<T>, current: Option<T>) -> bool {
    let Some(current) = current else {
        return false;
    };
    if stored.as_ref() == Some(&current) {
        return false;
    }
    *stored = Some(current);
    true
}

/// What a re-layout does about the workspace shown under the pointer ([`switch_hover_action`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SwitchHover {
    /// Record the shown workspace; no focus moves.
    Settle,
    /// The shown workspace changed but an overlay or a pointer grab holds the refocus back; keep
    /// the old record so a later re-layout retries.
    Wait,
    /// The shown workspace changed: record it and focus what is under the pointer, as an enter.
    Refocus,
}

/// Whether a workspace switch on the output under the pointer refocuses the pane under it.
///
/// `seen` is the workspace last recorded under the pointer, `shown` the one shown there now
/// (`None` off every output). Without this, focus-follows-mouse lags a switch until the next
/// pointer motion. The first observation (`seen` is `None`), no change, the pointer off every
/// output, the option off or a locked session settle without moving focus; a mapped overlay or a
/// pointer grab (header drag) waits.
#[must_use]
pub(super) fn switch_hover_action<W: PartialEq>(
    seen: Option<&W>,
    shown: Option<&W>,
    enabled: bool,
    locked: bool,
    normal: bool,
    grabbed: bool,
) -> SwitchHover {
    match (seen, shown) {
        (Some(a), Some(b)) if a != b => {}
        _ => return SwitchHover::Settle,
    }
    if !enabled || locked {
        SwitchHover::Settle
    } else if !normal || grabbed {
        SwitchHover::Wait
    } else {
        SwitchHover::Refocus
    }
}

#[cfg(test)]
mod hover_tests {
    use super::hover_entered;

    #[test]
    fn a_switch_under_the_pointer_refocuses_only_when_free() {
        use super::{SwitchHover, switch_hover_action as act};
        // A real switch with nothing in the way refocuses.
        assert_eq!(
            act(Some(&1), Some(&2), true, false, true, false),
            SwitchHover::Refocus
        );
        // No change, the first observation, or the pointer off every output: settle.
        assert_eq!(
            act(Some(&1), Some(&1), true, false, true, false),
            SwitchHover::Settle
        );
        assert_eq!(
            act(None, Some(&2), true, false, true, false),
            SwitchHover::Settle
        );
        assert_eq!(
            act(Some(&1), None::<&i32>, true, false, true, false),
            SwitchHover::Settle
        );
        // Option off or locked: settle (no retry later).
        assert_eq!(
            act(Some(&1), Some(&2), false, false, true, false),
            SwitchHover::Settle
        );
        assert_eq!(
            act(Some(&1), Some(&2), true, true, false, false),
            SwitchHover::Settle
        );
        // An overlay (picker pick) or a header drag: wait and retry.
        assert_eq!(
            act(Some(&1), Some(&2), true, false, false, false),
            SwitchHover::Wait
        );
        assert_eq!(
            act(Some(&1), Some(&2), true, false, true, true),
            SwitchHover::Wait
        );
    }

    #[test]
    fn first_target_is_an_enter_and_repeats_are_not() {
        let mut stored = None;
        assert!(hover_entered(&mut stored, Some(1)));
        assert!(!hover_entered(&mut stored, Some(1)));
        assert!(hover_entered(&mut stored, Some(2)));
        assert_eq!(stored, Some(2));
    }

    #[test]
    fn none_leaves_the_tracker_unchanged() {
        let mut stored = Some(1);
        assert!(!hover_entered(&mut stored, None));
        assert_eq!(stored, Some(1));
        // Returning to the same target after an unmanaged detour is not a new enter.
        assert!(!hover_entered(&mut stored, Some(1)));
    }

    #[test]
    fn window_and_uncovered_area_of_one_pane_share_an_enter_identity() {
        use super::{HoverTarget, hover_pane_key};
        use crucible_layout::{Direction, World};

        let mut w = World::new("1");
        let win = w.open_window();
        let left = w.pane_of_window(win).unwrap();
        let right = w.split(Direction::Right);

        // Pointer over the left window: an enter into the left pane.
        let mut stored = None;
        let key = |m, p| hover_pane_key(&w, m, p).map(HoverTarget::Pane);
        assert!(hover_entered(&mut stored, key(Some(win), Some(left))));

        // Keyboard-driven focus moves elsewhere; the tracker is untouched by that.
        let mut w2 = w.clone();
        w2.focus(Direction::Right);
        assert_eq!(w2.focused_pane_id(), right);

        // Window -> uncovered area of the same pane and back: not enters.
        assert!(!hover_entered(&mut stored, key(None, Some(left))));
        assert!(!hover_entered(&mut stored, key(Some(win), Some(left))));
        // Into the other pane: an enter.
        assert!(hover_entered(&mut stored, key(None, Some(right))));
    }
}
