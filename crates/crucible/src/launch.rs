//! `crucible launch` — the program launcher TUI (run inside the compositor's terminal overlay).
//! Desktop-entry parsing, discovery, `$PATH` scanning, ranking and the key state machine are pure
//! (or take explicit paths); only [`run`] touches the terminal, the environment and the socket.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crucible_compositor::ipc::{
    Request, Response, SOCKET_ENV, TERMINAL_ENV, TERMINAL_EXEC_FLAG_ENV,
};

use crate::client::{Client, socket_from_env};
use crate::fuzzy;
use crate::pick::{Key, Outcome, decode_keys, fit, sanitize, window};
use crate::tty::{RawGuard, window_size};

/// Usage text for `crucible launch --help`.
pub const USAGE: &str = "usage: crucible launch\n\nThe CDE program launcher (needs $CDE_SOCKET; CDE runs it in an overlay on Super+Return).\nLists installed desktop applications (XDG .desktop entries) and executables on $PATH.\nType to filter; Up/Down or Ctrl+P/Ctrl+N move; Enter launches the selection into the focused\npane; Esc or Ctrl+C closes. Entries marked Terminal=true run in the configured terminal\n($CDE_TERMINAL, $CDE_TERMINAL_EXEC_FLAG, exported by CDE) and are hidden when it is unknown.\n";

/// A launchable desktop entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopEntry {
    /// The unlocalised `Name`.
    pub name: String,
    /// `Exec` with its field codes stripped.
    pub exec: String,
    /// `Terminal=true`.
    pub terminal: bool,
}

/// Removes the desktop-entry field codes (`%f %F %u %U %d %D %n %N %i %c %k %v %m`) wherever they
/// appear, turns `%%` into `%`, collapses the resulting runs of spaces and trims.
#[must_use]
pub fn strip_field_codes(exec: &str) -> String {
    let mut out = String::with_capacity(exec.len());
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('%') => {
                chars.next();
                out.push('%');
            }
            Some(n) if "fFuUdDnNickvm".contains(*n) => {
                chars.next();
            }
            _ => out.push('%'),
        }
    }
    out.split(' ')
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parses a `.desktop` file. `None` unless it is a visible `Type=Application` entry with a name
/// and a non-empty command after field-code stripping.
#[must_use]
pub fn parse_desktop_entry(text: &str) -> Option<DesktopEntry> {
    let mut in_group = false;
    let (mut name, mut exec, mut kind) = (None, None, None);
    let (mut no_display, mut hidden, mut terminal) = (false, false, false);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_group = line == "[Desktop Entry]";
            continue;
        }
        if !in_group {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "Name" => name = Some(value.to_string()),
            "Exec" => exec = Some(value.to_string()),
            "Type" => kind = Some(value.to_string()),
            "NoDisplay" => no_display = value == "true",
            "Hidden" => hidden = value == "true",
            "Terminal" => terminal = value == "true",
            _ => {}
        }
    }
    if kind.as_deref() != Some("Application") || no_display || hidden {
        return None;
    }
    let name = name.filter(|n| !n.is_empty())?;
    let exec = strip_field_codes(&exec?);
    if exec.is_empty() {
        return None;
    }
    Some(DesktopEntry {
        name,
        exec,
        terminal,
    })
}

/// The `applications` directories to scan, highest precedence first: `$XDG_DATA_HOME` (default
/// `$HOME/.local/share`), then each `$XDG_DATA_DIRS` entry (default `/usr/local/share:/usr/share`).
#[must_use]
pub fn data_dirs_from_env() -> Vec<PathBuf> {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    let home = var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| var("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let dirs = var("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    home.into_iter()
        .chain(std::env::split_paths(&dirs))
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join("applications"))
        .collect()
}

const MAX_DEPTH: usize = 8;

/// Recursively collects `*.desktop` files under `root` as `(desktop-file id, path)`, in sorted
/// order. Unreadable directories are skipped.
fn desktop_files(root: &Path, rel: &str, depth: usize, out: &mut Vec<(String, PathBuf)>) {
    if depth > MAX_DEPTH {
        return;
    }
    let dir = if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    let Ok(read) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut names: Vec<String> = read
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    for name in names {
        let rel_child = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let path = root.join(&rel_child);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            desktop_files(root, &rel_child, depth + 1, out);
        } else if meta.is_file() && name.ends_with(".desktop") {
            out.push((rel_child.replace('/', "-"), path));
        }
    }
}

