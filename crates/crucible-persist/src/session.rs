//! The session record: which workspaces were open on which output, and where focus was.
//!
//! Kept in `$XDG_STATE_HOME/cde/session.kdl` — a **sibling** of the workspace store directory
//! (never inside it, or [`crate::store::list_saved`] would list it as a workspace):
//!
//! ```kdl
//! focused-output "DP-1"
//! output "DP-1" "notes" "build" focused="build"
//! output "HDMI-A-1" "logs"
//! ```
//!
//! Each `output` node names a connector, then its open workspace names in stack order; the
//! optional `focused` property is that output's active workspace. [`plan_restore`] turns a record
//! into the startup restore plan.

use crate::{PersistError, slots, store};
use kdl::{KdlDocument, KdlEntry, KdlNode};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

/// File name of the session record.
const FILE: &str = "session.kdl";

/// The session record path for a workspace store directory: its parent joined with
/// `session.kdl` (`…/cde/workspaces` -> `…/cde/session.kdl`).
#[must_use]
pub fn session_path(store_dir: &Path) -> PathBuf {
    store_dir
        .parent()
        .map_or_else(|| PathBuf::from(FILE), |p| p.join(FILE))
}

/// One output's open workspaces.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutputSession {
    /// Connector name (e.g. `DP-1`).
    pub output: String,
    /// Open workspace names, in stack order.
    pub workspaces: Vec<String>,
    /// The output's active workspace.
    pub focused: Option<String>,
    /// The output's saved header-strip preference, `None` unknown. Encoded as
    /// `header="shown"|"hidden"`; an unrecognised value reads as `None`.
    pub header: Option<HeaderPref>,
}

/// An output's saved header-strip preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderPref {
    /// The strip is shown.
    Shown,
    /// The strip is hidden.
    Hidden,
}

impl HeaderPref {
    /// The `header=` value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            HeaderPref::Shown => "shown",
            HeaderPref::Hidden => "hidden",
        }
    }

    /// Parses a `header=` value; `None` for anything unrecognised.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "shown" => Some(HeaderPref::Shown),
            "hidden" => Some(HeaderPref::Hidden),
            _ => None,
        }
    }

    /// The preference for a strip that is (`true`) or is not visible.
    #[must_use]
    pub fn from_visible(visible: bool) -> Self {
        if visible {
            HeaderPref::Shown
        } else {
            HeaderPref::Hidden
        }
    }

    /// Whether the strip is shown.
    #[must_use]
    pub fn is_shown(self) -> bool {
        self == HeaderPref::Shown
    }
}

/// The open workspaces per output plus the focused output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Session {
    /// Outputs in record order.
    pub outputs: Vec<OutputSession>,
    /// Connector name of the focused output.
    pub focused_output: Option<String>,
}

impl Session {
    /// Encodes as KDL text.
    #[must_use]
    pub fn to_kdl(&self) -> String {
        let mut doc = KdlDocument::new();
        if let Some(name) = &self.focused_output {
            let mut node = KdlNode::new("focused-output");
            node.push(name.as_str());
            doc.nodes_mut().push(node);
        }
        for out in &self.outputs {
            let mut node = KdlNode::new("output");
            node.push(out.output.as_str());
            for ws in &out.workspaces {
                node.push(ws.as_str());
            }
            if let Some(f) = &out.focused {
                node.push(KdlEntry::new_prop("focused", f.as_str()));
            }
            if let Some(pref) = out.header {
                node.push(KdlEntry::new_prop("header", pref.as_str()));
            }
            doc.nodes_mut().push(node);
        }
        doc.autoformat();
        doc.to_string()
    }

