//! `crucible pick [--rename <encoded> | --root <encoded>]` — the workspace picker TUI (run inside the
//! compositor's terminal overlay). Only the picker's domain hooks live here (rename and workspace-root
//! modes, candidate rows, what
//! Enter sends); the runner, key decoding, shared state and frame scaffolding are in
//! [`crate::overlay`]. The state machine and rendering are pure.

use std::path::{Path, PathBuf};

use crucible_compositor::ipc::{Candidate, CandidateKind, Request, Response, parse_candidates};
use crucible_persist::PickTarget;

use crate::cli::{Parsed, help_requested};
use crate::client::{Client, ClientError};
use crate::fuzzy;
use crate::overlay::list::{Body, Frame, Line, ListState};
use crate::overlay::term::{Key, sanitize};
use crate::overlay::{Outcome, Overlay, run_overlay};
use crate::style::Style;
use crate::table::{self, Col};

/// The command-key glyph (`⌘`) prefixing a pinned row's slot number; matches the header's.
const SUPER_GLYPH: char = '\u{2318}';

/// Usage text for `crucible pick --help`.
pub const USAGE: &str = "usage: crucible pick [--rename <encoded-name> | --root <encoded-path>]\n\nThe CDE workspace picker (needs $CDE_SOCKET; CDE runs it in an overlay on Super+P).\nType to filter; Up/Down or Ctrl+P/Ctrl+N move; Enter opens the selection (or creates a new\nworkspace from the typed name); Esc or Ctrl+C closes. Delete on a saved row arms a confirm\n(\"Delete again to remove\"); a second Delete on the same row removes the saved workspace (its file\nmoves to the deleted/ folder next to the store) and closes the picker. An open workspace is never\ndeleted here: use the delete-workspace verb.\n\n  --rename <name>   single-line rename prompt prefilled with <name> (percent-encoded: %25 %3A %09 %0A %0D)\n  --root <path>     workspace-root prompt prefilled with <path> (percent-encoded; empty = none); Enter on empty clears\n";

/// Picker behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Fuzzy workspace picker.
    Pick,
    /// Workspace-root prompt: a path line over the configured folders.
    Root,
}

/// `path` with a leading `home` replaced by `~`, only on a path-component boundary.
fn tilde(path: &str, home: Option<&str>) -> String {
    match home.filter(|h| !h.is_empty() && *h != "/") {
        Some(h) => match path.strip_prefix(h) {
            Some("") => "~".to_string(),
            Some(rest) if rest.starts_with('/') => format!("~{rest}"),
            _ => path.to_string(),
        },
        None => path.to_string(),
    }
}

/// `typed` with a leading `~` (alone or before `/`) expanded to `home`; `None` when `~` needs a
/// `$HOME` that is unset. Anything else is returned as typed.
fn expand_tilde(typed: &str, home: Option<&str>) -> Option<String> {
    let expanded = crucible_config::expand_tilde(Path::new(typed), home);
    let needs_home = typed == "~" || typed.starts_with("~/");
    if needs_home && expanded == Path::new(typed) {
        return None;
    }
    // `~/x` joins onto `home` (a trailing `/` on `home` is dropped); other input is as typed.
    let mut out = expanded.to_string_lossy().into_owned();
    // `Path` drops a typed trailing `/` (`~/` -> `/home/u`); typed input keeps it.
    if typed.starts_with('~') && typed.ends_with('/') && !out.ends_with('/') {
        out.push('/');
    }
    Some(out)
}

/// One selectable row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// An index into the candidate list.
    Candidate(usize),
    /// The trailing "new workspace <typed>" row.
    New(String),
}

/// Picker state.
#[derive(Debug)]
pub struct Picker {
    mode: Mode,
    candidates: Vec<Candidate>,
    list: ListState,
    /// `$HOME`, for `~` display and expansion.
    home: Option<String>,
    /// The saved workspace name whose Delete is armed (the next Delete on it removes it).
    armed: Option<String>,
}

impl Picker {
    /// A fuzzy picker over `candidates`.
    #[must_use]
    pub fn new(candidates: Vec<Candidate>, home: Option<String>) -> Picker {
        Picker {
            mode: Mode::Pick,
            candidates,
            list: ListState::new(),
            home,
            armed: None,
        }
    }

    /// The workspace-root prompt over the folder candidates, prefilled with `current` (`$HOME`
    /// shown as `~`; empty = no root).
    #[must_use]
    pub fn root(current: &str, candidates: Vec<Candidate>, home: Option<String>) -> Picker {
        let candidates = candidates
            .into_iter()
            .filter(|c| c.kind == CandidateKind::Folder)
            .collect();
        Picker {
            mode: Mode::Root,
            candidates,
            list: ListState::with_query(tilde(current, home.as_deref())),
            home,
            armed: None,
        }
    }

    /// The current input line.
    #[cfg(test)]
    #[must_use]
    pub fn query(&self) -> &str {
        &self.list.query
    }

    /// The status line, if any.
    #[cfg(test)]
    #[must_use]
    pub fn status(&self) -> Option<&str> {
        self.list.status.as_deref()
    }

