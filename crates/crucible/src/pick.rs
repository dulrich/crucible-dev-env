//! `crucible pick [--rename <encoded>]` — the workspace picker TUI (run inside the compositor's
//! terminal overlay). Key decoding, the state machine and rendering are pure; only [`run`] touches
//! the terminal and the socket.

use std::io::{Read, Write};

use crucible_compositor::ipc::{
    Candidate, CandidateKind, Request, Response, SOCKET_ENV, decode_field, parse_candidates,
};

use crate::client::{Client, socket_from_env};
use crate::fuzzy;
use crate::style::Style;
use crate::tty::{RawGuard, window_size};

/// Usage text for `crucible pick --help`.
pub const USAGE: &str = "usage: crucible pick [--rename <encoded-name>]\n\nThe CDE workspace picker (needs $CDE_SOCKET; CDE runs it in an overlay on Super+P).\nType to filter; Up/Down or Ctrl+P/Ctrl+N move; Enter opens the selection (or creates a new\nworkspace from the typed name); Esc or Ctrl+C closes.\n\n  --rename <name>   single-line rename prompt prefilled with <name> (percent-encoded: %25 %3A %09 %0A %0D)\n";

/// A decoded key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A printable character.
    Char(char),
    /// Enter / Return.
    Enter,
    /// A lone Escape.
    Esc,
    /// Backspace.
    Backspace,
    /// Arrow up.
    Up,
    /// Arrow down.
    Down,
    /// Ctrl+C.
    CtrlC,
    /// Ctrl+D.
    CtrlD,
    /// Ctrl+N.
    CtrlN,
    /// Ctrl+P.
    CtrlP,
}

/// Decodes as many keys as possible from the front of `buf`, draining what it consumes. An
/// incomplete trailing escape sequence or UTF-8 character stays in the buffer for the next read.
pub fn decode_keys(buf: &mut Vec<u8>) -> Vec<Key> {
    let mut keys = Vec::new();
    while let Some(&b) = buf.first() {
        let consumed = match b {
            0x1b => match buf.get(1) {
                None => {
                    keys.push(Key::Esc);
                    1
                }
                Some(b'[') => {
                    // CSI: parameters, then a final byte 0x40..=0x7e.
                    let Some(end) = buf[2..].iter().position(|c| (0x40..=0x7e).contains(c)) else {
                        break;
                    };
                    let end = end + 2;
                    match (end, buf[end]) {
                        (2, b'A') => keys.push(Key::Up),
                        (2, b'B') => keys.push(Key::Down),
                        _ => {}
                    }
                    end + 1
                }
                Some(b'O') => {
                    let Some(&f) = buf.get(2) else { break };
                    match f {
                        b'A' => keys.push(Key::Up),
                        b'B' => keys.push(Key::Down),
                        _ => {}
                    }
                    3
                }
                Some(_) => {
                    keys.push(Key::Esc);
                    1
                }
            },
            b'\r' | b'\n' => {
                keys.push(Key::Enter);
                1
            }
            0x03 => {
                keys.push(Key::CtrlC);
                1
            }
            0x04 => {
                keys.push(Key::CtrlD);
                1
            }
            0x0e => {
                keys.push(Key::CtrlN);
                1
            }
            0x10 => {
                keys.push(Key::CtrlP);
                1
            }
            0x08 | 0x7f => {
                keys.push(Key::Backspace);
                1
            }
            0x00..=0x1f => 1,
            _ => {
                let need = match b {
                    0x20..=0x7f => 1,
                    0xc0..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf7 => 4,
                    _ => {
                        buf.remove(0);
                        continue;
                    }
                };
                if buf.len() < need {
                    break;
                }
                match std::str::from_utf8(&buf[..need])
                    .ok()
                    .and_then(|s| s.chars().next())
                {
                    Some(c) => {
                        keys.push(Key::Char(c));
                        need
                    }
                    None => 1,
                }
            }
        };
        buf.drain(..consumed);
    }
    keys
}

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

