//! The state and rendering every list overlay shares: the typed query, the selection index, the
//! status line, the navigation / editing keys, the reply handling and the frame scaffolding
//! (header, padded rows, status line, styled highlight). What the rows are, and what Enter does,
//! stays with each overlay.

use crucible_compositor::ipc::Response;

use super::Outcome;
use super::term::{Key, fit, sanitize, window};
use crate::style::Style;

/// What [`ListState::apply`] did with a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// Esc, Ctrl+C, or Ctrl+D on an empty query: leave the overlay.
    Exit,
    /// Up / Down / Ctrl+P / Ctrl+N: only the selection moved.
    Moved,
    /// A character was typed or deleted: the query changed and the selection reset to the top.
    Edited,
    /// Ctrl+D with text in the query: nothing happened.
    Ignored,
    /// Enter / Alt+Enter: the overlay decides.
    Enter(Key),
}

/// What the rows area of a frame shows.
#[derive(Debug, Clone, Copy)]
pub enum Body<'a> {
    /// Selectable rows (display text, sanitised, without the two-space indent); the row at
    /// [`ListState::selected`] is highlighted and the window scrolls to keep it visible.
    Rows(&'a [String]),
    /// One highlighted line that is not a selectable row (display text, sanitised).
    Highlighted(&'a str),
}

/// One frame's content: everything [`ListState::render`] needs besides the terminal size and style.
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    /// The prompt shown before the typed query.
    pub prompt: &'a str,
    /// The column header line, if the body is a table.
    pub header: Option<&'a str>,
    /// The rows area.
    pub body: Body<'a>,
    /// The status line shown when none is set.
    pub idle_status: &'a str,
}

/// Query, selection and status shared by every list overlay.
#[derive(Debug, Default)]
pub struct ListState {
    /// The typed input line.
    pub query: String,
    /// The selected row index (clamped by [`Self::apply`]).
    pub selected: usize,
    /// The status line, if any (an `err` reply or a local refusal).
    pub status: Option<String>,
}

impl ListState {
    /// An empty query at the top of the list.
    #[must_use]
    pub fn new() -> ListState {
        ListState::default()
    }

    /// A state whose query starts as `query`.
    #[must_use]
    pub fn with_query(query: String) -> ListState {
        ListState {
            query,
            ..ListState::default()
        }
    }

    /// Applies one key's shared behaviour. `row_count` is the number of selectable rows (needed
    /// to clamp Down). The status line is left alone: overlays that clear it on edits do so on
    /// [`Applied::Edited`].
    pub fn apply(&mut self, key: Key, row_count: usize) -> Applied {
        match key {
            Key::Esc | Key::CtrlC => Applied::Exit,
            Key::CtrlD if self.query.is_empty() => Applied::Exit,
            Key::CtrlD => Applied::Ignored,
            Key::Up | Key::CtrlP => {
                self.selected = self.selected.saturating_sub(1);
                Applied::Moved
            }
            Key::Down | Key::CtrlN => {
                self.selected = (self.selected + 1).min(row_count.saturating_sub(1));
                Applied::Moved
            }
            Key::Char(c) => {
                self.query.push(c);
                self.selected = 0;
                Applied::Edited
            }
            Key::Backspace => {
                self.query.pop();
                self.selected = 0;
                Applied::Edited
            }
            Key::Enter | Key::AltEnter => Applied::Enter(key),
        }
    }

    /// Applies the compositor's answer to the last [`Outcome::Send`]: `ok` leaves the overlay,
    /// `err` stays open and shows the reason in the status line.
    pub fn on_response(&mut self, resp: &Response) -> Outcome {
        match resp {
            Response::Ok(_) => Outcome::Exit,
            Response::Err(reason) => {
                self.status = Some(reason.clone());
                Outcome::Continue
            }
        }
    }

