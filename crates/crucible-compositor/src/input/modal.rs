//! Modal input routing: the one place the lock/overlay precedence is written.
//!
//! [`CrucibleState::input_mode`] resolves the mode **once per event** (`Greeter` > `Locked` >
//! `LoggingOut` > `Snip` > `Overlay` > `Normal`); the pure route functions below decide what each kind of event does in that mode, so
//! no handler re-asks "is the session locked?" or "is an overlay mapped?" on its own. The
//! `set_keyboard_focus` lock choke point (`keyboard.rs`) stays a separate, additional guard.

use crucible_keys::Action;

use crate::element::WindowElement;
use crate::state::{Backend, CrucibleState};

/// What input is currently allowed to do. The overlay payload is the mapped overlay (a
/// [`WindowElement`] in production; any `PartialEq` handle in tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InputMode<O = WindowElement> {
    /// Greeter mode: "locked plus". Every lock rule applies ([`Self::is_locked`]); in addition a
    /// non-admitted key goes to the login panel, the key press that woke blanked outputs is
    /// dropped, and no pane chrome is drawn.
    Greeter,
    /// The session is locked: only the lock surface receives input; only VT switching and `set-layout` run.
    Locked,
    /// The "Logging out" modal shows (below the lock, above snip): no input reaches clients;
    /// only `quit` (the repeat) and VT switches run.
    LoggingOut,
    /// Snip mode (`snip` action): every output shows a frozen frame and all pointer and key
    /// input feeds the selection; clients receive nothing. Only `Snip` and `VtSwitch` run.
    Snip,
    /// A modal overlay is mapped (and the session is not locked).
    Overlay(O),
    /// Neither: ordinary click-to-focus, header clicks and focus-follows-mouse.
    Normal,
}

impl<O> InputMode<O> {
    /// Whether input runs under the lock's rules: the session is locked, or greeter mode (which
    /// is "locked plus"). Exhaustive on purpose: a new mode must decide.
    pub(crate) fn is_locked(&self) -> bool {
        match self {
            Self::Greeter | Self::Locked => true,
            Self::LoggingOut | Self::Snip | Self::Overlay(_) | Self::Normal => false,
        }
    }

    /// Whether greeter mode is active (the greeter-only rules on top of [`Self::is_locked`]).
    pub(crate) fn is_greeter(&self) -> bool {
        matches!(self, Self::Greeter)
    }

    /// Whether a modal overlay is mapped (and not shadowed by the lock).
    pub(crate) fn has_overlay(&self) -> bool {
        matches!(self, Self::Overlay(_))
    }

    /// Whether the logout modal shows (and the session is not locked).
    pub(crate) fn is_logging_out(&self) -> bool {
        matches!(self, Self::LoggingOut)
    }

    /// Whether snip mode is active (and not shadowed by the lock).
    pub(crate) fn is_snipping(&self) -> bool {
        matches!(self, Self::Snip)
    }

    /// Whether the mode is modal (lock, snip or overlay): pointer constraints and fullscreen
    /// confinement are suspended.
    pub(crate) fn is_modal(&self) -> bool {
        !matches!(self, Self::Normal)
    }
}

/// The pure precedence resolver behind [`CrucibleState::input_mode`]:
/// **Greeter > Locked > LoggingOut > Snip > Overlay > Normal**.
#[must_use]
pub(crate) fn resolve_mode<O>(
    greeter: bool,
    locked: bool,
    logging_out: bool,
    snipping: bool,
    overlay: Option<O>,
) -> InputMode<O> {
    if greeter {
        InputMode::Greeter
    } else if locked {
        InputMode::Locked
    } else if logging_out {
        InputMode::LoggingOut
    } else if snipping {
        InputMode::Snip
    } else if let Some(overlay) = overlay {
        InputMode::Overlay(overlay)
    } else {
        InputMode::Normal
    }
}

