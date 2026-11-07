//! `crucible pick [--rename <encoded> | --root <encoded>]` — the workspace picker TUI (run inside the
//! compositor's terminal overlay). Only the picker's domain hooks live here (rename and workspace-root
//! modes, candidate rows, what
//! Enter sends); the runner, key decoding, shared state and frame scaffolding are in
//! [`crate::overlay`]. The state machine and rendering are pure.

use std::path::PathBuf;

use crucible_compositor::ipc::{Candidate, CandidateKind, Request, Response, parse_candidates};
use crucible_persist::PickTarget;

use crate::cli::{Parsed, help_requested};
use crate::client::{Client, ClientError};
use crate::fuzzy;
use crate::overlay::list::{Applied, Body, Frame, ListState};
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
    match typed.strip_prefix('~') {
        Some("") => home.map(str::to_string),
        Some(rest) if rest.starts_with('/') => {
            home.map(|h| format!("{}{rest}", h.trim_end_matches('/')))
        }
        _ => Some(typed.to_string()),
    }
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
    fn step(&mut self, key: Key) -> Outcome {
        if key == Key::Delete {
            return self.delete_selected();
        }
        if self.armed.take().is_some() {
            self.list.status = None;
        }
        let count = self.row_count();
        match self.list.apply(key, count) {
            Applied::Exit => Outcome::Exit,
            Applied::Edited => {
                self.list.status = None;
                Outcome::Continue
            }
            Applied::Moved | Applied::Ignored => Outcome::Continue,
            Applied::Enter(k) => self.enter(k),
        }
    }

    fn render_styled(&self, cols: u16, rows: u16, style: &Style) -> String {
        let prompt = match self.mode {
            Mode::Pick => "workspace> ",
            Mode::Root => "root> ",
        };
        let (header, texts) = self.table();
        let frame = Frame {
            prompt,
            header: header.as_deref(),
            body: Body::Rows(&texts),
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
mod tests {
    use super::*;

    fn cand(kind: CandidateKind, name: &str, path: Option<&str>) -> Candidate {
        Candidate {
            kind,
            name: name.into(),
            path: path.map(PathBuf::from),
            slot: None,
        }
    }

    #[test]
    fn table_has_a_header_and_the_slot_column_only_when_a_row_has_a_slot() {
        let mut p = sample();
        let (h, t) = p.table();
        assert_eq!(h.as_deref(), Some("kind    name   root"));
        assert_eq!(t[0], "live    alpha");
        assert_eq!(t[2], "folder  proj   /home/u/proj");
        p.candidates[0].slot = Some(3);
        let (h, t) = p.table();
        assert_eq!(h.as_deref(), Some("kind    slot  name   root"));
        assert_eq!(t[0], "live    \u{2318}3    alpha");
        assert_eq!(t[1], "saved         beta");
        // The new-workspace row is unpadded and carries no header on its own.
        type_str(&mut p, "zz");
        let (h, t) = p.table();
        assert_eq!((h, t), (None, vec!["new workspace zz".to_string()]));
    }

    fn select(p: &mut Picker, name: &str) {
        for _ in 0..4 {
            p.step(Key::Up);
        }
        while p
            .rows()
            .get(p.list.selected)
            .is_some_and(|r| !matches!(r, Row::Candidate(i) if p.candidates[*i].name == name))
        {
            p.step(Key::Down);
        }
    }

    #[test]
    fn delete_on_a_saved_row_needs_two_presses() {
        let mut p = sample();
        select(&mut p, "beta");
        assert_eq!(p.step(Key::Delete), Outcome::Continue);
        assert!(p.status().unwrap().contains("Delete again"));
        assert_eq!(
            p.step(Key::Delete),
            Outcome::Send(Request::Delete("beta".into()))
        );
        // Disarmed after the send: the next press arms again.
        assert_eq!(p.step(Key::Delete), Outcome::Continue);
    }

    #[test]
    fn any_other_key_disarms_the_delete_confirm() {
        let mut p = sample();
        select(&mut p, "beta");
        p.step(Key::Delete);
        // Moving away and back, or typing, disarms; the key is still handled normally.
        p.step(Key::Up);
        assert_eq!(p.status(), None);
        p.step(Key::Down);
        assert_eq!(p.step(Key::Delete), Outcome::Continue);
        p.step(Key::Char('b'));
        assert_eq!(p.query(), "b");
        assert_eq!(p.step(Key::Delete), Outcome::Continue);
        assert!(p.status().unwrap().contains("Delete again"));
    }

    #[test]
    fn delete_on_a_different_saved_row_re_arms_instead_of_sending() {
        let mut p = Picker::new(
            vec![
                cand(CandidateKind::Saved, "one", None),
                cand(CandidateKind::Saved, "two", None),
            ],
            None,
        );
        select(&mut p, "one");
        p.step(Key::Delete);
        // Down is "any other key": it disarms, so Delete on "two" only arms.
        p.step(Key::Down);
        assert_eq!(p.step(Key::Delete), Outcome::Continue);
        assert_eq!(
            p.step(Key::Delete),
            Outcome::Send(Request::Delete("two".into()))
        );
    }

    #[test]
    fn delete_on_live_folder_and_new_rows_sends_nothing() {
        let mut p = sample();
        select(&mut p, "alpha");
        assert_eq!(p.step(Key::Delete), Outcome::Continue);
        assert_eq!(p.status(), Some("open workspace: use delete-workspace"));
        assert_eq!(p.step(Key::Delete), Outcome::Continue);
        select(&mut p, "proj");
        assert_eq!(p.step(Key::Delete), Outcome::Continue);
        assert_eq!(p.step(Key::Delete), Outcome::Continue);
        let mut p = sample();
        type_str(&mut p, "zzz");
        assert_eq!(p.step(Key::Delete), Outcome::Continue);
        assert_eq!(p.step(Key::Delete), Outcome::Continue);
        // Root mode never deletes.
        let mut r = Picker::root(
            "",
            vec![cand(CandidateKind::Folder, "proj", Some("/p"))],
            None,
        );
        assert_eq!(r.step(Key::Delete), Outcome::Continue);
    }

    #[test]
    fn rename_mode_has_no_header() {
        let p = RenamePrompt::new("x".into());
        assert_eq!(
            p.render(40, 6),
            "\x1b[H\x1b[2J\r\n  rename> x_\r\n\r\n\r\n\r\n"
        );
    }

    fn sample() -> Picker {
        Picker::new(
            vec![
                cand(CandidateKind::Live, "alpha", None),
                cand(CandidateKind::Saved, "beta", None),
                cand(CandidateKind::Folder, "proj", Some("/home/u/proj")),
            ],
            None,
        )
    }

    fn type_str(p: &mut Picker, s: &str) {
        for c in s.chars() {
            p.step(Key::Char(c));
        }
    }

    #[test]
    fn enter_opens_live_saved_and_folder() {
        let mut p = sample();
        assert_eq!(
            p.step(Key::Enter),
            Outcome::Send(Request::Open("alpha".into()))
        );
        p.step(Key::Down);
        assert_eq!(
            p.step(Key::Enter),
            Outcome::Send(Request::Open("beta".into()))
        );
        p.step(Key::Down);
        assert_eq!(
            p.step(Key::Enter),
            Outcome::Send(Request::OpenFolder(PathBuf::from("/home/u/proj")))
        );
    }

    #[test]
    fn selection_is_clamped() {
        let mut p = sample();
        p.step(Key::Up);
        assert_eq!(
            p.step(Key::Enter),
            Outcome::Send(Request::Open("alpha".into()))
        );
        for _ in 0..10 {
            p.step(Key::CtrlN);
        }
        assert!(matches!(
            p.step(Key::Enter),
            Outcome::Send(Request::OpenFolder(_))
        ));
        p.step(Key::CtrlP);
        assert_eq!(
            p.step(Key::Enter),
            Outcome::Send(Request::Open("beta".into()))
        );
    }

    #[test]
    fn filtering_narrows_rows_and_appends_new_row() {
        let mut p = sample();
        type_str(&mut p, "bet");
        assert_eq!(p.rows(), vec![Row::Candidate(1), Row::New("bet".into())]);
        assert_eq!(
            p.step(Key::Enter),
            Outcome::Send(Request::Open("beta".into()))
        );
    }

    #[test]
    fn exact_workspace_name_has_no_new_row_but_folder_name_does() {
        let mut p = sample();
        type_str(&mut p, "beta");
        assert_eq!(p.rows(), vec![Row::Candidate(1)]);
        let mut q = sample();
        type_str(&mut q, "proj");
        assert_eq!(q.rows(), vec![Row::Candidate(2), Row::New("proj".into())]);
    }

    #[test]
    fn no_match_creates_new_workspace() {
        let mut p = sample();
        type_str(&mut p, "zzz");
        assert_eq!(p.rows(), vec![Row::New("zzz".into())]);
        assert_eq!(
            p.step(Key::Enter),
            Outcome::Send(Request::New("zzz".into()))
        );
    }

    #[test]
    fn new_row_can_be_selected_explicitly() {
        let mut p = sample();
        type_str(&mut p, "a");
        let n = p.rows().len();
        for _ in 0..n {
            p.step(Key::Down);
        }
        assert_eq!(p.step(Key::Enter), Outcome::Send(Request::New("a".into())));
    }

    #[test]
    fn empty_query_and_no_candidates_enter_is_noop() {
        let mut p = Picker::new(vec![], None);
        assert_eq!(p.step(Key::Enter), Outcome::Continue);
    }

    #[test]
    fn backspace_edits_and_resets_selection() {
        let mut p = sample();
        type_str(&mut p, "ab");
        p.step(Key::Backspace);
        assert_eq!(p.query(), "a");
        p.step(Key::Backspace);
        p.step(Key::Backspace);
        assert_eq!(p.query(), "");
    }

    #[test]
    fn exit_keys() {
        assert_eq!(sample().step(Key::Esc), Outcome::Exit);
        assert_eq!(sample().step(Key::CtrlC), Outcome::Exit);
        assert_eq!(sample().step(Key::CtrlD), Outcome::Exit);
        let mut p = sample();
        p.step(Key::Char('a'));
        assert_eq!(p.step(Key::CtrlD), Outcome::Continue);
        assert_eq!(p.step(Key::CtrlC), Outcome::Exit);
    }

    #[test]
    fn response_ok_exits_err_stays_with_status() {
        let mut p = sample();
        assert_eq!(p.on_response(&Response::Ok(String::new())), Outcome::Exit);
        assert_eq!(
            p.on_response(&Response::Err("already open".into())),
            Outcome::Continue
        );
        assert_eq!(p.status(), Some("already open"));
        assert!(p.render(40, 10).contains("already open"));
        p.step(Key::Char('x'));
        assert_eq!(p.status(), None);
    }

    #[test]
    fn rename_prompt_prefills_and_sends() {
        let mut p = RenamePrompt::new("old name".into());
        assert_eq!(p.list.query, "old name");
        assert_eq!(p.row_count(), 0);
        assert_eq!(
            p.step(Key::Enter),
            Outcome::Send(Request::Rename("old name".into()))
        );
        p.step(Key::Char('!'));
        assert_eq!(
            p.step(Key::Enter),
            Outcome::Send(Request::Rename("old name!".into()))
        );
    }

    #[test]
    fn rename_empty_is_rejected_locally() {
        let mut p = RenamePrompt::new("a".into());
        p.step(Key::Backspace);
        assert_eq!(p.step(Key::Enter), Outcome::Continue);
        assert_eq!(p.list.status.as_deref(), Some("name required"));
        assert_eq!(p.step(Key::Esc), Outcome::Exit);
    }

    #[test]
    fn render_styled_n1_paints_background_and_reapplies_after_highlight() {
        let st = Style::parse(Some("n1-dark"));
        let out = sample().render_styled(40, 8, &st);
        assert!(out.starts_with(&format!("\x1b[H{}\x1b[2J", st.base())));
        assert!(out.contains(&st.highlight_start()));
        assert!(out.contains(&format!("\x1b[0m{}", st.base())));
        assert!(!out.contains("\x1b[7m"));
        assert_eq!(
            sample().render_styled(40, 8, &Style::Terminal),
            sample().render(40, 8)
        );
    }
    #[test]
    fn render_shows_kinds_paths_and_marks_selection() {
        let p = sample();
        let s = p.render(60, 12);
        assert!(s.contains("workspace> _"));
        assert!(s.contains("  kind    name   root"));
        assert!(s.contains("live    alpha"));
        assert!(s.contains("saved   beta"));
        assert!(s.contains("folder  proj   /home/u/proj"));
        assert!(s.contains("\x1b[7m  live    alpha"));
        assert!(!s.contains("\x1b[7m  kind"));
        let mut q = sample();
        type_str(&mut q, "zz");
        assert!(q.render(60, 12).contains("new workspace zz"));
    }

    #[test]
    fn render_neutralises_control_characters_and_truncates() {
        let p = Picker::new(vec![cand(CandidateKind::Live, "a\x1b[31mb", None)], None);
        let s = p.render(60, 8);
        assert!(!s.contains("a\x1b[31m"));
        assert!(s.contains("a?[31mb"));
        let long = Picker::new(
            vec![cand(CandidateKind::Live, &"x".repeat(200), None)],
            None,
        );
        assert!(long.render(20, 8).lines().all(|l| l.chars().count() < 40));
    }
    #[test]
    fn args_parse_and_decode() {
        assert_eq!(parse_args(&[]).unwrap(), Parsed::Run(PickTarget::Workspace));
        assert_eq!(
            parse_args(&["--rename".into(), "a%3Ab%25".into()]).unwrap(),
            Parsed::Run(PickTarget::Rename("a:b%".into()))
        );
        assert_eq!(
            parse_args(&["--root".into(), "/a%3Ab".into()]).unwrap(),
            Parsed::Run(PickTarget::Root("/a:b".into()))
        );
        assert_eq!(
            parse_args(&["--root".into(), String::new()]).unwrap(),
            Parsed::Run(PickTarget::Root(String::new()))
        );
        assert!(parse_args(&["--root".into()]).is_err());
        let both: Vec<String> = ["--rename", "a", "--root", "b"].map(String::from).to_vec();
        assert!(
            parse_args(&both)
                .unwrap_err()
                .contains("cannot be combined")
        );
        assert!(parse_args(&["--rename".into(), "bad%zz".into()]).is_err());
        assert!(parse_args(&["--rename".into()]).is_err());
        assert!(parse_args(&["--nope".into()]).is_err());
    }

    #[test]
    fn help_never_eats_the_rename_value() {
        let a = |l: &[&str]| l.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(parse_args(&a(&["--help"])), Ok(Parsed::Help));
        assert_eq!(parse_args(&a(&["-h"])), Ok(Parsed::Help));
        // `--help` as the rename value is payload (and an invalid escape-free name).
        assert_eq!(
            parse_args(&a(&["--rename", "--help"])),
            Ok(Parsed::Run(PickTarget::Rename("--help".into())))
        );
        assert_eq!(
            parse_args(&a(&["--root", "--help"])),
            Ok(Parsed::Run(PickTarget::Root("--help".into())))
        );
    }

    fn root_picker(current: &str) -> Picker {
        Picker::root(
            current,
            vec![
                cand(CandidateKind::Live, "alpha", Some("/live")),
                cand(CandidateKind::Folder, "proj", Some("/home/u/proj")),
                cand(CandidateKind::Folder, "other", Some("/srv/other")),
            ],
            Some("/home/u".into()),
        )
    }

    #[test]
    fn root_prompt_prefills_with_tilde_and_lists_only_folders() {
        let p = root_picker("/home/u/code");
        assert_eq!(p.query(), "~/code");
        assert_eq!(root_picker("/home/user").query(), "/home/user");
        assert_eq!(root_picker("/home/u").query(), "~");
        assert_eq!(root_picker("").query(), "");
        assert_eq!(
            root_picker("").rows(),
            vec![Row::Candidate(0), Row::Candidate(1)]
        );
        assert!(root_picker("").render(60, 8).contains("root> _"));
        assert_eq!(
            expand_tilde("~/x", Some("/home/u/")).as_deref(),
            Some("/home/u/x")
        );
        assert_eq!(expand_tilde("~", None), None);
    }

    #[test]
    fn root_enter_precedence_empty_typed_row_hint() {
        // (a) empty query clears, even with a folder row selected.
        let mut p = root_picker("");
        assert_eq!(p.step(Key::Enter), Outcome::Send(Request::SetRoot(None)));
        // (b) the prefilled `~/x` sends itself, expanded, over the selected row.
        let mut p = root_picker("/home/u/x");
        assert_eq!(
            p.step(Key::Enter),
            Outcome::Send(Request::SetRoot(Some("/home/u/x".into())))
        );
        type_str(&mut p, "/y");
        assert_eq!(
            p.step(Key::Enter),
            Outcome::Send(Request::SetRoot(Some("/home/u/x/y".into())))
        );
        // (c) a non-path query selects the filtered folder row.
        let mut p = root_picker("");
        type_str(&mut p, "oth");
        assert_eq!(
            p.step(Key::Enter),
            Outcome::Send(Request::SetRoot(Some("/srv/other".into())))
        );
        // (d) nothing matches: hint, stay open.
        let mut p = root_picker("");
        type_str(&mut p, "zzz");
        assert_eq!(p.step(Key::Enter), Outcome::Continue);
        assert_eq!(p.status(), Some("type a path or pick a folder"));
    }

    #[test]
    fn live_and_saved_rows_show_their_root_with_tilde() {
        let p = Picker::new(
            vec![
                cand(CandidateKind::Live, "alpha", Some("/home/u/code")),
                cand(CandidateKind::Saved, "beta", Some("/srv/b")),
                cand(CandidateKind::Saved, "gamma", None),
            ],
            Some("/home/u".into()),
        );
        let s = p.render(60, 8);
        assert!(s.contains("live   alpha  ~/code"));
        assert!(s.contains("saved  beta   /srv/b"));
        assert!(s.contains("saved  gamma\r\n"));
    }
}
