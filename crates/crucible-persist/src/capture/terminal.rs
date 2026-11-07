//! Terminal capture: the working directory and foreground command of whatever runs in a
//! terminal, with the foreground decision made by a table of program policies.

use std::path::{Path, PathBuf};

use super::{
    CaptureEnv, claude,
    procfs::{children_of, read_cmdline, read_cwd, read_stat},
    shell::{is_shell_program, join_command},
};

/// What a terminal window was doing when captured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalCapture {
    /// The working directory (of the foreground process, or the idle shell).
    pub cwd: PathBuf,
    /// The foreground command as one shell-quoted string; `None` when the shell is idle.
    pub command: Option<String>,
}

/// The foreground process of a terminal, as the [`ForegroundPolicy`] table sees it. Only built
/// once its cwd and argv are known to be readable (and the argv non-empty).
#[derive(Debug, Clone, Copy)]
pub(super) struct Foreground<'a> {
    /// The foreground process group leader.
    pub(super) pid: u32,
    /// Its command line (non-empty).
    pub(super) argv: &'a [String],
    /// Whether its parent is the terminal's shell.
    pub(super) direct_child_of_shell: bool,
}

/// What a matching policy says the terminal is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Nothing to relaunch: the terminal is idle (at the foreground's cwd).
    Idle,
    /// Relaunch with this shell-quoted command.
    Command(String),
}

/// One row of the foreground-program table: `apply` decides in one step whether this policy
/// matches the foreground (`None` = no match, try the next row) and, when it does, what to
/// record. A row may read the world (`env`) to decide, e.g. for a session record.
pub(super) struct ForegroundPolicy {
    /// The row's name, for tests and diagnostics.
    pub(super) name: &'static str,
    /// Matches and decides.
    pub(super) apply: fn(&CaptureEnv, &Foreground<'_>) -> Option<Verdict>,
}

/// A shell that is a direct child of the terminal's shell is the real interactive shell behind
/// an integration wrapper (ghostty's `sh -c "bash --posix"`): the terminal is idle.
const IDLE_SHELL: ForegroundPolicy = ForegroundPolicy {
    name: "idle-shell",
    apply: |_, fg| {
        (fg.direct_child_of_shell && is_shell_program(&fg.argv[0])).then_some(Verdict::Idle)
    },
};

/// The table, consulted in order; the first row that matches wins. Idle precedes any
/// program-specific row. A foreground no row matches is recorded as its joined argv.
const POLICIES: &[ForegroundPolicy] = &[IDLE_SHELL, claude::POLICY];

/// The first row of `table` that matches `fg` (its name, and its verdict), else no name and the
/// joined argv.
fn decide_in(
    table: &[ForegroundPolicy],
    env: &CaptureEnv,
    fg: &Foreground<'_>,
) -> (Option<&'static str>, Verdict) {
    table
        .iter()
        .find_map(|p| (p.apply)(env, fg).map(|v| (Some(p.name), v)))
        .unwrap_or_else(|| (None, Verdict::Command(join_command(fg.argv))))
}

/// What the foreground process `fg` of a terminal is doing, by the [`POLICIES`] table.
pub(super) fn decide(env: &CaptureEnv, fg: &Foreground<'_>) -> Verdict {
    decide_in(POLICIES, env, fg).1
}

/// Captures the terminal client `terminal_pid`: the working directory and foreground command of
/// whatever runs in it. `None` when the pid has no readable children/cwd.
///
/// The shell is the first child (with several, the first with a set `tpgid`); its `tpgid` is the
/// foreground group leader. If that is the shell itself (or unset) the command is `None` and the
/// cwd is the shell's; otherwise cwd and command come from the foreground process (its command
/// decided by the [`POLICIES`] table), falling back to the shell if it vanished meanwhile. A
/// foreground process that is itself a shell and a direct child of the session leader (ghostty's
/// `sh -c "bash --posix"` integration wrapper) is the real interactive shell: the terminal is
/// idle, with that shell's cwd. CDE's own wrappers are not special here; [`super::window_record`]
/// handles them first.
#[must_use]
pub fn capture_terminal(env: &CaptureEnv, terminal_pid: u32) -> Option<TerminalCapture> {
    let proc_root = env.proc_root.as_path();
    let children = children_of(proc_root, terminal_pid);
    let shell = if children.len() == 1 {
        children[0]
    } else {
        *children
            .iter()
            .find(|c| read_stat(proc_root, **c).is_some_and(|s| s.tpgid > 0))
            .or(children.first())?
    };
    capture_shell(env, shell)
}

/// [`capture_terminal`] after shell selection: captures from `shell` directly (a bound cterm
/// window's root process, whose terminal is a shared daemon). Same rules as [`capture_terminal`].
#[must_use]
pub fn capture_shell(env: &CaptureEnv, shell: u32) -> Option<TerminalCapture> {
    let proc_root = env.proc_root.as_path();
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
    let direct_child_of_shell =
        read_stat(proc_root, tpgid).is_some_and(|s| s.ppid == i64::from(shell));
    let (Some(cwd), false) = (read_cwd(proc_root, tpgid), args.is_empty()) else {
        return Some(idle);
    };
    let fg = Foreground {
        pid: tpgid,
        argv: &args,
        direct_child_of_shell,
    };
    let command = match decide(env, &fg) {
        Verdict::Idle => None,
        Verdict::Command(c) => Some(c),
    };
    Some(TerminalCapture { cwd, command })
}

/// Where a window's `/proc` reads start, decided once per window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcAnchor {
    /// An unbound window's client: the terminal process itself (its shell is a child).
    Terminal(u32),
    /// A bound cterm window's identity root: its shell or CDE wrapper under the shared daemon
    /// (the client is only the thin client, so it is never read).
    Shell(u32),
}

