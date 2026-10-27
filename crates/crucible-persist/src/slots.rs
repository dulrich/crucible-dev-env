//! Named workspace slots: a pin store mapping slot `0`-`9` to a workspace name.
//!
//! Pins are runtime state, kept in `$XDG_STATE_HOME/cde/slots.kdl` — a **sibling** of the
//! workspace store directory (never inside it, or [`crate::store::list_saved`] would list it as a
//! workspace). One node per pin:
//!
//! ```kdl
//! slot 3 "my-workspace"
//! ```
//!
//! A missing file is an empty store; a malformed one is an error the caller logs before starting
//! empty.

use crate::{PersistError, store};
use kdl::{KdlDocument, KdlNode};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

/// Number of slots (`0`-`9`).
pub const SLOT_COUNT: u8 = 10;

/// File name of the pin store.
const FILE: &str = "slots.kdl";

/// The slot pin store path for a workspace store directory: its parent joined with `slots.kdl`
/// (`…/cde/workspaces` -> `…/cde/slots.kdl`).
#[must_use]
pub fn slots_path(store_dir: &Path) -> PathBuf {
    store_dir
        .parent()
        .map_or_else(|| PathBuf::from(FILE), |p| p.join(FILE))
}

/// Slot (0-9) -> pinned workspace name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Slots {
    pins: BTreeMap<u8, String>,
}

impl Slots {
    /// An empty pin store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The workspace name pinned to slot `n`, if any.
    #[must_use]
    pub fn get(&self, n: u8) -> Option<&str> {
        self.pins.get(&n).map(String::as_str)
    }

    /// The lowest slot pinned to `name`, if any.
    #[must_use]
    pub fn slot_of(&self, name: &str) -> Option<u8> {
        self.pins.iter().find(|(_, v)| *v == name).map(|(k, _)| *k)
    }

    /// Pins `name` to slot `n`, overwriting any previous pin. The same name may sit in several
    /// slots. Slots above 9 are ignored (returns `false`).
    pub fn pin(&mut self, n: u8, name: &str) -> bool {
        if n >= SLOT_COUNT {
            return false;
        }
        self.pins.insert(n, name.to_string());
        true
    }

    /// Makes every slot naming `old` name `new`; returns whether anything changed.
    pub fn rename(&mut self, old: &str, new: &str) -> bool {
        let mut changed = false;
        for name in self.pins.values_mut() {
            if name == old && old != new {
                *name = new.to_string();
                changed = true;
            }
        }
        changed
    }

    /// Encodes as KDL text, one `slot <n> "<name>"` node per pin.
    #[must_use]
    pub fn to_kdl(&self) -> String {
        let mut doc = KdlDocument::new();
        for (n, name) in &self.pins {
            let mut node = KdlNode::new("slot");
            node.push(i128::from(*n));
            node.push(name.as_str());
            doc.nodes_mut().push(node);
        }
        doc.autoformat();
        doc.to_string()
    }

