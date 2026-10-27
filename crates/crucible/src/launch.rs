//! `crucible launch` — the program launcher TUI (run inside the compositor's terminal overlay).
//! Desktop-entry parsing, discovery, ranking and the key state machine are pure
//! (or take explicit paths); only [`run`] touches the terminal, the environment and the socket.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crucible_compositor::ipc::{
    LAUNCHER_TUI_ENV, Request, Response, SOCKET_ENV, TERMINAL_ENV, TERMINAL_EXEC_FLAG_ENV,
};

use crate::client::{Client, socket_from_env};
use crate::fuzzy;
use crate::pick::{Key, Outcome, decode_keys, fit, sanitize, window};
use crate::style::Style;
use crate::tty::{RawGuard, window_size};

/// Usage text for `crucible launch --help`.
pub const USAGE: &str = "usage: crucible launch\n\nThe CDE program launcher (needs $CDE_SOCKET; CDE runs it in an overlay on Super+Return).\nLists installed desktop applications (XDG .desktop entries), each labelled GUI or TUI\n(Terminal=true), plus TUI programs from a built-in list (htop, vim, lazygit, …) and the config's\nlauncher { tui } entries when they are found on PATH. Type to filter by name; Up/Down or\nCtrl+P/Ctrl+N move; Enter launches the selection into the focused pane; Esc or Ctrl+C closes. TUI entries run in the configured terminal\n($CDE_TERMINAL, $CDE_TERMINAL_EXEC_FLAG, exported by CDE), which closes when the program exits, and are hidden when it is unknown.\n\nShell-line mode: start the query with ! to type a shell command instead. Enter runs it in a\nterminal; Alt+Enter runs it detached without a terminal. An empty command does nothing.\n";

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

/// Single-quotes `s` for a POSIX shell.
#[must_use]
pub fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// One selectable row: the name (matched and shown), its kind and the shell command it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    filter: String,
    label: String,
    /// `Terminal=true` (shown as `TUI`, else `GUI`).
    tui: bool,
    cmd: String,
}

impl Item {
    fn kind(&self) -> &'static str {
        if self.tui { "TUI" } else { "GUI" }
    }
}

/// Builds the candidate list from desktop entries. A `Terminal=true` entry keeps its bare `Exec`
/// (the compositor wraps it in the terminal via `run-tui`) and is left out unless `terminal_known`.
#[must_use]
pub fn build_items(entries: &[DesktopEntry], terminal_known: bool) -> Vec<Item> {
    let mut items = Vec::new();
    for e in entries {
        if e.terminal && !terminal_known {
            continue;
        }
        items.push(Item {
            filter: e.name.clone(),
            label: e.name.clone(),
            tui: e.terminal,
            cmd: e.exec.clone(),
        });
    }
    items
}

/// TUI programs offered when found on `PATH`, beside any `launcher { tui }` config entries.
pub const BUILTIN_TUI: &[&str] = &[
    "htop",
    "btop",
    "nvtop",
    "top",
    "nano",
    "vim",
    "nvim",
    "mc",
    "ranger",
    "yazi",
    "lazygit",
    "tig",
    "ncdu",
    "claude",
    "codex",
    "glances",
    "bottom",
    "lazydocker",
    "k9s",
    "ncmpcpp",
    "weechat",
    "irssi",
    "cmus",
    "newsboat",
];