/// Launchable entries from `dirs` (highest precedence first). The first directory to provide a
/// desktop-file id wins even when its file is filtered out, so a hidden user file shadows the
/// system one. Sorted by name, case-insensitively.
#[must_use]
pub fn desktop_entries(dirs: &[PathBuf]) -> Vec<DesktopEntry> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut entries = Vec::new();
    for dir in dirs {
        let mut files = Vec::new();
        desktop_files(dir, "", 0, &mut files);
        for (id, path) in files {
            if !seen.insert(id) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Some(entry) = parse_desktop_entry(&text) {
                entries.push(entry);
            }
        }
    }
    entries.sort_by_key(|e| e.name.to_lowercase());
    entries
}

/// Executable regular-file names on `$PATH` (first occurrence wins), sorted. Missing directories
/// and non-UTF-8 names are skipped.
#[must_use]
pub fn path_executables(path_var: &OsStr) -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;
    let mut seen: HashSet<String> = HashSet::new();
    for dir in std::env::split_paths(path_var) {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.filter_map(Result::ok) {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let Ok(meta) = std::fs::metadata(entry.path()) else {
                continue;
            };
            if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
                seen.insert(name);
            }
        }
    }
    let mut names: Vec<String> = seen.into_iter().collect();
    names.sort();
    names
}

/// Single-quotes `s` for a POSIX shell.
#[must_use]
pub fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// One selectable row: what is matched, what is shown, and the shell command it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    filter: String,
    label: String,
    cmd: String,
}

/// Builds the candidate list: desktop entries first, then `$PATH` executables. `terminal` is the
/// `(program, exec-flag)` pair; without it `Terminal=true` entries are left out.
#[must_use]
pub fn build_items(
    entries: &[DesktopEntry],
    execs: &[String],
    terminal: Option<(&str, &str)>,
) -> Vec<Item> {
    let mut items = Vec::new();
    for e in entries {
        let cmd = match (e.terminal, terminal) {
            (false, _) => e.exec.clone(),
            (true, Some((prog, flag))) => format!("{} {} {}", sq(prog), sq(flag), e.exec),
            (true, None) => continue,
        };
        let first = e.exec.split(' ').next().unwrap_or("");
        items.push(Item {
            filter: e.name.clone(),
            label: format!("{} [{first}]", e.name),
            cmd,
        });
    }
    for name in execs {
        items.push(Item {
            filter: name.clone(),
            label: name.clone(),
            cmd: sq(name),
        });
    }
    items
}

/// Launcher state.
#[derive(Debug)]
pub struct Launcher {
    items: Vec<Item>,
    query: String,
    selected: usize,
    status: Option<String>,
}

impl Launcher {
    /// A launcher over `items`.
    #[must_use]
    pub fn new(items: Vec<Item>) -> Launcher {
        Launcher {
            items,
            query: String::new(),
            selected: 0,
            status: None,
        }
    }

    /// Indices of the items matching the query, best first (ties keep list order).
    #[must_use]
    pub fn rows(&self) -> Vec<usize> {
        let names: Vec<&str> = self.items.iter().map(|i| i.filter.as_str()).collect();
        fuzzy::filter(&self.query, &names)
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
            Key::Enter => {
                return match self.rows().get(self.selected) {
                    Some(&i) => Outcome::Send(Request::Run(self.items[i].cmd.clone())),
                    None => Outcome::Continue,
                };
            }
        }
        Outcome::Continue
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
    #[must_use]
    pub fn render(&self, cols: u16, rows: u16) -> String {
        let width = usize::from(cols).max(10);
        let height = usize::from(rows).max(4);
        let mut lines: Vec<String> = vec![fit(&format!("run> {}_", sanitize(&self.query)), width)];
        let all = self.rows();
        let (start, end) = window(self.selected, all.len(), height - 2);
        for (i, &idx) in all.iter().enumerate().take(end).skip(start) {
            let text = fit(&format!("  {}", sanitize(&self.items[idx].label)), width);
            lines.push(if i == self.selected {
                format!("\x1b[7m{text}\x1b[0m")
            } else {
                text
            });
        }
        while lines.len() < height - 1 {
            lines.push(String::new());
        }
        lines.push(fit(self.status.as_deref().unwrap_or(""), width));
        let mut out = String::from("\x1b[H\x1b[2J");
        out.push_str(&lines.join("\r\n"));
        out
    }
}

