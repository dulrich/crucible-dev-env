//! `crucible launch` — the program launcher TUI (run inside the compositor's terminal overlay).
//! Desktop-entry parsing ([`desktop`]), TUI program resolution ([`tui_programs`]), ranking and the
//! key state machine are pure (or take explicit paths); only [`run`] touches the terminal, the
//! environment and the socket.

mod desktop;
mod tui_programs;

use std::path::PathBuf;

use crucible_compositor::ipc::{LAUNCHER_TUI_ENV, Request, TERMINAL_ENV};

use crate::fuzzy;
use crate::overlay::list::{Applied, Body, ListState};
use crate::overlay::term::{Key, sanitize};
use crate::overlay::{Outcome, Overlay, run_overlay};
use crate::style::Style;
use crate::{Parsed, help_requested};

use desktop::{DesktopEntry, data_dirs_from_env, desktop_entries};
use tui_programs::{resolve_tui, tui_items, tui_patterns};

/// Usage text for `crucible launch --help`.
pub const USAGE: &str = "usage: crucible launch\n\nThe CDE program launcher (needs $CDE_SOCKET; CDE runs it in an overlay on Super+Return).\nLists installed desktop applications (XDG .desktop entries), each labelled GUI, TUI\n(Terminal=true) or TERM (Categories has TerminalEmulator; launched as a CDE terminal), plus TUI programs from a built-in list (htop, vim, lazygit, …) and the config's\nlauncher { tui } entries when they are found on PATH. Type to filter by name; Up/Down or\nCtrl+P/Ctrl+N move; Enter launches the selection into the focused pane; Esc or Ctrl+C closes. TUI entries run in the configured terminal\n($CDE_TERMINAL, exported by CDE), which closes when the program exits, and are hidden when it is unknown.\n\nShell-line mode: start the query with ! to type a shell command instead. Enter runs it in a\nterminal; Alt+Enter runs it detached without a terminal. An empty command shows a hint, and Enter with no\nmatching row shows `unknown app <query>`.\n";

/// How a row is launched: a GUI application, or a TUI program run inside the configured terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchClass {
    Gui,
    Tui,
    Term,
}

impl LaunchClass {
    /// `TerminalEmulator` entries are TERM (winning over `Terminal=true`); `Terminal=true` entries
    /// and PATH-resolved programs are TUI; everything else is GUI.
    fn of_desktop(e: &DesktopEntry) -> LaunchClass {
        if e.terminal_emulator {
            LaunchClass::Term
        } else if e.terminal {
            LaunchClass::Tui
        } else {
            LaunchClass::Gui
        }
    }

    /// The label shown beside the row.
    fn label(self) -> &'static str {
        match self {
            LaunchClass::Gui => "GUI",
            LaunchClass::Tui => "TUI",
            LaunchClass::Term => "TERM",
        }
    }

    /// The request that launches `cmd` with this class (`run-tui` wraps it in the terminal).
    fn request(self, cmd: String) -> Request {
        match self {
            LaunchClass::Gui => Request::run(cmd),
            LaunchClass::Tui => Request::RunTui(cmd),
            LaunchClass::Term => Request::RunTerm(cmd),
        }
    }
}

/// The bare program of an `Exec` line (field codes already stripped): the text between a leading
/// pair of double quotes, else the first whitespace-separated word. Desktop arguments are dropped
/// so a single-instance emulator is never asked to forward the window to another process.
fn term_program(exec: &str) -> String {
    let exec = exec.trim_start();
    match exec.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or_default().to_string(),
        None => exec
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string(),
    }
}

/// One selectable row: the name (matched and shown), its class and the shell command it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    filter: String,
    label: String,
    class: LaunchClass,
    cmd: String,
}

/// The launcher's rows: desktop entries first, then the PATH-resolved TUI `programs`. A TUI row
/// keeps its bare command (the compositor wraps it in the terminal via `run-tui`) and is left out
/// unless `terminal_known` -- the one place that gating is applied. A TERM row is itself a
/// terminal, so it is never gated.
#[must_use]
pub fn build_items(
    entries: &[DesktopEntry],
    programs: &[(String, PathBuf)],
    terminal_known: bool,
) -> Vec<Item> {
    let mut items: Vec<Item> = entries
        .iter()
        .map(|e| Item {
            filter: e.name.clone(),
            label: e.name.clone(),
            class: LaunchClass::of_desktop(e),
            cmd: match LaunchClass::of_desktop(e) {
                LaunchClass::Term => term_program(&e.exec),
                _ => e.exec.clone(),
            },
        })
        .collect();
    items.extend(tui_items(programs, entries));
    if !terminal_known {
        items.retain(|i| i.class != LaunchClass::Tui);
    }
    items
}

