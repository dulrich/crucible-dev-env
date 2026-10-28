//! Modal input routing: the one place the lock/overlay precedence is written.
//!
//! [`CrucibleState::input_mode`] resolves the mode **once per event** (`Locked` > `Overlay` >
//! `Normal`); the pure route functions below decide what each kind of event does in that mode, so
//! no handler re-asks "is the session locked?" or "is an overlay mapped?" on its own. The
//! `set_keyboard_focus` lock choke point (`keyboard.rs`) stays a separate, additional guard.

use crucible_keys::Action;

use crate::element::WindowElement;
use crate::state::{Backend, CrucibleState};

/// What input is currently allowed to do. The overlay payload is the mapped overlay (a
/// [`WindowElement`] in production; any `PartialEq` handle in tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InputMode<O = WindowElement> {
    /// The session is locked: only the lock surface receives input; only VT switching runs.
    Locked,
    /// A modal overlay is mapped (and the session is not locked).
    Overlay(O),
    /// Neither: ordinary click-to-focus, header clicks and focus-follows-mouse.
    Normal,
}

impl<O> InputMode<O> {
    /// Whether the session is locked.
    pub(crate) fn is_locked(&self) -> bool {
        matches!(self, Self::Locked)
    }

    /// Whether a modal overlay is mapped (and not shadowed by the lock).
    pub(crate) fn has_overlay(&self) -> bool {
        matches!(self, Self::Overlay(_))
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Resolves the input mode for one event. Precedence is **Locked > Overlay > Normal**, and
    /// this is the only place it is written.
    pub(crate) fn input_mode(&self) -> InputMode {
        if self.is_session_locked() {
            InputMode::Locked
        } else if let Some(overlay) = self.mapped_overlay() {
            InputMode::Overlay(overlay.clone())
        } else {
            InputMode::Normal
        }
    }
}

/// What a button press does in a given mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PressRoute {
    /// Try the header strip first (a hit consumes the press).
    pub(super) try_header: bool,
    /// Close the mapped overlay (the click missed it).
    pub(super) close_overlay: bool,
    /// Run click-to-focus (after the optional overlay close).
    pub(super) click_focus: bool,
}

/// Routes a press. `under` is the element under the pointer.
///
/// - `Locked`: the press goes to the lock surface; no header, no focus change.
/// - `Overlay`: modal — no header consume; the overlay closes unless the click hit it.
/// - `Normal`: header first, else click-to-focus.
#[must_use]
pub(super) fn press_route<O: PartialEq>(mode: &InputMode<O>, under: Option<&O>) -> PressRoute {
    match mode {
        InputMode::Locked => PressRoute {
            try_header: false,
            close_overlay: false,
            click_focus: false,
        },
        InputMode::Overlay(overlay) => PressRoute {
            try_header: false,
            close_overlay: under != Some(overlay),
            click_focus: true,
        },
        InputMode::Normal => PressRoute {
            try_header: true,
            close_overlay: false,
            click_focus: true,
        },
    }
}

/// Whether focus-follows-mouse tracks hover enters at all (not while locked).
#[must_use]
pub(super) fn hover_tracks<O>(mode: &InputMode<O>) -> bool {
    !mode.is_locked()
}

/// Whether a hover may move focus: only on a fresh enter, with no pointer grab and no modal
/// (lock or overlay) mode.
#[must_use]
pub(super) fn hover_moves_focus<O>(mode: &InputMode<O>, entered: bool, grabbed: bool) -> bool {
    entered && !grabbed && matches!(mode, InputMode::Normal)
}