    /// Renders the whole screen for a `cols` x `rows` terminal (raw-mode line endings): the
    /// `prompt` + query line, the optional column `header` (display text, sanitised, without the
    /// indent; dimmed, never highlighted, and it costs the rows window one row so selection indices
    /// stay row indices), the body, blank padding, then the status line (`idle_status` when none is
    /// set).
    #[must_use]
    pub fn render(&self, frame: Frame<'_>, cols: u16, rows: u16, style: &Style) -> String {
        let Frame {
            prompt,
            header,
            body,
            idle_status,
        } = frame;
        let width = usize::from(cols).max(10);
        let height = usize::from(rows).max(4);
        let mut lines: Vec<String> =
            vec![fit(&format!("{prompt}{}_", sanitize(&self.query)), width)];
        if let Some(h) = header {
            lines.push(style.dim(&fit(&format!("  {h}"), width)));
        }
        match body {
            Body::Highlighted(text) => {
                lines.push(style.highlight(&fit(&format!("  {text}"), width)));
            }
            Body::Rows(all) => {
                let (start, end) = window(
                    self.selected,
                    all.len(),
                    height - 2 - usize::from(header.is_some()),
                );
                for (i, row) in all.iter().enumerate().take(end).skip(start) {
                    let text = fit(&format!("  {row}"), width);
                    lines.push(if i == self.selected {
                        style.highlight(&text)
                    } else {
                        text
                    });
                }
            }
        }
        while lines.len() < height - 1 {
            lines.push(String::new());
        }
        lines.push(fit(self.status.as_deref().unwrap_or(idle_status), width));
        let mut out = style.frame_start();
        out.push_str(&lines.join("\r\n"));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame<'a>(
        prompt: &'a str,
        header: Option<&'a str>,
        body: Body<'a>,
        idle_status: &'a str,
    ) -> Frame<'a> {
        Frame {
            prompt,
            header,
            body,
            idle_status,
        }
    }

    fn typed(s: &str) -> ListState {
        let mut l = ListState::new();
        for c in s.chars() {
            l.apply(Key::Char(c), 0);
        }
        l
    }

    #[test]
    fn selection_moves_and_clamps_at_both_ends() {
        let mut l = ListState::new();
        assert_eq!(l.apply(Key::Up, 3), Applied::Moved);
        assert_eq!(l.selected, 0);
        for key in [Key::Down, Key::CtrlN, Key::Down, Key::Down] {
            assert_eq!(l.apply(key, 3), Applied::Moved);
        }
        assert_eq!(l.selected, 2);
        l.apply(Key::CtrlP, 3);
        assert_eq!(l.selected, 1);
        l.apply(Key::Up, 3);
        l.apply(Key::Up, 3);
        assert_eq!(l.selected, 0);
    }

    #[test]
    fn down_with_no_rows_stays_at_zero() {
        let mut l = ListState::new();
        l.apply(Key::Down, 0);
        assert_eq!(l.selected, 0);
    }

    #[test]
    fn typing_and_backspace_edit_the_query_and_reset_the_selection() {
        let mut l = typed("ab");
        assert_eq!(l.query, "ab");
        l.selected = 4;
        assert_eq!(l.apply(Key::Backspace, 9), Applied::Edited);
        assert_eq!((l.query.as_str(), l.selected), ("a", 0));
        l.apply(Key::Backspace, 9);
        assert_eq!(l.apply(Key::Backspace, 9), Applied::Edited);
        assert_eq!(l.query, "");
    }

    #[test]
    fn edits_leave_the_status_line_to_the_overlay() {
        let mut l = ListState::new();
        l.status = Some("boom".into());
        l.apply(Key::Char('x'), 0);
        assert_eq!(l.status.as_deref(), Some("boom"));
    }

    #[test]
    fn exit_keys_and_ctrl_d_on_text() {
        assert_eq!(ListState::new().apply(Key::Esc, 0), Applied::Exit);
        assert_eq!(ListState::new().apply(Key::CtrlC, 0), Applied::Exit);
        assert_eq!(ListState::new().apply(Key::CtrlD, 0), Applied::Exit);
        let mut l = typed("a");
        assert_eq!(l.apply(Key::CtrlD, 0), Applied::Ignored);
        assert_eq!(l.apply(Key::CtrlC, 0), Applied::Exit);
    }

    #[test]
    fn enter_keys_are_handed_back() {
        let mut l = ListState::new();
        assert_eq!(l.apply(Key::Enter, 1), Applied::Enter(Key::Enter));
        assert_eq!(l.apply(Key::AltEnter, 1), Applied::Enter(Key::AltEnter));
    }

    #[test]
    fn on_response_ok_exits_err_stays_with_status() {
        let mut l = ListState::new();
        assert_eq!(l.on_response(&Response::Ok(String::new())), Outcome::Exit);
        assert_eq!(
            l.on_response(&Response::Err("unknown command bogus".into())),
            Outcome::Continue
        );
        assert_eq!(l.status.as_deref(), Some("unknown command bogus"));
    }

    #[test]
    fn render_has_header_rows_padding_and_status() {
        let mut l = typed("a");
        l.selected = 1;
        let rows = vec!["one".to_string(), "two".to_string()];
        let out = l.render(
            frame("p> ", None, Body::Rows(&rows), "idle"),
            20,
            6,
            &Style::Terminal,
        );
        assert_eq!(
            out,
            "\x1b[H\x1b[2Jp> a_\r\n  one\r\n\x1b[7m  two\x1b[0m\r\n\r\n\r\nidle"
        );
        l.status = Some("shown".into());
        assert!(
            l.render(
                frame("p> ", None, Body::Rows(&rows), "idle"),
                20,
                6,
                &Style::Terminal
            )
            .ends_with("shown")
        );
    }

    #[test]
    fn render_header_sits_under_the_prompt_and_shrinks_the_window() {
        let l = ListState::new();
        let rows: Vec<String> = (0..9).map(|i| format!("r{i}")).collect();
        let out = l.render(
            frame("p> ", Some("H"), Body::Rows(&rows), "idle"),
            20,
            6,
            &Style::Terminal,
        );
        assert_eq!(
            out,
            "\x1b[H\x1b[2Jp> _\r\n\x1b[2m  H\x1b[22m\r\n\x1b[7m  r0\x1b[0m\r\n  r1\r\n  r2\r\nidle"
        );
    }

    #[test]
    fn n1_header_is_muted_then_restored_without_a_reset() {
        let st = Style::parse(Some("n1-dark"));
        let rows = vec!["one".to_string()];
        let out =
            ListState::new().render(frame("p> ", Some("H"), Body::Rows(&rows), ""), 20, 6, &st);
        let line = out.split("\r\n").nth(1).unwrap();
        assert_eq!(line, st.dim("  H"));
        assert!(
            line.starts_with("\x1b[38;2;123;123;125m") && line.ends_with("\x1b[38;2;233;233;234m")
        );
        assert!(!line.contains("\x1b[0m"));
    }

    #[test]
    fn render_highlighted_body_and_control_neutralised_query() {
        let l = typed("\x1b");
        let out = l.render(
            frame("p> ", None, Body::Highlighted("SH  x"), ""),
            30,
            5,
            &Style::Terminal,
        );
        assert!(out.contains("p> ?_"));
        assert!(out.contains("\x1b[7m  SH  x\x1b[0m"));
    }
}