    /// Visible rows: fuzzy matches best first, then a "new workspace" row whenever the query is
    /// non-empty and is not exactly a live/saved workspace name.
    #[must_use]
    pub fn rows(&self) -> Vec<Row> {
        let query = &self.list.query;
        let root = self.mode == Mode::Root;
        let names: Vec<&str> = self.candidates.iter().map(|c| c.name.as_str()).collect();
        let mut rows: Vec<Row> = fuzzy::filter(query, &names)
            .into_iter()
            .map(Row::Candidate)
            .collect();
        let exact = self
            .candidates
            .iter()
            .any(|c| c.kind != CandidateKind::Folder && c.name == *query);
        if !root && !query.is_empty() && !exact {
            rows.push(Row::New(query.clone()));
        }
        rows
    }

    /// Delete on the selected row: a saved (not live) row arms, then a second Delete on the same
    /// row sends [`Request::Delete`]; a live row only shows a hint; folder / new rows are ignored.
    fn delete_selected(&mut self) -> Outcome {
        if self.mode != Mode::Pick {
            return Outcome::Continue;
        }
        let Some(Row::Candidate(i)) = self.rows().get(self.list.selected).cloned() else {
            return Outcome::Continue;
        };
        let c = &self.candidates[i];
        match c.kind {
            CandidateKind::Saved => {
                let name = c.name.clone();
                if self.armed.as_deref() == Some(name.as_str()) {
                    self.armed = None;
                    return Outcome::Send(Request::Delete(name));
                }
                self.list.status = Some(format!(
                    "Delete again to remove saved workspace {}",
                    sanitize(&name)
                ));
                self.armed = Some(name);
            }
            CandidateKind::Live => {
                self.list.status = Some("open workspace: use delete-workspace".into());
            }
            CandidateKind::Folder => {}
        }
        Outcome::Continue
    }

    /// Enter / Alt+Enter: open / create the selected row (or, in root mode, set the root).
    fn enter_selected(&mut self) -> Outcome {
        if self.mode == Mode::Root {
            return self.enter_root();
        }
        match self.rows().get(self.list.selected) {
            Some(Row::Candidate(i)) => {
                let c = &self.candidates[*i];
                match (c.kind, &c.path) {
                    (CandidateKind::Folder, Some(p)) => {
                        Outcome::Send(Request::OpenFolder(p.clone()))
                    }
                    (CandidateKind::Folder, None) => {
                        self.list.status = Some("folder has no path".into());
                        Outcome::Continue
                    }
                    _ => Outcome::Send(Request::Open(c.name.clone())),
                }
            }
            Some(Row::New(n)) => Outcome::Send(Request::New(n.clone())),
            None => Outcome::Continue,
        }
    }

    /// Root-mode Enter, in precedence order: an empty query clears the root; a typed path (starting
    /// with `/` or `~`) wins over the selected row; else the selected folder row; else a hint.
    fn enter_root(&mut self) -> Outcome {
        let query = self.list.query.clone();
        if query.is_empty() {
            return Outcome::Send(Request::SetRoot(None));
        }
        if query.starts_with(['/', '~']) {
            return match expand_tilde(&query, self.home.as_deref()) {
                Some(p) => Outcome::Send(Request::SetRoot(Some(PathBuf::from(p)))),
                None => {
                    self.list.status = Some("$HOME is not set".into());
                    Outcome::Continue
                }
            };
        }
        let path = match self.rows().get(self.list.selected) {
            Some(Row::Candidate(i)) => self.candidates[*i].path.clone(),
            _ => None,
        };
        if let Some(p) = path {
            return Outcome::Send(Request::SetRoot(Some(p)));
        }
        self.list.status = Some("type a path or pick a folder".into());
        Outcome::Continue
    }

    /// The table cells of a candidate row: kind, slot badge (or empty), name, root/path (or
    /// empty), all sanitised.
    fn cells(&self, c: &Candidate) -> Vec<String> {
        let kind = match c.kind {
            CandidateKind::Live => "live",
            CandidateKind::Saved => "saved",
            CandidateKind::Folder => "folder",
        };
        let badge = c
            .slot
            .map_or(String::new(), |n| format!("{SUPER_GLYPH}{n}"));
        // Folder rows show their path; live and saved rows show their workspace root.
        let shown = c.path.as_ref().map_or(String::new(), |p| {
            if c.kind == CandidateKind::Folder {
                p.to_string_lossy().into_owned()
            } else {
                tilde(&p.to_string_lossy(), self.home.as_deref())
            }
        });
        vec![kind.to_string(), badge, sanitize(&c.name), sanitize(&shown)]
    }

