//! `crucible keys` — the settings overlay: lists the compositor's effective keybindings
//! (fetched over the `bindings` IPC request); Enter runs the selected one (`exec`). Only the
//! binding rows and Enter live here; the runner and shared state are in [`crate::overlay`]. The
//! state machine and rendering are pure.

use crucible_compositor::ipc::{Request, Response, parse_bindings};
use crucible_keys::{Action, Area};

use crate::cli::{Parsed, help_requested};
use crate::client::{Client, ClientError};
use crate::fuzzy;
use crate::overlay::list::{Body, Frame, Line, ListState};
use crate::overlay::term::{Key, sanitize};
use crate::overlay::{Outcome, Overlay, run_overlay};
use crate::style::Style;
use crate::table::{self, Col};

/// Usage text for `crucible keys --help`.
pub const USAGE: &str = "usage: crucible keys\n\nThe CDE settings overlay (needs $CDE_SOCKET; CDE runs it in an overlay on Super+Comma).\nLists the effective keybindings, grouped by area until you type. Type to filter; Up/Down or Ctrl+P/Ctrl+N move; Enter runs the\nselected binding; Esc or Ctrl+C closes. Edit binds in config.kdl (docs/config.md).\n";

/// Status line shown under the rows.
const STATUS: &str = "Enter runs the selected binding — edit binds in config.kdl (docs/config.md)";

/// Parses the reply to `bindings` into `(chord, action)` rows.
///
/// # Errors
/// The compositor's `err` reason, or a description of a malformed reply.
pub fn parse_reply(resp: &Response) -> Result<Vec<(String, String)>, String> {
    match resp {
        Response::Ok(payload) => parse_bindings(payload)
            .ok_or_else(|| format!("malformed bindings reply {:?}", resp.to_line())),
        Response::Err(reason) => Err(reason.clone()),
    }
}

/// Fetches the effective bindings over the typed `bindings` request.
fn fetch_bindings(client: &mut Client) -> Result<Vec<(String, String)>, String> {
    match client.send(&Request::Bindings) {
        Ok(resp) => parse_reply(&resp),
        Err(ClientError::BadResponse(line)) => Err(format!("bad response {line:?}")),
        Err(e) => Err(format!("compositor error: {e}")),
    }
}

/// The heading of the trailing group for actions that do not parse back to an [`Action`].
const OTHER_LABEL: &str = "Other";

/// The area of an action display string, derived by parsing it back through `Action::from_str`
/// (no wire change: the `bindings` reply is unchanged). `None` when it does not parse.
#[must_use]
pub fn area_of(action: &str) -> Option<Area> {
    action.parse::<Action>().ok().map(|a| a.area())
}

/// One line of the empty-query view: a non-selectable heading or a binding row (index into rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    Heading(&'static str),
    Row(usize),
}

