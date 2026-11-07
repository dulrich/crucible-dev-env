//! Tests for the binding table and its `binds` overrides.

use super::*;

#[test]
fn bind_target_parses_actions_and_none() {
    assert_eq!(
        "focus left".parse(),
        Ok(BindTarget::Action(Action::Focus(Direction::Left)))
    );
    assert_eq!("none".parse(), Ok(BindTarget::Unbind));
    assert_eq!("NONE".parse(), Ok(BindTarget::Unbind));
    assert!("none now".parse::<BindTarget>().is_err());
    assert!("bogus".parse::<BindTarget>().is_err());
}

#[test]
fn vt_switch_is_not_bindable() {
    let err = Bindings::with_overrides(&[pair("Super+F3", "vt-switch 3")]).unwrap_err();
    assert!(
        matches!(&err, BindError::BadAction { chord, action, .. }
            if chord == "Super+F3" && action == "vt-switch 3"),
        "{err}"
    );
}

fn pair(c: &str, a: &str) -> (String, String) {
    (c.to_string(), a.to_string())
}

fn sup() -> Modifiers {
    Modifiers {
        logo: true,
        ..Default::default()
    }
}

#[test]
fn name_lookup_covers_letters_digits_function_and_named_keys() {
    assert_eq!(keysym_from_name("q"), Some(Keysym::q));
    assert_eq!(keysym_from_name("Q"), Some(Keysym::q));
    assert_eq!(keysym_from_name("7"), Some(Keysym::_7));
    assert_eq!(keysym_from_name("F1"), Some(Keysym::F1));
    assert_eq!(keysym_from_name("f12"), Some(Keysym::F12));
    assert_eq!(keysym_from_name("F13"), None);
    assert_eq!(keysym_from_name("Return"), Some(Keysym::Return));
    assert_eq!(keysym_from_name("Backslash"), Some(Keysym::backslash));
    assert_eq!(keysym_from_name("bracketright"), Some(Keysym::bracketright));
    assert_eq!(keysym_from_name("ISO_Left_Tab"), Some(Keysym::ISO_Left_Tab));
    assert_eq!(keysym_from_name("nonsense"), None);
}

#[test]
fn default_table_has_expected_size() {
    // The exact table is pinned by the `chord` legacy sweep; only guard against emptiness.
    assert!(Bindings::default().len() > 40);
}

#[test]
fn screenshot_binds_are_on_super_print_and_bare_print_is_unbound() {
    let b = Bindings::default();
    let shift = Modifiers {
        shift: true,
        ..sup()
    };
    assert_eq!(b.get(sup(), Keysym::Print), Some(&Action::Screenshot));
    assert_eq!(b.get(shift, Keysym::Print), Some(&Action::Snip));
    assert_eq!(b.get(Modifiers::default(), Keysym::Print), None);
}

#[test]
fn override_replaces_only_the_named_chord() {
    let b = Bindings::with_overrides(&[pair("Super+Q", "quit")]).unwrap();
    let mut expected = Bindings::default();
    expected.map.insert(
        Chord {
            modifiers: sup(),
            keysym: Keysym::q,
        },
        Action::Quit,
    );
    expected.map.insert(
        Chord {
            modifiers: sup(),
            keysym: Keysym::Q,
        },
        Action::Quit,
    );
    assert_eq!(b, expected);
}

#[test]
fn modifier_order_and_case_are_free() {
    let a = Bindings::with_overrides(&[pair("ctrl+SHIFT+super+X", "quit")]).unwrap();
    let b = Bindings::with_overrides(&[pair("Super+Shift+Control+x", "quit")]).unwrap();
    assert_eq!(a, b);
    let mods = Modifiers {
        logo: true,
        shift: true,
        ctrl: true,
    };
    assert_eq!(a.get(mods, Keysym::x), Some(&Action::Quit));
    assert_eq!(a.get(mods, Keysym::X), Some(&Action::Quit));
}