    /// The table header (`None` when there are no candidate rows) and one display string per
    /// row of [`Self::rows`]. The table covers the candidate rows; the `new workspace` row is
    /// always last and is appended unpadded. The slot column is dropped when no row has a slot.
    /// Widths cover every row of the current filter, so columns do not jitter while scrolling.
    fn table(&self) -> (Option<String>, Vec<String>) {
        let rows = self.rows();
        let cells: Vec<Vec<String>> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Candidate(i) => Some(self.cells(&self.candidates[*i])),
                Row::New(_) => None,
            })
            .collect();
        let cols = [
            Col::left("kind"),
            Col::left("slot").hidden_if_empty(),
            Col::left("name"),
            Col::left("root"),
        ];
        let (header, mut texts) = table::layout(&cols, &cells);
        if let Some(Row::New(n)) = rows.last() {
            texts.push(format!("new workspace {}", sanitize(n)));
        }
        ((!cells.is_empty()).then_some(header), texts)
    }
}

impl Overlay for Picker {
    fn list_mut(&mut self) -> &mut ListState {
        &mut self.list
    }

    fn row_count(&self) -> usize {
        self.rows().len()
    }

    fn enter(&mut self, _key: Key) -> Outcome {
        self.enter_selected()
    }

    /// Delete is the picker's two-press remove; any other key disarms a pending confirm (and
    /// clears its hint) before being handled normally.
    fn intercept(&mut self, key: Key) -> Option<Outcome> {
        if key == Key::Delete {
            return Some(self.delete_selected());
        }
        if self.armed.take().is_some() {
            self.list.status = None;
        }
        None
    }

    fn render_styled(&self, cols: u16, rows: u16, style: &Style) -> String {
        let prompt = match self.mode {
            Mode::Pick => "workspace> ",
            Mode::Root => "root> ",
        };
        let (header, texts) = self.table();
        let lines: Vec<Line> = texts.into_iter().map(Line::Row).collect();
        let frame = Frame {
            prompt,
            header: header.as_deref(),
            body: Body::Rows(&lines),
            idle_status: "",
        };
        self.list.render(frame, cols, rows, style)
    }
}

/// The single-line rename prompt (`crucible pick --rename`): no rows, Enter sends the typed name.
#[derive(Debug)]
pub struct RenamePrompt {
    list: ListState,
}

impl RenamePrompt {
    /// A rename prompt prefilled with `current`.
    #[must_use]
    pub fn new(current: String) -> RenamePrompt {
        RenamePrompt {
            list: ListState::with_query(current),
        }
    }
}

impl Overlay for RenamePrompt {
    fn list_mut(&mut self) -> &mut ListState {
        &mut self.list
    }

    fn row_count(&self) -> usize {
        0
    }

    /// An empty name is refused locally; anything else is sent as the rename.
    fn enter(&mut self, _key: Key) -> Outcome {
        if self.list.query.is_empty() {
            self.list.status = Some("name required".into());
            return Outcome::Continue;
        }
        Outcome::Send(Request::Rename(self.list.query.clone()))
    }

    fn render_styled(&self, cols: u16, rows: u16, style: &Style) -> String {
        let frame = Frame {
            prompt: "rename> ",
            header: None,
            body: Body::Rows(&[]),
            idle_status: "",
        };
        self.list.render(frame, cols, rows, style)
    }
}

/// Parsed `pick` arguments (`--help`/`-h` outside a `--rename`/`--root` value is
/// [`Parsed::Help`]).
///
/// # Errors
/// A usage message for unknown flags, a missing value, a bad percent escape, or `--rename`
/// together with `--root`.
pub fn parse_args(args: &[String]) -> Result<Parsed<PickTarget>, String> {
    if help_requested(args, &["--rename", "--root"], false) {
        return Ok(Parsed::Help);
    }
    PickTarget::parse(args).map(Parsed::Run)
}

/// `$HOME`, if set and non-empty.
fn home_dir() -> Option<String> {
    std::env::var("HOME").ok().filter(|h| !h.is_empty())
}

/// Runs the subcommand; returns the exit code.
pub fn run(args: &[String]) -> i32 {
    let parsed = match crate::cli::parse_or_exit("pick", USAGE, parse_args(args)) {
        Ok(v) => v,
        Err(code) => return code,
    };
    // The one place `$HOME` is read; every constructor takes it.
    let home = home_dir();
    match parsed {
        PickTarget::Rename(current) => run_overlay("pick", |_| Ok(RenamePrompt::new(current))),
        PickTarget::Root(current) => run_overlay("pick", |client| {
            fetch_candidates(client).map(|candidates| Picker::root(&current, candidates, home))
        }),
        PickTarget::Workspace => run_overlay("pick", |client| {
            fetch_candidates(client).map(|candidates| Picker::new(candidates, home))
        }),
    }
}

fn fetch_candidates(client: &mut Client) -> Result<Vec<Candidate>, String> {
    match client.send(&Request::Candidates) {
        Ok(Response::Ok(payload)) => {
            parse_candidates(&payload).ok_or_else(|| "malformed candidates response".to_string())
        }
        Ok(Response::Err(r)) => Err(format!("compositor refused: {r}")),
        Err(e @ ClientError::BadResponse(_)) => Err(e.to_string()),
        Err(e) => Err(format!("candidates request failed: {e}")),
    }
}

#[cfg(test)]
mod tests;