/// Whether a chord closes the mapped overlay before it runs: the keyboard twin of the click rule
/// ([`press_route`]), so a workspace verb never leaves an overlay stranded over another
/// workspace. Overlay-opening chords are exempt (they replace or toggle the overlay themselves),
/// as are VT switches (the overlay is still there on return).
#[must_use]
pub(super) fn chord_closes_overlay<O>(mode: &InputMode<O>, action: &Action) -> bool {
    mode.has_overlay()
        && !matches!(
            action,
            Action::OpenPicker
                | Action::RenameWorkspace
                | Action::OpenLauncher
                | Action::OpenSettings
                | Action::ToggleHeader
                | Action::VtSwitch(_)
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_layout::Direction;

    const OVERLAY: u32 = 1;
    const OTHER: u32 = 2;

    fn locked() -> InputMode<u32> {
        InputMode::Locked
    }
    fn overlay() -> InputMode<u32> {
        InputMode::Overlay(OVERLAY)
    }
    fn normal() -> InputMode<u32> {
        InputMode::Normal
    }

    #[test]
    fn locked_header_click_is_not_consumed_and_moves_no_focus() {
        // A header click has no element under it (`None`).
        let route = press_route(&locked(), None);
        assert!(!route.try_header, "no header consume while locked");
        assert!(!route.click_focus, "no focus change while locked");
        assert!(!route.close_overlay);
    }

    #[test]
    fn overlay_header_click_is_not_consumed_and_closes_the_overlay() {
        let route = press_route(&overlay(), None);
        assert!(
            !route.try_header,
            "header press falls through to an outside click"
        );
        assert!(route.close_overlay);
        assert!(route.click_focus);
    }

    #[test]
    fn overlay_click_closes_only_when_missing_the_overlay() {
        assert!(press_route(&overlay(), Some(&OTHER)).close_overlay);
        assert!(press_route(&overlay(), None).close_overlay);
        assert!(!press_route(&overlay(), Some(&OVERLAY)).close_overlay);
        for under in [None, Some(&OVERLAY), Some(&OTHER)] {
            assert!(!press_route(&normal(), under).close_overlay);
        }
    }

    #[test]
    fn normal_press_tries_the_header_then_click_focuses() {
        let route = press_route(&normal(), Some(&OTHER));
        assert!(route.try_header);
        assert!(route.click_focus);
    }

    #[test]
    fn hover_moves_focus_only_when_unblocked() {
        assert!(hover_moves_focus(&normal(), true, false));
        assert!(!hover_moves_focus(&normal(), false, false), "no enter");
        assert!(!hover_moves_focus(&normal(), true, true), "grabbed");
    }

    #[test]
    fn overlay_hover_moves_no_focus_but_still_tracks() {
        assert!(!hover_moves_focus(&overlay(), true, false));
        assert!(hover_tracks(&overlay()));
    }

    #[test]
    fn locked_hover_neither_tracks_nor_moves_focus() {
        assert!(!hover_moves_focus(&locked(), true, false));
        assert!(!hover_tracks(&locked()));
    }

    #[test]
    fn locked_wins_over_a_mapped_overlay() {
        // `input_mode` never yields `Overlay` while locked, so the routes for the resolved mode
        // are the `Locked` ones: nothing closes, nothing focuses.
        let resolved = locked();
        assert!(!press_route(&resolved, Some(&OTHER)).close_overlay);
        assert!(!chord_closes_overlay(&resolved, &Action::Slot(1)));
    }

    #[test]
    fn chord_closes_a_mapped_overlay_except_overlay_openers_and_vt_switch() {
        for action in [
            Action::Slot(7),
            Action::CycleWorkspace(1),
            Action::NewWorkspace,
            Action::CloseWorkspace,
            Action::Focus(Direction::Left),
            Action::RunTerminal,
        ] {
            assert!(chord_closes_overlay(&overlay(), &action), "{action}");
            assert!(!chord_closes_overlay(&normal(), &action), "{action}");
        }
        for action in [
            Action::OpenPicker,
            Action::RenameWorkspace,
            Action::OpenLauncher,
            Action::OpenSettings,
            Action::ToggleHeader,
            Action::VtSwitch(2),
        ] {
            assert!(!chord_closes_overlay(&overlay(), &action), "{action}");
        }
    }
}