/// The `(program, exec-flag)` pair from the two env values; `None` unless both are non-empty.
#[must_use]
pub fn terminal_from_env(
    program: Option<std::ffi::OsString>,
    flag: Option<std::ffi::OsString>,
) -> Option<(String, String)> {
    let program = program?.into_string().ok().filter(|p| !p.is_empty())?;
    let flag = flag?.into_string().ok().filter(|f| !f.is_empty())?;
    Some((program, flag))
}

/// Runs the subcommand; returns the exit code.
pub fn run(args: &[String]) -> i32 {
    if let Some(extra) = args.first() {
        eprintln!("crucible launch: unexpected argument {extra:?}\n\n{USAGE}");
        return 2;
    }
    let path = match socket_from_env(std::env::var_os(SOCKET_ENV)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("crucible launch: {e}");
            return 2;
        }
    };
    let mut client = match Client::connect(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("crucible launch: cannot connect to {}: {e}", path.display());
            return 1;
        }
    };
    let terminal = terminal_from_env(
        std::env::var_os(TERMINAL_ENV),
        std::env::var_os(TERMINAL_EXEC_FLAG_ENV),
    );
    let entries = desktop_entries(&data_dirs_from_env());
    let execs = std::env::var_os("PATH")
        .map(|p| path_executables(&p))
        .unwrap_or_default();
    let items = build_items(
        &entries,
        &execs,
        terminal.as_ref().map(|(p, f)| (p.as_str(), f.as_str())),
    );
    let mut launcher = Launcher::new(items);
    let Some(guard) = RawGuard::enter(true) else {
        eprintln!("crucible launch: stdin is not a terminal");
        return 1;
    };
    let code = event_loop(&mut launcher, &mut client);
    drop(guard);
    code
}