/// What the driver must do after a key or response.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Redraw and keep reading.
    Continue,
    /// Leave the picker with exit code 0.
    Exit,
    /// Send this request, then feed the response to [`Picker::on_response`].
    Send(Request),
}

/// Picker state.
#[derive(Debug)]
pub struct Picker {
    mode: Mode,
    candidates: Vec<Candidate>,
    query: String,
    selected: usize,
    status: Option<String>,
}

impl Picker {
    /// A fuzzy picker over `candidates`.
    #[must_use]
    pub fn new(candidates: Vec<Candidate>) -> Picker {
        Picker {
            mode: Mode::Pick,
            candidates,
            query: String::new(),
            selected: 0,
            status: None,
        }
    }

    /// A rename prompt prefilled with `current`.
    #[must_use]
    pub fn rename(current: String) -> Picker {
        Picker {
            mode: Mode::Rename,
            candidates: Vec::new(),
            query: current,
            selected: 0,
            status: None,
        }
    }

    /// The current input line.
    #[cfg(test)]
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// The status line, if any.
    #[cfg(test)]
    #[must_use]
    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    /// Visible rows: fuzzy matches best first, then a "new workspace" row whenever the query is
    /// non-empty and is not exactly a live/saved workspace name.
    #[must_use]
    pub fn rows(&self) -> Vec<Row> {
        if self.mode == Mode::Rename {
            return Vec::new();
        }
        let names: Vec<&str> = self.candidates.iter().map(|c| c.name.as_str()).collect();
        let mut rows: Vec<Row> = fuzzy::filter(&self.query, &names)
            .into_iter()
            .map(Row::Candidate)
            .collect();
        let exact = self
            .candidates
            .iter()
            .any(|c| c.kind != CandidateKind::Folder && c.name == self.query);
        if !self.query.is_empty() && !exact {
            rows.push(Row::New(self.query.clone()));
        }
        rows
    }

    /// Applies one key.
    pub fn step(&mut self, key: Key) -> Outcome {
        match key {
            Key::Esc | Key::CtrlC => return Outcome::Exit,
            Key::CtrlD if self.query.is_empty() => return Outcome::Exit,
            Key::CtrlD => {}
            Key::Up | Key::CtrlP => self.selected = self.selected.saturating_sub(1),
            Key::Down | Key::CtrlN => {
                let last = self.rows().len().saturating_sub(1);
                self.selected = (self.selected + 1).min(last);
            }
            Key::Char(c) => {
                self.query.push(c);
                self.selected = 0;
                self.status = None;
            }
            Key::Backspace => {
                self.query.pop();
                self.selected = 0;
                self.status = None;
            }
            Key::Enter => return self.enter(),
        }
        Outcome::Continue
    }

    fn enter(&mut self) -> Outcome {
        if self.mode == Mode::Rename {
            if self.query.is_empty() {
                self.status = Some("name required".into());
                return Outcome::Continue;
            }
            return Outcome::Send(Request::Rename(self.query.clone()));
        }
        match self.rows().get(self.selected) {
            Some(Row::Candidate(i)) => {
                let c = &self.candidates[*i];
                match (c.kind, &c.path) {
                    (CandidateKind::Folder, Some(p)) => {
                        Outcome::Send(Request::OpenFolder(p.clone()))
                    }
                    (CandidateKind::Folder, None) => {
                        self.status = Some("folder has no path".into());
                        Outcome::Continue
                    }
                    _ => Outcome::Send(Request::Open(c.name.clone())),
                }
            }
            Some(Row::New(n)) => Outcome::Send(Request::New(n.clone())),
            None => Outcome::Continue,
        }
    }

    /// Applies the compositor's answer to the last [`Outcome::Send`].
    pub fn on_response(&mut self, resp: &Response) -> Outcome {
        match resp {
            Response::Ok(_) => Outcome::Exit,
            Response::Err(reason) => {
                self.status = Some(reason.clone());
                Outcome::Continue
            }
        }
    }

    /// Renders the whole screen for a `cols` x `rows` terminal (raw-mode line endings).
    #[cfg(test)]
    #[must_use]
    pub fn render(&self, cols: u16, rows: u16) -> String {
        self.render_styled(cols, rows, &Style::Terminal)
    }