/// Launcher state.
#[derive(Debug)]
pub struct Launcher {
    items: Vec<Item>,
    list: ListState,
}

impl Launcher {
    /// A launcher over `items`.
    #[must_use]
    pub fn new(items: Vec<Item>) -> Launcher {
        Launcher {
            items,
            list: ListState::new(),
        }
    }

    /// Indices of the items matching the query, best first (ties keep list order).
    #[must_use]
    pub fn rows(&self) -> Vec<usize> {
        let names: Vec<&str> = self.items.iter().map(|i| i.filter.as_str()).collect();
        fuzzy::filter(&self.list.query, &names)
    }

    /// The shell command typed in `!` mode: `None` outside the mode, `Some("")` while empty.
    #[must_use]
    pub fn shell_line(&self) -> Option<&str> {
        self.list.query.strip_prefix('!').map(str::trim_start)
    }

    /// Enter (run in a terminal) / Alt+Enter (run detached) in `!` mode; an empty command sends
    /// nothing and says so.
    fn enter_shell(&mut self, cmd: &str, key: Key) -> Outcome {
        let cmd = cmd.trim_end();
        if cmd.is_empty() {
            self.list.status = Some("type a command after !".into());
            return Outcome::Continue;
        }
        let cmd = cmd.to_string();
        Outcome::Send(if key == Key::Enter {
            Request::run_in_terminal(cmd)
        } else {
            Request::run(cmd)
        })
    }