/// Settings-overlay state.
#[derive(Debug)]
pub struct Keys {
    rows: Vec<(String, String)>,
    /// Display-order row indices (area order; rows inside an area keep list order) paired with
    /// their group label.
    grouped: Vec<(&'static str, usize)>,
    list: ListState,
}

impl Keys {
    /// A viewer over `rows` (`(chord, action)` display pairs).
    #[must_use]
    pub fn new(rows: Vec<(String, String)>) -> Keys {
        // Stable sort by area position; unparseable actions (`None`) go last as "Other".
        let mut grouped: Vec<(usize, &'static str, usize)> = rows
            .iter()
            .enumerate()
            .map(|(i, (_, a))| match area_of(a) {
                Some(area) => (area as usize, area.label(), i),
                None => (usize::MAX, OTHER_LABEL, i),
            })
            .collect();
        grouped.sort_by_key(|&(order, _, _)| order);
        let grouped = grouped.into_iter().map(|(_, l, i)| (l, i)).collect();
        let mut keys = Keys {
            rows,
            grouped,
            list: ListState::new(),
        };
        keys.snap_to_row();
        keys
    }

    /// What is on screen now: headings and rows under area headings with an empty query, else
    /// the flat fuzzy matches best first.
    fn entries(&self) -> Vec<Entry> {
        if !self.list.query.is_empty() {
            return self.matches().into_iter().map(Entry::Row).collect();
        }
        let mut out = Vec::new();
        let mut current = None;
        for &(label, idx) in &self.grouped {
            if current != Some(label) {
                out.push(Entry::Heading(label));
                current = Some(label);
            }
            out.push(Entry::Row(idx));
        }
        out
    }

    /// Moves the selection off a heading to the next row (the first real row after a reset).
    fn snap_to_row(&mut self) {
        let entries = self.entries();
        while matches!(entries.get(self.list.selected), Some(Entry::Heading(_))) {
            self.list.selected += 1;
        }
    }

    /// Moves the selection one selectable row `down` or up, skipping headings; stays put at the
    /// ends.
    fn move_over_headings(&mut self, down: bool) {
        let entries = self.entries();
        let mut i = self.list.selected;
        loop {
            let next = if down {
                i + 1
            } else {
                match i.checked_sub(1) {
                    Some(n) => n,
                    None => return,
                }
            };
            match entries.get(next) {
                None => return,
                Some(Entry::Heading(_)) => i = next,
                Some(Entry::Row(_)) => {
                    self.list.selected = next;
                    return;
                }
            }
        }
    }

    /// Indices of the rows matching the query, best first (ties keep list order).
    #[must_use]
    pub fn matches(&self) -> Vec<usize> {
        let texts: Vec<String> = self.rows.iter().map(|(c, a)| format!("{c}  {a}")).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        fuzzy::filter(&self.list.query, &refs)
    }
}

impl Overlay for Keys {
    fn list_mut(&mut self) -> &mut ListState {
        &mut self.list
    }

    fn row_count(&self) -> usize {
        self.entries().len()
    }

    /// With an empty query Up/Down/Ctrl+P/Ctrl+N skip the headings.
    fn intercept(&mut self, key: Key) -> Option<Outcome> {
        if self.list.query.is_empty()
            && matches!(key, Key::Up | Key::CtrlP | Key::Down | Key::CtrlN)
        {
            self.move_over_headings(matches!(key, Key::Down | Key::CtrlN));
            return Some(Outcome::Continue);
        }
        None
    }

    /// Clearing the query lands on the first real row. Typing keeps a refusal visible (the status
    /// clears only when Enter runs a binding).
    fn after_edit(&mut self) {
        self.snap_to_row();
    }

    /// Enter sends the selected row's action for execution.
    fn enter(&mut self, _key: Key) -> Outcome {
        if let Some(&Entry::Row(idx)) = self.entries().get(self.list.selected) {
            self.list.status = None;
            return Outcome::Send(Request::Exec(self.rows[idx].1.clone()));
        }
        Outcome::Continue
    }

