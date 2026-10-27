//! The data-driven binding table: [`Chord`] → [`Action`], with config-string parsing.
//!
//! [`Bindings::default`] is CDE's built-in `Super`-leader table (see the table in
//! [`crate::chord`]). [`Bindings::with_overrides`] applies the `(chord, action)` string pairs from
//! the `binds` block of the config file on top of it, replacing only the chords they name.
//!
//! ## Chord grammar
//!
//! `Super[+Shift][+Ctrl]+<keysym name>` — modifiers in any order, case-insensitively (`ctrl` and
//! `control` are synonyms), `Super` mandatory. The keysym name is looked up case-insensitively in a
//! curated table ([`keysym_from_name`]): `xkeysym` has a keysym-to-name API but no name-to-keysym
//! one, so the table covers every keysym the default bindings use plus `a`–`z`, `0`–`9`,
//! `F1`–`F12` and a handful of common navigation/punctuation keys. A letter binds **both cases**
//! (so the chord also works under Caps Lock), and `Shift+Tab` also binds `ISO_Left_Tab`, the
//! keysym many layouts deliver for it — mirroring the default table.
//!
//! ## Action grammar
//!
//! `quit`, `close-pane`, `run-terminal`, `run <command…>`, `new-workspace`, `open-picker`, `open-launcher`, `open-settings`,
//! `rename-workspace`, `focus|move|split|resize <left|right|up|down>`, `cycle-stack <±n>`,
//! `cycle-workspace <±n>`, `move-window-to-workspace <±n>`, `slot <0-9>`, `pin-slot <0-9>`, and `none`
//! (unbind the chord). `Super+Shift+<digit>` also binds the digit's US shifted symbol (`exclam`, ...),
//! since the compositor delivers the modified keysym.

use std::collections::HashMap;

use crucible_layout::Direction;
use xkeysym::Keysym;

use crate::action::Action;
use crate::chord::Modifiers;

/// A keyboard chord: modifier set plus keysym.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chord {
    /// The modifiers that must be held (exactly; extra modifiers do not match).
    pub modifiers: Modifiers,
    /// The keysym delivered with them.
    pub keysym: Keysym,
}

/// The canonical display name of a keysym: letters uppercase, digits, `F<n>`, the canonical X11
/// spelling of every named key, else `0x<hex>`. Inverse of [`keysym_from_name`] (up to letter case).
#[must_use]
pub fn keysym_name(keysym: Keysym) -> String {
    let raw = keysym.raw();
    if let Ok(byte) = u8::try_from(raw) {
        if byte.is_ascii_alphabetic() {
            return char::from(byte.to_ascii_uppercase()).to_string();
        }
        if byte.is_ascii_digit() {
            return char::from(byte).to_string();
        }
    }
    let f1 = Keysym::F1.raw();
    if (f1..f1 + 12).contains(&raw) {
        return format!("F{}", raw - f1 + 1);
    }
    NAMED
        .iter()
        .find(|(_, k)| *k == keysym)
        .map_or_else(|| format!("0x{raw:x}"), |(n, _)| (*n).to_string())
}

impl std::fmt::Display for Chord {
    /// `Super[+Shift][+Ctrl]+<Key>`, parseable by the `binds` chord grammar.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Super")?;
        if self.modifiers.shift {
            f.write_str("+Shift")?;
        }
        if self.modifiers.ctrl {
            f.write_str("+Ctrl")?;
        }
        write!(f, "+{}", keysym_name(self.keysym))
    }
}

/// Why a `binds` entry was rejected. Every variant names the offending entry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BindError {
    /// The chord string does not follow the chord grammar.
    #[error("bad chord `{chord}`: {reason}")]
    BadChord {
        /// The chord string as written.
        chord: String,
        /// What is wrong with it.
        reason: String,
    },
    /// The action string does not follow the action grammar.
    #[error("bad action `{action}` for chord `{chord}`: {reason}")]
    BadAction {
        /// The chord the action was bound to.
        chord: String,
        /// The action string as written.
        action: String,
        /// What is wrong with it.
        reason: String,
    },
}