    /// Enter / Alt+Enter outside `!` mode: launch the selected item; no match sets `unknown app
    /// <query>` (nothing after it when the query is empty).
    fn enter_row(&mut self) -> Outcome {
        match self.rows().get(self.list.selected) {
            Some(&i) => {
                let item = &self.items[i];
                Outcome::Send(item.class.request(item.cmd.clone()))
            }
            None => {
                self.list.status = Some(format!("unknown app {}", self.list.query.trim()));
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
}

impl Overlay for Launcher {
    fn step(&mut self, key: Key) -> Outcome {
        let count = self.rows().len();
        match self.list.apply(key, count) {
            Applied::Exit => Outcome::Exit,
            Applied::Edited => {
                self.list.status = None;
                Outcome::Continue
            }
            Applied::Moved | Applied::Ignored => Outcome::Continue,
            Applied::Enter(key) => match self.shell_line().map(str::to_string) {
                Some(cmd) => self.enter_shell(&cmd, key),
                None => self.enter_row(),
            },
        }
    }

    fn list_mut(&mut self) -> &mut ListState {
        &mut self.list
    }

    fn render_styled(&self, cols: u16, rows: u16, style: &Style) -> String {
        if let Some(cmd) = self.shell_line() {
            let text = format!(
                "SH  {}  [Enter: run in terminal, Alt+Enter: run detached]",
                sanitize(cmd)
            );
            return self
                .list
                .render("run> ", Body::Highlighted(&text), "", cols, rows, style);
        }
        let texts: Vec<String> = self
            .rows()
            .iter()
            .map(|&idx| {
                let item = &self.items[idx];
                format!("{}  {}", item.class.label(), sanitize(&item.label))
            })
            .collect();
        self.list
            .render("run> ", Body::Rows(&texts), "", cols, rows, style)
    }
}

/// Whether `$CDE_TERMINAL` names a terminal program: set, valid UTF-8 and non-empty.
#[must_use]
pub fn terminal_known(program: Option<std::ffi::OsString>) -> bool {
    program
        .and_then(|p| p.into_string().ok())
        .is_some_and(|p| !p.is_empty())
}

/// The launcher's rows from the environment: desktop entries, then PATH-derived TUI programs.
fn discover_items() -> Vec<Item> {
    let terminal_known = terminal_known(std::env::var_os(TERMINAL_ENV));
    let entries = desktop_entries(&data_dirs_from_env());
    let dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let patterns = tui_patterns(std::env::var_os(LAUNCHER_TUI_ENV).as_deref());
    build_items(&entries, &resolve_tui(&patterns, &dirs), terminal_known)
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

/// Runs the subcommand; returns the exit code.
pub fn run(args: &[String]) -> i32 {
    match parse_args(args) {
        Ok(Parsed::Help) => {
            print!("{USAGE}");
            0
        }
        Ok(Parsed::Run(())) => run_overlay("launch", |_client| Ok(Launcher::new(discover_items()))),
        Err(e) => {
            eprintln!("crucible launch: {e}\n\n{USAGE}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_compositor::ipc::Response;

    fn entry(name: &str, exec: &str, terminal: bool) -> DesktopEntry {
        DesktopEntry {
            name: name.into(),
            exec: exec.into(),
            terminal,
            terminal_emulator: false,
        }
    }

    fn term_entry(name: &str, exec: &str, terminal: bool) -> DesktopEntry {
        DesktopEntry {
            terminal_emulator: true,
            ..entry(name, exec, terminal)
        }
    }

    #[test]
    fn enter_builds_the_run_command_for_each_kind() {
        let entries = [
            entry("Browser", "firefox --new", false),
            entry("Editor", "vim", true),
        ];
        let items = build_items(&entries, &[], true);
        let mut l = Launcher::new(items);
        assert_eq!(
            l.step(Key::Enter),
            Outcome::Send(Request::Exec("run firefox --new".into()))
        );
        l.step(Key::Down);
        assert_eq!(
            l.step(Key::Enter),
            Outcome::Send(Request::RunTui("vim".into()))
        );
        assert_eq!(
            l.step(Key::AltEnter),
            Outcome::Send(Request::RunTui("vim".into()))
        );
    }

    #[test]
    fn terminal_entries_are_hidden_without_a_terminal() {
        let entries = [entry("Editor", "vim", true), entry("Browser", "ff", false)];
        let items = build_items(&entries, &[], false);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].filter, "Browser");
    }

    #[test]
    fn terminal_gating_is_applied_once_over_the_combined_list() {
        let entries = [entry("Editor", "vim", true), entry("Browser", "ff", false)];
        let programs = vec![("k9s".to_string(), PathBuf::from("/usr/bin/k9s"))];
        let names = |terminal_known| -> Vec<String> {
            build_items(&entries, &programs, terminal_known)
                .iter()
                .map(|i| i.label.clone())
                .collect()
        };
        assert_eq!(names(true), ["Editor", "Browser", "k9s"]);
        assert_eq!(names(false), ["Browser"]);
    }

    #[test]
    fn launch_class_carries_the_label_and_the_request() {
        assert_eq!(LaunchClass::Gui.label(), "GUI");
        assert_eq!(LaunchClass::Tui.label(), "TUI");
        assert_eq!(LaunchClass::Term.label(), "TERM");
        assert_eq!(
            LaunchClass::of_desktop(&entry("a", "a", false)),
            LaunchClass::Gui
        );
        assert_eq!(
            LaunchClass::of_desktop(&entry("a", "a", true)),
            LaunchClass::Tui
        );
        assert_eq!(
            LaunchClass::of_desktop(&term_entry("a", "a", false)),
            LaunchClass::Term
        );
        assert_eq!(
            LaunchClass::of_desktop(&term_entry("a", "a", true)),
            LaunchClass::Term
        );
        assert_eq!(
            LaunchClass::Term.request("ghostty".into()),
            Request::RunTerm("ghostty".into())
        );
        assert_eq!(
            LaunchClass::Gui.request("ff".into()),
            Request::Exec("run ff".into())
        );
        assert_eq!(
            LaunchClass::Tui.request("vim".into()),
            Request::RunTui("vim".into())
        );
    }

    #[test]
    fn term_program_is_the_bare_first_word() {
        assert_eq!(term_program("kitty"), "kitty");
        assert_eq!(
            term_program("ghostty --gtk-single-instance=true"),
            "ghostty"
        );
        assert_eq!(
            term_program("\"/opt/my term/bin\" --x y"),
            "/opt/my term/bin"
        );
        assert_eq!(term_program(""), "");
    }

    #[test]
    fn term_rows_launch_the_bare_program_and_survive_no_terminal() {
        let entries = [
            term_entry("Ghostty", "ghostty --gtk-single-instance=true", true),
            entry("Editor", "vim", true),
        ];
        let items = build_items(&entries, &[], false);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].class, LaunchClass::Term);
        assert_eq!(items[0].cmd, "ghostty");
        let mut l = Launcher::new(items);
        assert!(l.render(60, 8).contains("TERM  Ghostty"));
        assert_eq!(
            l.step(Key::Enter),
            Outcome::Send(Request::RunTerm("ghostty".into()))
        );
    }

    #[test]
    fn enter_without_a_match_says_unknown_app() {
        let mut l = shell_launcher("zz");
        assert_eq!(l.step(Key::Enter), Outcome::Continue);
        assert!(l.render(60, 8).contains("unknown app zz"));
        l.step(Key::Backspace);
        assert!(!l.render(60, 8).contains("unknown app"));
    }

    #[test]
    fn empty_shell_command_says_to_type_one() {
        let mut l = shell_launcher("!  ");
        assert_eq!(l.step(Key::Enter), Outcome::Continue);
        assert!(l.render(60, 8).contains("type a command after !"));
    }

    #[test]
    fn parse_args_owns_help_and_rejects_everything_else() {
        let a = |l: &[&str]| l.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(parse_args(&a(&[])), Ok(Parsed::Run(())));
        assert_eq!(parse_args(&a(&["--help"])), Ok(Parsed::Help));
        assert_eq!(parse_args(&a(&["x", "-h"])), Ok(Parsed::Help));
        assert!(parse_args(&a(&["x"])).is_err());
    }

    #[test]
    fn terminal_known_needs_only_cde_terminal() {
        // `CDE_TERMINAL` alone decides; the exec flag is never consulted.
        assert!(terminal_known(Some("kitty".into())));
        assert!(!terminal_known(Some("".into())));
        assert!(!terminal_known(None));
    }

    #[test]
    fn err_response_stays_open_ok_and_exit_keys_leave() {
        let items = build_items(&[entry("A", "a", false)], &[], false);
        let mut l = Launcher::new(items);
        assert_eq!(
            l.on_response(&Response::Err("boom".into())),
            Outcome::Continue
        );
        assert!(l.render(40, 10).contains("boom"));
        assert_eq!(l.on_response(&Response::Ok(String::new())), Outcome::Exit);
        assert_eq!(l.step(Key::Esc), Outcome::Exit);
        assert_eq!(l.step(Key::CtrlC), Outcome::Exit);
    }

    #[test]
    fn enter_without_a_match_does_nothing() {
        let items = build_items(&[entry("A", "a", false)], &[], false);
        let mut l = Launcher::new(items);
        l.step(Key::Char('z'));
        l.step(Key::Char('z'));
        assert_eq!(l.step(Key::Enter), Outcome::Continue);
    }

    #[test]
    fn render_styled_n1_paints_background_and_reapplies_after_highlight() {
        let st = Style::parse(Some("n1-dark"));
        let out = Launcher::new(build_items(&[entry("a", "a", false)], &[], false))
            .render_styled(40, 8, &st);
        assert!(out.starts_with(&format!("\x1b[H{}\x1b[2J", st.base())));
        assert!(out.contains(&st.highlight("")[..20]));
        assert!(out.contains(&format!("\x1b[0m{}", st.base())));
        assert!(!out.contains("\x1b[7m"));
        assert_eq!(
            Launcher::new(build_items(&[entry("a", "a", false)], &[], false)).render_styled(
                40,
                8,
                &Style::Terminal
            ),
            Launcher::new(build_items(&[entry("a", "a", false)], &[], false)).render(40, 8)
        );
    }
    #[test]
    fn render_marks_selection_and_neutralises_controls() {
        let items = build_items(&[entry("A\x1b[31m", "a", false)], &[], false);
        let out = Launcher::new(items).render(40, 8);
        assert!(out.contains("\x1b[7m"));
        assert!(!out.contains("A\x1b[31m"));
    }

    fn shell_launcher(typed: &str) -> Launcher {
        let mut l = Launcher::new(build_items(&[entry("A", "a", false)], &[], false));
        for c in typed.chars() {
            l.step(Key::Char(c));
        }
        l
    }

    #[test]
    fn bang_enters_and_leaves_shell_mode() {
        let mut l = shell_launcher("!");
        assert_eq!(l.shell_line(), Some(""));
        assert!(l.render(80, 8).contains("SH"));
        l.step(Key::Backspace);
        assert_eq!(l.shell_line(), None);
        assert!(l.render(80, 8).contains("GUI  A"));
        assert_eq!(shell_launcher("a!").shell_line(), None);
    }

    #[test]
    fn shell_mode_enter_and_alt_enter() {
        let mut l = shell_launcher("!  ls -l");
        assert_eq!(l.shell_line(), Some("ls -l"));
        let out = l.render(80, 8);
        assert!(out.contains("ls -l") && out.contains("Alt+Enter"));
        assert_eq!(
            l.step(Key::Enter),
            Outcome::Send(Request::run_in_terminal("ls -l"))
        );
        assert_eq!(l.step(Key::AltEnter), Outcome::Send(Request::run("ls -l")));
    }

    #[test]
    fn shell_mode_empty_command_is_a_no_op() {
        for typed in ["!", "!   "] {
            let mut l = shell_launcher(typed);
            assert_eq!(l.step(Key::Enter), Outcome::Continue);
            assert_eq!(l.step(Key::AltEnter), Outcome::Continue);
        }
    }

    #[test]
    fn alt_enter_outside_shell_mode_is_enter() {
        let mut l = shell_launcher("");
        assert_eq!(l.step(Key::AltEnter), Outcome::Send(Request::run("a")));
    }

    #[test]
    fn rows_are_labelled_gui_or_tui_and_labels_are_not_matched() {
        let entries = [entry("Browser", "ff", false), entry("Editor", "vim", true)];
        let mut l = Launcher::new(build_items(&entries, &[], true));
        let out = l.render(60, 8);
        assert!(out.contains("GUI  Browser") && out.contains("TUI  Editor"));
        for c in "tui".chars() {
            l.step(Key::Char(c));
        }
        assert!(l.rows().is_empty());
    }
}
