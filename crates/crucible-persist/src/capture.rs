//! `/proc` capture of a terminal window's working directory and foreground command, and the one
//! window classifier ([`window_record`]) that turns a managed window into its saved record.
//!
//! Pure parsers over `/proc` text plus a thin filesystem edge that takes a `proc_root` (normally
//! `/proc`; tests point it at a fixture tree).

use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::record::WindowRecord;
use crate::wrapper::{CdeWrapper, RelaunchWrapper, ResumeWrapper};

/// What a terminal window was doing when captured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalCapture {
    /// The working directory (of the foreground process, or the idle shell).
    pub cwd: PathBuf,
    /// The foreground command as one shell-quoted string; `None` when the shell is idle.
    pub command: Option<String>,
}

/// The `/proc/<pid>/stat` fields capture needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    /// Parent pid (4th field).
    pub ppid: i64,
    /// Foreground process group of the controlling terminal (8th field); `-1` when none.
    pub tpgid: i64,
}

/// Parses the text of `/proc/<pid>/children` (whitespace-separated pids).
#[must_use]
pub fn parse_children(text: &str) -> Vec<u32> {
    text.split_whitespace()
        .filter_map(|t| t.parse().ok())
        .collect()
}

/// Parses `/proc/<pid>/stat`. The `comm` field is parenthesised and may contain spaces and `)`,
/// so fields are read after the **last** `)`.
#[must_use]
pub fn parse_stat(text: &str) -> Option<Stat> {
    let rest = &text[text.rfind(')')? + 1..];
    // After comm: state(3) ppid(4) pgrp(5) session(6) tty_nr(7) tpgid(8)
    let mut f = rest.split_whitespace();
    let _state = f.next()?;
    let ppid = f.next()?.parse().ok()?;
    let _pgrp = f.next()?;
    let _session = f.next()?;
    let _tty = f.next()?;
    let tpgid = f.next()?.parse().ok()?;
    Some(Stat { ppid, tpgid })
}

/// Splits the bytes of `/proc/<pid>/cmdline` into arguments (NUL-separated). Non-UTF-8 bytes are
/// replaced lossily: the result is for display and relaunch, not identity.
#[must_use]
pub fn parse_cmdline(bytes: &[u8]) -> Vec<String> {
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    if bytes.is_empty() {
        return Vec::new();
    }
    bytes
        .split(|b| *b == 0)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect()
}