impl ProcAnchor {
    /// The anchor of a window with client `pid` and bound identity root `root`: the root when
    /// bound, else the client pid; `None` when neither is known.
    #[must_use]
    pub fn of(pid: Option<u32>, root: Option<u32>) -> Option<Self> {
        root.map(Self::Shell).or(pid.map(Self::Terminal))
    }

    /// A [`ProcAnchor::Shell`]'s root; `None` for a terminal.
    #[must_use]
    pub fn shell_root(self) -> Option<u32> {
        match self {
            Self::Shell(root) => Some(root),
            Self::Terminal(_) => None,
        }
    }

    /// The anchored pid.
    #[must_use]
    pub fn pid(self) -> u32 {
        match self {
            Self::Terminal(pid) | Self::Shell(pid) => pid,
        }
    }
}

/// The processes a CDE wrapper is looked for in under `anchor`: a [`ProcAnchor::Shell`] root
/// itself then its children (the root may be the wrapper), a [`ProcAnchor::Terminal`]'s children
/// only (the terminal is never its own foreground).
pub(super) fn wrapper_candidates(proc_root: &Path, anchor: ProcAnchor) -> Vec<u32> {
    match anchor {
        ProcAnchor::Shell(root) => std::iter::once(root)
            .chain(children_of(proc_root, root))
            .collect(),
        ProcAnchor::Terminal(pid) => children_of(proc_root, pid),
    }
}

