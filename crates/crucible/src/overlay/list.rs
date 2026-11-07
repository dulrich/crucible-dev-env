//! The state and rendering every list overlay shares: the typed query, the selection index, the
//! status line, the navigation / editing keys, the reply handling and the frame scaffolding
//! (header, padded rows, status line, styled highlight). What the rows are, and what Enter does,
//! stays with each overlay.

use crucible_compositor::ipc::Response;

use super::Outcome;
use super::term::{Key, fit, sanitize, window};
use crate::style::Style;

/// Blank columns each side of the overlay text: two, so the side margin looks as wide as the one
/// blank line at the top and bottom (a terminal cell is about twice as tall as it is wide).
pub const SIDE_MARGIN: usize = 2;

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
    /// Rows mixed with headings: `(is_heading, text)`. A heading is flush with the prompt (no
    /// indent), never highlighted; other rows get the two-space indent and the selection
    /// highlight. `selected` indexes this slice.
    Sectioned(&'a [(bool, String)]),
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
            // Delete is acted on only by overlays that override `step` (the picker).
            Key::CtrlD | Key::Delete => Applied::Ignored,
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

    /// Renders the whole screen for a `cols` x `rows` terminal (raw-mode line endings) inside a
    /// blank margin (first and last lines blank, [`SIDE_MARGIN`] leading spaces and as many free
    /// columns on the right; the highlight covers the fitted text only). Never more than `rows` lines nor a
    /// line wider than `cols`; on tiny sizes the prompt wins, then the status line, then the rows.
    /// Inside: the `prompt` + query line, the optional column `header` (display text, sanitised, without the
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
        // A blank margin: one line top and bottom, [`SIDE_MARGIN`] columns each side. The interior
        // saturates to nothing on tiny sizes, and a line with no room for text stays empty.
        let width = usize::from(cols).saturating_sub(2 * SIDE_MARGIN);
        let pad = " ".repeat(SIDE_MARGIN);
        let rows = usize::from(rows);
        let inner = rows.saturating_sub(2);
        let text_line = |text: &str, wrap: &dyn Fn(&str) -> String| -> String {
            if width == 0 || text.is_empty() {
                String::new()
            } else {
                format!("{pad}{}", wrap(&fit(text, width)))
            }
        };
        let plain = |t: &str| t.to_string();
        let mut lines: Vec<String> = Vec::new();
        if inner >= 1 {
            lines.push(text_line(
                &format!("{prompt}{}_", sanitize(&self.query)),
                &plain,
            ));
        }
        // Slots between the prompt and the status line (the status needs inner >= 2).
        let middle = inner.saturating_sub(2);
        let show_header = header.is_some() && middle >= 1;
        if let (Some(h), true) = (header, show_header) {
            lines.push(text_line(&format!("  {h}"), &|t| style.dim(t)));
        }
        let body_h = middle - usize::from(show_header);
        match body {
            Body::Highlighted(text) if body_h >= 1 => {
                lines.push(text_line(&format!("  {text}"), &|t| style.highlight(t)));
            }
            Body::Highlighted(_) => {}
            Body::Rows(all) => {
                let (start, end) = window(self.selected, all.len(), body_h);
                for (i, row) in all.iter().enumerate().take(end).skip(start) {
                    let text = format!("  {row}");
                    lines.push(if i == self.selected {
                        text_line(&text, &|t| style.highlight(t))
                    } else {
                        text_line(&text, &plain)
                    });
                }
            }
            Body::Sectioned(all) => {
                let (start, end) = window(self.selected, all.len(), body_h);
                for (i, (heading, row)) in all.iter().enumerate().take(end).skip(start) {
                    lines.push(if *heading {
                        text_line(row, &plain)
                    } else if i == self.selected {
                        text_line(&format!("  {row}"), &|t| style.highlight(t))
                    } else {
                        text_line(&format!("  {row}"), &plain)
                    });
                }
            }
        }
        lines.truncate(inner);
        if inner >= 2 {
            while lines.len() < inner - 1 {
                lines.push(String::new());
            }
            lines.push(text_line(
                self.status.as_deref().unwrap_or(idle_status),
                &plain,
            ));
        }
        let mut all_lines = Vec::with_capacity(rows);
        if rows >= 1 {
            all_lines.push(String::new());
        }
        all_lines.extend(lines);
        while all_lines.len() < rows {
            all_lines.push(String::new());
        }
        let mut out = style.frame_start();
        out.push_str(&all_lines.join("\r\n"));
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
            8,
            &Style::Terminal,
        );
        assert_eq!(
            out,
            "\x1b[H\x1b[2J\r\n  p> a_\r\n    one\r\n  \x1b[7m  two\x1b[0m\r\n\r\n\r\n  idle\r\n"
        );
        l.status = Some("shown".into());
        assert!(
            l.render(
                frame("p> ", None, Body::Rows(&rows), "idle"),
                20,
                8,
                &Style::Terminal
            )
            .ends_with("\r\n  shown\r\n")
        );
    }

    #[test]
    fn render_header_sits_under_the_prompt_and_shrinks_the_window() {
        let l = ListState::new();
        let rows: Vec<String> = (0..9).map(|i| format!("r{i}")).collect();
        let out = l.render(
            frame("p> ", Some("H"), Body::Rows(&rows), "idle"),
            20,
            8,
            &Style::Terminal,
        );
        assert_eq!(
            out,
            "\x1b[H\x1b[2J\r\n  p> _\r\n  \x1b[2m  H\x1b[22m\r\n  \x1b[7m  r0\x1b[0m\r\n    r1\r\n    r2\r\n  idle\r\n"
        );
    }

    #[test]
    fn n1_header_is_muted_then_restored_without_a_reset() {
        let st = Style::parse(Some("n1-dark"));
        let rows = vec!["one".to_string()];
        let out =
            ListState::new().render(frame("p> ", Some("H"), Body::Rows(&rows), ""), 20, 8, &st);
        let line = out.split("\r\n").nth(2).unwrap();
        assert_eq!(line, format!("  {}", st.dim("  H")));
        assert!(
            line.starts_with("  \x1b[38;2;123;123;125m")
                && line.ends_with("\x1b[38;2;233;233;234m")
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

    /// The rendered frame split into lines, ANSI-free.
    fn lines_of(out: &str) -> Vec<String> {
        let body = out.strip_prefix("\x1b[H\x1b[2J").unwrap_or(out);
        body.split("\r\n").map(str::to_string).collect()
    }

    #[test]
    fn margin_blank_first_and_last_and_two_columns_each_side() {
        let rows = vec!["a-very-long-row-text-indeed".to_string()];
        let out = ListState::new().render(
            frame(
                "p> ",
                None,
                Body::Rows(&rows),
                "a-very-long-status-line-here",
            ),
            12,
            6,
            &Style::Terminal,
        );
        let lines = lines_of(&out);
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[0], "");
        assert_eq!(lines[5], "");
        for l in &lines[1..5] {
            assert!(l.is_empty() || l.starts_with("  "), "{l:?}");
        }
        let plain: Vec<String> = lines
            .iter()
            .map(|l| l.replace("\x1b[7m", "").replace("\x1b[0m", ""))
            .collect();
        assert!(plain.iter().all(|l| l.chars().count() <= 10), "{plain:?}");
        assert_eq!(
            plain[4], "  a-very-l",
            "status fitted to cols - 4 with the margin"
        );
    }

    #[test]
    fn sectioned_headings_sit_at_prompt_level_and_rows_are_indented() {
        let body = vec![
            (true, "Head".to_string()),
            (false, "row".to_string()),
            (true, "Head2".to_string()),
            (false, "row2".to_string()),
        ];
        let mut l = ListState::new();
        l.selected = 1;
        let out = l.render(
            frame("keys> ", None, Body::Sectioned(&body), "idle"),
            40,
            10,
            &Style::Terminal,
        );
        let lines = lines_of(&out);
        assert_eq!(
            &lines[1..6],
            [
                "  keys> _",
                "  Head",
                "  \x1b[7m  row\x1b[0m",
                "  Head2",
                "    row2"
            ]
        );
        assert!(!out.contains("\x1b[7m Head"));
    }

    #[test]
    fn selection_scrolls_within_the_reduced_interior_height() {
        let rows: Vec<String> = (0..9).map(|i| format!("r{i}")).collect();
        let mut l = ListState::new();
        l.selected = 8;
        // 7 rows: interior 5 = prompt + 3 body rows + status.
        let out = l.render(
            frame("p> ", None, Body::Rows(&rows), "idle"),
            20,
            7,
            &Style::Terminal,
        );
        assert!(out.contains("\x1b[7m  r8\x1b[0m") && out.contains("  r6") && !out.contains("r5"));
    }

    #[test]
    fn tiny_terminals_never_exceed_rows_lines_or_cols_cells() {
        let rows: Vec<String> = (0..5).map(|i| format!("row{i}")).collect();
        let sect = vec![(true, "H".to_string()), (false, "row".to_string())];
        for cols in 0..=12u16 {
            for nrows in 0..=8u16 {
                for header in [None, Some("HDR")] {
                    for body in [
                        Body::Rows(&rows),
                        Body::Highlighted("hl"),
                        Body::Sectioned(&sect),
                    ] {
                        let out = ListState::new().render(
                            frame("p> ", header, body, "status"),
                            cols,
                            nrows,
                            &Style::Terminal,
                        );
                        let text = out.strip_prefix("\x1b[H\x1b[2J").unwrap();
                        let lines: Vec<&str> = if nrows == 0 {
                            vec![]
                        } else {
                            text.split("\r\n").collect()
                        };
                        assert!(lines.len() <= usize::from(nrows), "{cols}x{nrows}");
                        for l in lines {
                            let plain = l
                                .replace("\x1b[7m", "")
                                .replace("\x1b[0m", "")
                                .replace("\x1b[2m", "")
                                .replace("\x1b[22m", "");
                            assert!(
                                plain.chars().count() <= usize::from(cols),
                                "{cols}x{nrows} {l:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_three_row_terminal_keeps_the_prompt_between_blank_margins() {
        let rows = vec!["x".to_string()];
        let out = ListState::new().render(
            frame("p> ", None, Body::Rows(&rows), "idle"),
            20,
            3,
            &Style::Terminal,
        );
        assert_eq!(lines_of(&out), ["", "  p> _", ""]);
    }
}
