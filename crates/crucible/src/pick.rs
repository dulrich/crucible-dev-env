//! `crucible pick [--rename <encoded>]` — the workspace picker TUI (run inside the compositor's
//! terminal overlay). Only the picker's domain hooks live here (rename mode, candidate rows, what
//! Enter sends); the runner, key decoding, shared state and frame scaffolding are in
//! [`crate::overlay`]. The state machine and rendering are pure.

use crucible_compositor::ipc::{
    Candidate, CandidateKind, Request, Response, decode_field, parse_candidates,
};

use crate::client::{Client, ClientError};
use crate::fuzzy;
use crate::overlay::list::{Applied, Body, ListState};
use crate::overlay::term::{Key, sanitize};
use crate::overlay::{Outcome, Overlay, run_overlay};
use crate::style::Style;
use crate::{Parsed, help_requested};

/// The command-key glyph (`⌘`) prefixing a pinned row's slot number; matches the header's.
const SUPER_GLYPH: char = '\u{2318}';

/// Usage text for `crucible pick --help`.
pub const USAGE: &str = "usage: crucible pick [--rename <encoded-name>]\n\nThe CDE workspace picker (needs $CDE_SOCKET; CDE runs it in an overlay on Super+P).\nType to filter; Up/Down or Ctrl+P/Ctrl+N move; Enter opens the selection (or creates a new\nworkspace from the typed name); Esc or Ctrl+C closes.\n\n  --rename <name>   single-line rename prompt prefilled with <name> (percent-encoded: %25 %3A %09 %0A %0D)\n";

/// Picker or rename-prompt behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Fuzzy workspace picker.
    Pick,
    /// Single-line rename prompt.
    Rename,
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
}

impl Picker {
    /// A fuzzy picker over `candidates`.
    #[must_use]
    pub fn new(candidates: Vec<Candidate>) -> Picker {
        Picker {
            mode: Mode::Pick,
            candidates,
            list: ListState::new(),
        }
    }