/// The chord → [`Action`] table consulted by [`crate::chord::resolve`].
///
/// `Super+Return` binds `Action::OpenLauncher`; `Super+Shift+Return` binds `Action::RunTerminal` (the compositor chooses the
/// terminal program).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bindings {
    map: HashMap<Chord, Action>,
}

impl Bindings {
    /// Looks up the action bound to exactly this chord.
    #[must_use]
    pub fn get(&self, modifiers: Modifiers, keysym: Keysym) -> Option<&Action> {
        self.map.get(&Chord { modifiers, keysym })
    }

    /// Number of bound chords.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// True when no chord is bound.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The effective table as `(chord display, action display)` pairs, de-duplicated (both cases
    /// of a letter, and `ISO_Left_Tab` beside `Shift+Tab`, collapse) and sorted by chord display.
    #[must_use]
    pub fn entries(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .map
            .iter()
            .filter(|(chord, action)| {
                // `ISO_Left_Tab` is the layout-delivered alias of `Shift+Tab`: hide it when the
                // `Tab` chord carries the same action.
                !(chord.keysym == Keysym::ISO_Left_Tab
                    && self.map.get(&Chord {
                        keysym: Keysym::Tab,
                        ..**chord
                    }) == Some(*action))
                    // A shifted digit symbol is the layout-delivered alias of `Shift+<digit>`.
                    && !(chord.modifiers.shift
                        && digit_of_shifted_symbol(chord.keysym).is_some_and(|digit| {
                            self.map.get(&Chord {
                                keysym: digit,
                                ..**chord
                            }) == Some(*action)
                        }))
            })
            .map(|(chord, action)| (chord.to_string(), action.to_string()))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// The default table with each `(chord, action)` entry applied in order; later entries win.
    /// An action of `none` unbinds the chord.
    ///
    /// # Errors
    /// The first malformed entry, as a [`BindError`] naming it.
    pub fn with_overrides(overrides: &[(String, String)]) -> Result<Self, BindError> {
        let mut bindings = Self::default();
        for (chord_text, action_text) in overrides {
            let chord = parse_chord(chord_text)?;
            let action = parse_action(chord_text, action_text)?;
            for keysym in aliases(chord.modifiers, chord.keysym) {
                let key = Chord { keysym, ..chord };
                match &action {
                    Some(action) => {
                        bindings.map.insert(key, action.clone());
                    }
                    None => {
                        bindings.map.remove(&key);
                    }
                }
            }
        }
        Ok(bindings)
    }
}

impl Default for Bindings {
    /// CDE's built-in table, identical to the pre-config `match` (see [`crate::chord`] docs).
    fn default() -> Self {
        let sup = Modifiers {
            logo: true,
            shift: false,
            ctrl: false,
        };
        let sup_shift = Modifiers { shift: true, ..sup };
        let sup_ctrl = Modifiers { ctrl: true, ..sup };

        let mut map = HashMap::new();
        let mut bind = |modifiers: Modifiers, keysyms: &[Keysym], action: Action| {
            for keysym in keysyms {
                map.insert(
                    Chord {
                        modifiers,
                        keysym: *keysym,
                    },
                    action.clone(),
                );
            }
        };

        // ---- plain Super (note: focus h/j/k/l are lowercase-only, as before) ----
        bind(sup, &[Keysym::Return], Action::OpenLauncher);
        bind(sup, &[Keysym::comma], Action::OpenSettings);
        bind(sup, &[Keysym::b, Keysym::B], Action::ToggleHeader);
        bind(sup, &[Keysym::w, Keysym::W], Action::ClosePane);
        bind(sup, &[Keysym::h], Action::Focus(Direction::Left));
        bind(sup, &[Keysym::j], Action::Focus(Direction::Down));
        bind(sup, &[Keysym::k], Action::Focus(Direction::Up));
        bind(sup, &[Keysym::l], Action::Focus(Direction::Right));
        bind(sup, &[Keysym::backslash], Action::Split(Direction::Right));
        bind(sup, &[Keysym::minus], Action::Split(Direction::Down));
        bind(sup, &[Keysym::Tab], Action::CycleStack(1));
        bind(sup, &[Keysym::bracketright], Action::CycleWorkspace(1));
        bind(sup, &[Keysym::bracketleft], Action::CycleWorkspace(-1));
        bind(sup, &[Keysym::n, Keysym::N], Action::NewWorkspace);
        bind(sup, &[Keysym::p, Keysym::P], Action::OpenPicker);

        // ---- Super+Shift ----
        bind(sup_shift, &[Keysym::Return], Action::RunTerminal);
        bind(sup_shift, &[Keysym::q, Keysym::Q], Action::Quit);
        bind(sup_shift, &[Keysym::r, Keysym::R], Action::RenameWorkspace);
        bind(
            sup_shift,
            &[Keysym::h, Keysym::H],
            Action::Move(Direction::Left),
        );
        bind(
            sup_shift,
            &[Keysym::j, Keysym::J],
            Action::Move(Direction::Down),
        );
        bind(
            sup_shift,
            &[Keysym::k, Keysym::K],
            Action::Move(Direction::Up),
        );
        bind(
            sup_shift,
            &[Keysym::l, Keysym::L],
            Action::Move(Direction::Right),
        );
        bind(
            sup_shift,
            &[Keysym::Tab, Keysym::ISO_Left_Tab],
            Action::CycleStack(-1),
        );
        bind(
            sup_shift,
            &[Keysym::bracketright],
            Action::MoveWindowToWorkspace(1),
        );
        bind(
            sup_shift,
            &[Keysym::bracketleft],
            Action::MoveWindowToWorkspace(-1),
        );
        bind(sup_shift, &[Keysym::w, Keysym::W], Action::CloseWorkspace);

        // ---- workspace slots: Super+<d> switches, Super+Shift+<d> pins ----
        for (d, shifted) in SHIFTED_DIGITS.iter().enumerate() {
            let n = u8::try_from(d).expect("ten digits");
            bind(sup, &[Keysym::new(0x30 + u32::from(n))], Action::Slot(n));
            bind(
                sup_shift,
                &[Keysym::new(0x30 + u32::from(n)), shifted.1],
                Action::PinSlot(n),
            );
        }

        // ---- Super+Ctrl ----
        bind(
            sup_ctrl,
            &[Keysym::bracketright],
            Action::MoveWorkspaceToOutput(1),
        );
        bind(
            sup_ctrl,
            &[Keysym::bracketleft],
            Action::MoveWorkspaceToOutput(-1),
        );
        bind(
            sup_ctrl,
            &[Keysym::h, Keysym::H],
            Action::Resize(Direction::Left),
        );
        bind(
            sup_ctrl,
            &[Keysym::j, Keysym::J],
            Action::Resize(Direction::Down),
        );
        bind(
            sup_ctrl,
            &[Keysym::k, Keysym::K],
            Action::Resize(Direction::Up),
        );
        bind(
            sup_ctrl,
            &[Keysym::l, Keysym::L],
            Action::Resize(Direction::Right),
        );

        Self { map }
    }
}

/// Every keysym a chord written with `keysym` should bind: both cases of a letter, and
/// `ISO_Left_Tab` alongside `Shift+Tab`.
fn aliases(modifiers: Modifiers, keysym: Keysym) -> Vec<Keysym> {
    let raw = keysym.raw();
    if let Ok(byte) = u8::try_from(raw)
        && byte.is_ascii_alphabetic()
    {
        return vec![
            Keysym::new(u32::from(byte.to_ascii_lowercase())),
            Keysym::new(u32::from(byte.to_ascii_uppercase())),
        ];
    }
    if modifiers.shift && keysym == Keysym::Tab {
        return vec![Keysym::Tab, Keysym::ISO_Left_Tab];
    }
    if modifiers.shift
        && let Some(shifted) = shifted_digit_symbol(keysym)
    {
        return vec![keysym, shifted];
    }
    vec![keysym]
}

/// The digits `0`-`9` (index = digit) paired with their US shifted symbols.
const SHIFTED_DIGITS: [(&str, Keysym); 10] = [
    ("parenright", Keysym::parenright),
    ("exclam", Keysym::exclam),
    ("at", Keysym::at),
    ("numbersign", Keysym::numbersign),
    ("dollar", Keysym::dollar),
    ("percent", Keysym::percent),
    ("asciicircum", Keysym::asciicircum),
    ("ampersand", Keysym::ampersand),
    ("asterisk", Keysym::asterisk),
    ("parenleft", Keysym::parenleft),
];

/// The US shifted symbol of a digit keysym (`3` -> `numbersign`), else `None`.
fn shifted_digit_symbol(keysym: Keysym) -> Option<Keysym> {
    let digit = keysym.raw().checked_sub(0x30).filter(|d| *d < 10)?;
    Some(SHIFTED_DIGITS[usize::try_from(digit).ok()?].1)
}

/// The digit keysym a US shifted symbol stands for (`numbersign` -> `3`), else `None`.
fn digit_of_shifted_symbol(keysym: Keysym) -> Option<Keysym> {
    let digit = SHIFTED_DIGITS.iter().position(|(_, k)| *k == keysym)?;
    Some(Keysym::new(0x30 + u32::try_from(digit).ok()?))
}

/// Canonical X11 spellings of the named keys; lookup compares lowercased, display prints these.
const NAMED: &[(&str, Keysym)] = &[
    ("Return", Keysym::Return),
    ("Tab", Keysym::Tab),
    ("ISO_Left_Tab", Keysym::ISO_Left_Tab),
    ("backslash", Keysym::backslash),
    ("minus", Keysym::minus),
    ("bracketright", Keysym::bracketright),
    ("bracketleft", Keysym::bracketleft),
    ("space", Keysym::space),
    ("Escape", Keysym::Escape),
    ("BackSpace", Keysym::BackSpace),
    ("Delete", Keysym::Delete),
    ("Home", Keysym::Home),
    ("End", Keysym::End),
    ("Page_Up", Keysym::Page_Up),
    ("Page_Down", Keysym::Page_Down),
    ("Left", Keysym::Left),
    ("Right", Keysym::Right),
    ("Up", Keysym::Up),
    ("Down", Keysym::Down),
    ("equal", Keysym::equal),
    ("plus", Keysym::plus),
    ("comma", Keysym::comma),
    ("period", Keysym::period),
    ("slash", Keysym::slash),
    ("semicolon", Keysym::semicolon),
    ("apostrophe", Keysym::apostrophe),
    ("grave", Keysym::grave),
    ("Print", Keysym::Print),
    ("exclam", Keysym::exclam),
    ("at", Keysym::at),
    ("numbersign", Keysym::numbersign),
    ("dollar", Keysym::dollar),
    ("percent", Keysym::percent),
    ("asciicircum", Keysym::asciicircum),
    ("ampersand", Keysym::ampersand),
    ("asterisk", Keysym::asterisk),
    ("parenleft", Keysym::parenleft),
    ("parenright", Keysym::parenright),
];
/// Looks up a keysym by X11 name, case-insensitively, in the curated table (see module docs).
/// Single letters resolve to the lowercase keysym.
#[must_use]
pub fn keysym_from_name(name: &str) -> Option<Keysym> {
    let lower = name.to_ascii_lowercase();
    if let [byte] = lower.as_bytes()
        && (byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return Some(Keysym::new(u32::from(*byte)));
    }
    if let Some(n) = lower.strip_prefix('f')
        && let Ok(n) = n.parse::<u32>()
        && (1..=12).contains(&n)
    {
        return Some(Keysym::new(Keysym::F1.raw() + n - 1));
    }

    NAMED
        .iter()
        .find(|(n, _)| n.to_ascii_lowercase() == lower)
        .map(|(_, keysym)| *keysym)
}

fn bad_chord(chord: &str, reason: impl Into<String>) -> BindError {
    BindError::BadChord {
        chord: chord.to_string(),
        reason: reason.into(),
    }
}

/// Parses `Super[+Shift][+Ctrl]+<key>` (see module docs).
fn parse_chord(text: &str) -> Result<Chord, BindError> {
    let mut parts: Vec<&str> = text.split('+').map(str::trim).collect();
    let key = parts.pop().filter(|k| !k.is_empty());
    let Some(key) = key else {
        return Err(bad_chord(text, "missing key name"));
    };
    let mut modifiers = Modifiers::default();
    for part in parts {
        let slot = match part.to_ascii_lowercase().as_str() {
            "super" => &mut modifiers.logo,
            "shift" => &mut modifiers.shift,
            "ctrl" | "control" => &mut modifiers.ctrl,
            _ => return Err(bad_chord(text, format!("unknown modifier `{part}`"))),
        };
        if *slot {
            return Err(bad_chord(text, format!("modifier `{part}` repeated")));
        }
        *slot = true;
    }
    if !modifiers.logo {
        return Err(bad_chord(text, "chords must include Super"));
    }
    let keysym = keysym_from_name(key)
        .ok_or_else(|| bad_chord(text, format!("unknown key name `{key}`")))?;
    Ok(Chord { modifiers, keysym })
}

/// Parses a bare action string (the `binds` action grammar); `Ok(None)` is `none`. Errors carry
/// an empty chord.
///
/// # Errors
/// [`BindError::BadAction`] for an unknown or malformed action.
pub fn parse_action_text(text: &str) -> Result<Option<Action>, BindError> {
    parse_action("", text)
}

/// Parses an action string; `Ok(None)` is `none` (unbind).
fn parse_action(chord: &str, text: &str) -> Result<Option<Action>, BindError> {
    let bad = |reason: String| BindError::BadAction {
        chord: chord.to_string(),
        action: text.to_string(),
        reason,
    };
    let words: Vec<&str> = text.split_whitespace().collect();
    let direction = |arg: Option<&&str>| -> Result<Direction, BindError> {
        match arg.map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("left") => Ok(Direction::Left),
            Some("right") => Ok(Direction::Right),
            Some("up") => Ok(Direction::Up),
            Some("down") => Ok(Direction::Down),
            Some(other) => Err(bad(format!(
                "`{other}` is not a direction (left, right, up, down)"
            ))),
            None => Err(bad("expected a direction (left, right, up, down)".into())),
        }
    };
    let delta = |arg: Option<&&str>| -> Result<i64, BindError> {
        let arg = arg.ok_or_else(|| bad("expected a signed integer step".into()))?;
        arg.parse::<i64>()
            .map_err(|_| bad(format!("`{arg}` is not an integer")))
    };
    let slot_index = |arg: Option<&&str>| -> Result<u8, BindError> {
        let arg = arg.ok_or_else(|| bad("expected a slot number 0-9".into()))?;
        match arg.parse::<u8>() {
            Ok(n) if n <= 9 => Ok(n),
            Ok(_) => Err(bad(format!("slot `{arg}` is out of range (0-9)"))),
            Err(_) => Err(bad(format!("`{arg}` is not a slot number 0-9"))),
        }
    };
    let nullary = |action: Action| -> Result<Option<Action>, BindError> {
        match words.get(1) {
            None => Ok(Some(action)),
            Some(extra) => Err(bad(format!("unexpected argument `{extra}`"))),
        }
    };
    let unary = |action: Action| -> Result<Option<Action>, BindError> {
        match words.get(2) {
            None => Ok(Some(action)),
            Some(extra) => Err(bad(format!("unexpected argument `{extra}`"))),
        }
    };

