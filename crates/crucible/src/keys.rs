//! `crucible keys` — the settings overlay: lists the compositor's effective keybindings
//! (fetched over the `bindings` IPC request); Enter runs the selected one (`exec`). Only the
//! binding rows and Enter live here; the runner and shared state are in [`crate::overlay`]. The
//! state machine and rendering are pure.

use crucible_compositor::ipc::{Request, Response, parse_bindings};

use crate::cli::{Parsed, help_requested};
use crate::client::{Client, ClientError};
use crate::fuzzy;
use crate::overlay::list::{Body, Frame, ListState};
use crate::overlay::term::{Key, sanitize};
use crate::overlay::{Outcome, Overlay, run_overlay};
use crate::style::Style;
use crate::table::{self, Col};

/// Usage text for `crucible keys --help`.
pub const USAGE: &str = "usage: crucible keys\n\nThe CDE settings overlay (needs $CDE_SOCKET; CDE runs it in an overlay on Super+Comma).\nLists the effective keybindings. Type to filter; Up/Down or Ctrl+P/Ctrl+N move; Enter runs the\nselected binding; Esc or Ctrl+C closes. Edit binds in config.kdl (docs/config.md).\n";

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

/// Settings-overlay state.
#[derive(Debug)]
pub struct Keys {
    rows: Vec<(String, String)>,
    list: ListState,
}

impl Keys {
    /// A viewer over `rows` (`(chord, action)` display pairs).
    #[must_use]
    pub fn new(rows: Vec<(String, String)>) -> Keys {
        Keys {
            rows,
            list: ListState::new(),
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
        self.matches().len()
    }

    /// Typing keeps a refusal visible (it clears only when Enter runs a binding).
    fn clears_status_on_edit(&self) -> bool {
        false
    }

    /// Enter sends the selected row's action for execution.
    fn enter(&mut self, _key: Key) -> Outcome {
        if let Some(&idx) = self.matches().get(self.list.selected) {
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
        let texts: Vec<String> = self
            .matches()
            .iter()
            .map(|&idx| lines[idx].clone())
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
    fn selection_moves_and_clamps() {
        let mut k = Keys::new(rows());
        k.step(Key::Up);
        assert_eq!(k.list.selected, 0);
        k.step(Key::Down);
        k.step(Key::CtrlN);
        k.step(Key::Down);
        assert_eq!(k.list.selected, 2);
        k.step(Key::CtrlP);
        assert_eq!(k.list.selected, 1);
    }

    #[test]
    fn enter_sends_exec_for_the_selected_row_and_esc_exits() {
        let mut k = Keys::new(rows());
        assert_eq!(
            k.step(Key::Enter),
            Outcome::Send(Request::Exec("open-launcher".into()))
        );
        k.step(Key::Down);
        assert_eq!(
            k.step(Key::AltEnter),
            Outcome::Send(Request::Exec("quit".into()))
        );
        assert_eq!(k.list.selected, 1);
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
