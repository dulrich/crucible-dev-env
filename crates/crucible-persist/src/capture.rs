//! `/proc` capture of a terminal window's working directory and foreground command.
//!
//! Pure parsers over `/proc` text plus a thin filesystem edge that takes a `proc_root` (normally
//! `/proc`; tests point it at a fixture tree).

use std::{
    fs,
    path::{Path, PathBuf},
};

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

/// Recognises CDE's own pending restore banner,
/// `[<…/crucible>, "resume", "--cwd", <dir>, "--", <command>]` (the command is ONE argv element),
/// or the same without `--`/command. Returns the encoded `(cwd, command)`.
#[must_use]
pub fn parse_resume_banner(args: &[String]) -> Option<TerminalCapture> {
    let exe = Path::new(args.first()?).file_name()?;
    if exe != "crucible" || args.get(1)? != "resume" {
        return None;
    }
    // A `run-in-terminal` terminal passes `--now`; a restore still shows the plain banner.
    let rest = &args[2..];
    let rest = rest.strip_prefix(&["--now".to_string()]).unwrap_or(rest);
    let [flag, dir, tail @ ..] = rest else {
        return None;
    };
    if flag != "--cwd" {
        return None;
    }
    let cwd = PathBuf::from(dir);
    match tail {
        [] => Some(TerminalCapture { cwd, command: None }),
        [dd] if dd == "--" => Some(TerminalCapture { cwd, command: None }),
        [dd, cmd] if dd == "--" => Some(TerminalCapture {
            cwd,
            command: Some(cmd.clone()),
        }),
        _ => None,
    }
}

/// A decoded app relaunch placeholder banner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelaunchCapture {
    /// The saved app id, if any.
    pub app_id: Option<String>,
    /// The saved command line as one shell-quoted string.
    pub cmdline: String,
}

/// Recognises CDE's app placeholder banner,
/// `[<…/crucible>, "relaunch", ("--app-id", <id>)?, "--", <cmdline>]` (the cmdline is ONE
/// non-empty argv element). Anything else is `None`.
#[must_use]
pub fn parse_relaunch_banner(args: &[String]) -> Option<RelaunchCapture> {
    let exe = Path::new(args.first()?).file_name()?;
    if exe != "crucible" || args.get(1)? != "relaunch" {
        return None;
    }
    let (app_id, rest) = if args.get(2)? == "--app-id" {
        (Some(args.get(3)?.clone()), &args[4..])
    } else {
        (None, &args[2..])
    };
    match rest {
        [dd, cmdline] if dd == "--" && !cmdline.is_empty() => Some(RelaunchCapture {
            app_id,
            cmdline: cmdline.clone(),
        }),
        _ => None,
    }
}