#[test]
fn none_unbinds_both_cases_and_iso_left_tab() {
    let b = Bindings::with_overrides(&[
        pair("Super+Shift+Q", "none"),
        pair("Super+Shift+Tab", "none"),
    ])
    .unwrap();
    let ss = Modifiers {
        logo: true,
        shift: true,
        ctrl: false,
    };
    assert_eq!(b.get(ss, Keysym::q), None);
    assert_eq!(b.get(ss, Keysym::Q), None);
    assert_eq!(b.get(ss, Keysym::Tab), None);
    assert_eq!(b.get(ss, Keysym::ISO_Left_Tab), None);
    // Untouched neighbours survive.
    assert_eq!(b.get(sup(), Keysym::Tab), Some(&Action::CycleStack(1)));
}

#[test]
fn later_entries_win_and_rebind_to_new_actions() {
    let b = Bindings::with_overrides(&[
        pair("Super+Backslash", "split down"),
        pair("Super+Backslash", "split left"),
        pair("Super+F5", "cycle-workspace -2"),
        pair("Super+Shift+Ctrl+F6", "move-window-to-workspace +3"),
    ])
    .unwrap();
    assert_eq!(
        b.get(sup(), Keysym::backslash),
        Some(&Action::Split(Direction::Left))
    );
    assert_eq!(b.get(sup(), Keysym::F5), Some(&Action::CycleWorkspace(-2)));
    let m = Modifiers {
        logo: true,
        shift: true,
        ctrl: true,
    };
    assert_eq!(
        b.get(m, Keysym::F6),
        Some(&Action::MoveWindowToWorkspace(3))
    );
}

#[test]
fn every_action_form_parses() {
    let cases: &[(&str, Action)] = &[
        ("quit", Action::Quit),
        ("close-pane", Action::ClosePane),
        ("close-window", Action::CloseWindow),
        ("set-layout es", Action::SetLayout("es".into())),
        ("run-terminal", Action::RunTerminal),
        ("run firefox", Action::Run("firefox".into())),
        (
            "run-in-terminal htop -d 5",
            Action::RunInTerminal("htop -d 5".into()),
        ),
        ("mark-window", Action::MarkWindow),
        ("swap-marked-window", Action::SwapMarkedWindow),
        ("capture-workspace-root", Action::CaptureWorkspaceRoot),
        ("set-workspace-root", Action::SetWorkspaceRoot),
        ("new-workspace", Action::NewWorkspace),
        ("close-workspace", Action::CloseWorkspace),
        ("close-all-workspaces", Action::CloseAllWorkspaces),
        ("lock", Action::Lock),
        ("screenshot", Action::Screenshot),
        ("snip", Action::Snip),
        (
            "move-workspace-to-output -2",
            Action::MoveWorkspaceToOutput(-2),
        ),
        ("slot 3", Action::Slot(3)),
        ("pin-slot 0", Action::PinSlot(0)),
        ("open-picker", Action::OpenPicker),
        ("open-launcher", Action::OpenLauncher),
        ("open-settings", Action::OpenSettings),
        ("toggle-header", Action::ToggleHeader),
        ("toggle-fullscreen", Action::ToggleFullscreen),
        ("rename-workspace", Action::RenameWorkspace),
        ("focus left", Action::Focus(Direction::Left)),
        ("move right", Action::Move(Direction::Right)),
        ("split up", Action::Split(Direction::Up)),
        ("resize down", Action::Resize(Direction::Down)),
        ("cycle-stack -1", Action::CycleStack(-1)),
        ("cycle-workspace +1", Action::CycleWorkspace(1)),
        (
            "move-window-to-workspace 2",
            Action::MoveWindowToWorkspace(2),
        ),
    ];
    for (text, expected) in cases {
        let b = Bindings::with_overrides(&[pair("Super+F1", text)]).unwrap();
        assert_eq!(b.get(sup(), Keysym::F1), Some(expected), "{text}");
    }
}

#[test]
fn bad_chords_are_rejected_and_named() {
    for chord in [
        "",
        "Super+",
        "Q",
        "Shift+Q",
        "Super+Hyper+Q",
        "Super+Super+Q",
        "Super+Shift+Shift+Q",
        "Super+NoSuchKey",
        "Super+F13",
    ] {
        let err = Bindings::with_overrides(&[pair(chord, "quit")]).unwrap_err();
        assert!(
            matches!(&err, BindError::BadChord { chord: c, .. } if c == chord),
            "{chord}: {err}"
        );
        assert!(err.to_string().contains(&format!("`{chord}`")));
    }
}