/// The admission of an IPC focus request (`focus-pid`, `focus-live`) in `mode`, decided before any
/// mutation: only Normal input admits it. The lock answers `session locked` (as the lock admission
/// does, which runs first); greeter, logging out, snip and a mapped overlay answer `input busy`.
pub(crate) fn admit_ipc_focus<O>(mode: &InputMode<O>) -> Result<(), &'static str> {
    match mode {
        InputMode::Normal => Ok(()),
        InputMode::Locked => Err("session locked"),
        _ => Err("input busy"),
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Resolves the input mode for one event. Precedence is **Greeter > Locked > LoggingOut >
    /// Snip > Overlay > Normal** ([`resolve_mode`]), and this is the only place it is written.
    pub(crate) fn input_mode(&self) -> InputMode {
        resolve_mode(
            self.greeter.is_some(),
            self.is_session_locked(),
            self.logging_out(),
            self.snip.is_some(),
            self.mapped_overlay().cloned(),
        )
    }

    /// [`admit_ipc_focus`] for the current input mode; the error is the reply reason.
    pub(crate) fn ipc_focus_admitted(&self) -> Result<(), String> {
        admit_ipc_focus(&self.input_mode()).map_err(str::to_string)
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
/// - `Greeter` / `Locked`: the press goes to the lock surface; no header, no focus change.
/// - `LoggingOut`: never reached — the routers' pre-dispatch step drops every button while the
///   logout notice shows; routed like the lock for exhaustiveness.
/// - `Snip`: the press feeds the selection (handled before routing); nothing else.
/// - `Overlay`: modal — no header consume; the overlay closes unless the click hit it.
/// - `Normal`: header first, else click-to-focus.
#[must_use]
pub(super) fn press_route<O: PartialEq>(mode: &InputMode<O>, under: Option<&O>) -> PressRoute {
    match mode {
        InputMode::Greeter | InputMode::Locked | InputMode::LoggingOut | InputMode::Snip => {
            PressRoute {
                try_header: false,
                close_overlay: false,
                click_focus: false,
            }
        }
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

/// Whether focus-follows-mouse tracks hover enters at all (not while locked or snipping).
#[must_use]
pub(super) fn hover_tracks<O>(mode: &InputMode<O>) -> bool {
    !mode.is_locked() && !mode.is_logging_out() && !mode.is_snipping()
}

/// What a hover does to focus ([`hover_decision`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HoverDecision {
    /// Nothing: no fresh enter, a pointer grab, or a locked session.
    Ignore,
    /// A mapped overlay holds the enter back: it is deferred to the next overlay open, which
    /// applies it so the overlay opens on the pointer's output.
    Defer,
    /// Focus moves to the hover target.
    Move,
}

/// What a hover does: `Move` only on a fresh enter, with no pointer grab and no modal (lock or
/// overlay) mode; `Defer` on a fresh ungrabbed enter while an overlay is mapped; else `Ignore`.
#[must_use]
pub(super) fn hover_decision<O>(
    mode: &InputMode<O>,
    entered: bool,
    grabbed: bool,
) -> HoverDecision {
    if !entered || grabbed {
        HoverDecision::Ignore
    } else if !mode.is_modal() {
        HoverDecision::Move
    } else if mode.has_overlay() {
        HoverDecision::Defer
    } else {
        HoverDecision::Ignore
    }
}

/// What applying a deferred hover does ([`deferred_hover_action`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeferredHover {
    /// Nothing resolvable under the pointer: the hover tracker is cleared, no focus moves.
    Clear,
    /// Record the target, but a pointer grab is active so no focus moves.
    Track,
    /// Record the target and focus it.
    Focus,
}

/// How a deferred hover resolves, given whether a hover target is under the pointer now and
/// whether a pointer grab is active (the same outcome as a fresh enter in `Normal` mode).
#[must_use]
pub(super) fn deferred_hover_action(current_is_some: bool, grabbed: bool) -> DeferredHover {
    match (current_is_some, grabbed) {
        (false, _) => DeferredHover::Clear,
        (true, true) => DeferredHover::Track,
        (true, false) => DeferredHover::Focus,
    }
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
                | Action::SetWorkspaceRoot
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

    /// The IPC focus admission table: only Normal admits; the precedence winner decides.
    #[test]
    fn ipc_focus_requests_are_admitted_only_in_normal_mode() {
        let table: [(InputMode<u32>, Result<(), &str>); 6] = [
            (InputMode::Normal, Ok(())),
            (InputMode::Locked, Err("session locked")),
            (InputMode::Greeter, Err("input busy")),
            (InputMode::LoggingOut, Err("input busy")),
            (InputMode::Snip, Err("input busy")),
            (InputMode::Overlay(OVERLAY), Err("input busy")),
        ];
        for (mode, want) in table {
            assert_eq!(admit_ipc_focus(&mode), want, "{mode:?}");
        }
        // A mapped overlay under the lock: the lock wins.
        let mode = resolve_mode(false, true, false, false, Some(OVERLAY));
        assert_eq!(admit_ipc_focus(&mode), Err("session locked"));
    }

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
        assert_eq!(hover_decision(&normal(), true, false), HoverDecision::Move);
        assert_eq!(
            hover_decision(&normal(), false, false),
            HoverDecision::Ignore,
            "no enter"
        );
        assert_eq!(
            hover_decision(&normal(), true, true),
            HoverDecision::Ignore,
            "grabbed"
        );
    }

    #[test]
    fn overlay_hover_moves_no_focus_but_still_tracks() {
        assert_ne!(hover_decision(&overlay(), true, false), HoverDecision::Move);
        assert!(hover_tracks(&overlay()));
    }

    #[test]
    fn only_an_overlay_defers_a_hover_enter() {
        assert_eq!(
            hover_decision(&overlay(), true, false),
            HoverDecision::Defer
        );
        assert_eq!(
            hover_decision(&overlay(), false, false),
            HoverDecision::Ignore,
            "no enter"
        );
        assert_eq!(
            hover_decision(&overlay(), true, true),
            HoverDecision::Ignore,
            "grabbed"
        );
        assert_ne!(
            hover_decision(&normal(), true, false),
            HoverDecision::Defer,
            "normal mode moves focus instead"
        );
        assert_ne!(hover_decision(&locked(), true, false), HoverDecision::Defer);
    }

    #[test]
    fn locked_hover_neither_tracks_nor_moves_focus() {
        assert_eq!(
            hover_decision(&locked(), true, false),
            HoverDecision::Ignore
        );
        assert!(!hover_tracks(&locked()));
    }

    #[test]
    fn hover_decision_table() {
        use HoverDecision::{Defer, Ignore, Move};
        for (mode, name, enter_free) in [
            (normal(), "normal", Move),
            (overlay(), "overlay", Defer),
            (locked(), "locked", Ignore),
        ] {
            for entered in [false, true] {
                for grabbed in [false, true] {
                    let want = if entered && !grabbed {
                        enter_free
                    } else {
                        Ignore
                    };
                    assert_eq!(
                        hover_decision(&mode, entered, grabbed),
                        want,
                        "{name} entered={entered} grabbed={grabbed}"
                    );
                }
            }
        }
    }

    #[test]
    fn deferred_hover_action_table() {
        assert_eq!(deferred_hover_action(false, false), DeferredHover::Clear);
        assert_eq!(deferred_hover_action(false, true), DeferredHover::Clear);
        assert_eq!(deferred_hover_action(true, true), DeferredHover::Track);
        assert_eq!(deferred_hover_action(true, false), DeferredHover::Focus);
    }

    #[test]
    fn mode_precedence_is_locked_snip_overlay_normal() {
        assert_eq!(
            resolve_mode(false, true, true, true, Some(OVERLAY)),
            InputMode::Locked
        );
        assert_eq!(
            resolve_mode(false, true, false, false, None::<u32>),
            InputMode::Locked
        );
        assert_eq!(
            resolve_mode(false, false, false, true, Some(OVERLAY)),
            InputMode::Snip
        );
        assert_eq!(
            resolve_mode(false, false, false, true, None::<u32>),
            InputMode::Snip
        );
        assert_eq!(
            resolve_mode(false, false, false, false, Some(OVERLAY)),
            InputMode::Overlay(OVERLAY)
        );
        assert_eq!(
            resolve_mode(false, false, false, false, None::<u32>),
            InputMode::Normal
        );
    }

    #[test]
    fn greeter_wins_everything_and_routes_like_the_lock() {
        for locked in [false, true] {
            for logging_out in [false, true] {
                for snipping in [false, true] {
                    let m = resolve_mode(true, locked, logging_out, snipping, Some(OVERLAY));
                    assert_eq!(m, InputMode::Greeter);
                }
            }
        }
        let greeter: InputMode<u32> = InputMode::Greeter;
        assert!(greeter.is_locked() && greeter.is_greeter() && greeter.is_modal());
        assert!(!locked().is_greeter());
        assert!(!greeter.has_overlay() && !greeter.is_logging_out() && !greeter.is_snipping());
        assert_eq!(
            press_route(&greeter, Some(&OTHER)),
            press_route(&locked(), Some(&OTHER))
        );
        assert!(!hover_tracks(&greeter));
        assert_eq!(hover_decision(&greeter, true, false), HoverDecision::Ignore);
        assert!(!chord_closes_overlay(&greeter, &Action::Slot(1)));
    }

    #[test]
    fn snip_is_modal_and_routes_nothing() {
        let snip: InputMode<u32> = InputMode::Snip;
        assert!(snip.is_modal() && snip.is_snipping());
        assert!(locked().is_modal() && overlay().is_modal() && !normal().is_modal());
        let r = press_route(&snip, Some(&OTHER));
        assert!(!r.try_header && !r.close_overlay && !r.click_focus);
        assert!(!hover_tracks(&snip));
        assert_eq!(hover_decision(&snip, true, false), HoverDecision::Ignore);
        assert!(!chord_closes_overlay(&snip, &Action::Slot(1)));
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
            Action::ReorderWorkspace(1),
            Action::NewWorkspace,
            Action::CloseWindow,
            Action::CloseWorkspace,
            Action::DeleteWorkspace,
            Action::Focus(Direction::Left),
            Action::RunTerminal,
        ] {
            assert!(chord_closes_overlay(&overlay(), &action), "{action}");
            assert!(!chord_closes_overlay(&normal(), &action), "{action}");
        }
        for action in [
            Action::OpenPicker,
            Action::RenameWorkspace,
            Action::SetWorkspaceRoot,
            Action::OpenLauncher,
            Action::OpenSettings,
            Action::ToggleHeader,
            Action::VtSwitch(2),
        ] {
            assert!(!chord_closes_overlay(&overlay(), &action), "{action}");
        }
    }

    #[test]
    fn logging_out_blocks_clients_below_the_lock_above_snip() {
        assert_eq!(
            resolve_mode(false, true, true, true, Some(OVERLAY)),
            InputMode::Locked,
            "lock wins"
        );
        let m = resolve_mode(false, false, true, true, Some(OVERLAY));
        assert_eq!(m, InputMode::LoggingOut);
        assert!(m.is_modal());
        let r = press_route(&m, Some(&OTHER));
        assert!(!r.try_header && !r.close_overlay && !r.click_focus);
        assert!(!hover_tracks(&m));
        assert_eq!(hover_decision(&m, true, false), HoverDecision::Ignore);
        assert!(!chord_closes_overlay(&m, &Action::Slot(1)));
    }
}