/// Whether `*` globbing `pattern` (`*` = any run of characters, possibly empty) matches `name`.
#[must_use]
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if name.len() < first.len() + last.len() || !name.starts_with(first) || !name.ends_with(last) {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The built-in names plus the `:`-separated `configured` env value, deduped, built-ins first.
#[must_use]
pub fn tui_patterns(configured: Option<&std::ffi::OsStr>) -> Vec<String> {
    let mut out: Vec<String> = BUILTIN_TUI.iter().map(|s| (*s).to_string()).collect();
    if let Some(v) = configured.and_then(|v| v.to_str()) {
        for p in v.split(':').filter(|p| !p.is_empty()) {
            if !out.iter().any(|o| o == p) {
                out.push(p.to_string());
            }
        }
    }
    out
}

/// Resolves exact names and `*` globs against `dirs` (pure over the explicit list). Exact names
/// take the first directory holding an executable regular file (symlinks followed); globs match
/// every executable basename, the earlier directory winning a duplicate. Relative hits are made
/// absolute (canonicalized against the cwd) or dropped. Returns `(name, absolute path)` deduped by
/// name and sorted by name. Unreadable directories are skipped.
#[must_use]
pub fn resolve_tui(patterns: &[String], dirs: &[PathBuf]) -> Vec<(String, PathBuf)> {
    let mut found: std::collections::BTreeMap<String, PathBuf> = std::collections::BTreeMap::new();
    for pat in patterns {
        if pat.contains('*') {
            for dir in dirs {
                let Ok(rd) = std::fs::read_dir(dir) else {
                    continue;
                };
                for e in rd.flatten() {
                    let Some(name) = e.file_name().to_str().map(str::to_string) else {
                        continue;
                    };
                    if glob_match(pat, &name) && is_executable_file(&e.path()) {
                        found.entry(name).or_insert_with(|| e.path());
                    }
                }
            }
        } else if let Some(p) = dirs
            .iter()
            .map(|d| d.join(pat))
            .find(|p| is_executable_file(p))
        {
            found.entry(pat.clone()).or_insert(p);
        }
    }
    found
        .into_iter()
        .filter_map(|(name, p)| {
            let abs = if p.is_absolute() {
                p
            } else {
                std::fs::canonicalize(&p).ok()?
            };
            Some((name, abs))
        })
        .collect()
}

/// TUI rows for the resolved programs: skipped without a `terminal`, and for any program whose
/// name equals the basename of a desktop entry's `Exec` first word (wrapper commands such as
/// `env … vim` are not recognised). The command is the quoted absolute path (the compositor wraps
/// it in the terminal via `run-tui`).
#[must_use]
pub fn tui_items(
    programs: &[(String, PathBuf)],
    entries: &[DesktopEntry],
    terminal_known: bool,
) -> Vec<Item> {
    if !terminal_known {
        return Vec::new();
    }
    let desktop: HashSet<&str> = entries
        .iter()
        .filter_map(|e| e.exec.split_whitespace().next())
        .map(|w| w.rsplit('/').next().unwrap_or(w))
        .collect();
    programs
        .iter()
        .filter(|(name, _)| !desktop.contains(name.as_str()))
        .map(|(name, path)| Item {
            filter: name.clone(),
            label: name.clone(),
            tui: true,
            cmd: sq(&path.to_string_lossy()),
        })
        .collect()
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

    /// The shell command typed in `!` mode: `None` outside the mode, `Some("")` while empty.
    #[must_use]
    pub fn shell_line(&self) -> Option<&str> {
        self.query.strip_prefix('!').map(str::trim_start)
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
            Key::Enter | Key::AltEnter if self.shell_line().is_some() => {
                let cmd = self.shell_line().unwrap_or("").trim_end();
                if cmd.is_empty() {
                    return Outcome::Continue;
                }
                let cmd = cmd.to_string();
                return Outcome::Send(if key == Key::Enter {
                    Request::RunInTerminal(cmd)
                } else {
                    Request::Run(cmd)
                });
            }
            Key::Enter | Key::AltEnter => {
                return match self.rows().get(self.selected) {
                    Some(&i) => {
                        let item = &self.items[i];
                        let cmd = item.cmd.clone();
                        Outcome::Send(if item.tui {
                            Request::RunTui(cmd)
                        } else {
                            Request::Run(cmd)
                        })
                    }
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
        let mut lines: Vec<String> = vec![fit(&format!("run> {}_", sanitize(&self.query)), width)];
        if let Some(cmd) = self.shell_line() {
            let text = fit(
                &format!(
                    "  SH  {}  [Enter: run in terminal, Alt+Enter: run detached]",
                    sanitize(cmd)
                ),
                width,
            );
            lines.push(style.highlight(&text));
        } else {
            let all = self.rows();
            let (start, end) = window(self.selected, all.len(), height - 2);
            for (i, &idx) in all.iter().enumerate().take(end).skip(start) {
                let item = &self.items[idx];
                let text = fit(
                    &format!("  {}  {}", item.kind(), sanitize(&item.label)),
                    width,
                );
                lines.push(if i == self.selected {
                    style.highlight(&text)
                } else {
                    text
                });
            }
        }
        while lines.len() < height - 1 {
            lines.push(String::new());
        }
        lines.push(fit(self.status.as_deref().unwrap_or(""), width));
        let mut out = style.frame_start();
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
    let terminal_known = terminal.is_some();
    let mut items = build_items(&entries, terminal_known);
    // PATH-derived TUI rows are appended after the desktop entries (name-sorted).
    let dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let patterns = tui_patterns(std::env::var_os(LAUNCHER_TUI_ENV).as_deref());
    items.extend(tui_items(
        &resolve_tui(&patterns, &dirs),
        &entries,
        terminal_known,
    ));
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
    let style = Style::from_env();
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 256];
    let mut stdin = std::io::stdin();
    loop {
        let (cols, rows) = window_size();
        let mut out = std::io::stdout();
        let _ = out.write_all(launcher.render_styled(cols, rows, &style).as_bytes());
        let _ = out.flush();
        crate::latency::first_frame("launch");
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
    fn enter_builds_the_run_command_for_each_kind() {
        let entries = [
            entry("Browser", "firefox --new", false),
            entry("Editor", "vim", true),
        ];
        let items = build_items(&entries, true);
        let mut l = Launcher::new(items);
        assert_eq!(
            l.step(Key::Enter),
            Outcome::Send(Request::Run("firefox --new".into()))
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
        let items = build_items(&entries, false);
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
    fn err_response_stays_open_ok_and_exit_keys_leave() {
        let items = build_items(&[entry("A", "a", false)], false);
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
        let items = build_items(&[entry("A", "a", false)], false);
        let mut l = Launcher::new(items);
        l.step(Key::Char('z'));
        l.step(Key::Char('z'));
        assert_eq!(l.step(Key::Enter), Outcome::Continue);
    }

    #[test]
    fn render_styled_n1_paints_background_and_reapplies_after_highlight() {
        let st = Style::parse(Some("n1-dark"));
        let out =
            Launcher::new(build_items(&[entry("a", "a", false)], false)).render_styled(40, 8, &st);
        assert!(out.starts_with(&format!("\x1b[H{}\x1b[2J", st.base())));
        assert!(out.contains(&st.highlight("")[..20]));
        assert!(out.contains(&format!("\x1b[0m{}", st.base())));
        assert!(!out.contains("\x1b[7m"));
        assert_eq!(
            Launcher::new(build_items(&[entry("a", "a", false)], false)).render_styled(
                40,
                8,
                &Style::Terminal
            ),
            Launcher::new(build_items(&[entry("a", "a", false)], false)).render(40, 8)
        );
    }
    #[test]
    fn render_marks_selection_and_neutralises_controls() {
        let items = build_items(&[entry("A\x1b[31m", "a", false)], false);
        let out = Launcher::new(items).render(40, 8);
        assert!(out.contains("\x1b[7m"));
        assert!(!out.contains("A\x1b[31m"));
    }

    fn shell_launcher(typed: &str) -> Launcher {
        let mut l = Launcher::new(build_items(&[entry("A", "a", false)], false));
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
            Outcome::Send(Request::RunInTerminal("ls -l".into()))
        );
        assert_eq!(
            l.step(Key::AltEnter),
            Outcome::Send(Request::Run("ls -l".into()))
        );
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
        assert_eq!(
            l.step(Key::AltEnter),
            Outcome::Send(Request::Run("a".into()))
        );
    }

    #[test]
    fn rows_are_labelled_gui_or_tui_and_labels_are_not_matched() {
        let entries = [entry("Browser", "ff", false), entry("Editor", "vim", true)];
        let mut l = Launcher::new(build_items(&entries, true));
        let out = l.render(60, 8);
        assert!(out.contains("GUI  Browser") && out.contains("TUI  Editor"));
        for c in "tui".chars() {
            l.step(Key::Char(c));
        }
        assert!(l.rows().is_empty());
    }

    fn put(dir: &Path, name: &str, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn pats(p: &[&str]) -> Vec<String> {
        p.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn glob_matching_is_star_only() {
        assert!(glob_match("codex-*", "codex-cli"));
        assert!(glob_match("codex-*", "codex-"));
        assert!(glob_match("a*b*c", "aXbYc"));
        assert!(!glob_match("a*b*c", "acb"));
        assert!(!glob_match("codex-*", "codex"));
        assert!(glob_match("htop", "htop") && !glob_match("htop", "htop2"));
    }

    #[test]
    fn resolution_skips_non_executables_and_dirs_follows_symlinks_and_prefers_earlier() {
        let root = tmp("resolve");
        let (a, b, missing) = (root.join("a"), root.join("b"), root.join("nope"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        put(&a, "plain", 0o644);
        put(&b, "plain", 0o755);
        put(&a, "both", 0o755);
        put(&b, "both", 0o755);
        std::fs::create_dir(a.join("adir")).unwrap();
        put(&b, "real", 0o755);
        std::os::unix::fs::symlink(b.join("real"), a.join("link")).unwrap();
        put(&a, "tool-1", 0o755);
        put(&b, "tool-2", 0o755);
        put(&b, "tool-1", 0o755);
        put(&b, "tool-x", 0o600);
        let dirs = [missing, a.clone(), b.clone()];
        let got = resolve_tui(
            &pats(&["plain", "both", "adir", "link", "absent", "tool-*", "zzz-*"]),
            &dirs,
        );
        let want = vec![
            ("both".to_string(), a.join("both")),
            ("link".to_string(), a.join("link")),
            ("plain".to_string(), b.join("plain")),
            ("tool-1".to_string(), a.join("tool-1")),
            ("tool-2".to_string(), b.join("tool-2")),
        ];
        assert_eq!(got, want);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tui_patterns_merge_builtin_and_env() {
        let p = tui_patterns(Some(std::ffi::OsStr::new("k9s:foo-*::htop")));
        assert!(p.contains(&"htop".to_string()) && p.contains(&"foo-*".to_string()));
        assert_eq!(p.iter().filter(|x| *x == "htop").count(), 1);
        assert_eq!(tui_patterns(None).len(), BUILTIN_TUI.len());
    }

    #[test]
    fn tui_rows_dedupe_against_desktop_use_absolute_path_and_need_a_terminal() {
        let programs = vec![
            ("htop".to_string(), PathBuf::from("/usr/bin/htop")),
            ("k9s".to_string(), PathBuf::from("/opt/it's/k9s")),
        ];
        let entries = [entry("Htop", "/usr/bin/htop", true)];
        assert!(tui_items(&programs, &entries, false).is_empty());
        let items = tui_items(&programs, &entries, true);
        assert_eq!(items.len(), 1);
        assert!(items[0].tui && items[0].label == "k9s");
        assert_eq!(items[0].cmd, "'/opt/it'\\''s/k9s'");
        let none = tui_items(&programs, &[], true);
        assert_eq!(none.len(), 2);
    }
}
