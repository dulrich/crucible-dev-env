//! The `Super`-leader chord table: maps a modifier + keysym combination to an [`Action`].
//!
//! This is the canonical key policy (`reviews/2026-07-30-tn-code-review.md` F1): the compositor
//! converts its own `ModifiersState`/`Keysym` types to [`Modifiers`]/[`Keysym`] at the edge and
//! calls [`resolve`] with its [`Bindings`]; it owns no binding table of its own. The table is data
//! ([`Bindings::default`] plus config overrides, see [`crate::bindings`]), not a `match`.
//!
//! ## Default binding table
//!
//! | Chord                 | [`Action`]                    | Model op                          |
//! |-----------------------|--------------------------------|-----------------------------------|
//! | `Super+Return`        | `OpenLauncher`                 | program launcher (overlay)        |
//! | `Super+comma`         | `OpenSettings`                 | settings overlay (`crucible keys`)|
//! | `Super+B`             | `ToggleHeader`                 | show/hide the workspace header    |
//! | `Super+Shift+Q`       | `Quit`                         | stop compositor                   |
//! | `Super+W`             | `ClosePane`                    | `World::close_pane()`             |
//! | `Super+H/J/K/L`       | `Focus(Left/Down/Up/Right)`    | `World::focus(dir)`               |
//! | `Super+Shift+H/J/K/L` | `Move(Left/Down/Up/Right)`     | `World::move_(dir)`               |
//! | `Super+Backslash`     | `Split(Right)`                 | `World::split(Right)`             |
//! | `Super+Minus`         | `Split(Down)`                  | `World::split(Down)`              |
//! | `Super+Shift+Return`  | `RunTerminal`                  | spawn terminal (stacks in pane)   |
//! | `Super+Tab`           | `CycleStack(1)`                | `World::cycle_stack(1)`           |
//! | `Super+Shift+Tab`     | `CycleStack(-1)`               | `World::cycle_stack(-1)`          |
//! | `Super+Ctrl+H/J/K/L`  | `Resize(Left/Down/Up/Right)`   | `World::resize(dir, delta)`       |
//! | `Super+]`             | `CycleWorkspace(1)`            | `World::cycle_workspace(1)`       |
//! | `Super+[`             | `CycleWorkspace(-1)`           | `World::cycle_workspace(-1)`      |
//! | `Super+N`             | `NewWorkspace`                 | `World::new_workspace(auto_name)` |
//! | `Super+Shift+]`       | `MoveWindowToWorkspace(1)`     | `World::move_window_to_workspace` |
//! | `Super+Shift+[`       | `MoveWindowToWorkspace(-1)`    | `World::move_window_to_workspace` |
//! | `Super+M`             | `MarkWindow`                   | `World::mark_window`              |
//! | `Super+Shift+M`       | `SwapMarkedWindow`             | `World::swap_marked_window`       |
//! | `Super+Shift+W`       | `CloseWorkspace`               | save, then `World::remove_workspace` |
//! | `Super+Ctrl+]`        | `MoveWorkspaceToOutput(1)`     | `World::move_workspace_to_output` |
//! | `Super+Ctrl+[`        | `MoveWorkspaceToOutput(-1)`    | `World::move_workspace_to_output` |
//! | `Super+<0-9>`         | `Slot(n)`                      | open the workspace pinned to slot n |
//! | `Super+Shift+<0-9>`   | `PinSlot(n)`                   | pin the active workspace to slot n |
//! | `Super+P`             | `OpenPicker`                   | fuzzy workspace picker           |
//! | `Super+Shift+R`       | `RenameWorkspace`              | rename the active workspace     |
//! | `Super+Shift+D`       | `CaptureWorkspaceRoot`         | focused terminal's cwd becomes the workspace root |
//! | `Super+Ctrl+Shift+D`  | `SetWorkspaceRoot`             | workspace-root prompt in the picker overlay |
//! | `Super+Escape`        | `Lock`                         | spawn `lock-command` (screen lock) |
//! | `Ctrl+Alt+F<n>`       | `VtSwitch(n)`                  | backend VT switch (hard-wired, independent of `Super`) |
//!
//! `Super+Shift+<0-9>` also binds the US shifted symbol (`exclam`, `at`, ...), which is what the
//! compositor delivers with Shift held.
//!
//! Letter chords bind both cases (Caps Lock), except plain `Super+H/J/K/L` which, as always,
//! match the lowercase keysym only. Any chord except VT switching can be rebound or unbound from
//! the `binds` block of the config file (`docs/config.md`).