#[test]
fn bad_actions_are_rejected_and_named() {
    for action in [
        "",
        "teleport",
        "focus",
        "focus sideways",
        "focus left now",
        "cycle-stack",
        "cycle-stack many",
        "quit now",
        "none now",
        "split 3",
        "slot",
        "slot 10",
        "slot -1",
        "slot x",
        "pin-slot 3 4",
    ] {
        let err = Bindings::with_overrides(&[pair("Super+Q", action)]).unwrap_err();
        assert!(
            matches!(&err, BindError::BadAction { chord, action: a, .. }
                if chord == "Super+Q" && a == action),
            "{action:?}: {err}"
        );
    }
}

#[test]
fn error_in_a_later_entry_rejects_the_whole_set() {
    assert!(
        Bindings::with_overrides(&[pair("Super+Q", "quit"), pair("Super+Z", "bogus")]).is_err()
    );
}

#[test]
fn no_overrides_is_default() {
    assert_eq!(Bindings::with_overrides(&[]).unwrap(), Bindings::default());
}

#[test]
fn run_verb_keeps_the_rest_of_the_text_verbatim() {
    let b =
        Bindings::with_overrides(&[pair("Super+F2", "  run   sh -c  'echo  \"a  b\"'  ")]).unwrap();
    assert_eq!(
        b.get(sup(), Keysym::F2),
        Some(&Action::Run("sh -c  'echo  \"a  b\"'".into()))
    );
}

#[test]
fn bare_run_is_rejected() {
    for text in ["run", "run   ", "run-in-terminal", "run-in-terminal  "] {
        let err = Bindings::with_overrides(&[pair("Super+F2", text)]).unwrap_err();
        assert!(
            matches!(&err, BindError::BadAction { reason, .. } if reason.contains("needs a command")),
            "{text:?}: {err}"
        );
    }
}

#[test]
fn stack_window_is_retired_with_a_pointer_to_run_terminal() {
    let err = Bindings::with_overrides(&[pair("Super+F2", "stack-window")]).unwrap_err();
    assert!(
        matches!(&err, BindError::BadAction { reason, .. }
            if reason.contains("stack-window") && reason.contains("run-terminal")),
        "{err}"
    );
}

#[test]
fn default_super_b_toggles_the_header() {
    let b = Bindings::default();
    for k in [Keysym::b, Keysym::B] {
        assert_eq!(b.get(sup(), k), Some(&Action::ToggleHeader));
    }
    assert_eq!(Action::ToggleHeader.to_string(), "toggle-header");
}

#[test]
fn default_return_chord_opens_the_launcher() {
    let b = Bindings::default();
    assert_eq!(b.get(sup(), Keysym::Return), Some(&Action::OpenLauncher));
}

#[test]
fn keysym_names_are_canonical() {
    assert_eq!(keysym_name(Keysym::w), "W");
    assert_eq!(keysym_name(Keysym::W), "W");
    assert_eq!(keysym_name(Keysym::_7), "7");
    assert_eq!(keysym_name(Keysym::F11), "F11");
    assert_eq!(keysym_name(Keysym::ISO_Left_Tab), "ISO_Left_Tab");
    assert_eq!(keysym_name(Keysym::BackSpace), "BackSpace");
    assert_eq!(keysym_name(Keysym::Page_Up), "Page_Up");
    assert_eq!(keysym_name(Keysym::new(0x1234)), "0x1234");
}

#[test]
fn chord_display_orders_modifiers() {
    let c = Chord {
        modifiers: Modifiers {
            logo: true,
            shift: true,
            ctrl: true,
        },
        keysym: Keysym::h,
    };
    assert_eq!(c.to_string(), "Super+Shift+Ctrl+H");
}

