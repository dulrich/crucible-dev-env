//! The KDL workspace file format.
//!
//! ```kdl
//! version 1
//! workspace "name" root="/path" {
//!     focused-pane 1
//!     split axis="horizontal" ratio=0.5 {
//!         pane focused=0 { terminal cwd="/path" command="cargo test" }
//!         pane focused=0 { app app-id="firefox" cmdline="firefox-esr" }
//!     }
//! }
//! ```
//!
//! Parsing is strict: unknown nodes and properties are errors that name them, a file is exactly
//! one workspace, and a `version` newer than [`FORMAT_VERSION`] is refused.

use crate::{
    PersistError,
    record::{PaneRecord, RecordNode, WindowRecord, WorkspaceRecord},
};
use crucible_layout::Axis;
use kdl::{KdlDocument, KdlEntry, KdlNode, KdlValue};
use std::path::{Path, PathBuf};

/// The newest workspace-file version this build reads and the one it writes.
pub const FORMAT_VERSION: i128 = 1;

type Result<T> = std::result::Result<T, PersistError>;

// ---------------------------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------------------------

fn path_str(p: &Path) -> Result<&str> {
    p.to_str()
        .ok_or_else(|| PersistError::NonUtf8Path(p.to_path_buf()))
}

fn prop(node: &mut KdlNode, key: &str, value: impl Into<KdlValue>) {
    node.entries_mut().push(KdlEntry::new_prop(key, value));
}

fn push_child(node: &mut KdlNode, child: KdlNode) {
    node.ensure_children().nodes_mut().push(child);
}

fn axis_name(axis: Axis) -> &'static str {
    match axis {
        Axis::Horizontal => "horizontal",
        Axis::Vertical => "vertical",
    }
}

fn window_node(w: &WindowRecord) -> Result<KdlNode> {
    Ok(match w {
        WindowRecord::Terminal {
            cwd,
            command,
            session,
        } => {
            let mut n = KdlNode::new("terminal");
            prop(&mut n, "cwd", path_str(cwd)?);
            if let Some(c) = command {
                prop(&mut n, "command", c.as_str());
            }
            if let Some(k) = session {
                prop(&mut n, "session", k.as_str());
            }
            n
        }
        WindowRecord::App { app_id, cmdline } => {
            let mut n = KdlNode::new("app");
            if let Some(a) = app_id {
                prop(&mut n, "app-id", a.as_str());
            }
            if let Some(c) = cmdline {
                prop(&mut n, "cmdline", c.as_str());
            }
            n
        }
    })
}

fn tree_node(t: &RecordNode) -> Result<KdlNode> {
    match t {
        RecordNode::Pane(p) => {
            let mut n = KdlNode::new("pane");
            prop(&mut n, "focused", p.focused as i128);
            for w in &p.windows {
                push_child(&mut n, window_node(w)?);
            }
            Ok(n)
        }
        RecordNode::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let mut n = KdlNode::new("split");
            prop(&mut n, "axis", axis_name(*axis));
            prop(&mut n, "ratio", *ratio);
            push_child(&mut n, tree_node(first)?);
            push_child(&mut n, tree_node(second)?);
            Ok(n)
        }
    }
}

impl WorkspaceRecord {
    /// Serializes this record to the KDL workspace file text.
    ///
    /// Errors with [`PersistError::NonUtf8Path`] if a path is not valid UTF-8 (never lossy).
    pub fn to_kdl(&self) -> Result<String> {
        let mut doc = KdlDocument::new();
        let mut version = KdlNode::new("version");
        version.entries_mut().push(KdlEntry::new(FORMAT_VERSION));
        doc.nodes_mut().push(version);

        let mut ws = KdlNode::new("workspace");
        ws.entries_mut().push(KdlEntry::new(self.name.as_str()));
        if let Some(root) = &self.root {
            prop(&mut ws, "root", path_str(root)?);
        }
        if let Some(output) = &self.output {
            prop(&mut ws, "output", output.as_str());
        }
        let mut focused = KdlNode::new("focused-pane");
        focused
            .entries_mut()
            .push(KdlEntry::new(self.focused_pane as i128));
        push_child(&mut ws, focused);
        push_child(&mut ws, tree_node(&self.tree)?);
        doc.nodes_mut().push(ws);

        doc.autoformat();
        Ok(doc.to_string())
    }

    /// Parses the KDL workspace file text.
    pub fn from_kdl(text: &str) -> Result<WorkspaceRecord> {
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
        Reader { text }.document(&doc)
    }
}

// ---------------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------------