    let Some(verb) = words.first() else {
        return Err(bad("empty action".into()));
    };
    match verb.to_ascii_lowercase().as_str() {
        "none" => nullary(Action::Quit).map(|_| None),
        "quit" => nullary(Action::Quit),
        "close-pane" => nullary(Action::ClosePane),
        "run-terminal" => nullary(Action::RunTerminal),
        "run" => {
            // The command is the rest of the original text after the verb, verbatim.
            let rest = text.trim().get(verb.len()..).unwrap_or("").trim();
            if rest.is_empty() {
                Err(bad("`run` needs a command".into()))
            } else {
                Ok(Some(Action::Run(rest.to_string())))
            }
        }
        "run-in-terminal" => {
            let rest = text.trim().get(verb.len()..).unwrap_or("").trim();
            if rest.is_empty() {
                Err(bad("`run-in-terminal` needs a command".into()))
            } else {
                Ok(Some(Action::RunInTerminal(rest.to_string())))
            }
        }
        "stack-window" => Err(bad(
            "`stack-window` was retired: every launch already stacks into the focused pane; use `run-terminal`".into(),
        )),
        "new-workspace" => nullary(Action::NewWorkspace),
        "open-picker" => nullary(Action::OpenPicker),
        "open-launcher" => nullary(Action::OpenLauncher),
        "open-settings" => nullary(Action::OpenSettings),
        "toggle-header" => nullary(Action::ToggleHeader),
        "rename-workspace" => nullary(Action::RenameWorkspace),
        "focus" => unary(Action::Focus(direction(words.get(1))?)),
        "move" => unary(Action::Move(direction(words.get(1))?)),
        "split" => unary(Action::Split(direction(words.get(1))?)),
        "resize" => unary(Action::Resize(direction(words.get(1))?)),
        "cycle-stack" => unary(Action::CycleStack(delta(words.get(1))?)),
        "cycle-workspace" => unary(Action::CycleWorkspace(delta(words.get(1))?)),
        "move-window-to-workspace" => unary(Action::MoveWindowToWorkspace(delta(words.get(1))?)),
        "close-workspace" => nullary(Action::CloseWorkspace),
        "close-all-workspaces" => nullary(Action::CloseAllWorkspaces),
        "move-workspace-to-output" => unary(Action::MoveWorkspaceToOutput(delta(words.get(1))?)),
        "slot" => unary(Action::Slot(slot_index(words.get(1))?)),
        "pin-slot" => unary(Action::PinSlot(slot_index(words.get(1))?)),
        other => Err(bad(format!("unknown action `{other}`"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_text_round_trips_through_display() {
        let a = parse_action_text("focus left").unwrap().unwrap();
        assert_eq!(a.to_string(), "focus left");
        assert_eq!(parse_action_text(&a.to_string()).unwrap(), Some(a));
        assert_eq!(parse_action_text("none").unwrap(), None);
        assert!(parse_action_text("bogus").is_err());
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
            ("run-terminal", Action::RunTerminal),
            ("run firefox", Action::Run("firefox".into())),
            (
                "run-in-terminal htop -d 5",
                Action::RunInTerminal("htop -d 5".into()),
            ),
            ("new-workspace", Action::NewWorkspace),
            ("close-workspace", Action::CloseWorkspace),
            ("close-all-workspaces", Action::CloseAllWorkspaces),
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
        let b = Bindings::with_overrides(&[pair("Super+F2", "  run   sh -c  'echo  \"a  b\"'  ")])
            .unwrap();
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
        let o =
            Bindings::with_overrides(&[pair("Super+W", "quit"), pair("Super+N", "none")]).unwrap();
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
}