    /// Parses KDL text produced by [`Session::to_kdl`]. Strict: unknown nodes or properties,
    /// non-string or empty names, and duplicate outputs are errors.
    pub fn from_kdl(text: &str) -> Result<Self, PersistError> {
        let doc = KdlDocument::parse_v2(text).map_err(|e| {
            let (offset, message) = e
                .diagnostics
                .first()
                .map(|d| (d.span.offset(), d.to_string()))
                .unwrap_or((0, e.to_string()));
            let (line, column) = slots::line_col(text, offset);
            PersistError::Parse {
                line,
                column,
                message,
            }
        })?;
        let mut session = Self::default();
        for node in doc.nodes() {
            let line = slots::line_col(text, node.span().offset()).0;
            let kind = node.name().value();
            let invalid = |field: &str, expected: &str| PersistError::Invalid {
                node: kind.into(),
                field: field.into(),
                expected: expected.into(),
                line,
            };
            let mut args = Vec::new();
            for e in node.entries().iter().filter(|e| e.name().is_none()) {
                let s = e
                    .value()
                    .as_string()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| invalid("argument", "a non-empty string"))?;
                args.push(s.to_string());
            }
            match kind {
                "focused-output" => {
                    if node.entries().iter().any(|e| e.name().is_some()) {
                        return Err(invalid("property", "no properties"));
                    }
                    let [name] = args.as_slice() else {
                        return Err(invalid("arguments", "one output name"));
                    };
                    if session.focused_output.is_some() {
                        return Err(PersistError::Duplicate {
                            name: "focused-output".into(),
                            line,
                        });
                    }
                    session.focused_output = Some(name.clone());
                }
                "output" => {
                    let mut focused = None;
                    let (mut header, mut header_seen) = (None, false);
                    for e in node.entries().iter() {
                        let Some(key) = e.name() else { continue };
                        if key.value() == "header" && !header_seen {
                            // Lenient: an unknown or non-string value is ignored, never fatal.
                            header_seen = true;
                            header = e.value().as_string().and_then(HeaderPref::parse);
                            continue;
                        }
                        if key.value() != "focused" || focused.is_some() {
                            return Err(invalid("property", "only one focused=\"<workspace>\""));
                        }
                        focused = Some(
                            e.value()
                                .as_string()
                                .filter(|s| !s.is_empty())
                                .ok_or_else(|| invalid("focused", "a non-empty string"))?
                                .to_string(),
                        );
                    }
                    let Some((output, workspaces)) = args.split_first() else {
                        return Err(invalid("arguments", "an output name"));
                    };
                    if session.outputs.iter().any(|o| &o.output == output) {
                        return Err(PersistError::Duplicate {
                            name: format!("output {output}"),
                            line,
                        });
                    }
                    session.outputs.push(OutputSession {
                        output: output.clone(),
                        workspaces: workspaces.to_vec(),
                        focused,
                        header,
                    });
                }
                other => {
                    return Err(PersistError::UnknownNode {
                        name: other.to_string(),
                        line,
                    });
                }
            }
        }
        Ok(session)
    }

    /// Loads the record at `path`. A missing file is `Ok(None)`; any other read or parse failure is
    /// an error.
    pub fn load(path: &Path) -> Result<Option<Self>, PersistError> {
        match fs::read_to_string(path) {
            Ok(text) => Self::from_kdl(&text).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
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

/// The header preferences recorded in `record`, by connector name, for outputs that carry one.
/// Read from the raw record, independent of [`plan_restore`]. Pure.
#[must_use]
pub fn header_prefs(record: &Session) -> BTreeMap<String, HeaderPref> {
    record
        .outputs
        .iter()
        .filter_map(|o| Some((o.output.clone(), o.header?)))
        .collect()
}

/// Stamps each of `outputs` with its preference from `prefs`, then appends a workspace-less
/// record for every preference whose connector is not among `outputs` (an unplugged monitor), so
/// a write never drops it. Pure.
#[must_use]
pub fn with_header_prefs(
    mut outputs: Vec<OutputSession>,
    prefs: &BTreeMap<String, HeaderPref>,
) -> Vec<OutputSession> {
    for out in &mut outputs {
        out.header = prefs.get(&out.output).copied();
    }
    for (name, pref) in prefs {
        if !outputs.iter().any(|o| &o.output == name) {
            outputs.push(OutputSession {
                output: name.clone(),
                header: Some(*pref),
                ..OutputSession::default()
            });
        }
    }
    outputs
}

/// Decides the startup restore plan from `record`, the workspace names `saved` on disk and the
/// `connected` output connector names (in output order).
///
/// Workspaces not saved on disk (or repeated) are dropped. An output not connected falls back to
/// the first connected output: its workspaces are appended there, and its focus is used only if
/// that output has none. The returned plan names only connected outputs, each with at least one
/// workspace; its focus fields name planned workspaces/outputs or are `None`. Nothing restorable
/// (or no output) is `None`, meaning a cold start.
#[must_use]
pub fn plan_restore(
    record: &Session,
    saved: &HashSet<String>,
    connected: &[String],
) -> Option<Session> {
    let first = connected.first()?;
    let target = |name: &str| -> String {
        if connected.iter().any(|c| c == name) {
            name.to_string()
        } else {
            first.clone()
        }
    };
    let mut seen: HashSet<&str> = HashSet::new();
    let mut plan = Session::default();
    for out in &record.outputs {
        let names: Vec<String> = out
            .workspaces
            .iter()
            .filter(|n| saved.contains(n.as_str()) && seen.insert(n.as_str()))
            .cloned()
            .collect();
        if names.is_empty() {
            continue;
        }
        let at = target(&out.output);
        let focused = out.focused.clone().filter(|f| names.contains(f));
        match plan.outputs.iter_mut().find(|o| o.output == at) {
            Some(existing) => {
                existing.workspaces.extend(names);
                if existing.focused.is_none() {
                    existing.focused = focused;
                }
            }
            None => plan.outputs.push(OutputSession {
                output: at,
                workspaces: names,
                focused,
                header: None,
            }),
        }
    }
    if plan.outputs.is_empty() {
        return None;
    }
    plan.focused_output = record
        .focused_output
        .as_deref()
        .map(target)
        .filter(|f| plan.outputs.iter().any(|o| &o.output == f));
    Some(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(name: &str, ws: &[&str], focused: Option<&str>) -> OutputSession {
        OutputSession {
            output: name.into(),
            workspaces: ws.iter().map(|s| (*s).to_string()).collect(),
            focused: focused.map(str::to_string),
            header: None,
        }
    }

    fn names(list: &[&str]) -> HashSet<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    fn conn(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn kdl_round_trips() {
        let s = Session {
            outputs: vec![
                out("DP-1", &["notes", "quote \" b"], Some("quote \" b")),
                out("HDMI-A-1", &["logs"], None),
            ],
            focused_output: Some("HDMI-A-1".into()),
        };
        let text = s.to_kdl();
        assert!(text.contains("focused-output HDMI-A-1"), "{text}");
        assert_eq!(Session::from_kdl(&text).unwrap(), s);
        assert_eq!(Session::from_kdl("").unwrap(), Session::default());
    }

    #[test]
    fn header_property_round_trips_and_old_records_parse() {
        let mut shown = out("DP-1", &["a"], Some("a"));
        shown.header = Some(HeaderPref::Shown);
        let mut hidden = out("HDMI-A-1", &[], None);
        hidden.header = Some(HeaderPref::Hidden);
        let s = Session {
            outputs: vec![shown, hidden, out("eDP-1", &["b"], None)],
            focused_output: None,
        };
        let text = s.to_kdl();
        assert!(
            text.contains("header=shown") || text.contains("header=\"shown\""),
            "{text}"
        );
        assert_eq!(Session::from_kdl(&text).unwrap(), s);
        // A record written before the property existed.
        let old = Session::from_kdl("focused-output DP-1\noutput DP-1 a focused=a\n").unwrap();
        assert_eq!(old.outputs[0].header, None);
    }

    #[test]
    fn unknown_header_value_is_ignored_not_fatal() {
        for bad in ["header=\"sideways\"", "header=3", "header=#true"] {
            let s = Session::from_kdl(&format!("output DP-1 a {bad}\n")).unwrap();
            assert_eq!(s.outputs[0].header, None, "{bad}");
            assert_eq!(s.outputs[0].workspaces, ["a"]);
        }
    }

    #[test]
    fn header_prefs_survive_unplug_and_empty_outputs() {
        let record = Session::from_kdl(
            "output DP-1 a header=shown\noutput HDMI-A-1 header=hidden\noutput eDP-1 b\n",
        )
        .unwrap();
        // Read from the raw record, even for an output with no workspaces.
        let prefs = header_prefs(&record);
        assert_eq!(prefs.len(), 2);
        assert!(prefs["DP-1"].is_shown());
        assert!(!prefs["HDMI-A-1"].is_shown());
        // Only DP-1 connected: HDMI-A-1's preference is carried as a workspace-less record.
        let outs = with_header_prefs(vec![out("DP-1", &["a"], None)], &prefs);
        assert_eq!(outs.len(), 2);
        assert_eq!(outs[0].header, Some(HeaderPref::Shown));
        assert_eq!(outs[1].output, "HDMI-A-1");
        assert!(outs[1].workspaces.is_empty());
        assert_eq!(outs[1].header, Some(HeaderPref::Hidden));
        // The carried-forward record re-parses and plan_restore ignores it.
        let text = Session {
            outputs: outs,
            focused_output: None,
        }
        .to_kdl();
        let again = Session::from_kdl(&text).unwrap();
        assert_eq!(header_prefs(&again), prefs);
        let plan = plan_restore(&again, &names(&["a"]), &conn(&["DP-1"])).unwrap();
        assert_eq!(plan.outputs.len(), 1);
    }

    #[test]
    fn malformed_text_is_an_error() {
        for bad in [
            "output",
            "output 3 \"a\"",
            "output \"DP-1\" \"\"",
            "output \"DP-1\" \"a\" other=\"a\"",
            "output \"DP-1\" \"a\" focused=1",
            "output \"DP-1\"\noutput \"DP-1\"",
            "focused-output",
            "focused-output \"a\" \"b\"",
            "focused-output \"a\"\nfocused-output \"b\"",
            "workspace \"a\"",
            "output \"DP-1",
        ] {
            assert!(Session::from_kdl(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn path_is_a_sibling_and_save_load_round_trips() {
        let root = std::env::temp_dir().join(format!("cde-session-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let store_dir = root.join("cde").join("workspaces");
        let path = session_path(&store_dir);
        assert_eq!(path, root.join("cde").join("session.kdl"));
        assert!(!path.starts_with(&store_dir));
        assert_eq!(Session::load(&path).unwrap(), None);
        let s = Session {
            outputs: vec![out("DP-1", &["a"], Some("a"))],
            focused_output: Some("DP-1".into()),
        };
        s.save(&path).unwrap();
        assert_eq!(Session::load(&path).unwrap(), Some(s));
        fs::create_dir_all(&store_dir).unwrap();
        assert!(store::list_saved(&store_dir).is_empty());
        fs::write(&path, "not kdl {{{").unwrap();
        assert!(Session::load(&path).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn plan_drops_missing_workspaces_and_keeps_focus() {
        let record = Session {
            outputs: vec![
                out("DP-1", &["a", "gone", "b"], Some("b")),
                out("HDMI-A-1", &["c"], Some("c")),
            ],
            focused_output: Some("HDMI-A-1".into()),
        };
        let plan = plan_restore(
            &record,
            &names(&["a", "b", "c"]),
            &conn(&["DP-1", "HDMI-A-1"]),
        )
        .unwrap();
        assert_eq!(plan.outputs[0], out("DP-1", &["a", "b"], Some("b")));
        assert_eq!(plan.outputs[1], out("HDMI-A-1", &["c"], Some("c")));
        assert_eq!(plan.focused_output.as_deref(), Some("HDMI-A-1"));
        // A missing focused workspace leaves that output's focus unset.
        let plan =
            plan_restore(&record, &names(&["a", "c"]), &conn(&["DP-1", "HDMI-A-1"])).unwrap();
        assert_eq!(plan.outputs[0], out("DP-1", &["a"], None));
    }

    #[test]
    fn unknown_outputs_fall_back_to_the_first_connected() {
        let record = Session {
            outputs: vec![
                out("DP-9", &["x", "a"], Some("x")),
                out("DP-1", &["b"], Some("b")),
            ],
            focused_output: Some("DP-9".into()),
        };
        let plan = plan_restore(&record, &names(&["a", "b", "x"]), &conn(&["DP-1"])).unwrap();
        assert_eq!(plan.outputs, vec![out("DP-1", &["x", "a", "b"], Some("x"))]);
        assert_eq!(plan.focused_output.as_deref(), Some("DP-1"));
    }

    #[test]
    fn nothing_restorable_is_a_cold_start() {
        let record = Session {
            outputs: vec![out("DP-1", &["gone"], Some("gone")), out("DP-2", &[], None)],
            focused_output: Some("DP-1".into()),
        };
        assert_eq!(
            plan_restore(&record, &names(&["a"]), &conn(&["DP-1"])),
            None
        );
        assert_eq!(
            plan_restore(&Session::default(), &names(&["a"]), &conn(&["DP-1"])),
            None
        );
        let ok = Session {
            outputs: vec![out("DP-1", &["a"], None)],
            focused_output: None,
        };
        assert_eq!(plan_restore(&ok, &names(&["a"]), &[]), None);
        // Duplicates across outputs are restored once.
        let dup = Session {
            outputs: vec![out("DP-1", &["a"], None), out("DP-2", &["a"], Some("a"))],
            focused_output: Some("DP-2".into()),
        };
        let plan = plan_restore(&dup, &names(&["a"]), &conn(&["DP-1", "DP-2"])).unwrap();
        assert_eq!(plan.outputs, vec![out("DP-1", &["a"], None)]);
        assert_eq!(plan.focused_output, None);
    }
}