/// Scans the direct children of `terminal_pid` for an app relaunch placeholder banner.
#[must_use]
pub fn capture_relaunch(proc_root: &Path, terminal_pid: u32) -> Option<RelaunchCapture> {
    children_of(proc_root, terminal_pid)
        .into_iter()
        .find_map(|c| parse_relaunch_banner(&read_cmdline(proc_root, c)))
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

/// Captures the terminal client `terminal_pid`: the working directory and foreground command of
/// whatever runs in it. `None` when the pid has no readable children/cwd.
///
/// 1. A child that is CDE's pending restore banner decodes straight back to its `--cwd` and
///    command (so a record survives a second restart before Enter and never nests `resume`).
/// 2. Otherwise the shell is the first child (with several, the first with a set `tpgid`); its
///    `tpgid` is the foreground group leader. If that is the shell itself (or unset) the command
///    is `None` and the cwd is the shell's; otherwise cwd and command come from the foreground
///    process, falling back to the shell if it vanished meanwhile. A foreground process that is itself a
///    shell and a direct child of the session leader (ghostty's `sh -c "bash --posix"` integration
///    wrapper) is the real interactive shell: the terminal is idle, with that shell's cwd.
#[must_use]
pub fn capture_terminal(proc_root: &Path, terminal_pid: u32) -> Option<TerminalCapture> {
    let children = children_of(proc_root, terminal_pid);
    for child in &children {
        if let Some(banner) = parse_resume_banner(&read_cmdline(proc_root, *child)) {
            return Some(banner);
        }
    }
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
            command: Some(join_command(&args)),
        }),
        _ => Some(idle),
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
        let c = capture_terminal(&p.0, 100).unwrap();
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
        let c = capture_terminal(&p.0, 100).unwrap();
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
        let c = capture_terminal(&p.0, 100).unwrap();
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
        let c = capture_terminal(&p.0, 100).unwrap();
        assert_eq!(c.cwd, PathBuf::from("/var/log"));
        assert_eq!(c.command.as_deref(), Some("htop -p 1,2 'a b' 'it'\\''s'"));
    }

    #[test]
    fn comm_with_spaces_and_paren_in_shell_and_foreground() {
        let p = Proc::new("comm");
        p.add((100, "my term)", 1), 0, "/", &["t"], &[200]);
        p.add((200, "ba sh) x", 100), 300, "/h", &["bash"], &[300]);
        p.add((300, "we (ird) p", 200), 300, "/w", &["weird"], &[]);
        let c = capture_terminal(&p.0, 100).unwrap();
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
        let c = capture_terminal(&p.0, 100).unwrap();
        assert_eq!(c.cwd, PathBuf::from("/sh"));
        assert_eq!(c.command, None);
    }

    #[test]
    fn vanished_foreground_falls_back_to_the_shell() {
        let p = Proc::new("gone");
        p.add((100, "term", 1), 0, "/", &["term"], &[200]);
        p.add((200, "bash", 100), 999, "/sh", &["bash"], &[]);
        let c = capture_terminal(&p.0, 100).unwrap();
        assert_eq!(c.cwd, PathBuf::from("/sh"));
        assert_eq!(c.command, None);
    }

    #[test]
    fn resume_banner_is_decoded_not_nested() {
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
        let c = capture_terminal(&p.0, 100).unwrap();
        assert_eq!(c.cwd, PathBuf::from("/proj"));
        assert_eq!(c.command.as_deref(), Some("cargo test -- --nocapture"));

        let p = Proc::new("banner2");
        p.add((100, "term", 1), 0, "/", &["term"], &[200]);
        p.add(
            (200, "crucible", 100),
            200,
            "/e",
            &["crucible", "resume", "--cwd", "/proj"],
            &[],
        );
        let c = capture_terminal(&p.0, 100).unwrap();
        assert_eq!((c.cwd, c.command), (PathBuf::from("/proj"), None));
    }

    #[test]
    fn banner_parser_rejects_lookalikes() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(parse_resume_banner(&v(&["/bin/crucible", "pick"])).is_none());
        assert!(parse_resume_banner(&v(&["/bin/other", "resume", "--cwd", "/p"])).is_none());
        assert!(
            parse_resume_banner(&v(&["/bin/crucible", "resume", "--cwd", "/p", "x"])).is_none()
        );
        assert!(
            parse_resume_banner(&v(&[
                "/bin/crucible",
                "resume",
                "--cwd",
                "/p",
                "--",
                "a",
                "b"
            ]))
            .is_none()
        );
        assert!(parse_resume_banner(&[]).is_none());
        let b = parse_resume_banner(&v(&["/bin/crucible", "resume", "--cwd", "/p", "--"])).unwrap();
        assert_eq!(b.command, None);
    }

    #[test]
    fn banner_parser_accepts_the_now_form() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let plain = parse_resume_banner(&v(&[
            "/bin/crucible",
            "resume",
            "--cwd",
            "/p",
            "--",
            "htop",
        ]))
        .unwrap();
        let now = parse_resume_banner(&v(&[
            "/bin/crucible",
            "resume",
            "--now",
            "--cwd",
            "/p",
            "--",
            "htop",
        ]))
        .unwrap();
        assert_eq!(plain, now);
        assert_eq!(now.command.as_deref(), Some("htop"));
        assert!(parse_resume_banner(&v(&["/bin/crucible", "resume", "--now"])).is_none());
        assert!(
            parse_resume_banner(&v(&[
                "/bin/crucible",
                "resume",
                "--now",
                "--now",
                "--cwd",
                "/p"
            ]))
            .is_none()
        );
    }

    #[test]
    fn missing_pid_and_childless_terminal_are_none() {
        let p = Proc::new("none");
        assert_eq!(capture_terminal(&p.0, 4242), None);
        p.add((100, "term", 1), 0, "/", &["term"], &[]);
        assert_eq!(capture_terminal(&p.0, 100), None);
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

    fn sv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn relaunch_banner_round_trips() {
        let c = parse_relaunch_banner(&sv(&[
            "/usr/bin/crucible",
            "relaunch",
            "--app-id",
            "firefox",
            "--",
            "firefox --new-window",
        ]))
        .unwrap();
        assert_eq!(c.app_id.as_deref(), Some("firefox"));
        assert_eq!(c.cmdline, "firefox --new-window");
        let c = parse_relaunch_banner(&sv(&["crucible", "relaunch", "--", "gimp"])).unwrap();
        assert_eq!(c.app_id, None);
        assert_eq!(c.cmdline, "gimp");
    }

    #[test]
    fn relaunch_banner_rejects_lookalikes() {
        for bad in [
            &["/usr/bin/crucible", "resume", "--cwd", "/p", "--", "x"][..],
            &["/usr/bin/notcrucible", "relaunch", "--", "x"],
            &["crucible", "relaunch", "--"],
            &["crucible", "relaunch", "--", ""],
            &["crucible", "relaunch", "--", "x", "y"],
            &["crucible", "relaunch", "--app-id"],
            &["crucible", "relaunch", "--app-id", "a", "x"],
            &["crucible", "relaunch", "--bogus", "--", "x"],
            &["crucible", "relaunch", "x"],
        ] {
            assert_eq!(parse_relaunch_banner(&sv(bad)), None, "{bad:?}");
        }
    }

    #[test]
    fn capture_relaunch_reads_direct_children() {
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
        let c = capture_relaunch(&p.0, 100).unwrap();
        assert_eq!(c.app_id.as_deref(), Some("gimp"));
        assert_eq!(c.cmdline, "gimp a.png");
        assert_eq!(capture_relaunch(&p.0, 200), None);

        let p = Proc::new("relaunch2");
        p.add((100, "term", 1), 0, "/", &["term"], &[200]);
        p.add((200, "bash", 100), 200, "/", &["bash"], &[]);
        assert_eq!(capture_relaunch(&p.0, 100), None);
    }
}