    /// A rename prompt prefilled with `current`.
    #[must_use]
    pub fn rename(current: String) -> Picker {
        Picker {
            mode: Mode::Rename,
            candidates: Vec::new(),
            list: ListState::with_query(current),
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
        if self.mode == Mode::Rename {
            return Vec::new();
        }
        let query = &self.list.query;
        let names: Vec<&str> = self.candidates.iter().map(|c| c.name.as_str()).collect();
        let mut rows: Vec<Row> = fuzzy::filter(query, &names)
            .into_iter()
            .map(Row::Candidate)
            .collect();
        let exact = self
            .candidates
            .iter()
            .any(|c| c.kind != CandidateKind::Folder && c.name == *query);
        if !query.is_empty() && !exact {
            rows.push(Row::New(query.clone()));
        }
        rows
    }

    /// Enter / Alt+Enter: rename the workspace, or open / create the selected row.
    fn enter(&mut self) -> Outcome {
        if self.mode == Mode::Rename {
            if self.list.query.is_empty() {
                self.list.status = Some("name required".into());
                return Outcome::Continue;
            }
            return Outcome::Send(Request::Rename(self.list.query.clone()));
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

    /// Renders the whole screen for a `cols` x `rows` terminal (raw-mode line endings).
    #[cfg(test)]
    #[must_use]
    pub fn render(&self, cols: u16, rows: u16) -> String {
        self.render_styled(cols, rows, &Style::Terminal)
    }

    fn row_text(&self, row: &Row) -> String {
        match row {
            Row::New(n) => format!("new workspace {}", sanitize(n)),
            Row::Candidate(i) => {
                let c = &self.candidates[*i];
                let kind = match c.kind {
                    CandidateKind::Live => "live  ",
                    CandidateKind::Saved => "saved ",
                    CandidateKind::Folder => "folder",
                };
                let badge = c
                    .slot
                    .map_or(String::new(), |n| format!("{SUPER_GLYPH}{n} "));
                match (c.kind, &c.path) {
                    (CandidateKind::Folder, Some(p)) => {
                        format!(
                            "{kind} {badge}{}  {}",
                            sanitize(&c.name),
                            sanitize(&p.to_string_lossy())
                        )
                    }
                    _ => format!("{kind} {badge}{}", sanitize(&c.name)),
                }
            }
        }
    }
}

impl Overlay for Picker {
    fn step(&mut self, key: Key) -> Outcome {
        let count = self.rows().len();
        match self.list.apply(key, count) {
            Applied::Exit => Outcome::Exit,
            Applied::Edited => {
                self.list.status = None;
                Outcome::Continue
            }
            Applied::Moved | Applied::Ignored => Outcome::Continue,
            Applied::Enter(_) => self.enter(),
        }
    }

    fn list_mut(&mut self) -> &mut ListState {
        &mut self.list
    }

    fn render_styled(&self, cols: u16, rows: u16, style: &Style) -> String {
        let prompt = match self.mode {
            Mode::Pick => "workspace> ",
            Mode::Rename => "rename> ",
        };
        let texts: Vec<String> = self.rows().iter().map(|r| self.row_text(r)).collect();
        self.list
            .render(prompt, Body::Rows(&texts), "", cols, rows, style)
    }
}

/// Parsed `pick` arguments: the decoded rename seed, if any (`--help`/`-h` outside the
/// `--rename` value is [`Parsed::Help`]).
///
/// # Errors
/// A usage message for unknown flags, a missing value or a bad percent escape.
pub fn parse_args(args: &[String]) -> Result<Parsed<Option<String>>, String> {
    if help_requested(args, Some("--rename"), false) {
        return Ok(Parsed::Help);
    }
    match args {
        [] => Ok(Parsed::Run(None)),
        [flag, value] if flag == "--rename" => decode_field(value)
            .map(|v| Parsed::Run(Some(v)))
            .ok_or_else(|| "malformed percent-encoding in --rename value".to_string()),
        [flag] if flag == "--rename" => Err("--rename needs a value".into()),
        [other, ..] => Err(format!("unexpected argument {other:?}")),
    }
}

/// Runs the subcommand; returns the exit code.
pub fn run(args: &[String]) -> i32 {
    let rename = match parse_args(args) {
        Ok(Parsed::Run(r)) => r,
        Ok(Parsed::Help) => {
            print!("{USAGE}");
            return 0;
        }
        Err(e) => {
            eprintln!("crucible pick: {e}\n\n{USAGE}");
            return 2;
        }
    };
    run_overlay("pick", |client| match rename {
        Some(current) => Ok(Picker::rename(current)),
        None => fetch_candidates(client).map(Picker::new),
    })
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
    use std::path::PathBuf;

    fn cand(kind: CandidateKind, name: &str, path: Option<&str>) -> Candidate {
        Candidate {
            kind,
            name: name.into(),
            path: path.map(PathBuf::from),
            slot: None,
        }
    }

    #[test]
    fn row_text_shows_slot_badge_only_when_present() {
        let mut p = sample();
        let live = p
            .rows()
            .iter()
            .position(|r| matches!(r, Row::Candidate(0)))
            .unwrap();
        assert_eq!(p.row_text(&p.rows()[live]), "live   alpha");
        p.candidates[0].slot = Some(3);
        assert_eq!(p.row_text(&p.rows()[live]), "live   \u{2318}3 alpha");
    }

    fn sample() -> Picker {
        Picker::new(vec![
            cand(CandidateKind::Live, "alpha", None),
            cand(CandidateKind::Saved, "beta", None),
            cand(CandidateKind::Folder, "proj", Some("/home/u/proj")),
        ])
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
        let mut p = Picker::new(vec![]);
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
        let mut p = Picker::rename("old name".into());
        assert_eq!(p.query(), "old name");
        assert!(p.rows().is_empty());
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
        let mut p = Picker::rename("a".into());
        p.step(Key::Backspace);
        assert_eq!(p.step(Key::Enter), Outcome::Continue);
        assert_eq!(p.status(), Some("name required"));
        assert_eq!(p.step(Key::Esc), Outcome::Exit);
    }

    #[test]
    fn render_styled_n1_paints_background_and_reapplies_after_highlight() {
        let st = Style::parse(Some("n1-dark"));
        let out = sample().render_styled(40, 8, &st);
        assert!(out.starts_with(&format!("\x1b[H{}\x1b[2J", st.base())));
        assert!(out.contains(&st.highlight("")[..20]));
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
        assert!(s.contains("live   alpha"));
        assert!(s.contains("saved  beta"));
        assert!(s.contains("folder proj  /home/u/proj"));
        assert!(s.contains("\x1b[7m  live   alpha"));
        let mut q = sample();
        type_str(&mut q, "zz");
        assert!(q.render(60, 12).contains("new workspace zz"));
    }

    #[test]
    fn render_neutralises_control_characters_and_truncates() {
        let p = Picker::new(vec![cand(CandidateKind::Live, "a\x1b[31mb", None)]);
        let s = p.render(60, 8);
        assert!(!s.contains("a\x1b[31m"));
        assert!(s.contains("a?[31mb"));
        let long = Picker::new(vec![cand(CandidateKind::Live, &"x".repeat(200), None)]);
        assert!(long.render(20, 8).lines().all(|l| l.chars().count() < 40));
    }
    #[test]
    fn args_parse_and_decode() {
        assert_eq!(parse_args(&[]).unwrap(), Parsed::Run(None));
        assert_eq!(
            parse_args(&["--rename".into(), "a%3Ab%25".into()]).unwrap(),
            Parsed::Run(Some("a:b%".into()))
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
            Ok(Parsed::Run(Some("--help".into())))
        );
    }
}