fn event_loop(launcher: &mut Launcher, client: &mut Client) -> i32 {
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 256];
    let mut stdin = std::io::stdin();
    loop {
        let (cols, rows) = window_size();
        let mut out = std::io::stdout();
        let _ = out.write_all(launcher.render(cols, rows).as_bytes());
        let _ = out.flush();
        let n = match stdin.read(&mut chunk) {
            Ok(0) => return 0,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return 1,
        };
        pending.extend_from_slice(&chunk[..n]);
        for key in decode_keys(&mut pending) {
            let mut outcome = launcher.step(key);
            if let Outcome::Send(req) = &outcome {
                outcome = match client.request(&req.to_line()) {
                    Ok(line) => match Response::parse(&line) {
                        Some(resp) => launcher.on_response(&resp),
                        None => {
                            launcher.on_response(&Response::Err(format!("bad response {line:?}")))
                        }
                    },
                    Err(e) => {
                        launcher.on_response(&Response::Err(format!("compositor error: {e}")))
                    }
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
    use std::os::unix::fs::PermissionsExt;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "crucible-launch-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(name: &str, exec: &str, terminal: bool) -> DesktopEntry {
        DesktopEntry {
            name: name.into(),
            exec: exec.into(),
            terminal,
        }
    }

    #[test]
    fn field_codes_are_stripped() {
        assert_eq!(strip_field_codes("firefox %u"), "firefox");
        assert_eq!(strip_field_codes("app --f=%f  %U x"), "app --f= x");
        assert_eq!(strip_field_codes("echo 100%% %k"), "echo 100%");
        assert_eq!(strip_field_codes("%F"), "");
    }

    #[test]
    fn desktop_entry_parsing_honours_groups_and_filters() {
        let text = "# c\n[Desktop Entry]\nType=Application\nName=Foo\nName[de]=Fuu\nExec = foo %U\nTerminal=true\n\n[Desktop Action x]\nName=Other\nExec=other\n";
        assert_eq!(parse_desktop_entry(text), Some(entry("Foo", "foo", true)));
        let hidden = "[Desktop Entry]\nType=Application\nName=A\nExec=a\nNoDisplay=true\n";
        assert_eq!(parse_desktop_entry(hidden), None);
        let hidden = "[Desktop Entry]\nType=Application\nName=A\nExec=a\nHidden=true\n";
        assert_eq!(parse_desktop_entry(hidden), None);
        let link = "[Desktop Entry]\nType=Link\nName=A\nExec=a\n";
        assert_eq!(parse_desktop_entry(link), None);
        let only_code = "[Desktop Entry]\nType=Application\nName=A\nExec=%U\n";
        assert_eq!(parse_desktop_entry(only_code), None);
        let wrong_group = "[Other]\nType=Application\nName=A\nExec=a\n";
        assert_eq!(parse_desktop_entry(wrong_group), None);
    }

    #[test]
    fn earlier_dir_shadows_even_when_hidden() {
        let root = tmp("shadow");
        let (user, sys) = (root.join("user"), root.join("sys"));
        std::fs::create_dir_all(sys.join("sub")).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        let app = |n: &str| format!("[Desktop Entry]\nType=Application\nName={n}\nExec={n}\n");
        std::fs::write(sys.join("a.desktop"), app("Alpha")).unwrap();
        std::fs::write(sys.join("sub").join("b.desktop"), app("Bravo")).unwrap();
        std::fs::write(sys.join("c.desktop"), app("charlie")).unwrap();
        std::fs::write(
            user.join("a.desktop"),
            "[Desktop Entry]\nType=Application\nName=Alpha\nExec=a\nHidden=true\n",
        )
        .unwrap();
        std::fs::write(user.join("sub-b.desktop"), app("Bravo-user")).unwrap();
        std::fs::write(user.join("notes.txt"), "x").unwrap();
        let got = desktop_entries(&[user, sys, root.join("missing")]);
        let names: Vec<&str> = got.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Bravo-user", "charlie"]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn path_scan_finds_executables_first_wins() {
        let root = tmp("path");
        let (d1, d2) = (root.join("d1"), root.join("d2"));
        std::fs::create_dir_all(&d1).unwrap();
        std::fs::create_dir_all(&d2).unwrap();
        for (dir, name, mode) in [
            (&d1, "zed", 0o755),
            (&d1, "plain", 0o644),
            (&d2, "zed", 0o755),
            (&d2, "alpha", 0o700),
        ] {
            let p = dir.join(name);
            std::fs::write(&p, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        std::fs::create_dir_all(d1.join("subdir")).unwrap();
        let var = std::env::join_paths([d1, root.join("nope"), d2]).unwrap();
        assert_eq!(path_executables(&var), ["alpha", "zed"]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn enter_builds_the_run_command_for_each_kind() {
        let entries = [
            entry("Browser", "firefox --new", false),
            entry("Editor", "vim", true),
        ];
        let items = build_items(&entries, &["it's".into()], Some(("kitty", "-e")));
        let mut l = Launcher::new(items);
        assert_eq!(
            l.step(Key::Enter),
            Outcome::Send(Request::Run("firefox --new".into()))
        );
        l.step(Key::Down);
        assert_eq!(
            l.step(Key::Enter),
            Outcome::Send(Request::Run("'kitty' '-e' vim".into()))
        );
        l.step(Key::Down);
        assert_eq!(
            l.step(Key::Enter),
            Outcome::Send(Request::Run("'it'\\''s'".into()))
        );
    }

    #[test]
    fn terminal_entries_are_hidden_without_a_terminal() {
        let entries = [entry("Editor", "vim", true), entry("Browser", "ff", false)];
        let items = build_items(&entries, &[], None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].filter, "Browser");
        assert_eq!(terminal_from_env(Some("k".into()), Some("".into())), None);
        assert_eq!(terminal_from_env(None, Some("-e".into())), None);
        assert_eq!(
            terminal_from_env(Some("k".into()), Some("-e".into())),
            Some(("k".into(), "-e".into()))
        );
    }

    #[test]
    fn desktop_entries_rank_ahead_of_executables_on_ties() {
        let items = build_items(&[entry("vim", "vim", false)], &["vim".into()], None);
        let l = Launcher::new(items);
        assert_eq!(l.rows(), [0, 1]);
    }

    #[test]
    fn err_response_stays_open_ok_and_exit_keys_leave() {
        let items = build_items(&[entry("A", "a", false)], &[], None);
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
        let items = build_items(&[entry("A", "a", false)], &[], None);
        let mut l = Launcher::new(items);
        l.step(Key::Char('z'));
        l.step(Key::Char('z'));
        assert_eq!(l.step(Key::Enter), Outcome::Continue);
    }

    #[test]
    fn render_marks_selection_and_neutralises_controls() {
        let items = build_items(&[entry("A\x1b[31m", "a", false)], &[], None);
        let out = Launcher::new(items).render(40, 8);
        assert!(out.contains("\x1b[7m"));
        assert!(!out.contains("A\x1b[31m"));
    }
}