    /// Parses KDL text produced by [`Slots::to_kdl`]. Strict: unknown nodes, bad slot numbers,
    /// non-string names and duplicate slots are errors.
    pub fn from_kdl(text: &str) -> Result<Self, PersistError> {
        let doc = KdlDocument::parse_v2(text).map_err(|e| {
            let (offset, message) = e
                .diagnostics
                .first()
                .map(|d| (d.span.offset(), d.to_string()))
                .unwrap_or((0, e.to_string()));
            let (line, column) = line_col(text, offset);
            PersistError::Parse {
                line,
                column,
                message,
            }
        })?;
        let mut slots = Self::new();
        for node in doc.nodes() {
            let line = line_col(text, node.span().offset()).0;
            if node.name().value() != "slot" {
                return Err(PersistError::UnknownNode {
                    name: node.name().value().to_string(),
                    line,
                });
            }
            let invalid = |field: &str, expected: &str| PersistError::Invalid {
                node: "slot".into(),
                field: field.into(),
                expected: expected.into(),
                line,
            };
            let args: Vec<_> = node
                .entries()
                .iter()
                .filter(|e| e.name().is_none())
                .collect();
            if node.entries().iter().any(|e| e.name().is_some()) {
                return Err(invalid("property", "no properties"));
            }
            let [n, name] = args.as_slice() else {
                return Err(invalid("arguments", "a slot number and a workspace name"));
            };
            let n = n
                .value()
                .as_integer()
                .and_then(|n| u8::try_from(n).ok())
                .filter(|n| *n < SLOT_COUNT)
                .ok_or_else(|| invalid("slot number", "an integer 0-9"))?;
            let name = name
                .value()
                .as_string()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| invalid("workspace name", "a non-empty string"))?;
            if slots.pins.contains_key(&n) {
                return Err(PersistError::Duplicate {
                    name: format!("slot {n}"),
                    line,
                });
            }
            slots.pins.insert(n, name.to_string());
        }
        Ok(slots)
    }

    /// Loads the pin store at `path`. A missing file is an empty store; any other read or parse
    /// failure is an error.
    pub fn load(path: &Path) -> Result<Self, PersistError> {
        match fs::read_to_string(path) {
            Ok(text) => Self::from_kdl(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::new()),
            Err(source) => Err(PersistError::Io {
                op: "read",
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Atomically saves to `path` (parent directory created if missing).
    pub fn save(&self, path: &Path) -> Result<(), PersistError> {
        let dir = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        store::write_atomic(dir, path, &self.to_kdl())
    }
}

/// 1-based (line, column) of a char offset.
pub(crate) fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let mut line = 1;
    let mut col = 1;
    for (i, c) in text.chars().enumerate() {
        if i >= offset {
            break;
        }
        if c == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cde-slots-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn slot_of_finds_the_lowest_pinned_slot() {
        let mut s = Slots::new();
        s.pin(7, "a");
        s.pin(2, "a");
        s.pin(4, "b");
        assert_eq!(s.slot_of("a"), Some(2));
        assert_eq!(s.slot_of("b"), Some(4));
        assert_eq!(s.slot_of("c"), None);
    }

    #[test]
    fn kdl_round_trips() {
        let mut s = Slots::new();
        s.pin(3, "my workspace");
        s.pin(0, "quote \" and \\ back");
        s.pin(9, "my workspace");
        let text = s.to_kdl();
        assert!(text.contains("slot 3 \"my workspace\""), "{text}");
        assert_eq!(Slots::from_kdl(&text).unwrap(), s);
        assert_eq!(Slots::from_kdl("").unwrap(), Slots::new());
    }

    #[test]
    fn malformed_text_is_an_error() {
        for bad in [
            "slot 3",
            "slot 10 \"a\"",
            "slot -1 \"a\"",
            "slot x \"a\"",
            "slot 3 4",
            "slot 3 \"\"",
            "slot 3 \"a\" extra",
            "slot 3 \"a\" k=1",
            "pin 3 \"a\"",
            "slot 3 \"a\"\nslot 3 \"b\"",
            "slot 3 \"a",
        ] {
            assert!(Slots::from_kdl(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn pin_overwrites_and_rejects_out_of_range() {
        let mut s = Slots::new();
        assert_eq!(s.get(2), None);
        assert!(s.pin(2, "a"));
        assert!(s.pin(2, "b"));
        assert_eq!(s.get(2), Some("b"));
        assert!(!s.pin(10, "c"));
        assert_eq!(s.get(10), None);
    }

    #[test]
    fn rename_moves_every_matching_slot_only() {
        let mut s = Slots::new();
        s.pin(1, "old");
        s.pin(2, "keep");
        s.pin(7, "old");
        assert!(s.rename("old", "new"));
        assert_eq!(s.get(1), Some("new"));
        assert_eq!(s.get(7), Some("new"));
        assert_eq!(s.get(2), Some("keep"));
        assert!(!s.rename("old", "newer"));
        assert!(!s.rename("keep", "keep"));
    }

    #[test]
    fn slots_path_is_a_sibling_of_the_store_and_never_listed() {
        let root = temp("sibling");
        let store_dir = root.join("cde").join("workspaces");
        let path = slots_path(&store_dir);
        assert_eq!(path, root.join("cde").join("slots.kdl"));
        assert!(!path.starts_with(&store_dir));

        let mut s = Slots::new();
        s.pin(1, "a");
        s.save(&path).unwrap();
        assert!(path.is_file());
        // A saved workspace beside the pin file is the only thing listed.
        fs::create_dir_all(&store_dir).unwrap();
        fs::write(store_dir.join("note.txt"), "x").unwrap();
        assert!(store::list_saved(&store_dir).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn save_then_load_round_trips_and_missing_is_empty() {
        let root = temp("roundtrip");
        let path = root.join("nested").join("slots.kdl");
        assert_eq!(Slots::load(&path).unwrap(), Slots::new());
        let mut s = Slots::new();
        s.pin(4, "four");
        s.save(&path).unwrap();
        assert_eq!(Slots::load(&path).unwrap(), s);
        // No temp leftovers.
        let leftovers = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .count();
        assert_eq!(leftovers, 0);
        fs::write(&path, "not kdl {{{").unwrap();
        assert!(Slots::load(&path).is_err());
        let _ = fs::remove_dir_all(&root);
    }
}