/// 1-based (line, column) of a char offset.
fn line_col(text: &str, offset: usize) -> (usize, usize) {
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

struct Reader<'a> {
    text: &'a str,
}

impl Reader<'_> {
    /// Line of a node; the kdl span offset is in chars.
    fn line(&self, n: &KdlNode) -> usize {
        line_col(self.text, n.span().offset()).0
    }

    fn unknown_node(&self, n: &KdlNode) -> PersistError {
        PersistError::UnknownNode {
            name: n.name().value().to_string(),
            line: self.line(n),
        }
    }

    fn missing(&self, n: &KdlNode, field: &str) -> PersistError {
        PersistError::Missing {
            node: n.name().value().to_string(),
            field: field.to_string(),
            line: self.line(n),
        }
    }

    fn invalid(&self, n: &KdlNode, field: &str, expected: &str) -> PersistError {
        PersistError::Invalid {
            node: n.name().value().to_string(),
            field: field.to_string(),
            expected: expected.to_string(),
            line: self.line(n),
        }
    }

    /// Rejects any property not in `allowed` and any argument beyond `max_args`.
    fn check_entries(&self, n: &KdlNode, allowed: &[&str], max_args: usize) -> Result<()> {
        let mut args = 0;
        for e in n.entries() {
            match e.name() {
                Some(k) if allowed.contains(&k.value()) => {}
                Some(k) => {
                    return Err(PersistError::UnknownProperty {
                        node: n.name().value().to_string(),
                        prop: k.value().to_string(),
                        line: self.line(n),
                    });
                }
                None => {
                    args += 1;
                    if args > max_args {
                        return Err(self.invalid(
                            n,
                            "arguments",
                            "no further positional arguments",
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn string_prop(&self, n: &KdlNode, key: &str) -> Result<Option<String>> {
        match n.entry(key) {
            None => Ok(None),
            Some(e) => e
                .value()
                .as_string()
                .map(|s| Some(s.to_string()))
                .ok_or_else(|| self.invalid(n, key, "a string")),
        }
    }

    fn index_value(&self, n: &KdlNode, field: &str, v: Option<&KdlValue>) -> Result<usize> {
        let v = v.ok_or_else(|| self.missing(n, field))?;
        v.as_integer()
            .and_then(|i| usize::try_from(i).ok())
            .ok_or_else(|| self.invalid(n, field, "a non-negative integer"))
    }

    fn document(&self, doc: &KdlDocument) -> Result<WorkspaceRecord> {
        let mut version = None;
        let mut workspace = None;
        for n in doc.nodes() {
            match n.name().value() {
                "version" => {
                    if version.is_some() {
                        return Err(PersistError::Duplicate {
                            name: "version".into(),
                            line: self.line(n),
                        });
                    }
                    self.check_entries(n, &[], 1)?;
                    let v = n
                        .entry(0usize)
                        .and_then(|e| e.value().as_integer())
                        .ok_or_else(|| self.invalid(n, "version", "an integer argument"))?;
                    version = Some(v);
                }
                "workspace" => {
                    if workspace.is_some() {
                        return Err(PersistError::Duplicate {
                            name: "workspace".into(),
                            line: self.line(n),
                        });
                    }
                    workspace = Some(n);
                }
                _ => return Err(self.unknown_node(n)),
            }
        }
        let version = version.ok_or(PersistError::Missing {
            node: "document".into(),
            field: "version".into(),
            line: 1,
        })?;
        if version > FORMAT_VERSION {
            return Err(PersistError::UnsupportedVersion {
                found: version,
                supported: FORMAT_VERSION,
            });
        }
        if version < 1 {
            return Err(PersistError::Invalid {
                node: "version".into(),
                field: "version".into(),
                expected: "1 or newer".into(),
                line: 1,
            });
        }
        let ws = workspace.ok_or(PersistError::Missing {
            node: "document".into(),
            field: "workspace".into(),
            line: 1,
        })?;
        self.workspace(ws)
    }

    fn workspace(&self, n: &KdlNode) -> Result<WorkspaceRecord> {
        self.check_entries(n, &["root", "output"], 1)?;
        let name = n
            .entry(0usize)
            .ok_or_else(|| self.missing(n, "name argument"))?
            .value()
            .as_string()
            .ok_or_else(|| self.invalid(n, "name", "a string"))?
            .to_string();
        let root = self.string_prop(n, "root")?.map(PathBuf::from);
        let output = self.string_prop(n, "output")?;

        let children = n
            .children()
            .ok_or_else(|| self.missing(n, "focused-pane"))?;
        let mut focused_pane = None;
        let mut tree_node_ = None;
        for c in children.nodes() {
            match c.name().value() {
                "focused-pane" => {
                    if focused_pane.is_some() {
                        return Err(PersistError::Duplicate {
                            name: "focused-pane".into(),
                            line: self.line(c),
                        });
                    }
                    self.check_entries(c, &[], 1)?;
                    let v = c.entry(0usize).map(KdlEntry::value);
                    focused_pane = Some(self.index_value(c, "focused-pane", v)?);
                }
                "split" | "pane" => {
                    if tree_node_.is_some() {
                        return Err(PersistError::Duplicate {
                            name: "split/pane (a workspace has one tree)".into(),
                            line: self.line(c),
                        });
                    }
                    tree_node_ = Some(c);
                }
                _ => return Err(self.unknown_node(c)),
            }
        }
        let focused_pane = focused_pane.ok_or_else(|| self.missing(n, "focused-pane"))?;
        let tree = self.tree(tree_node_.ok_or_else(|| self.missing(n, "a pane or split"))?)?;
        let pane_count = tree.pane_count();
        if focused_pane >= pane_count {
            return Err(self.invalid(n, "focused-pane", "an index below the pane count"));
        }
        Ok(WorkspaceRecord {
            name,
            root,
            focused_pane,
            output,
            tree,
        })
    }

    fn tree(&self, n: &KdlNode) -> Result<RecordNode> {
        match n.name().value() {
            "pane" => self.pane(n),
            "split" => {
                self.check_entries(n, &["axis", "ratio"], 0)?;
                let axis = match self.string_prop(n, "axis")?.as_deref() {
                    Some("horizontal") => Axis::Horizontal,
                    Some("vertical") => Axis::Vertical,
                    Some(_) => {
                        return Err(self.invalid(n, "axis", "\"horizontal\" or \"vertical\""));
                    }
                    None => return Err(self.missing(n, "axis")),
                };
                let ratio = n
                    .entry("ratio")
                    .ok_or_else(|| self.missing(n, "ratio"))?
                    .value()
                    .as_float()
                    .filter(|r| r.is_finite() && *r > 0.0 && *r < 1.0)
                    .ok_or_else(|| {
                        self.invalid(n, "ratio", "a decimal strictly between 0 and 1")
                    })?;
                let kids = n.children().map_or(&[][..], KdlDocument::nodes);
                if kids.len() != 2 {
                    return Err(PersistError::WrongChildCount {
                        found: kids.len(),
                        line: self.line(n),
                    });
                }
                Ok(RecordNode::Split {
                    axis,
                    ratio,
                    first: Box::new(self.tree(&kids[0])?),
                    second: Box::new(self.tree(&kids[1])?),
                })
            }
            _ => Err(self.unknown_node(n)),
        }
    }

    fn pane(&self, n: &KdlNode) -> Result<RecordNode> {
        self.check_entries(n, &["focused"], 0)?;
        let focused = self.index_value(n, "focused", n.entry("focused").map(KdlEntry::value))?;
        let mut windows = Vec::new();
        for c in n.children().map_or(&[][..], KdlDocument::nodes) {
            windows.push(self.window(c)?);
        }
        if focused >= windows.len().max(1) {
            return Err(self.invalid(n, "focused", "an index below the window count"));
        }
        Ok(RecordNode::Pane(PaneRecord { focused, windows }))
    }

    fn window(&self, n: &KdlNode) -> Result<WindowRecord> {
        match n.name().value() {
            "terminal" => {
                self.check_entries(n, &["cwd", "command", "session"], 0)?;
                let cwd = self
                    .string_prop(n, "cwd")?
                    .ok_or_else(|| self.missing(n, "cwd"))?;
                Ok(WindowRecord::Terminal {
                    cwd: PathBuf::from(cwd),
                    command: self.string_prop(n, "command")?,
                    // A malformed key (hand-edited file) loads as `None`, never an error, so it
                    // cannot block a restore; the window then restores as a key-less terminal.
                    session: self
                        .string_prop(n, "session")?
                        .filter(|k| is_session_key(k)),
                })
            }
            "app" => {
                self.check_entries(n, &["app-id", "cmdline"], 0)?;
                Ok(WindowRecord::App {
                    app_id: self.string_prop(n, "app-id")?,
                    cmdline: self.string_prop(n, "cmdline")?,
                })
            }
            _ => Err(self.unknown_node(n)),
        }
    }
}

/// Whether `key` is a well-formed session key: 32 lowercase hex chars (128 bits).
#[must_use]
pub fn is_session_key(key: &str) -> bool {
    key.len() == 32
        && key
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