    /// The chord column is padded to the widest chord over all rows, so the layout is computed
    /// once over every row and the matching lines are selected.
    fn render_styled(&self, cols: u16, rows: u16, style: &Style) -> String {
        let cells: Vec<Vec<String>> = self
            .rows
            .iter()
            .map(|(c, a)| vec![sanitize(c), sanitize(a)])
            .collect();
        let (_, lines) = table::layout(&[Col::left(""), Col::left("")], &cells);
        let texts: Vec<Line> = self
            .entries()
            .iter()
            .map(|e| match e {
                Entry::Heading(label) => Line::Heading((*label).to_string()),
                Entry::Row(idx) => Line::Row(lines[*idx].clone()),
            })
            .collect();
        let frame = Frame {
            prompt: "keys> ",
            header: None,
            body: Body::Rows(&texts),
            idle_status: STATUS,
        };
        self.list.render(frame, cols, rows, style)
    }
}

/// Runs the subcommand; returns the exit code.
pub fn run(args: &[String]) -> i32 {
    match crate::cli::parse_or_exit("keys", USAGE, parse_args(args)) {
        Ok(()) => run_overlay("keys", |client| fetch_bindings(client).map(Keys::new)),
        Err(code) => code,
    }
}

/// Parses the arguments: `--help`/`-h` anywhere is [`Parsed::Help`]; any other argument is an
/// error.
///
/// # Errors
/// A usage message naming the unexpected argument.
pub fn parse_args(args: &[String]) -> Result<Parsed<()>, String> {
    if help_requested(args, &[], false) {
        return Ok(Parsed::Help);
    }
    match args.first() {
        Some(extra) => Err(format!("unexpected argument {extra:?}")),
        None => Ok(Parsed::Run(())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_owns_help_and_rejects_everything_else() {
        let a = |l: &[&str]| l.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(parse_args(&a(&[])), Ok(Parsed::Run(())));
        assert_eq!(parse_args(&a(&["--help"])), Ok(Parsed::Help));
        assert_eq!(parse_args(&a(&["x", "-h"])), Ok(Parsed::Help));
        assert!(parse_args(&a(&["x"])).is_err());
    }

    fn rows() -> Vec<(String, String)> {
        [
            ("Super+Return", "open-launcher"),
            ("Super+Shift+Q", "quit"),
            ("Super+W", "close-pane"),
        ]
        .iter()
        .map(|(c, a)| ((*c).to_string(), (*a).to_string()))
        .collect()
    }

    #[test]
    fn typing_filters_and_backspace_edits() {
        let mut k = Keys::new(rows());
        assert_eq!(k.matches().len(), 3);
        for c in "quit".chars() {
            k.step(Key::Char(c));
        }
        assert_eq!(k.matches(), [1]);
        for _ in 0..4 {
            k.step(Key::Backspace);
        }
        assert_eq!(k.matches().len(), 3);
    }

    #[test]
    fn selection_moves_over_headings_and_clamps() {
        // Entries: [H Panes, close-pane, H Launch, open-launcher, H Session, quit].
        let mut k = Keys::new(rows());
        assert_eq!(k.list.selected, 1, "starts on the first real row");
        k.step(Key::Up);
        assert_eq!(k.list.selected, 1);
        k.step(Key::Down);
        assert_eq!(k.list.selected, 3);
        k.step(Key::CtrlN);
        assert_eq!(k.list.selected, 5);
        k.step(Key::Down);
        assert_eq!(k.list.selected, 5);
        k.step(Key::CtrlP);
        assert_eq!(k.list.selected, 3);
        k.step(Key::Up);
        assert_eq!(k.list.selected, 1);
    }

    #[test]
    fn empty_query_groups_rows_under_headings_in_area_order() {
        let mut r = rows();
        r.push(("Super+Z".into(), "frobnicate".into()));
        r.push(("Super+H".into(), "focus left".into()));
        let k = Keys::new(r);
        let heads: Vec<_> = k
            .entries()
            .iter()
            .filter_map(|e| match e {
                Entry::Heading(h) => Some(*h),
                Entry::Row(_) => None,
            })
            .collect();
        assert_eq!(
            heads,
            [
                "Panes & windows",
                "Launch & overlays",
                "Session & system",
                "Other"
            ]
        );
        // Inside Panes & windows, list order is kept: close-pane (2) before focus left (4).
        assert_eq!(
            &k.entries()[..3],
            [
                Entry::Heading("Panes & windows"),
                Entry::Row(2),
                Entry::Row(4)
            ]
        );
        let out = k.render(80, 20);
        assert!(
            out.contains("Panes & windows") && out.contains("Other"),
            "{out:?}"
        );
    }

    #[test]
    fn headings_sit_at_prompt_level_and_rows_two_columns_in() {
        let out = Keys::new(rows()).render(40, 10);
        let plain = out.replace("\x1b[7m", "").replace("\x1b[0m", "");
        let lines: Vec<&str> = plain
            .strip_prefix("\x1b[H\x1b[2J")
            .unwrap()
            .split("\r\n")
            .collect();
        assert_eq!(
            &lines[..8],
            [
                "",
                "  keys> _",
                "  Panes & windows",
                "    Super+W        close-pane",
                "  Launch & overlays",
                "    Super+Return   open-launcher",
                "  Session & system",
                "    Super+Shift+Q  quit",
            ]
        );
        assert!(
            !out.contains("\x1b[7m Panes"),
            "headings are never highlighted"
        );
    }

    #[test]
    fn a_query_flattens_to_the_ranked_list_without_headings() {
        let mut k = Keys::new(rows());
        for c in "quit".chars() {
            k.step(Key::Char(c));
        }
        assert_eq!(k.entries(), [Entry::Row(1)]);
        assert_eq!(k.list.selected, 0);
        assert!(!k.render(80, 10).contains("Session & system"));
        for _ in 0..4 {
            k.step(Key::Backspace);
        }
        assert_eq!(k.list.selected, 1, "clearing the query lands on a real row");
    }

    #[test]
    fn area_is_derived_by_parsing_the_action_display_back() {
        let reply = |line: &str| parse_reply(&Response::parse(line).unwrap()).unwrap();
        let rows = reply("ok Super+W:close-pane\tSuper+F2:run x%3Ay\tSuper+X:bogus verb");
        let areas: Vec<_> = rows.iter().map(|(_, a)| area_of(a)).collect();
        assert_eq!(
            areas,
            [
                Some(Area::PanesAndWindows),
                Some(Area::LaunchAndOverlays),
                None
            ]
        );
        for action in [
            Action::DeleteWorkspace,
            Action::ReorderWorkspace(-1),
            Action::CycleStack(1),
        ] {
            assert_eq!(area_of(&action.to_string()), Some(action.area()));
        }
    }

    #[test]
    fn enter_sends_exec_for_the_selected_row_and_esc_exits() {
        let mut k = Keys::new(rows());
        assert_eq!(
            k.step(Key::Enter),
            Outcome::Send(Request::Exec("close-pane".into()))
        );
        k.step(Key::Down);
        assert_eq!(
            k.step(Key::AltEnter),
            Outcome::Send(Request::Exec("open-launcher".into()))
        );
        assert_eq!(k.list.selected, 3);
        assert_eq!(k.step(Key::Esc), Outcome::Exit);
        assert_eq!(k.step(Key::CtrlC), Outcome::Exit);
        assert_eq!(k.step(Key::CtrlD), Outcome::Exit);
        k.step(Key::Char('x'));
        assert_eq!(k.step(Key::CtrlD), Outcome::Continue);
    }
    #[test]
    fn enter_with_no_match_does_nothing() {
        let mut k = Keys::new(rows());
        for c in "zzzz".chars() {
            k.step(Key::Char(c));
        }
        assert_eq!(k.step(Key::Enter), Outcome::Continue);
    }

    #[test]
    fn responses_exit_on_ok_and_stay_open_with_status_on_err() {
        let mut k = Keys::new(rows());
        assert_eq!(
            k.on_response(&Response::Err("boom".into())),
            Outcome::Continue
        );
        assert!(k.render(80, 10).contains("boom"));
        assert_eq!(k.on_response(&Response::Ok(String::new())), Outcome::Exit);
    }

    #[test]
    fn render_styled_n1_paints_background_and_reapplies_after_highlight() {
        let st = Style::parse(Some("n1-dark"));
        let out = Keys::new(rows()).render_styled(40, 8, &st);
        assert!(out.starts_with(&format!("\x1b[H{}\x1b[2J", st.base())));
        assert!(out.contains(&st.highlight_start()));
        assert!(out.contains(&format!("\x1b[0m{}", st.base())));
        assert!(!out.contains("\x1b[7m"));
        assert_eq!(
            Keys::new(rows()).render_styled(40, 8, &Style::Terminal),
            Keys::new(rows()).render(40, 8)
        );
    }
    #[test]
    fn render_has_header_padded_rows_and_status() {
        let k = Keys::new(rows());
        let out = k.render(80, 10);
        assert!(out.contains("keys> _"));
        assert!(out.contains("Super+W        close-pane"), "{out:?}");
        assert!(out.contains("Enter runs the selected binding"));
    }

    #[test]
    fn reply_parsing() {
        let reply = |line: &str| parse_reply(&Response::parse(line).unwrap());
        assert_eq!(
            reply("ok Super+W:close-pane\tSuper+F2:run x%3Ay"),
            Ok(vec![
                ("Super+W".into(), "close-pane".into()),
                ("Super+F2".into(), "run x:y".into())
            ])
        );
        assert_eq!(reply("ok"), Ok(vec![]));
        assert_eq!(reply("err nope"), Err("nope".into()));
        assert_eq!(
            reply("ok garbage"),
            Err("malformed bindings reply \"ok garbage\"".into())
        );
        // An unparseable line never reaches `parse_reply`: `Client::send` reports it as
        // `ClientError::BadResponse` (see the client tests).
        assert_eq!(Response::parse("???"), None);
    }
}
