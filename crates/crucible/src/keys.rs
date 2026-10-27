//! `crucible keys` — the read-only settings overlay: lists the compositor's effective keybindings
//! (fetched over the `bindings` IPC request). The state machine and rendering are pure; only
//! [`run`] touches the terminal, the environment and the socket.

use std::io::{Read, Write};

use crucible_compositor::ipc::{Response, SOCKET_ENV, parse_bindings};

use crate::client::{Client, socket_from_env};
use crate::fuzzy;
use crate::pick::{Key, Outcome, decode_keys, fit, sanitize, window};
use crate::style::Style;
use crate::tty::{RawGuard, window_size};

/// Usage text for `crucible keys --help`.
pub const USAGE: &str = "usage: crucible keys\n\nThe CDE settings overlay (needs $CDE_SOCKET; CDE runs it in an overlay on Super+Comma).\nLists the effective keybindings, read-only. Type to filter; Up/Down or Ctrl+P/Ctrl+N move;\nEsc or Ctrl+C closes. Edit binds in config.kdl (docs/config.md).\n";

/// Status line shown under the rows.
const STATUS: &str = "read-only — edit binds in config.kdl (docs/config.md)";

/// Parses the reply to `bindings` into `(chord, action)` rows.
///
/// # Errors
/// The compositor's `err` reason, or a description of a malformed reply.
pub fn parse_reply(line: &str) -> Result<Vec<(String, String)>, String> {
    match Response::parse(line) {
        Some(Response::Ok(payload)) => {
            parse_bindings(&payload).ok_or_else(|| format!("malformed bindings reply {line:?}"))
        }
        Some(Response::Err(reason)) => Err(reason),
        None => Err(format!("bad response {line:?}")),
    }
}

/// Settings-overlay state.
#[derive(Debug)]
pub struct Keys {
    rows: Vec<(String, String)>,
    query: String,
    selected: usize,
}

impl Keys {
    /// A viewer over `rows` (`(chord, action)` display pairs).
    #[must_use]
    pub fn new(rows: Vec<(String, String)>) -> Keys {
        Keys {
            rows,
            query: String::new(),
            selected: 0,
        }
    }

    /// Indices of the rows matching the query, best first (ties keep list order).
    #[must_use]
    pub fn matches(&self) -> Vec<usize> {
        let texts: Vec<String> = self.rows.iter().map(|(c, a)| format!("{c}  {a}")).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        fuzzy::filter(&self.query, &refs)
    }

    /// Applies one key. Enter does nothing: the overlay is read-only.
    pub fn step(&mut self, key: Key) -> Outcome {
        match key {
            Key::Esc | Key::CtrlC => return Outcome::Exit,
            Key::CtrlD if self.query.is_empty() => return Outcome::Exit,
            Key::CtrlD | Key::Enter => {}
            Key::Up | Key::CtrlP => self.selected = self.selected.saturating_sub(1),
            Key::Down | Key::CtrlN => {
                let last = self.matches().len().saturating_sub(1);
                self.selected = (self.selected + 1).min(last);
            }
            Key::Char(c) => {
                self.query.push(c);
                self.selected = 0;
            }
            Key::Backspace => {
                self.query.pop();
                self.selected = 0;
            }
        }
        Outcome::Continue
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
        let mut lines: Vec<String> = vec![fit(&format!("keys> {}_", sanitize(&self.query)), width)];
        let pad = self
            .rows
            .iter()
            .map(|(c, _)| sanitize(c).chars().count())
            .max()
            .unwrap_or(0);
        let all = self.matches();
        let (start, end) = window(self.selected, all.len(), height - 2);
        for (i, &idx) in all.iter().enumerate().take(end).skip(start) {
            let (chord, action) = &self.rows[idx];
            let text = fit(
                &format!("  {:<pad$}  {}", sanitize(chord), sanitize(action)),
                width,
            );
            lines.push(if i == self.selected {
                style.highlight(&text)
            } else {
                text
            });
        }
        while lines.len() < height - 1 {
            lines.push(String::new());
        }
        lines.push(fit(STATUS, width));
        let mut out = style.frame_start();
        out.push_str(&lines.join("\r\n"));
        out
    }
}

/// Runs the subcommand; returns the exit code.
pub fn run(args: &[String]) -> i32 {
    if let Some(extra) = args.first() {
        eprintln!("crucible keys: unexpected argument {extra:?}\n\n{USAGE}");
        return 2;
    }
    let path = match socket_from_env(std::env::var_os(SOCKET_ENV)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("crucible keys: {e}");
            return 2;
        }
    };
    let mut client = match Client::connect(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("crucible keys: cannot connect to {}: {e}", path.display());
            return 1;
        }
    };
    let rows = match client
        .request("bindings")
        .map_err(|e| format!("compositor error: {e}"))
        .and_then(|line| parse_reply(&line))
    {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("crucible keys: {e}");
            return 1;
        }
    };
    let mut keys = Keys::new(rows);
    let Some(guard) = RawGuard::enter(true) else {
        eprintln!("crucible keys: stdin is not a terminal");
        return 1;
    };
    let code = event_loop(&mut keys);
    drop(guard);
    code
}

fn event_loop(keys: &mut Keys) -> i32 {
    let style = Style::from_env();
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 256];
    let mut stdin = std::io::stdin();
    loop {
        let (cols, rows) = window_size();
        let mut out = std::io::stdout();
        let _ = out.write_all(keys.render_styled(cols, rows, &style).as_bytes());
        let _ = out.flush();
        crate::latency::first_frame("keys");
        let n = match stdin.read(&mut chunk) {
            Ok(0) => return 0,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return 1,
        };
        pending.extend_from_slice(&chunk[..n]);
        for key in decode_keys(&mut pending) {
            if keys.step(key) == Outcome::Exit {
                return 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(k.selected, 0);
        k.step(Key::Down);
        k.step(Key::CtrlN);
        k.step(Key::Down);
        assert_eq!(k.selected, 2);
        k.step(Key::CtrlP);
        assert_eq!(k.selected, 1);
    }

    #[test]
    fn enter_is_a_no_op_and_esc_exits() {
        let mut k = Keys::new(rows());
        assert_eq!(k.step(Key::Enter), Outcome::Continue);
        assert_eq!(k.selected, 0);
        assert_eq!(k.step(Key::Esc), Outcome::Exit);
        assert_eq!(k.step(Key::CtrlC), Outcome::Exit);
        assert_eq!(k.step(Key::CtrlD), Outcome::Exit);
        k.step(Key::Char('x'));
        assert_eq!(k.step(Key::CtrlD), Outcome::Continue);
    }

    #[test]
    fn render_styled_n1_paints_background_and_reapplies_after_highlight() {
        let st = Style::parse(Some("n1-dark"));
        let out = Keys::new(rows()).render_styled(40, 8, &st);
        assert!(out.starts_with(&format!("\x1b[H{}\x1b[2J", st.base())));
        assert!(out.contains(&st.highlight("")[..20]));
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
        assert!(out.contains("read-only"));
    }

    #[test]
    fn reply_parsing() {
        assert_eq!(
            parse_reply("ok Super+W:close-pane\tSuper+F2:run x%3Ay"),
            Ok(vec![
                ("Super+W".into(), "close-pane".into()),
                ("Super+F2".into(), "run x:y".into())
            ])
        );
        assert_eq!(parse_reply("ok"), Ok(vec![]));
        assert_eq!(parse_reply("err nope"), Err("nope".into()));
        assert!(parse_reply("ok garbage").is_err());
        assert!(parse_reply("???").is_err());
    }
}
