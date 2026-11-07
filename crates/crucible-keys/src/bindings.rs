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
//! Every verb of the [`Action`] verb table (its `FromStr`; see [`crate::action`]), plus `none`
//! (unbind the chord) via [`BindTarget`]. `Super+Shift+<digit>` also binds the digit's US shifted symbol (`exclam`, ...),
//! since the compositor delivers the modified keysym.

use std::collections::HashMap;

use crucible_layout::Direction;
use xkeysym::Keysym;

use std::str::FromStr;

use crate::action::{Action, ActionParseError};
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
            let target = parse_target(chord_text, action_text)?;
            for keysym in aliases(chord.modifiers, chord.keysym) {
                let key = Chord { keysym, ..chord };
                match &target {
                    BindTarget::Action(action) => {
                        bindings.map.insert(key, action.clone());
                    }
                    BindTarget::Unbind => {
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
        let sup_ctrl_shift = Modifiers {
            ctrl: true,
            shift: true,
            ..sup
        };

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
        bind(sup, &[Keysym::f, Keysym::F], Action::ToggleFullscreen);
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
        bind(sup, &[Keysym::e, Keysym::E], Action::SetLayout("us".into()));
        bind(sup, &[Keysym::s, Keysym::S], Action::SetLayout("es".into()));
        bind(sup, &[Keysym::m, Keysym::M], Action::MarkWindow);
        bind(sup, &[Keysym::p, Keysym::P], Action::OpenPicker);
        bind(sup, &[Keysym::Escape], Action::Lock);
        bind(sup, &[Keysym::Print], Action::Screenshot);

        // ---- Super+Shift ----
        bind(sup_shift, &[Keysym::Return], Action::RunTerminal);
        bind(sup_shift, &[Keysym::q, Keysym::Q], Action::Quit);
        bind(sup_shift, &[Keysym::r, Keysym::R], Action::RenameWorkspace);
        bind(sup_shift, &[Keysym::m, Keysym::M], Action::SwapMarkedWindow);
        bind(
            sup_shift,
            &[Keysym::d, Keysym::D],
            Action::CaptureWorkspaceRoot,
        );
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
        bind(sup_shift, &[Keysym::c, Keysym::C], Action::CloseWindow);
        bind(sup_shift, &[Keysym::Print], Action::Snip);

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
        // ---- Super+Ctrl+Shift ----
        bind(
            sup_ctrl_shift,
            &[Keysym::d, Keysym::D],
            Action::SetWorkspaceRoot,
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

/// What a `binds` entry binds its chord to: an [`Action`] (the [`Action`] `FromStr` grammar), or
/// `none`, which unbinds the chord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindTarget {
    /// Bind the chord to this action.
    Action(Action),
    /// `none`: remove the chord's binding.
    Unbind,
}

impl FromStr for BindTarget {
    type Err = ActionParseError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut words = text.split_whitespace();
        if words
            .next()
            .is_some_and(|verb| verb.eq_ignore_ascii_case("none"))
        {
            return match words.next() {
                None => Ok(Self::Unbind),
                Some(extra) => Err(ActionParseError(format!("unexpected argument `{extra}`"))),
            };
        }
        text.parse().map(Self::Action)
    }
}

/// Parses a `binds` action string, naming `chord` in the error.
fn parse_target(chord: &str, text: &str) -> Result<BindTarget, BindError> {
    text.parse()
        .map_err(|ActionParseError(reason)| BindError::BadAction {
            chord: chord.to_string(),
            action: text.to_string(),
            reason,
        })
}

#[cfg(test)]
mod tests;