    /// Like [`Self::render`], painted with the overlay `style`.
    #[must_use]
    pub fn render_styled(&self, cols: u16, rows: u16, style: &Style) -> String {
        let width = usize::from(cols).max(10);
        let height = usize::from(rows).max(4);
        let prompt = match self.mode {
            Mode::Pick => "workspace> ",
            Mode::Rename => "rename> ",
        };
        let mut lines: Vec<String> =
            vec![fit(&format!("{prompt}{}_", sanitize(&self.query)), width)];
        let all = self.rows();
        let list_height = height - 2;
        let (start, end) = window(self.selected, all.len(), list_height);
        for (i, row) in all.iter().enumerate().take(end).skip(start) {
            let text = fit(&format!("  {}", self.row_text(row)), width);
            lines.push(if i == self.selected {
                style.highlight(&text)
            } else {
                text
            });
        }
        while lines.len() < height - 1 {
            lines.push(String::new());
        }
        lines.push(fit(self.status.as_deref().map_or("", |s| s), width));
        let mut out = style.frame_start();
        out.push_str(&lines.join("\r\n"));
        out
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
                match (c.kind, &c.path) {
                    (CandidateKind::Folder, Some(p)) => {
                        format!(
                            "{kind} {}  {}",
                            sanitize(&c.name),
                            sanitize(&p.to_string_lossy())
                        )
                    }
                    _ => format!("{kind} {}", sanitize(&c.name)),
                }
            }
        }
    }
}

/// Replaces control characters so a name can never inject terminal escapes.
pub(crate) fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// Truncates to `width` characters.
pub(crate) fn fit(s: &str, width: usize) -> String {
    s.chars().take(width).collect()
}

/// The `[start, end)` slice of `total` rows to show in `height` lines, keeping `selected` visible.
#[must_use]
pub fn window(selected: usize, total: usize, height: usize) -> (usize, usize) {
    if height == 0 || total <= height {
        return (0, total);
    }
    let start = selected.saturating_sub(height - 1).min(total - height);
    (start, start + height)
}

/// Parsed `pick` arguments: the decoded rename seed, if any.
///
/// # Errors
/// A usage message for unknown flags, a missing value or a bad percent escape.
pub fn parse_args(args: &[String]) -> Result<Option<String>, String> {
    match args {
        [] => Ok(None),
        [flag, value] if flag == "--rename" => decode_field(value)
            .map(Some)
            .ok_or_else(|| "malformed percent-encoding in --rename value".to_string()),
        [flag] if flag == "--rename" => Err("--rename needs a value".into()),
        [other, ..] => Err(format!("unexpected argument {other:?}")),
    }
}

/// Runs the subcommand; returns the exit code.
pub fn run(args: &[String]) -> i32 {
    let rename = match parse_args(args) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("crucible pick: {e}\n\n{USAGE}");
            return 2;
        }
    };
    let path = match socket_from_env(std::env::var_os(SOCKET_ENV)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("crucible pick: {e}");
            return 2;
        }
    };
    let mut client = match Client::connect(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("crucible pick: cannot connect to {}: {e}", path.display());
            return 1;
        }
    };
    let mut picker = match rename {
        Some(current) => Picker::rename(current),
        None => match fetch_candidates(&mut client) {
            Ok(c) => Picker::new(c),
            Err(e) => {
                eprintln!("crucible pick: {e}");
                return 1;
            }
        },
    };
    let Some(guard) = RawGuard::enter(true) else {
        eprintln!("crucible pick: stdin is not a terminal");
        return 1;
    };
    let code = event_loop(&mut picker, &mut client);
    drop(guard);
    code
}

fn fetch_candidates(client: &mut Client) -> Result<Vec<Candidate>, String> {
    let line = client
        .request(&Request::Candidates.to_line())
        .map_err(|e| format!("candidates request failed: {e}"))?;
    match Response::parse(&line) {
        Some(Response::Ok(payload)) => {
            parse_candidates(&payload).ok_or_else(|| "malformed candidates response".to_string())
        }
        Some(Response::Err(r)) => Err(format!("compositor refused: {r}")),
        None => Err(format!("unrecognised response {line:?}")),
    }
}