#[test]
fn default_table_round_trips_through_display_and_parse() {
    let defaults = Bindings::default();
    for (chord, action) in &defaults.map {
        if matches!(action, Action::VtSwitch(_)) {
            continue;
        }
        let (chord_text, action_text) = (chord.to_string(), action.to_string());
        let b = Bindings::with_overrides(&[pair(&chord_text, &action_text)]).unwrap();
        assert_eq!(
            b.get(chord.modifiers, chord.keysym),
            Some(action),
            "{chord_text} -> {action_text}"
        );
    }
    for action in [
        Action::Run("echo 50%:a b".into()),
        Action::RunInTerminal("echo 50%:a b".into()),
        Action::CycleStack(-1),
        Action::MoveWindowToWorkspace(1),
        Action::OpenSettings,
        Action::ToggleHeader,
        Action::Resize(Direction::Up),
        Action::CloseAllWorkspaces,
    ] {
        let b = Bindings::with_overrides(&[pair("Super+F1", &action.to_string())]).unwrap();
        assert_eq!(b.get(sup(), Keysym::F1), Some(&action));
    }
    assert_eq!(Action::VtSwitch(3).to_string(), "vt-switch 3");
    assert!(
        !Bindings::default()
            .entries()
            .iter()
            .any(|(_, a)| a == "close-all-workspaces"),
        "close-all-workspaces is unbound by default"
    );
}

#[test]
fn entries_dedupe_sort_and_show_overrides() {
    let e = Bindings::default().entries();
    assert_eq!(e.iter().filter(|(c, _)| c == "Super+W").count(), 1);
    assert!(e.contains(&pair("Super+comma", "open-settings")));
    assert!(!e.iter().any(|(c, _)| c.contains("ISO_Left_Tab")));
    let mut sorted = e.clone();
    sorted.sort();
    assert_eq!(e, sorted);
    let o = Bindings::with_overrides(&[pair("Super+W", "quit"), pair("Super+N", "none")]).unwrap();
    let e = o.entries();
    assert!(e.contains(&pair("Super+W", "quit")));
    assert!(!e.iter().any(|(c, _)| c == "Super+N"));
}

fn sup_shift() -> Modifiers {
    Modifiers {
        shift: true,
        ..sup()
    }
}

#[test]
fn default_slot_chords_bind_digit_and_shifted_symbol() {
    let b = Bindings::default();
    assert_eq!(b.get(sup(), Keysym::_3), Some(&Action::Slot(3)));
    assert_eq!(b.get(sup(), Keysym::_0), Some(&Action::Slot(0)));
    assert_eq!(b.get(sup_shift(), Keysym::_3), Some(&Action::PinSlot(3)));
    assert_eq!(
        b.get(sup_shift(), Keysym::numbersign),
        Some(&Action::PinSlot(3))
    );
    assert_eq!(
        b.get(sup_shift(), Keysym::parenright),
        Some(&Action::PinSlot(0))
    );
    // The shifted symbol is not a plain-Super chord.
    assert_eq!(b.get(sup(), Keysym::numbersign), None);
}

#[test]
fn shift_digit_override_binds_the_shifted_symbol_too() {
    let b = Bindings::with_overrides(&[pair("Super+Shift+5", "quit")]).unwrap();
    assert_eq!(b.get(sup_shift(), Keysym::_5), Some(&Action::Quit));
    assert_eq!(b.get(sup_shift(), Keysym::percent), Some(&Action::Quit));
    let b = Bindings::with_overrides(&[pair("Super+5", "quit")]).unwrap();
    assert_eq!(b.get(sup(), Keysym::_5), Some(&Action::Quit));
    assert_eq!(b.get(sup(), Keysym::percent), None);
    assert_eq!(
        b.get(sup_shift(), Keysym::percent),
        Some(&Action::PinSlot(5))
    );
}

#[test]
fn entries_list_each_pin_chord_once() {
    let e = Bindings::default().entries();
    assert!(e.contains(&pair("Super+Shift+3", "pin-slot 3")));
    assert!(e.contains(&pair("Super+3", "slot 3")));
    for (chord, _) in &e {
        for (name, _) in &SHIFTED_DIGITS {
            assert!(!chord.ends_with(&format!("+{name}")), "{chord}");
        }
    }
}