use crate::action::Action;
use crate::bindings::Bindings;
#[cfg(test)]
use crucible_layout::Direction;

/// Re-export of the keysym type the chord table matches against.
///
/// This is exactly the type `smithay::input::keyboard::Keysym` re-exports (via `xkbcommon`), so
/// the compositor passes its keysyms straight through with no conversion — only [`Modifiers`]
/// needs an edge conversion, since `ModifiersState` is Smithay's own bitset type.
pub use xkeysym::Keysym;

/// The subset of keyboard modifier state the chord table cares about.
///
/// Deliberately not the compositor's `ModifiersState`: `crucible-keys` has no Smithay dependency,
/// so the compositor converts at the edge (see `crucible-compositor::input::keyboard`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    /// The `Super` (Mod4 / "logo"/"windows") key. CDE's leader; every default chord requires it
    /// except VT switching (`Ctrl+Alt+F<n>`), which is independent of the leader.
    pub logo: bool,
    /// The `Shift` key.
    pub shift: bool,
    /// The `Ctrl` key.
    pub ctrl: bool,
}

/// Maps a modifier + keysym combination to an [`Action`] via `bindings`, or `None` to forward the
/// key.
///
/// **Pure** (no compositor state) so the `Super`-leader binding table is unit-testable without a
/// running compositor. VT switching (`Ctrl+Alt+F<n>` → `XF86_Switch_VT_<n>`) is hard-wired first,
/// outside the table and independent of `Super`; after that only `Super` (`modifiers.logo`) chords
/// are looked up, so every non-`Super` key returns `None` and is forwarded to the focused client.
/// The bound action is returned unchanged; terminal choice belongs to the compositor.
#[must_use]
pub fn resolve(bindings: &Bindings, modifiers: Modifiers, keysym: Keysym) -> Option<Action> {
    // VT switching (`Ctrl+Alt+F<n>`) takes priority over the Super leader and is independent of it.
    // libinput maps `Ctrl+Alt+F<n>` to the `XF86Switch_VT_<n>` keysyms; map those to a VT switch.
    if (Keysym::XF86_Switch_VT_1.raw()..=Keysym::XF86_Switch_VT_12.raw()).contains(&keysym.raw()) {
        let vt = (keysym.raw() - Keysym::XF86_Switch_VT_1.raw() + 1) as i32;
        return Some(Action::VtSwitch(vt));
    }

    // Only the Super (logo / Mod4) leader is bound; everything else forwards to the client.
    if !modifiers.logo {
        return None;
    }

    bindings.get(modifiers, keysym).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `resolve` against the default table (the pre-config behaviour these tests were written for).
    fn resolve(modifiers: Modifiers, keysym: Keysym) -> Option<Action> {
        super::resolve(&Bindings::default(), modifiers, keysym)
    }

    /// Builds a [`Modifiers`] with only `logo` (Super) set.
    fn super_only() -> Modifiers {
        Modifiers {
            logo: true,
            ..Default::default()
        }
    }

    /// Builds a [`Modifiers`] with `logo` + `shift` (Super+Shift).
    fn super_shift() -> Modifiers {
        Modifiers {
            logo: true,
            shift: true,
            ..Default::default()
        }
    }

    /// Builds a [`Modifiers`] with `logo` + `ctrl` (Super+Ctrl).
    fn super_ctrl() -> Modifiers {
        Modifiers {
            logo: true,
            ctrl: true,
            ..Default::default()
        }
    }

    // ---- exhaustive default-chord table --------------------------------------------------
    //
    // One row per default chord in the module-level doc table (case-folded pairs collapsed to a
    // single row where the resolver treats them identically), asserted in one pass so a future
    // change to the table cannot silently drop or misroute a chord.

    #[test]
    fn super_return_opens_the_launcher() {
        assert_eq!(
            resolve(super_only(), Keysym::Return),
            Some(Action::OpenLauncher)
        );
    }

    #[test]
    fn every_default_chord_resolves_to_its_action() {
        let plain_super: &[(Keysym, Action)] = &[
            (Keysym::w, Action::ClosePane),
            (Keysym::W, Action::ClosePane),
            (Keysym::h, Action::Focus(Direction::Left)),
            (Keysym::j, Action::Focus(Direction::Down)),
            (Keysym::k, Action::Focus(Direction::Up)),
            (Keysym::l, Action::Focus(Direction::Right)),
            (Keysym::backslash, Action::Split(Direction::Right)),
            (Keysym::minus, Action::Split(Direction::Down)),
            (Keysym::Tab, Action::CycleStack(1)),
            (Keysym::bracketright, Action::CycleWorkspace(1)),
            (Keysym::bracketleft, Action::CycleWorkspace(-1)),
            (Keysym::n, Action::NewWorkspace),
            (Keysym::N, Action::NewWorkspace),
            (Keysym::m, Action::MarkWindow),
            (Keysym::M, Action::MarkWindow),
            (Keysym::_0, Action::Slot(0)),
            (Keysym::_1, Action::Slot(1)),
            (Keysym::_2, Action::Slot(2)),
            (Keysym::_3, Action::Slot(3)),
            (Keysym::_4, Action::Slot(4)),
            (Keysym::_5, Action::Slot(5)),
            (Keysym::_6, Action::Slot(6)),
            (Keysym::_7, Action::Slot(7)),
            (Keysym::_8, Action::Slot(8)),
            (Keysym::_9, Action::Slot(9)),
        ];
        for (keysym, expected) in plain_super {
            assert_eq!(
                resolve(super_only(), *keysym),
                Some(expected.clone()),
                "Super+{keysym:?}"
            );
        }

        let super_shift_chords: &[(Keysym, Action)] = &[
            (Keysym::Return, Action::RunTerminal),
            (Keysym::q, Action::Quit),
            (Keysym::Q, Action::Quit),
            (Keysym::h, Action::Move(Direction::Left)),
            (Keysym::H, Action::Move(Direction::Left)),
            (Keysym::j, Action::Move(Direction::Down)),
            (Keysym::J, Action::Move(Direction::Down)),
            (Keysym::k, Action::Move(Direction::Up)),
            (Keysym::K, Action::Move(Direction::Up)),
            (Keysym::l, Action::Move(Direction::Right)),
            (Keysym::L, Action::Move(Direction::Right)),
            (Keysym::Tab, Action::CycleStack(-1)),
            (Keysym::ISO_Left_Tab, Action::CycleStack(-1)),
            (Keysym::m, Action::SwapMarkedWindow),
            (Keysym::M, Action::SwapMarkedWindow),
            (Keysym::bracketright, Action::MoveWindowToWorkspace(1)),
            (Keysym::bracketleft, Action::MoveWindowToWorkspace(-1)),
            (Keysym::w, Action::CloseWorkspace),
            (Keysym::W, Action::CloseWorkspace),
            (Keysym::_0, Action::PinSlot(0)),
            (Keysym::parenright, Action::PinSlot(0)),
            (Keysym::_1, Action::PinSlot(1)),
            (Keysym::exclam, Action::PinSlot(1)),
            (Keysym::_2, Action::PinSlot(2)),
            (Keysym::at, Action::PinSlot(2)),
            (Keysym::_3, Action::PinSlot(3)),
            (Keysym::numbersign, Action::PinSlot(3)),
            (Keysym::_4, Action::PinSlot(4)),
            (Keysym::dollar, Action::PinSlot(4)),
            (Keysym::_5, Action::PinSlot(5)),
            (Keysym::percent, Action::PinSlot(5)),
            (Keysym::_6, Action::PinSlot(6)),
            (Keysym::asciicircum, Action::PinSlot(6)),
            (Keysym::_7, Action::PinSlot(7)),
            (Keysym::ampersand, Action::PinSlot(7)),
            (Keysym::_8, Action::PinSlot(8)),
            (Keysym::asterisk, Action::PinSlot(8)),
            (Keysym::_9, Action::PinSlot(9)),
            (Keysym::parenleft, Action::PinSlot(9)),
        ];
        for (keysym, expected) in super_shift_chords {
            assert_eq!(
                resolve(super_shift(), *keysym),
                Some(expected.clone()),
                "Super+Shift+{keysym:?}"
            );
        }

        let super_ctrl_chords: &[(Keysym, Action)] = &[
            (Keysym::h, Action::Resize(Direction::Left)),
            (Keysym::H, Action::Resize(Direction::Left)),
            (Keysym::j, Action::Resize(Direction::Down)),
            (Keysym::J, Action::Resize(Direction::Down)),
            (Keysym::k, Action::Resize(Direction::Up)),
            (Keysym::K, Action::Resize(Direction::Up)),
            (Keysym::l, Action::Resize(Direction::Right)),
            (Keysym::L, Action::Resize(Direction::Right)),
            (Keysym::bracketright, Action::MoveWorkspaceToOutput(1)),
            (Keysym::bracketleft, Action::MoveWorkspaceToOutput(-1)),
        ];
        for (keysym, expected) in super_ctrl_chords {
            assert_eq!(
                resolve(super_ctrl(), *keysym),
                Some(expected.clone()),
                "Super+Ctrl+{keysym:?}"
            );
        }
    }

    #[test]
    fn vt_switch_is_independent_of_super() {
        // No modifiers at all still resolves a VT-switch keysym.
        assert_eq!(
            resolve(Modifiers::default(), Keysym::XF86_Switch_VT_1),
            Some(Action::VtSwitch(1))
        );
        assert_eq!(
            resolve(super_only(), Keysym::XF86_Switch_VT_12),
            Some(Action::VtSwitch(12))
        );
        // Every VT in the advertised range resolves to its 1-based index.
        for n in 1..=12 {
            let raw = Keysym::XF86_Switch_VT_1.raw() + (n - 1);
            assert_eq!(
                resolve(Modifiers::default(), Keysym::new(raw)),
                Some(Action::VtSwitch(n as i32))
            );
        }
    }

    // ---- capitalization and modifier precedence -------------------------------------------

    #[test]
    fn shift_modifier_precedence_over_keysym_case() {
        // The resolver decides the verb from the *modifier bit*, not from whether the keysym
        // already arrived case-shifted — `h` and `H` under Super+Shift must resolve identically.
        assert_eq!(
            resolve(super_shift(), Keysym::h),
            resolve(super_shift(), Keysym::H)
        );
        assert_eq!(
            resolve(super_shift(), Keysym::h),
            Some(Action::Move(Direction::Left))
        );
    }

    // ---- unmatched / no-Super chords → forward ---------------------------------------------

    #[test]
    fn unmatched_super_chord_is_none() {
        assert_eq!(resolve(super_only(), Keysym::z), None);
    }

    #[test]
    fn plain_super_q_no_longer_quits() {
        assert_eq!(resolve(super_only(), Keysym::q), None);
        assert_eq!(resolve(super_only(), Keysym::Q), None);
    }

    #[test]
    fn non_super_keys_are_forwarded() {
        // Without the logo modifier, even bound keysyms must forward to the focused client.
        let no_mods = Modifiers::default();
        assert_eq!(resolve(no_mods, Keysym::Return), None);
        assert_eq!(resolve(no_mods, Keysym::q), None);
        assert_eq!(resolve(no_mods, Keysym::h), None);
        assert_eq!(resolve(no_mods, Keysym::backslash), None);
        assert_eq!(resolve(no_mods, Keysym::Tab), None);
        assert_eq!(resolve(no_mods, Keysym::bracketright), None);
    }

    // ---- new verbs and the pre-config oracle ------------------------------------------------

    #[test]
    fn new_verbs_have_default_chords() {
        assert_eq!(resolve(super_only(), Keysym::p), Some(Action::OpenPicker));
        assert_eq!(resolve(super_only(), Keysym::P), Some(Action::OpenPicker));
        assert_eq!(
            resolve(super_shift(), Keysym::r),
            Some(Action::RenameWorkspace)
        );
        assert_eq!(
            resolve(super_shift(), Keysym::R),
            Some(Action::RenameWorkspace)
        );
        assert_eq!(resolve(Modifiers::default(), Keysym::p), None);
    }

    #[test]
    fn workspace_root_verbs_have_default_chords_in_both_cases() {
        let sup_ctrl_shift = Modifiers {
            logo: true,
            shift: true,
            ctrl: true,
        };
        for k in [Keysym::d, Keysym::D] {
            assert_eq!(
                resolve(super_shift(), k),
                Some(Action::CaptureWorkspaceRoot)
            );
            assert_eq!(resolve(sup_ctrl_shift, k), Some(Action::SetWorkspaceRoot));
            assert_eq!(resolve(super_only(), k), None);
        }
    }

    #[test]
    fn overrides_flow_through_resolve_and_vt_switch_stays_hardwired() {
        let b = Bindings::with_overrides(&[
            ("Super+Shift+Q".to_string(), "none".to_string()),
            ("Super+X".to_string(), "quit".to_string()),
        ])
        .unwrap();
        assert_eq!(super::resolve(&b, super_shift(), Keysym::q), None);
        assert_eq!(
            super::resolve(&b, super_only(), Keysym::x),
            Some(Action::Quit)
        );
        // The table is empty, yet VT switching still resolves.
        let empty = Bindings::with_overrides(&[]).unwrap();
        assert_eq!(
            super::resolve(&empty, Modifiers::default(), Keysym::XF86_Switch_VT_3),
            Some(Action::VtSwitch(3))
        );
    }

    /// The pre-config `match` table, verbatim kept as an oracle.
    fn legacy(modifiers: Modifiers, keysym: Keysym) -> Option<Action> {
        if !modifiers.logo {
            return None;
        }
        match (modifiers.shift, modifiers.ctrl, keysym) {
            // Intended change (WP-2): plain Super+Return now opens the launcher.
            (false, false, Keysym::Return) => Some(Action::OpenLauncher),
            (false, false, Keysym::w | Keysym::W) => Some(Action::ClosePane),
            (false, false, Keysym::h) => Some(Action::Focus(Direction::Left)),
            (false, false, Keysym::j) => Some(Action::Focus(Direction::Down)),
            (false, false, Keysym::k) => Some(Action::Focus(Direction::Up)),
            (false, false, Keysym::l) => Some(Action::Focus(Direction::Right)),
            (false, false, Keysym::backslash) => Some(Action::Split(Direction::Right)),
            (false, false, Keysym::minus) => Some(Action::Split(Direction::Down)),
            (false, false, Keysym::Tab) => Some(Action::CycleStack(1)),
            (false, false, Keysym::bracketright) => Some(Action::CycleWorkspace(1)),
            (false, false, Keysym::bracketleft) => Some(Action::CycleWorkspace(-1)),
            (false, false, Keysym::n | Keysym::N) => Some(Action::NewWorkspace),
            (false, false, Keysym::m | Keysym::M) => Some(Action::MarkWindow),
            (false, false, k) if (0x30..=0x39).contains(&k.raw()) => {
                Some(Action::Slot(u8::try_from(k.raw() - 0x30).unwrap()))
            }
            (true, false, k) if (0x30..=0x39).contains(&k.raw()) => {
                Some(Action::PinSlot(u8::try_from(k.raw() - 0x30).unwrap()))
            }
            (true, false, Keysym::parenright) => Some(Action::PinSlot(0)),
            (true, false, Keysym::exclam) => Some(Action::PinSlot(1)),
            (true, false, Keysym::at) => Some(Action::PinSlot(2)),
            (true, false, Keysym::numbersign) => Some(Action::PinSlot(3)),
            (true, false, Keysym::dollar) => Some(Action::PinSlot(4)),
            (true, false, Keysym::percent) => Some(Action::PinSlot(5)),
            (true, false, Keysym::asciicircum) => Some(Action::PinSlot(6)),
            (true, false, Keysym::ampersand) => Some(Action::PinSlot(7)),
            (true, false, Keysym::asterisk) => Some(Action::PinSlot(8)),
            (true, false, Keysym::parenleft) => Some(Action::PinSlot(9)),
            (true, false, Keysym::Return) => Some(Action::RunTerminal),
            (true, false, Keysym::q | Keysym::Q) => Some(Action::Quit),
            (true, false, Keysym::h | Keysym::H) => Some(Action::Move(Direction::Left)),
            (true, false, Keysym::j | Keysym::J) => Some(Action::Move(Direction::Down)),
            (true, false, Keysym::k | Keysym::K) => Some(Action::Move(Direction::Up)),
            (true, false, Keysym::l | Keysym::L) => Some(Action::Move(Direction::Right)),
            (true, false, Keysym::Tab | Keysym::ISO_Left_Tab) => Some(Action::CycleStack(-1)),
            (true, false, Keysym::m | Keysym::M) => Some(Action::SwapMarkedWindow),
            (true, false, Keysym::bracketright) => Some(Action::MoveWindowToWorkspace(1)),
            (true, false, Keysym::bracketleft) => Some(Action::MoveWindowToWorkspace(-1)),
            (true, false, Keysym::w | Keysym::W) => Some(Action::CloseWorkspace),
            (false, true, Keysym::bracketright) => Some(Action::MoveWorkspaceToOutput(1)),
            (false, true, Keysym::bracketleft) => Some(Action::MoveWorkspaceToOutput(-1)),
            (false, true, Keysym::h | Keysym::H) => Some(Action::Resize(Direction::Left)),
            (false, true, Keysym::j | Keysym::J) => Some(Action::Resize(Direction::Down)),
            (false, true, Keysym::k | Keysym::K) => Some(Action::Resize(Direction::Up)),
            (false, true, Keysym::l | Keysym::L) => Some(Action::Resize(Direction::Right)),
            _ => None,
        }
    }

    #[test]
    fn default_bindings_equal_the_legacy_match_over_a_full_sweep() {
        let mut keysyms: Vec<Keysym> = (0x20..0x7f).map(Keysym::new).collect();
        keysyms.extend([
            Keysym::Return,
            Keysym::Tab,
            Keysym::ISO_Left_Tab,
            Keysym::Escape,
            Keysym::Left,
            Keysym::F1,
        ]);
        let mut checked = 0;
        for logo in [false, true] {
            for shift in [false, true] {
                for ctrl in [false, true] {
                    let mods = Modifiers { logo, shift, ctrl };
                    for keysym in &keysyms {
                        let new = resolve(mods, *keysym);
                        let old = legacy(mods, *keysym);
                        let is_new_verb = matches!(
                            new,
                            Some(
                                Action::OpenPicker
                                    | Action::RenameWorkspace
                                    | Action::CaptureWorkspaceRoot
                                    | Action::SetWorkspaceRoot
                                    | Action::OpenSettings
                                    | Action::ToggleHeader
                                    | Action::Lock
                            )
                        );
                        if is_new_verb {
                            assert_eq!(old, None, "{mods:?} {keysym:?}: new verb shadows old");
                            continue;
                        }
                        assert_eq!(new, old, "{mods:?} {keysym:?}");
                        checked += 1;
                    }
                }
            }
        }
        assert!(checked > 700, "sweep too small: {checked}");
    }
}