fn event_loop(picker: &mut Picker, client: &mut Client) -> i32 {
    let style = Style::from_env();
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 256];
    let mut stdin = std::io::stdin();
    loop {
        let (cols, rows) = window_size();
        let mut out = std::io::stdout();
        let _ = out.write_all(picker.render_styled(cols, rows, &style).as_bytes());
        let _ = out.flush();
        let n = match stdin.read(&mut chunk) {
            Ok(0) => return 0,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return 1,
        };
        pending.extend_from_slice(&chunk[..n]);
        for key in decode_keys(&mut pending) {
            let mut outcome = picker.step(key);
            if let Outcome::Send(req) = &outcome {
                outcome = match client.request(&req.to_line()) {
                    Ok(line) => match Response::parse(&line) {
                        Some(resp) => picker.on_response(&resp),
                        None => {
                            picker.on_response(&Response::Err(format!("bad response {line:?}")))
                        }
                    },
                    Err(e) => picker.on_response(&Response::Err(format!("compositor error: {e}"))),
                };
            }
            if outcome == Outcome::Exit {
                return 0;
            }
        }
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
        }
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

    fn keys(bytes: &[u8]) -> Vec<Key> {
        decode_keys(&mut bytes.to_vec())
    }

    #[test]
    fn decodes_basic_keys() {
        assert_eq!(
            keys(b"a\x7f\r\x03\x04\x0e\x10"),
            vec![
                Key::Char('a'),
                Key::Backspace,
                Key::Enter,
                Key::CtrlC,
                Key::CtrlD,
                Key::CtrlN,
                Key::CtrlP
            ]
        );
    }

    #[test]
    fn decodes_arrows_and_esc() {
        assert_eq!(keys(b"\x1b[A\x1b[B"), vec![Key::Up, Key::Down]);
        assert_eq!(keys(b"\x1bOA\x1bOB"), vec![Key::Up, Key::Down]);
        assert_eq!(keys(b"\x1b"), vec![Key::Esc]);
        assert_eq!(keys(b"\x1b[1;5C"), vec![]);
    }

    #[test]
    fn decodes_utf8_and_keeps_partial_input() {
        assert_eq!(keys("é".as_bytes()), vec![Key::Char('é')]);
        let mut buf = vec![0xc3];
        assert!(decode_keys(&mut buf).is_empty());
        assert_eq!(buf, vec![0xc3]);
        buf.push(0xa9);
        assert_eq!(decode_keys(&mut buf), vec![Key::Char('é')]);
        assert!(buf.is_empty());
        let mut csi = b"\x1b[".to_vec();
        assert!(decode_keys(&mut csi).is_empty());
        assert_eq!(csi.len(), 2);
    }

    #[test]
    fn invalid_utf8_does_not_wedge() {
        let mut buf = vec![0xff, b'a'];
        assert_eq!(decode_keys(&mut buf), vec![Key::Char('a')]);
        assert!(buf.is_empty());
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
    fn window_keeps_selection_visible() {
        assert_eq!(window(0, 3, 10), (0, 3));
        assert_eq!(window(0, 20, 5), (0, 5));
        assert_eq!(window(4, 20, 5), (0, 5));
        assert_eq!(window(5, 20, 5), (1, 6));
        assert_eq!(window(19, 20, 5), (15, 20));
    }

    #[test]
    fn args_parse_and_decode() {
        assert_eq!(parse_args(&[]).unwrap(), None);
        assert_eq!(
            parse_args(&["--rename".into(), "a%3Ab%25".into()]).unwrap(),
            Some("a:b%".into())
        );
        assert!(parse_args(&["--rename".into(), "bad%zz".into()]).is_err());
        assert!(parse_args(&["--rename".into()]).is_err());
        assert!(parse_args(&["--nope".into()]).is_err());
    }
}