/// The live capture under `anchor`: [`capture_shell`] on a root, [`capture_terminal`] on a
/// terminal (which first picks the terminal's shell).
#[must_use]
pub fn capture_anchor(env: &CaptureEnv, anchor: ProcAnchor) -> Option<TerminalCapture> {
    match anchor {
        ProcAnchor::Shell(root) => capture_shell(env, root),
        ProcAnchor::Terminal(pid) => capture_terminal(env, pid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::fake_proc::FakeProc;

    /// One anchor decides both the live capture and the wrapper candidates: a terminal anchor
    /// captures through its shell child and never treats itself as its wrapper; a shell anchor
    /// captures the root itself and does. `relaunch_wrapper_of` searches the pid itself either way.
    #[test]
    fn one_anchor_picks_capture_and_candidates() {
        let p = FakeProc::new("anchor");
        p.add((100, "ghostty", 1), 0, "/t", &["ghostty"], &[200]);
        p.add((200, "bash", 100), 200, "/home/u", &["bash"], &[]);
        let env = p.env();
        assert_eq!(
            capture_anchor(&env, ProcAnchor::Terminal(100)).map(|c| c.cwd),
            Some("/home/u".into())
        );
        assert_eq!(
            capture_anchor(&env, ProcAnchor::Shell(200)).map(|c| c.cwd),
            Some("/home/u".into())
        );
        let root = env.proc_root.as_path();
        assert_eq!(wrapper_candidates(root, ProcAnchor::Terminal(100)), [200]);
        assert_eq!(wrapper_candidates(root, ProcAnchor::Shell(100)), [100, 200]);
        assert_eq!(ProcAnchor::of(Some(5), Some(9)), Some(ProcAnchor::Shell(9)));
        assert_eq!(ProcAnchor::of(Some(5), None), Some(ProcAnchor::Terminal(5)));
        assert_eq!(ProcAnchor::of(None, None), None);
        // A terminal whose own argv is the relaunch wrapper is found by `relaunch_wrapper_of`.
        p.add(
            (300, "crucible", 1),
            0,
            "/",
            &["crucible", "relaunch", "--", "gimp"],
            &[],
        );
        assert_eq!(
            crate::capture::relaunch_wrapper_of(&env, ProcAnchor::Terminal(300))
                .map(|(pid, _)| pid),
            Some(300)
        );
    }

    fn fg<'a>(argv: &'a [String], direct_child_of_shell: bool) -> Foreground<'a> {
        Foreground {
            pid: 7,
            argv,
            direct_child_of_shell,
        }
    }

    fn strings(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn policy_table_order_is_idle_then_claude() {
        let names: Vec<_> = POLICIES.iter().map(|p| p.name).collect();
        assert_eq!(names, ["idle-shell", "claude"]);
    }

    #[test]
    fn first_matching_policy_wins_and_no_match_joins_argv() {
        let env = FakeProc::new("policy-order").env();
        let argv = strings(&["tool", "a b"]);
        let first = ForegroundPolicy {
            name: "first",
            apply: |_, _| Some(Verdict::Command("first".into())),
        };
        let second = ForegroundPolicy {
            name: "second",
            apply: |_, _| Some(Verdict::Idle),
        };
        let never = ForegroundPolicy {
            name: "never",
            apply: |_, _| None,
        };
        let f = fg(&argv, false);
        assert_eq!(
            decide_in(&[never, first, second], &env, &f),
            (Some("first"), Verdict::Command("first".into()))
        );
        assert_eq!(
            decide_in(&[], &env, &f),
            (None, Verdict::Command("tool 'a b'".into()))
        );
    }

    #[test]
    fn idle_shell_requires_a_direct_shell_child() {
        let env = FakeProc::new("policy-idle").env();
        let bash = strings(&["/bin/bash", "--posix"]);
        assert_eq!(decide(&env, &fg(&bash, true)), Verdict::Idle);
        // A shell deeper in the tree is a command like any other.
        assert_eq!(
            decide(&env, &fg(&bash, false)),
            Verdict::Command("/bin/bash --posix".into())
        );
        let htop = strings(&["htop"]);
        assert_eq!(
            decide(&env, &fg(&htop, true)),
            Verdict::Command("htop".into())
        );
    }

    #[test]
    fn idle_shell_reports_cwd_only() {
        let p = FakeProc::new("idle");
        p.add((100, "ghostty", 1), 0, "/", &["ghostty"], &[200]);
        p.add((200, "bash", 100), 200, "/home/u/proj", &["bash"], &[]);
        let c = capture_terminal(&p.env(), 100).unwrap();
        assert_eq!(
            c,
            TerminalCapture {
                cwd: "/home/u/proj".into(),
                command: None
            }
        );
    }

    /// ghostty's bash integration (measured on 2026-10-26):
    /// `ghostty → /bin/sh -c "/bin/bash --posix" → /bin/bash --posix`, the inner bash foreground.
    #[test]
    fn ghostty_sh_c_wrapped_shell_is_idle_with_the_inner_cwd() {
        let p = FakeProc::new("wrapped-idle");
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
        let c = capture_terminal(&p.env(), 100).unwrap();
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
        let p = FakeProc::new("wrapped-fg");
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
        let c = capture_terminal(&p.env(), 100).unwrap();
        assert_eq!(c.cwd, PathBuf::from("/proj"));
        assert_eq!(c.command.as_deref(), Some("htop"));
    }

    #[test]
    fn foreground_command_with_quoting() {
        let p = FakeProc::new("fg");
        p.add((100, "ghostty", 1), 0, "/", &["ghostty"], &[200]);
        p.add((200, "bash", 100), 300, "/home/u", &["bash"], &[300]);
        p.add(
            (300, "htop", 200),
            300,
            "/var/log",
            &["htop", "-p", "1,2", "a b", "it's"],
            &[],
        );
        let c = capture_terminal(&p.env(), 100).unwrap();
        assert_eq!(c.cwd, PathBuf::from("/var/log"));
        assert_eq!(c.command.as_deref(), Some("htop -p 1,2 'a b' 'it'\\''s'"));
    }

    #[test]
    fn comm_with_spaces_and_paren_in_shell_and_foreground() {
        let p = FakeProc::new("comm");
        p.add((100, "my term)", 1), 0, "/", &["t"], &[200]);
        p.add((200, "ba sh) x", 100), 300, "/h", &["bash"], &[300]);
        p.add((300, "we (ird) p", 200), 300, "/w", &["weird"], &[]);
        let c = capture_terminal(&p.env(), 100).unwrap();
        assert_eq!(c.cwd, PathBuf::from("/w"));
        assert_eq!(c.command.as_deref(), Some("weird"));
        assert_eq!(
            crate::capture::ancestors(&p.proc_root(), 300, 10),
            vec![200, 100, 1]
        );
    }

    #[test]
    fn several_children_pick_the_first_with_a_tpgid() {
        let p = FakeProc::new("multi");
        p.add((100, "term", 1), 0, "/", &["term"], &[150, 200]);
        p.add((150, "helper", 100), -1, "/helper", &["helper"], &[]);
        p.add((200, "bash", 100), 200, "/sh", &["bash"], &[]);
        let c = capture_terminal(&p.env(), 100).unwrap();
        assert_eq!(c.cwd, PathBuf::from("/sh"));
        assert_eq!(c.command, None);
    }

    #[test]
    fn vanished_foreground_falls_back_to_the_shell() {
        let p = FakeProc::new("gone");
        p.add((100, "term", 1), 0, "/", &["term"], &[200]);
        p.add((200, "bash", 100), 999, "/sh", &["bash"], &[]);
        let c = capture_terminal(&p.env(), 100).unwrap();
        assert_eq!(c.cwd, PathBuf::from("/sh"));
        assert_eq!(c.command, None);
    }

    #[test]
    fn missing_pid_and_childless_terminal_are_none() {
        let p = FakeProc::new("none");
        assert_eq!(capture_terminal(&p.env(), 4242), None);
        p.add((100, "term", 1), 0, "/", &["term"], &[]);
        assert_eq!(capture_terminal(&p.env(), 100), None);
        assert!(children_of(&p.proc_root(), 4242).is_empty());
    }
}