/// Quotes one argument for a POSIX shell: left bare when it only holds `[A-Za-z0-9_./=:,+@%-]`,
/// otherwise single-quoted with `'` written as `'\''`. The empty argument becomes `''`.
#[must_use]
pub fn shell_quote(arg: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_./=:,+@%-".contains(c);
    if !arg.is_empty() && arg.chars().all(safe) {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

/// Joins arguments into one shell-quoted command string.
#[must_use]
pub fn join_command(args: &[String]) -> String {
    args.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `argv0` names an interactive shell (basename, a login shell's leading `-` ignored).
#[must_use]
pub fn is_shell_program(argv0: &str) -> bool {
    let name = Path::new(argv0)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(argv0);
    matches!(
        name.trim_start_matches('-'),
        "sh" | "bash"
            | "dash"
            | "zsh"
            | "fish"
            | "ksh"
            | "mksh"
            | "tcsh"
            | "csh"
            | "nu"
            | "elvish"
            | "xonsh"
    )
}

fn pid_dir(proc_root: &Path, pid: u32) -> PathBuf {
    proc_root.join(pid.to_string())
}

/// Direct children of `pid`, from `/proc/<pid>/task/*/children` (tasks in numeric order). A
/// missing pid yields an empty list.
#[must_use]
pub fn children_of(proc_root: &Path, pid: u32) -> Vec<u32> {
    let task_dir = pid_dir(proc_root, pid).join("task");
    let Ok(entries) = fs::read_dir(task_dir) else {
        return Vec::new();
    };
    let mut tasks: Vec<(u64, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| Some((e.file_name().to_str()?.parse().ok()?, e.path())))
        .collect();
    tasks.sort();
    tasks
        .into_iter()
        .filter_map(|(_, p)| fs::read_to_string(p.join("children")).ok())
        .flat_map(|t| parse_children(&t))
        .collect()
}

fn read_stat(proc_root: &Path, pid: u32) -> Option<Stat> {
    parse_stat(&fs::read_to_string(pid_dir(proc_root, pid).join("stat")).ok()?)
}

fn read_cwd(proc_root: &Path, pid: u32) -> Option<PathBuf> {
    fs::read_link(pid_dir(proc_root, pid).join("cwd")).ok()
}

fn read_cmdline(proc_root: &Path, pid: u32) -> Vec<String> {
    fs::read(pid_dir(proc_root, pid).join("cmdline"))
        .map(|b| parse_cmdline(&b))
        .unwrap_or_default()
}

/// The ancestors of `pid` (parent first), walking `ppid` up to `max_depth` steps; stops at pid 0
/// or an unreadable `stat`. `pid` itself is not included.
#[must_use]
pub fn ancestors(proc_root: &Path, pid: u32, max_depth: usize) -> Vec<u32> {
    let mut out = Vec::new();
    let mut cur = pid;
    while out.len() < max_depth {
        let Some(stat) = read_stat(proc_root, cur) else {
            break;
        };
        let Ok(parent) = u32::try_from(stat.ppid) else {
            break;
        };
        if parent == 0 {
            break;
        }
        out.push(parent);
        cur = parent;
    }
    out
}

/// Whether `argv0` names the `claude` CLI (basename).
fn is_claude_program(argv0: &str) -> bool {
    Path::new(argv0).file_name().and_then(|n| n.to_str()) == Some("claude")
}

/// Rewrites a `claude` argv into `claude --resume <session_id>` plus the remaining arguments.
/// `argv[0]` is kept as given. Dropped: `-r`/`--resume` (with its optional value, the next
/// argument unless it starts with `-`, or `--resume=<v>`), `-c`/`--continue`, `--session-id <v>`
/// / `--session-id=<v>` and `--fork-session`. Everything else (e.g. `-n <name>`) keeps its order.
#[must_use]
pub fn claude_resume_argv(argv: &[String], session_id: &str) -> Vec<String> {
    let mut out = vec![
        argv.first()
            .cloned()
            .unwrap_or_else(|| "claude".to_string()),
        "--resume".to_string(),
        session_id.to_string(),
    ];
    let mut rest = argv.iter().skip(1).peekable();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "-r" | "--resume" => {
                if rest.peek().is_some_and(|n| !n.starts_with('-')) {
                    rest.next();
                }
            }
            "--session-id" => {
                rest.next();
            }
            "-c" | "--continue" | "--fork-session" => {}
            a if a.starts_with("--resume=") || a.starts_with("--session-id=") => {}
            _ => out.push(arg.clone()),
        }
    }
    out
}

/// Extracts the `"sessionId"` string value from a claude session record. A minimal tokenizer (no
/// JSON dependency): a string followed by `:` is a key, so a `sessionId` inside another value is
/// never mistaken for the key. Escaped or empty ids yield `None`.
fn extract_session_id(text: &str) -> Option<String> {
    fn string(it: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<String> {
        let mut s = String::new();
        loop {
            match it.next()? {
                '"' => return Some(s),
                '\\' => {
                    s.push('\\');
                    s.push(it.next()?);
                }
                c => s.push(c),
            }
        }
    }
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        if c != '"' {
            continue;
        }
        let key = string(&mut it)?;
        while it.next_if(|c| c.is_whitespace()).is_some() {}
        if it.next_if_eq(&':').is_none() {
            continue;
        }
        while it.next_if(|c| c.is_whitespace()).is_some() {}
        if key == "sessionId" {
            it.next_if_eq(&'"')?;
            let id = string(&mut it)?;
            let ok = !id.is_empty()
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            return ok.then_some(id);
        }
    }
    None
}

/// The session id in `<sessions_dir>/<pid>.json`. Missing, unreadable, malformed or empty → `None`.
#[must_use]
pub fn claude_session_id(sessions_dir: &Path, pid: u32) -> Option<String> {
    extract_session_id(&fs::read_to_string(sessions_dir.join(format!("{pid}.json"))).ok()?)
}

/// The resume command for `pid` when its `argv` is `claude` and it has a session record.
fn claude_command(sessions_dir: Option<&Path>, pid: u32, argv: &[String]) -> Option<String> {
    if !is_claude_program(argv.first()?) {
        return None;
    }
    let id = claude_session_id(sessions_dir?, pid)?;
    Some(join_command(&claude_resume_argv(argv, &id)))
}

/// Max depth searched below a resume wrapper for its `claude` process.
const CLAUDE_SEARCH_DEPTH: usize = 4;

/// Breadth-first search below `wrapper_pid` for a `claude` process with a session record: its cwd
/// and resume command.
fn claude_descendant(
    proc_root: &Path,
    sessions_dir: &Path,
    wrapper_pid: u32,
) -> Option<(PathBuf, String)> {
    let mut level = vec![wrapper_pid];
    for _ in 0..CLAUDE_SEARCH_DEPTH {
        level = level
            .into_iter()
            .flat_map(|p| children_of(proc_root, p))
            .collect();
        for pid in &level {
            let argv = read_cmdline(proc_root, *pid);
            if let Some(command) = claude_command(Some(sessions_dir), *pid, &argv)
                && let Some(cwd) = read_cwd(proc_root, *pid)
            {
                return Some((cwd, command));
            }
        }
    }
    None
}

/// Captures the terminal client `terminal_pid`: the working directory and foreground command of
/// whatever runs in it. `None` when the pid has no readable children/cwd.
///
/// The shell is the first child (with several, the first with a set `tpgid`); its `tpgid` is the
/// foreground group leader. If that is the shell itself (or unset) the command is `None` and the
/// cwd is the shell's; otherwise cwd and command come from the foreground process, falling back
/// to the shell if it vanished meanwhile. A foreground process that is itself a shell and a
/// direct child of the session leader (ghostty's `sh -c "bash --posix"` integration wrapper) is
/// the real interactive shell: the terminal is idle, with that shell's cwd. CDE's own wrappers
/// are not special here; [`window_record`] handles them first.
#[must_use]
pub fn capture_terminal(
    proc_root: &Path,
    sessions_dir: Option<&Path>,
    terminal_pid: u32,
) -> Option<TerminalCapture> {
    let children = children_of(proc_root, terminal_pid);
    let shell = if children.len() == 1 {
        children[0]
    } else {
        *children
            .iter()
            .find(|c| read_stat(proc_root, **c).is_some_and(|s| s.tpgid > 0))
            .or(children.first())?
    };
    let shell_cwd = read_cwd(proc_root, shell)?;
    let idle = TerminalCapture {
        cwd: shell_cwd,
        command: None,
    };
    let Some(tpgid) = read_stat(proc_root, shell)
        .and_then(|s| u32::try_from(s.tpgid).ok())
        .filter(|t| *t != 0 && *t != shell)
    else {
        return Some(idle);
    };
    let args = read_cmdline(proc_root, tpgid);
    let fg_is_wrapped_shell = read_stat(proc_root, tpgid)
        .is_some_and(|s| s.ppid == i64::from(shell))
        && args.first().is_some_and(|a| is_shell_program(a));
    match (read_cwd(proc_root, tpgid), args.is_empty()) {
        (Some(cwd), false) if fg_is_wrapped_shell => Some(TerminalCapture { cwd, command: None }),
        (Some(cwd), false) => Some(TerminalCapture {
            cwd,
            command: Some(
                claude_command(sessions_dir, tpgid, &args).unwrap_or_else(|| join_command(&args)),
            ),
        }),
        _ => Some(idle),
    }
}

/// The record a CDE wrapper stands for: `resume` → a terminal, `relaunch` → an app.
fn wrapper_record(wrapper: CdeWrapper) -> WindowRecord {
    match wrapper {
        CdeWrapper::App(cap) => WindowRecord::App {
            app_id: cap.app_id,
            cmdline: Some(cap.cmdline),
        },
        CdeWrapper::Terminal(cap) => WindowRecord::Terminal {
            cwd: cap.cwd,
            command: cap.command,
        },
    }
}

/// A process whose own argv is a bare CDE wrapper (`crucible resume|relaunch …`).
fn wrapper_process(proc_root: &Path, pid: u32) -> Option<CdeWrapper> {
    let argv = read_cmdline(proc_root, pid);
    RelaunchWrapper::from_argv(&argv)
        .map(CdeWrapper::App)
        .or_else(|| ResumeWrapper::from_argv(&argv).map(CdeWrapper::Terminal))
}

/// The `crucible relaunch` wrapper a placeholder window runs, with the wrapper's pid:
/// `window_pid` is the window's own client (the terminal), so the wrapper is the pid itself or,
/// normally, one of its direct children. `None` for any other window.
#[must_use]
pub fn relaunch_wrapper_of(proc_root: &Path, window_pid: u32) -> Option<(u32, RelaunchWrapper)> {
    std::iter::once(window_pid)
        .chain(children_of(proc_root, window_pid))
        .find_map(|pid| RelaunchWrapper::from_argv(&read_cmdline(proc_root, pid)).map(|w| (pid, w)))
}

/// Classifies one managed window into the record to save. Pure over `proc_root` (normally
/// `/proc`). `pid` is the client PID, `terminal` whether it is a CDE-spawned terminal (exact
/// pid), `origin` the saved record its spawn stands for (a restored window or a relaunched app),
/// `app_id` the element's app id and `exec_flag` the terminal's exec flag. Origin first:
///
/// - **Terminal:** (1) a child of the terminal that is a CDE wrapper (a pending restore banner or
///   relaunch placeholder — the foreground is CDE itself) yields the origin, else the decoded
///   wrapper; (2) otherwise the live [`capture_terminal`] result (the user may have `cd`'d or run
///   something since the restore); (3) if that finds nothing, the origin; else the terminal's own
///   argv decoded as a wrapper ([`CdeWrapper::decode`]); else an idle terminal in the terminal
///   process's cwd (empty when unreadable). Never an app unless an origin or a relaunch wrapper
///   says so.
/// - **App:** the client's argv — a CDE wrapper in it is decoded before joining; otherwise the
///   app id and the joined argv. An app whose argv yields no cmdline but whose origin is an app
///   (a relaunched app) records the origin's cmdline.
#[must_use]
pub fn window_record(
    proc_root: &Path,
    sessions_dir: Option<&Path>,
    pid: Option<u32>,
    terminal: bool,
    origin: Option<&WindowRecord>,
    app_id: Option<String>,
    exec_flag: &str,
) -> WindowRecord {
    if terminal {
        let Some(pid) = pid else {
            return origin.cloned().unwrap_or(WindowRecord::Terminal {
                cwd: PathBuf::new(),
                command: None,
            });
        };
        if let Some((wrapper_pid, wrapper)) = children_of(proc_root, pid)
            .into_iter()
            .find_map(|c| wrapper_process(proc_root, c).map(|w| (c, w)))
        {
            if let (Some(dir), CdeWrapper::Terminal(_)) = (sessions_dir, &wrapper)
                && let Some((cwd, command)) = claude_descendant(proc_root, dir, wrapper_pid)
            {
                return WindowRecord::Terminal {
                    cwd,
                    command: Some(command),
                };
            }
            return origin.cloned().unwrap_or_else(|| wrapper_record(wrapper));
        }
        if let Some(cap) = capture_terminal(proc_root, sessions_dir, pid) {
            return WindowRecord::Terminal {
                cwd: cap.cwd,
                command: cap.command,
            };
        }
        if let Some(origin) = origin {
            return origin.clone();
        }
        return match CdeWrapper::decode(&read_cmdline(proc_root, pid), exec_flag) {
            Some(wrapper) => wrapper_record(wrapper),
            None => WindowRecord::Terminal {
                cwd: read_cwd(proc_root, pid).unwrap_or_default(),
                command: None,
            },
        };
    }
    let argv = pid.map(|p| read_cmdline(proc_root, p)).unwrap_or_default();
    if argv.is_empty() {
        if let Some(WindowRecord::App {
            app_id: origin_id,
            cmdline: Some(cmdline),
        }) = origin
        {
            return WindowRecord::App {
                app_id: origin_id.clone().or(app_id),
                cmdline: Some(cmdline.clone()),
            };
        }
        return WindowRecord::App {
            app_id,
            cmdline: None,
        };
    }
    match CdeWrapper::decode(&argv, exec_flag) {
        Some(CdeWrapper::App(cap)) => WindowRecord::App {
            app_id: cap.app_id.or(app_id),
            cmdline: Some(cap.cmdline),
        },
        Some(wrapper) => wrapper_record(wrapper),
        None => WindowRecord::App {
            app_id,
            cmdline: Some(join_command(&argv)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A fixture `/proc` tree under a temp dir.
    struct Proc(PathBuf);

    impl Proc {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("cde-proc-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&d);
            fs::create_dir_all(&d).unwrap();
            Proc(d)
        }
        /// Adds a process: `children` are read from `task/<pid>/children`.
        fn add(&self, id: (u32, &str, u32), tpgid: i64, cwd: &str, argv: &[&str], kids: &[u32]) {
            let (pid, comm, ppid) = id;
            let d = self.0.join(pid.to_string());
            fs::create_dir_all(d.join("task").join(pid.to_string())).unwrap();
            fs::write(
                d.join("stat"),
                format!("{pid} ({comm}) S {ppid} {pid} {pid} 34816 {tpgid} 4194304 100 0 0 0\n"),
            )
            .unwrap();
            symlink(cwd, d.join("cwd")).unwrap();
            let mut cmd = Vec::new();
            for a in argv {
                cmd.extend_from_slice(a.as_bytes());
                cmd.push(0);
            }
            fs::write(d.join("cmdline"), cmd).unwrap();
            let list: Vec<String> = kids.iter().map(u32::to_string).collect();
            fs::write(
                d.join("task").join(pid.to_string()).join("children"),
                list.join(" ") + if kids.is_empty() { "" } else { " " },
            )
            .unwrap();
        }
    }
    impl Drop for Proc {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn parse_stat_handles_awkward_comm() {
        let s = parse_stat("42 (bash) S 7 42 42 34816 99 4194560 1 2 3").unwrap();
        assert_eq!(s, Stat { ppid: 7, tpgid: 99 });
        let s = parse_stat("42 (we ird) x) S 7 42 42 34816 -1 0").unwrap();
        assert_eq!(s, Stat { ppid: 7, tpgid: -1 });
        assert_eq!(parse_stat("garbage"), None);
        assert_eq!(parse_stat("1 (x) S 1 2"), None);
        assert_eq!(parse_stat("1 (x) S notnum 2 3 4 5"), None);
    }

    #[test]
    fn parse_children_and_cmdline() {
        assert_eq!(parse_children("12 34 \n"), vec![12, 34]);
        assert!(parse_children("").is_empty());
        assert_eq!(parse_cmdline(b"a\0b c\0"), vec!["a", "b c"]);
        assert_eq!(parse_cmdline(b"a\0\0"), vec!["a", ""]);
        assert!(parse_cmdline(b"").is_empty());
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(shell_quote("cargo"), "cargo");
        assert_eq!(
            shell_quote("--flag=a,b:c+d@e%f-g_h./i"),
            "--flag=a,b:c+d@e%f-g_h./i"
        );
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a;b$c\"d"), "'a;b$c\"d'");
        assert_eq!(shell_quote("*"), "'*'");
        let args = ["echo".to_string(), "a b".to_string(), "c'd".to_string()];
        assert_eq!(join_command(&args), "echo 'a b' 'c'\\''d'");
    }

    #[test]
    fn idle_shell_reports_cwd_only() {
        let p = Proc::new("idle");
        p.add((100, "ghostty", 1), 0, "/", &["ghostty"], &[200]);
        p.add((200, "bash", 100), 200, "/home/u/proj", &["bash"], &[]);
        let c = capture_terminal(&p.0, None, 100).unwrap();
        assert_eq!(
            c,
            TerminalCapture {
                cwd: "/home/u/proj".into(),
                command: None
            }
        );
    }

    /// ghostty's bash integration (measured on host-b 2026-10-26):
    /// `ghostty → /bin/sh -c "/bin/bash --posix" → /bin/bash --posix`, the inner bash foreground.
    #[test]
    fn ghostty_sh_c_wrapped_shell_is_idle_with_the_inner_cwd() {
        let p = Proc::new("wrapped-idle");
        p.add((100, "ghostty", 1), 0, "/", &["ghostty"], &[200]);
        p.add(
            (200, "sh", 100),
            300,
            "/home/u",
            &["/bin/sh", "-c", "/bin/bash --posix"],
            &[300],
        );
        p.add(
            (300, "bash", 200),
            300,
            "/tmp",
            &["/bin/bash", "--posix"],
            &[],
        );
        let c = capture_terminal(&p.0, None, 100).unwrap();
        assert_eq!(
            c,
            TerminalCapture {
                cwd: "/tmp".into(),
                command: None
            }
        );
    }

    #[test]
    fn command_inside_a_wrapped_shell_is_recorded() {
        let p = Proc::new("wrapped-fg");
        p.add((100, "ghostty", 1), 0, "/", &["ghostty"], &[200]);
        p.add(
            (200, "sh", 100),
            400,
            "/home/u",
            &["/bin/sh", "-c", "/bin/bash --posix"],
            &[300],
        );
        p.add(
            (300, "bash", 200),
            400,
            "/tmp",
            &["/bin/bash", "--posix"],
            &[400],
        );
        p.add((400, "htop", 300), 400, "/proj", &["htop"], &[]);
        let c = capture_terminal(&p.0, None, 100).unwrap();
        assert_eq!(c.cwd, PathBuf::from("/proj"));
        assert_eq!(c.command.as_deref(), Some("htop"));
    }

    #[test]
    fn shell_program_names() {
        for s in [
            "/bin/bash",
            "bash",
            "-bash",
            "/usr/bin/zsh",
            "fish",
            "/bin/sh",
        ] {
            assert!(is_shell_program(s), "{s}");
        }
        for s in ["htop", "/usr/bin/vim", "bashtop", "cargo"] {
            assert!(!is_shell_program(s), "{s}");
        }
    }

    #[test]
    fn foreground_command_with_quoting() {
        let p = Proc::new("fg");
        p.add((100, "ghostty", 1), 0, "/", &["ghostty"], &[200]);
        p.add((200, "bash", 100), 300, "/home/u", &["bash"], &[300]);
        p.add(
            (300, "htop", 200),
            300,
            "/var/log",
            &["htop", "-p", "1,2", "a b", "it's"],
            &[],
        );
        let c = capture_terminal(&p.0, None, 100).unwrap();
        assert_eq!(c.cwd, PathBuf::from("/var/log"));
        assert_eq!(c.command.as_deref(), Some("htop -p 1,2 'a b' 'it'\\''s'"));
    }

    #[test]
    fn comm_with_spaces_and_paren_in_shell_and_foreground() {
        let p = Proc::new("comm");
        p.add((100, "my term)", 1), 0, "/", &["t"], &[200]);
        p.add((200, "ba sh) x", 100), 300, "/h", &["bash"], &[300]);
        p.add((300, "we (ird) p", 200), 300, "/w", &["weird"], &[]);
        let c = capture_terminal(&p.0, None, 100).unwrap();
        assert_eq!(c.cwd, PathBuf::from("/w"));
        assert_eq!(c.command.as_deref(), Some("weird"));
        assert_eq!(ancestors(&p.0, 300, 10), vec![200, 100, 1]);
    }

    #[test]
    fn several_children_pick_the_first_with_a_tpgid() {
        let p = Proc::new("multi");
        p.add((100, "term", 1), 0, "/", &["term"], &[150, 200]);
        p.add((150, "helper", 100), -1, "/helper", &["helper"], &[]);
        p.add((200, "bash", 100), 200, "/sh", &["bash"], &[]);
        let c = capture_terminal(&p.0, None, 100).unwrap();
        assert_eq!(c.cwd, PathBuf::from("/sh"));
        assert_eq!(c.command, None);
    }

    #[test]
    fn vanished_foreground_falls_back_to_the_shell() {
        let p = Proc::new("gone");
        p.add((100, "term", 1), 0, "/", &["term"], &[200]);
        p.add((200, "bash", 100), 999, "/sh", &["bash"], &[]);
        let c = capture_terminal(&p.0, None, 100).unwrap();
        assert_eq!(c.cwd, PathBuf::from("/sh"));
        assert_eq!(c.command, None);
    }

    #[test]
    fn missing_pid_and_childless_terminal_are_none() {
        let p = Proc::new("none");
        assert_eq!(capture_terminal(&p.0, None, 4242), None);
        p.add((100, "term", 1), 0, "/", &["term"], &[]);
        assert_eq!(capture_terminal(&p.0, None, 100), None);
        assert!(children_of(&p.0, 4242).is_empty());
    }

    #[test]
    fn ancestors_walks_ppid_with_depth_limit() {
        let p = Proc::new("anc");
        p.add((10, "a", 0), 0, "/", &["a"], &[]);
        p.add((20, "b", 10), 0, "/", &["b"], &[]);
        p.add((30, "c", 20), 0, "/", &["c"], &[]);
        assert_eq!(ancestors(&p.0, 30, 10), vec![20, 10]);
        assert_eq!(ancestors(&p.0, 30, 1), vec![20]);
        assert_eq!(ancestors(&p.0, 30, 0), Vec::<u32>::new());
        assert!(ancestors(&p.0, 999, 5).is_empty());
    }

    fn term(cwd: &str, command: Option<&str>) -> WindowRecord {
        WindowRecord::Terminal {
            cwd: cwd.into(),
            command: command.map(str::to_string),
        }
    }

    fn app(app_id: Option<&str>, cmdline: Option<&str>) -> WindowRecord {
        WindowRecord::App {
            app_id: app_id.map(str::to_string),
            cmdline: cmdline.map(str::to_string),
        }
    }

    /// Classifies terminal `pid` (exec flag `-e`, no element app id).
    fn terminal_record(p: &Proc, pid: u32, origin: Option<&WindowRecord>) -> WindowRecord {
        window_record(&p.0, None, Some(pid), true, origin, None, "-e")
    }

    /// A terminal process with no readable argv or cwd and no children.
    fn add_bare(p: &Proc, pid: u32) {
        let task = p.0.join(pid.to_string()).join("task").join(pid.to_string());
        fs::create_dir_all(&task).unwrap();
        fs::write(task.join("children"), "").unwrap();
    }

    /// Moved from the compositor's `uncaptured_terminal_never_becomes_an_app` (the `bc86b6c`
    /// guarantee): a CDE-spawned terminal with no origin whose capture finds nothing is a terminal.
    #[test]
    fn uncaptured_terminal_never_becomes_an_app() {
        let p = Proc::new("uncaptured");
        // A bare terminal saved before its shell started (the host-b 2026-10-27 regression).
        p.add(
            (100, "ghostty", 1),
            0,
            "/w",
            &["ghostty", "--gtk-single-instance=false"],
            &[],
        );
        assert_eq!(terminal_record(&p, 100, None), term("/w", None));
        // Unreadable argv and cwd.
        add_bare(&p, 101);
        assert_eq!(
            terminal_record(&p, 101, None),
            WindowRecord::Terminal {
                cwd: PathBuf::new(),
                command: None
            }
        );
        // A pending restore banner in its own argv still decodes to its command.
        p.add(
            (102, "g", 1),
            0,
            "/w",
            &[
                "g",
                "-e",
                "/x/crucible",
                "resume",
                "--cwd",
                "/p",
                "--",
                "htop",
            ],
            &[],
        );
        assert_eq!(terminal_record(&p, 102, None), term("/p", Some("htop")));
        // A relaunch placeholder whose child already exited keeps its app record.
        p.add(
            (103, "g", 1),
            0,
            "/w",
            &["g", "-e", "/x/crucible", "relaunch", "--", "firefox"],
            &[],
        );
        assert_eq!(terminal_record(&p, 103, None), app(None, Some("firefox")));
        // A terminal class with no pid at all is an empty idle terminal.
        assert_eq!(
            window_record(&p.0, None, None, true, None, None, "-e"),
            WindowRecord::Terminal {
                cwd: PathBuf::new(),
                command: None
            }
        );
    }

    /// Moved from the compositor's `app_fallback_decodes_wrappers_before_joining`.
    #[test]
    fn app_decodes_wrappers_before_joining() {
        let p = Proc::new("app");
        let id = || Some("elem".to_string());
        let rec = |pid: u32| window_record(&p.0, None, Some(pid), false, None, id(), "-e");
        p.add(
            (100, "ghostty", 1),
            0,
            "/",
            &[
                "ghostty",
                "-e",
                "/x/crucible",
                "relaunch",
                "--",
                "firefox -p",
            ],
            &[],
        );
        assert_eq!(rec(100), app(Some("elem"), Some("firefox -p")));
        p.add(
            (101, "g", 1),
            0,
            "/",
            &[
                "g",
                "-e",
                "/x/crucible",
                "resume",
                "--now",
                "--exit",
                "--cwd",
                "/p",
                "--",
                "vim",
            ],
            &[],
        );
        assert_eq!(rec(101), term("/p", Some("vim")));
        p.add((102, "firefox", 1), 0, "/", &["firefox", "--new"], &[]);
        assert_eq!(rec(102), app(Some("elem"), Some("firefox --new")));
        assert_eq!(
            window_record(&p.0, None, None, false, None, None, "-e"),
            app(None, None)
        );
    }

    /// Moved from `resume_banner_is_decoded_not_nested` and `capture_relaunch_reads_direct_children`:
    /// a CDE wrapper child with no origin (one that outlived a compositor restart) is decoded.
    #[test]
    fn wrapper_child_without_origin_is_decoded_not_nested() {
        let p = Proc::new("banner");
        p.add((100, "term", 1), 0, "/", &["term"], &[200]);
        p.add(
            (200, "crucible", 100),
            200,
            "/elsewhere",
            &[
                "/usr/bin/crucible",
                "resume",
                "--cwd",
                "/proj",
                "--",
                "cargo test -- --nocapture",
            ],
            &[],
        );
        assert_eq!(
            terminal_record(&p, 100, None),
            term("/proj", Some("cargo test -- --nocapture"))
        );

        let p = Proc::new("banner2");
        p.add((100, "term", 1), 0, "/", &["term"], &[200]);
        p.add(
            (200, "crucible", 100),
            200,
            "/e",
            &["crucible", "resume", "--cwd", "/proj"],
            &[],
        );
        assert_eq!(terminal_record(&p, 100, None), term("/proj", None));

        let p = Proc::new("relaunch");
        p.add((100, "term", 1), 0, "/", &["term"], &[200]);
        p.add(
            (200, "crucible", 100),
            200,
            "/",
            &[
                "/usr/bin/crucible",
                "relaunch",
                "--app-id",
                "gimp",
                "--",
                "gimp a.png",
            ],
            &[],
        );
        assert_eq!(
            terminal_record(&p, 100, None),
            app(Some("gimp"), Some("gimp a.png"))
        );

        // An ordinary shell child is not a wrapper: the live capture.
        let p = Proc::new("relaunch2");
        p.add((100, "term", 1), 0, "/", &["term"], &[200]);
        p.add((200, "bash", 100), 200, "/", &["bash"], &[]);
        assert_eq!(terminal_record(&p, 100, None), term("/", None));
    }

    /// The placeholder's window pid is the terminal (`ghostty …`), never the wrapper itself: the
    /// wrapper is found among its direct children (the plan 18 sibling-lookup defect).
    #[test]
    fn relaunch_wrapper_of_reads_the_terminal_child() {
        let p = Proc::new("relaunch-of");
        p.add(
            (100, "ghostty", 1),
            0,
            "/",
            &[
                "ghostty",
                "--gtk-single-instance=false",
                "-e",
                "/x/crucible",
            ],
            &[200],
        );
        p.add(
            (200, "crucible", 100),
            200,
            "/",
            &[
                "/x/crucible",
                "relaunch",
                "--app-id",
                "firefox",
                "--",
                "firefox",
            ],
            &[],
        );
        p.add((300, "ghostty", 1), 0, "/", &["ghostty"], &[301]);
        p.add((301, "bash", 300), 301, "/", &["bash"], &[]);
        let (pid, got) = relaunch_wrapper_of(&p.0, 100).unwrap();
        assert_eq!(pid, 200);
        assert_eq!(got.cmdline, "firefox");
        assert_eq!(got.app_id.as_deref(), Some("firefox"));
        assert_eq!(relaunch_wrapper_of(&p.0, 200).unwrap().1.cmdline, "firefox");
        assert_eq!(relaunch_wrapper_of(&p.0, 300), None);
        assert_eq!(relaunch_wrapper_of(&p.0, 999), None);
    }

    #[test]
    fn origin_wins_over_a_cde_wrapper_foreground() {
        let p = Proc::new("origin-wrapper");
        p.add((100, "term", 1), 0, "/", &["term"], &[200]);
        p.add(
            (200, "crucible", 100),
            200,
            "/",
            &["crucible", "resume", "--cwd", "/decoded", "--", "x"],
            &[],
        );
        let origin = term("/saved", Some("cargo run"));
        assert_eq!(terminal_record(&p, 100, Some(&origin)), origin);
        let origin = app(Some("gimp"), Some("gimp a.png"));
        assert_eq!(terminal_record(&p, 100, Some(&origin)), origin);
    }

    #[test]
    fn live_capture_wins_over_the_origin() {
        let p = Proc::new("origin-live");
        p.add((100, "term", 1), 0, "/", &["term"], &[200]);
        p.add((200, "bash", 100), 300, "/home/u", &["bash"], &[300]);
        p.add((300, "htop", 200), 300, "/var/log", &["htop"], &[]);
        let origin = term("/saved", Some("cargo run"));
        assert_eq!(
            terminal_record(&p, 100, Some(&origin)),
            term("/var/log", Some("htop"))
        );
        // The user has since quit the restored command: an idle shell wins too.
        let p = Proc::new("origin-idle");
        p.add((100, "term", 1), 0, "/", &["term"], &[200]);
        p.add((200, "bash", 100), 200, "/cd-ed", &["bash"], &[]);
        assert_eq!(
            terminal_record(&p, 100, Some(&origin)),
            term("/cd-ed", None)
        );
    }

    #[test]
    fn uncaptured_terminal_with_an_origin_returns_the_origin() {
        let p = Proc::new("origin-none");
        p.add(
            (100, "g", 1),
            0,
            "/w",
            &[
                "g",
                "-e",
                "/x/crucible",
                "resume",
                "--cwd",
                "/argv",
                "--",
                "y",
            ],
            &[],
        );
        let origin = term("/saved", Some("cargo run"));
        assert_eq!(terminal_record(&p, 100, Some(&origin)), origin);
        add_bare(&p, 101);
        assert_eq!(terminal_record(&p, 101, Some(&origin)), origin);
    }

    /// Licensed change 2: a relaunched app whose argv is unreadable keeps its origin's cmdline;
    /// any readable argv still wins, and a non-app origin never makes an app cmdline.
    #[test]
    fn app_with_unreadable_argv_records_its_app_origin() {
        let p = Proc::new("app-origin");
        add_bare(&p, 100);
        let origin = app(Some("gimp"), Some("gimp a.png"));
        let rec = |pid, origin: Option<&WindowRecord>, id: Option<&str>| {
            window_record(
                &p.0,
                None,
                Some(pid),
                false,
                origin,
                id.map(str::to_string),
                "-e",
            )
        };
        assert_eq!(rec(100, Some(&origin), Some("elem")), origin);
        let unnamed = app(None, Some("gimp a.png"));
        assert_eq!(
            rec(100, Some(&unnamed), Some("elem")),
            app(Some("elem"), Some("gimp a.png"))
        );
        assert_eq!(rec(100, None, Some("elem")), app(Some("elem"), None));
        let t = term("/saved", None);
        assert_eq!(rec(100, Some(&t), Some("elem")), app(Some("elem"), None));
        p.add((101, "gimp", 1), 0, "/", &["gimp", "b.png"], &[]);
        assert_eq!(
            rec(101, Some(&origin), Some("elem")),
            app(Some("elem"), Some("gimp b.png"))
        );
    }
}

#[cfg(test)]
mod claude_tests {
    use super::*;
    use std::os::unix::fs::symlink;

    const ID: &str = "d6e856ed-0000-0000-0000-000000000000";

    struct Dirs {
        proc: PathBuf,
        sessions: PathBuf,
    }

    impl Dirs {
        fn new(tag: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("cde-claude-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            let (proc, sessions) = (base.join("proc"), base.join("sessions"));
            fs::create_dir_all(&proc).unwrap();
            fs::create_dir_all(&sessions).unwrap();
            Dirs { proc, sessions }
        }
        fn add(&self, pid: u32, ppid: u32, tpgid: i64, cwd: &str, argv: &[&str], kids: &[u32]) {
            let d = self.proc.join(pid.to_string());
            let task = d.join("task").join(pid.to_string());
            fs::create_dir_all(&task).unwrap();
            fs::write(
                d.join("stat"),
                format!("{pid} (x) S {ppid} {pid} {pid} 34816 {tpgid} 4194304 100 0 0 0\n"),
            )
            .unwrap();
            symlink(cwd, d.join("cwd")).unwrap();
            let cmd: Vec<u8> = argv
                .iter()
                .flat_map(|a| [a.as_bytes(), &[0]].concat())
                .collect();
            fs::write(d.join("cmdline"), cmd).unwrap();
            let list: Vec<String> = kids.iter().map(|k| format!("{k} ")).collect();
            fs::write(task.join("children"), list.concat()).unwrap();
        }
        fn record(&self, pid: u32, body: &str) {
            fs::write(self.sessions.join(format!("{pid}.json")), body).unwrap();
        }
        fn capture(&self) -> Option<TerminalCapture> {
            capture_terminal(&self.proc, Some(&self.sessions), 100)
        }
    }

    impl Drop for Dirs {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.proc.parent().unwrap());
        }
    }

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_string()).collect()
    }

    fn record_json(pid: u32) -> String {
        format!(r#"{{"pid":{pid},"sessionId":"{ID}","cwd":"/w","name":"Lush Egret"}}"#)
    }

    /// A terminal 100 -> shell 200 -> foreground `fg` (pid 300).
    fn live(d: &Dirs, fg: &[&str]) {
        d.add(100, 1, 0, "/", &["ghostty"], &[200]);
        d.add(200, 100, 300, "/sh", &["bash"], &[300]);
        d.add(300, 200, 300, "/proj", fg, &[]);
    }

    #[test]
    fn rewrite_keeps_name_and_replaces_resume_flags() {
        let want = argv(&["claude", "--resume", "ID", "-n", "Lush Egret"]);
        assert_eq!(
            claude_resume_argv(&argv(&["claude", "-n", "Lush Egret"]), "ID"),
            want
        );
        for old in [
            &["claude", "--resume", "old", "-n", "Lush Egret"][..],
            &["claude", "-r", "-n", "Lush Egret"],
            &["claude", "--resume=old", "-n", "Lush Egret"],
            &["claude", "-c", "-n", "Lush Egret"],
            &["claude", "--continue", "-n", "Lush Egret"],
            &["claude", "--session-id", "old", "-n", "Lush Egret"],
            &[
                "claude",
                "--session-id=old",
                "--fork-session",
                "-n",
                "Lush Egret",
            ],
        ] {
            assert_eq!(claude_resume_argv(&argv(old), "ID"), want, "{old:?}");
        }
        assert_eq!(
            claude_resume_argv(&argv(&["/opt/claude", "--model", "x", "hi"]), "ID"),
            argv(&["/opt/claude", "--resume", "ID", "--model", "x", "hi"])
        );
    }

    #[test]
    fn session_reader_is_silent_on_bad_records() {
        let d = Dirs::new("reader");
        d.record(1, &record_json(1));
        assert_eq!(claude_session_id(&d.sessions, 1).as_deref(), Some(ID));
        assert_eq!(claude_session_id(&d.sessions, 2), None);
        for (pid, body) in [
            (3, "not json"),
            (4, r#"{"sessionId":""}"#),
            (5, r#"{"sessionId":42}"#),
            (6, r#"{"sessionId":"abc"#),
            (7, r#"{"name":"sessionId","pid":1}"#),
            (8, ""),
        ] {
            d.record(pid, body);
            assert_eq!(claude_session_id(&d.sessions, pid), None, "{body}");
        }
        d.record(9, r#"{ "name" : "sessionId", "sessionId" : "ab-1" }"#);
        assert_eq!(claude_session_id(&d.sessions, 9).as_deref(), Some("ab-1"));
    }

    #[test]
    fn live_claude_foreground_is_captured_as_resume() {
        let d = Dirs::new("live");
        live(&d, &["claude", "--continue", "-n", "Lush Egret"]);
        d.record(300, &record_json(300));
        let c = d.capture().unwrap();
        assert_eq!(c.cwd, PathBuf::from("/proj"));
        assert_eq!(
            c.command.as_deref(),
            Some(format!("claude --resume {ID} -n 'Lush Egret'").as_str())
        );
    }

    #[test]
    fn missing_or_malformed_record_falls_back_to_plain_capture() {
        let d = Dirs::new("fallback");
        live(&d, &["claude", "-n", "Lush Egret"]);
        let plain = Some("claude -n 'Lush Egret'");
        assert_eq!(d.capture().unwrap().command.as_deref(), plain);
        d.record(300, "{garbage");
        assert_eq!(d.capture().unwrap().command.as_deref(), plain);
        // No sessions dir at all is also today's output.
        assert_eq!(
            capture_terminal(&d.proc, None, 100)
                .unwrap()
                .command
                .as_deref(),
            plain
        );
    }

    #[test]
    fn non_claude_program_is_untouched() {
        let d = Dirs::new("other");
        live(&d, &["htop", "-p", "1"]);
        d.record(300, &record_json(300));
        assert_eq!(d.capture().unwrap().command.as_deref(), Some("htop -p 1"));
    }

    /// Measured on host-a 2026-10-31: `ghostty -> crucible resume ... -- claude -n 'Lush Egret'
    /// -> claude -n Lush Egret`, the last two sharing pgrp 118161.
    fn measured_tree(d: &Dirs) {
        d.add(117_833, 1, 0, "/", &["ghostty"], &[118_161]);
        d.add(
            118_161,
            117_833,
            118_161,
            "/elsewhere",
            &[
                "crucible",
                "resume",
                "--cwd",
                "/w",
                "--",
                "claude -n 'Lush Egret'",
            ],
            &[118_191],
        );
        d.add(
            118_191,
            118_161,
            118_161,
            "/work/tree",
            &["claude", "-n", "Lush Egret"],
            &[],
        );
    }

    fn origin() -> WindowRecord {
        WindowRecord::Terminal {
            cwd: "/w".into(),
            command: Some("claude -n 'Lush Egret'".into()),
        }
    }

    fn classify(d: &Dirs, origin: Option<&WindowRecord>) -> WindowRecord {
        window_record(
            &d.proc,
            Some(&d.sessions),
            Some(117_833),
            true,
            origin,
            None,
            "-e",
        )
    }

    #[test]
    fn accepted_resume_wrapper_yields_the_claude_resume_command() {
        let d = Dirs::new("measured");
        measured_tree(&d);
        d.record(118_191, &record_json(118_191));
        let want = WindowRecord::Terminal {
            cwd: "/work/tree".into(),
            command: Some(format!("claude --resume {ID} -n 'Lush Egret'")),
        };
        assert_eq!(classify(&d, Some(&origin())), want);
        assert_eq!(classify(&d, None), want);
    }

    #[test]
    fn unaccepted_banner_keeps_the_origin() {
        let d = Dirs::new("banner");
        d.add(117_833, 1, 0, "/", &["ghostty"], &[118_161]);
        d.add(
            118_161,
            117_833,
            118_161,
            "/e",
            &[
                "crucible",
                "resume",
                "--cwd",
                "/w",
                "--",
                "claude -n 'Lush Egret'",
            ],
            &[],
        );
        d.record(118_191, &record_json(118_191));
        assert_eq!(classify(&d, Some(&origin())), origin());
        // An accepted tree without a session record also keeps the origin.
        let d = Dirs::new("banner-norec");
        measured_tree(&d);
        assert_eq!(classify(&d, Some(&origin())), origin());
    }
}
