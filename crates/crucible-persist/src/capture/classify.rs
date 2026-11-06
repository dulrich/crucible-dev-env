//! The one window classifier: turns a managed window into the record to save.

use std::path::{Path, PathBuf};

use super::{
    CaptureEnv, claude,
    procfs::{children_of, read_cmdline, read_cwd},
    shell::join_command,
    terminal::{capture_shell, capture_terminal},
};
use crate::invocation::{CdeWrapper, RelaunchWrapper, ResumeWrapper};
use crate::record::WindowRecord;

/// The record a CDE wrapper stands for: `resume` → a terminal, `relaunch` → an app.
fn wrapper_record(wrapper: CdeWrapper) -> WindowRecord {
    match wrapper {
        CdeWrapper::App(cap) => WindowRecord::App {
            app_id: non_empty(cap.app_id),
            cmdline: Some(cap.cmdline),
        },
        CdeWrapper::Terminal(cap) => WindowRecord::Terminal {
            cwd: cap.cwd,
            command: cap.command,
            session: None,
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

/// What kind of window the classifier is looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnClass {
    /// A CDE-spawned terminal (its client pid is the terminal itself).
    Terminal,
    /// Any other application window.
    App,
}

/// One window's facts for [`window_record`].
#[derive(Debug, Clone)]
pub struct WindowRecordInput<'a> {
    /// The client PID, if known.
    pub pid: Option<u32>,
    /// Whether the window is a CDE-spawned terminal or an app.
    pub class: SpawnClass,
    /// The saved record its spawn stands for (a restored window or a relaunched app).
    pub origin: Option<&'a WindowRecord>,
    /// The element's app id.
    pub app_id: Option<String>,
    /// The terminal's exec flag.
    pub exec_flag: &'a str,
    /// A bound cterm window's root process (its shell or CDE wrapper under the shared daemon),
    /// from its `WindowIdentity`. With it, every `/proc` read is rooted there instead of at `pid`
    /// (which is only the thin client). `None` for unbound windows: unchanged behaviour.
    pub root_pid: Option<u32>,
}

/// Classifies one managed window into the record to save. Pure over `env` (normally the real
/// `/proc` and claude home). The input's `pid` is the client PID, `class` whether it is a
/// CDE-spawned terminal (exact pid) or an app, `origin` the saved record its spawn stands for (a
/// restored window or a relaunched app), `app_id` the element's app id and `exec_flag` the
/// terminal's exec flag. Origin first:
///
/// - **Terminal:** (1) a child of the terminal that is a CDE wrapper (a pending restore banner or
///   relaunch placeholder — the foreground is CDE itself) yields the origin, else the decoded
///   wrapper; (2) otherwise the live [`capture_terminal`] result (the user may have `cd`'d or run
///   something since the restore); (3) if that finds nothing, the origin; else the terminal's own
///   argv decoded as a wrapper ([`CdeWrapper::decode`]); else an idle terminal in the terminal
///   process's cwd (empty when unreadable). Never an app unless an origin or a relaunch wrapper
///   says so.
/// - **App:** (1) a CDE wrapper in the client's argv is decoded; (2) else an origin that is an
///   app with a cmdline (a CDE-spawned or relaunched app — the spawn command beats a live helper
///   argv such as `steamwebhelper`) records that cmdline, the element app id filling a missing
///   origin id; (3) else the app id and the joined argv (no cmdline when the argv is empty).
///
/// **Rooted (bound cterm window, `root_pid` set):** the terminal arm reads the root instead of the
/// client: wrapper candidates are the root itself then its children, the live capture is
/// [`capture_shell`] on the root, and the decode / cwd fallbacks read the root.
#[must_use]
pub fn window_record(env: &CaptureEnv, mut input: WindowRecordInput<'_>) -> WindowRecord {
    input.app_id = non_empty(input.app_id);
    let record = match input.class {
        SpawnClass::Terminal => terminal_record(env, input),
        SpawnClass::App => app_record(env, input),
    };
    match record {
        WindowRecord::App { app_id, cmdline } => WindowRecord::App {
            app_id: non_empty(app_id),
            cmdline,
        },
        other => other,
    }
}

/// An empty app id carries no information: `None`.
fn non_empty(app_id: Option<String>) -> Option<String> {
    app_id.filter(|id| !id.is_empty())
}

/// The [`SpawnClass::Terminal`] arm of [`window_record`].
fn terminal_record(env: &CaptureEnv, input: WindowRecordInput<'_>) -> WindowRecord {
    let WindowRecordInput {
        pid,
        origin,
        exec_flag,
        root_pid,
        ..
    } = input;
    let proc_root = env.proc_root.as_path();
    let Some(pid) = root_pid.or(pid) else {
        return origin.cloned().unwrap_or(WindowRecord::Terminal {
            cwd: PathBuf::new(),
            command: None,
            session: None,
        });
    };
    let candidates = if root_pid.is_some() {
        std::iter::once(pid)
            .chain(children_of(proc_root, pid))
            .collect()
    } else {
        children_of(proc_root, pid)
    };
    if let Some((wrapper_pid, wrapper)) = candidates
        .into_iter()
        .find_map(|c| wrapper_process(proc_root, c).map(|w| (c, w)))
    {
        if matches!(wrapper, CdeWrapper::Terminal(_))
            && let Some((cwd, command)) = claude::resume_descendant(env, wrapper_pid)
        {
            return WindowRecord::Terminal {
                cwd,
                command: Some(command),
                session: None,
            };
        }
        return origin.cloned().unwrap_or_else(|| wrapper_record(wrapper));
    }
    let cap = if root_pid.is_some() {
        capture_shell(env, pid)
    } else {
        capture_terminal(env, pid)
    };
    if let Some(cap) = cap {
        return WindowRecord::Terminal {
            cwd: cap.cwd,
            command: cap.command,
            session: None,
        };
    }
    if let Some(origin) = origin {
        return origin.clone();
    }
    match CdeWrapper::decode(&read_cmdline(proc_root, pid), exec_flag) {
        Some(wrapper) => wrapper_record(wrapper),
        None => WindowRecord::Terminal {
            cwd: read_cwd(proc_root, pid).unwrap_or_default(),
            command: None,
            session: None,
        },
    }
}

/// The [`SpawnClass::App`] arm of [`window_record`].
fn app_record(env: &CaptureEnv, input: WindowRecordInput<'_>) -> WindowRecord {
    let WindowRecordInput {
        pid,
        origin,
        app_id,
        exec_flag,
        ..
    } = input;
    let argv = pid
        .map(|p| read_cmdline(&env.proc_root, p))
        .unwrap_or_default();
    if let Some(wrapper) = CdeWrapper::decode(&argv, exec_flag) {
        return match wrapper {
            CdeWrapper::App(cap) => WindowRecord::App {
                app_id: non_empty(cap.app_id).or(app_id),
                cmdline: Some(cap.cmdline),
            },
            wrapper => wrapper_record(wrapper),
        };
    }
    if let Some(WindowRecord::App {
        app_id: origin_id,
        cmdline: Some(cmdline),
    }) = origin
    {
        return WindowRecord::App {
            app_id: non_empty(origin_id.clone()).or(app_id),
            cmdline: Some(cmdline.clone()),
        };
    }
    WindowRecord::App {
        app_id,
        cmdline: (!argv.is_empty()).then(|| join_command(&argv)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::fake_proc::{FakeProc, SESSION_ID as ID, session_json};

    fn term(cwd: &str, command: Option<&str>) -> WindowRecord {
        WindowRecord::Terminal {
            cwd: cwd.into(),
            command: command.map(str::to_string),
            session: None,
        }
    }

    fn app(app_id: Option<&str>, cmdline: Option<&str>) -> WindowRecord {
        WindowRecord::App {
            app_id: app_id.map(str::to_string),
            cmdline: cmdline.map(str::to_string),
        }
    }

    /// Classifies `pid` as `class` (exec flag `-e`).
    fn classify(
        p: &FakeProc,
        pid: Option<u32>,
        class: SpawnClass,
        origin: Option<&WindowRecord>,
        app_id: Option<&str>,
    ) -> WindowRecord {
        window_record(
            &p.env(),
            WindowRecordInput {
                pid,
                class,
                origin,
                app_id: app_id.map(str::to_string),
                exec_flag: "-e",
                root_pid: None,
            },
        )
    }

    /// Classifies terminal `pid` (exec flag `-e`, no element app id).
    fn terminal_record(p: &FakeProc, pid: u32, origin: Option<&WindowRecord>) -> WindowRecord {
        classify(p, Some(pid), SpawnClass::Terminal, origin, None)
    }
    /// Moved from the compositor's `uncaptured_terminal_never_becomes_an_app` (the `bc86b6c`
    /// guarantee): a CDE-spawned terminal with no origin whose capture finds nothing is a terminal.
    #[test]
    fn uncaptured_terminal_never_becomes_an_app() {
        let p = FakeProc::new("uncaptured");
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
        p.add_bare(101);
        assert_eq!(
            terminal_record(&p, 101, None),
            WindowRecord::Terminal {
                cwd: PathBuf::new(),
                command: None,
                session: None,
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
            classify(&p, None, SpawnClass::Terminal, None, None),
            WindowRecord::Terminal {
                cwd: PathBuf::new(),
                command: None,
                session: None,
            }
        );
    }

    /// Moved from the compositor's `app_fallback_decodes_wrappers_before_joining`.
    #[test]
    fn app_decodes_wrappers_before_joining() {
        let p = FakeProc::new("app");
        let id = || Some("elem".to_string());
        let rec = |pid: u32| classify(&p, Some(pid), SpawnClass::App, None, id().as_deref());
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
            classify(&p, None, SpawnClass::App, None, None),
            app(None, None)
        );
    }

    /// Moved from `resume_banner_is_decoded_not_nested` and `capture_relaunch_reads_direct_children`:
    /// a CDE wrapper child with no origin (one that outlived a compositor restart) is decoded.
    #[test]
    fn wrapper_child_without_origin_is_decoded_not_nested() {
        let p = FakeProc::new("banner");
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

        let p = FakeProc::new("banner2");
        p.add((100, "term", 1), 0, "/", &["term"], &[200]);
        p.add(
            (200, "crucible", 100),
            200,
            "/e",
            &["crucible", "resume", "--cwd", "/proj"],
            &[],
        );
        assert_eq!(terminal_record(&p, 100, None), term("/proj", None));

        let p = FakeProc::new("relaunch");
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
        let p = FakeProc::new("relaunch2");
        p.add((100, "term", 1), 0, "/", &["term"], &[200]);
        p.add((200, "bash", 100), 200, "/", &["bash"], &[]);
        assert_eq!(terminal_record(&p, 100, None), term("/", None));
    }

    /// The placeholder's window pid is the terminal (`ghostty …`), never the wrapper itself: the
    /// wrapper is found among its direct children (the plan 18 sibling-lookup defect).
    #[test]
    fn relaunch_wrapper_of_reads_the_terminal_child() {
        let p = FakeProc::new("relaunch-of");
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
        let (pid, got) = relaunch_wrapper_of(&p.proc_root(), 100).unwrap();
        assert_eq!(pid, 200);
        assert_eq!(got.cmdline, "firefox");
        assert_eq!(got.app_id.as_deref(), Some("firefox"));
        assert_eq!(
            relaunch_wrapper_of(&p.proc_root(), 200).unwrap().1.cmdline,
            "firefox"
        );
        assert_eq!(relaunch_wrapper_of(&p.proc_root(), 300), None);
        assert_eq!(relaunch_wrapper_of(&p.proc_root(), 999), None);
    }

    #[test]
    fn origin_wins_over_a_cde_wrapper_foreground() {
        let p = FakeProc::new("origin-wrapper");
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
        let p = FakeProc::new("origin-live");
        p.add((100, "term", 1), 0, "/", &["term"], &[200]);
        p.add((200, "bash", 100), 300, "/home/u", &["bash"], &[300]);
        p.add((300, "htop", 200), 300, "/var/log", &["htop"], &[]);
        let origin = term("/saved", Some("cargo run"));
        assert_eq!(
            terminal_record(&p, 100, Some(&origin)),
            term("/var/log", Some("htop"))
        );
        // The user has since quit the restored command: an idle shell wins too.
        let p = FakeProc::new("origin-idle");
        p.add((100, "term", 1), 0, "/", &["term"], &[200]);
        p.add((200, "bash", 100), 200, "/cd-ed", &["bash"], &[]);
        assert_eq!(
            terminal_record(&p, 100, Some(&origin)),
            term("/cd-ed", None)
        );
    }

    #[test]
    fn uncaptured_terminal_with_an_origin_returns_the_origin() {
        let p = FakeProc::new("origin-none");
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
        p.add_bare(101);
        assert_eq!(terminal_record(&p, 101, Some(&origin)), origin);
    }

    /// An app whose argv is unreadable keeps its origin's cmdline; an app origin with a cmdline
    /// also beats a readable plain argv (plan 27), and a non-app origin never makes an app cmdline.
    #[test]
    fn app_with_unreadable_argv_records_its_app_origin() {
        let p = FakeProc::new("app-origin");
        p.add_bare(100);
        let origin = app(Some("gimp"), Some("gimp a.png"));
        let rec = |pid, origin: Option<&WindowRecord>, id: Option<&str>| {
            classify(&p, Some(pid), SpawnClass::App, origin, id)
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
        // Plan 27: the origin now beats a readable plain argv (was: argv won).
        assert_eq!(rec(101, Some(&origin), Some("elem")), origin);
        assert_eq!(
            rec(101, Some(&t), Some("elem")),
            app(Some("elem"), Some("gimp b.png"))
        );
    }

    /// Plan 27: the spawn command recorded as origin beats a live helper argv, and the element
    /// app id fills a missing origin id.
    #[test]
    fn app_origin_cmdline_beats_a_live_helper_argv() {
        let p = FakeProc::new("app-origin-helper");
        p.add(
            (100, "steamwebhelper", 1),
            0,
            "/",
            &["./steamwebhelper", "-x"],
            &[],
        );
        let origin = app(None, Some("steam"));
        assert_eq!(
            classify(&p, Some(100), SpawnClass::App, Some(&origin), Some("steam")),
            app(Some("steam"), Some("steam"))
        );
        // No origin: the argv is joined as before.
        assert_eq!(
            classify(&p, Some(100), SpawnClass::App, None, Some("steam")),
            app(Some("steam"), Some("./steamwebhelper -x"))
        );
    }

    /// Plan 27: a CDE wrapper in the argv still wins over an app origin.
    #[test]
    fn app_wrapper_argv_wins_over_the_origin() {
        let p = FakeProc::new("app-origin-wrapper");
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
        let origin = app(Some("o"), Some("steam"));
        assert_eq!(
            classify(&p, Some(100), SpawnClass::App, Some(&origin), Some("elem")),
            app(Some("elem"), Some("firefox -p"))
        );
    }

    /// Measured on host-a 2026-10-31: `ghostty -> crucible resume ... -- claude -n 'Lush Egret'
    /// -> claude -n Lush Egret`, the last two sharing pgrp 118161.
    fn measured_tree(d: &FakeProc) {
        d.add_anon(117_833, 1, 0, "/", &["ghostty"], &[118_161]);
        d.add_anon(
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
        d.add_anon(
            118_191,
            118_161,
            118_161,
            "/work/tree",
            &["claude", "-n", "Lush Egret"],
            &[],
        );
    }

    fn claude_origin() -> WindowRecord {
        term("/w", Some("claude -n 'Lush Egret'"))
    }

    #[test]
    fn accepted_resume_wrapper_yields_the_claude_resume_command() {
        let d = FakeProc::new("claude-measured");
        measured_tree(&d);
        d.record(118_191, &session_json(118_191));
        let want = term(
            "/work/tree",
            Some(&format!("claude --resume {ID} -n 'Lush Egret'")),
        );
        assert_eq!(terminal_record(&d, 117_833, Some(&claude_origin())), want);
        assert_eq!(terminal_record(&d, 117_833, None), want);
    }

    #[test]
    fn unaccepted_banner_keeps_the_origin() {
        let d = FakeProc::new("claude-banner");
        d.add_anon(117_833, 1, 0, "/", &["ghostty"], &[118_161]);
        d.add_anon(
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
        d.record(118_191, &session_json(118_191));
        assert_eq!(
            terminal_record(&d, 117_833, Some(&claude_origin())),
            claude_origin()
        );
        // An accepted tree without a session record also keeps the origin.
        let d = FakeProc::new("claude-banner-norec");
        measured_tree(&d);
        assert_eq!(
            terminal_record(&d, 117_833, Some(&claude_origin())),
            claude_origin()
        );
    }

    /// An empty app id never reaches a record: live id, decoded wrapper, saved origin and a
    /// terminal-class placeholder all yield `app_id: None`.
    #[test]
    fn empty_app_id_is_none_in_every_path() {
        let p = FakeProc::new("empty-app-id");
        p.add((200, "x", 1), 0, "/w", &["gimp"], &[]);
        assert_eq!(
            classify(&p, Some(200), SpawnClass::App, None, Some("")),
            app(None, Some("gimp"))
        );
        p.add(
            (201, "crucible", 1),
            0,
            "/w",
            &["/x/crucible", "relaunch", "--app-id", "", "--", "gimp"],
            &[],
        );
        assert_eq!(
            classify(&p, Some(201), SpawnClass::App, None, Some("")),
            app(None, Some("gimp"))
        );
        let origin = app(Some(""), Some("gimp"));
        assert_eq!(
            classify(&p, Some(200), SpawnClass::App, Some(&origin), Some("")),
            app(None, Some("gimp"))
        );
        assert_eq!(
            classify(&p, Some(200), SpawnClass::App, Some(&origin), Some("g")),
            app(Some("g"), Some("gimp"))
        );
        // A terminal-class placeholder: the terminal's child is the relaunch wrapper.
        p.add((300, "ghostty", 1), 0, "/w", &["ghostty"], &[301]);
        p.add(
            (301, "crucible", 300),
            0,
            "/w",
            &["/x/crucible", "relaunch", "--app-id", "", "--", "gimp"],
            &[],
        );
        assert_eq!(terminal_record(&p, 300, None), app(None, Some("gimp")));
        assert_eq!(
            terminal_record(&p, 300, Some(&origin)),
            app(None, Some("gimp"))
        );
    }

    /// Classifies bound terminal window `pid` rooted at `root` (exec flag `-e`, no origin).
    fn rooted_record(p: &FakeProc, pid: u32, root: u32) -> WindowRecord {
        window_record(
            &p.env(),
            WindowRecordInput {
                pid: Some(pid),
                class: SpawnClass::Terminal,
                origin: None,
                app_id: None,
                exec_flag: "-e",
                root_pid: Some(root),
            },
        )
    }

    /// Two thin clients C1, C2 share daemon D; their roots R1, R2 (children of D) run their own
    /// `crucible resume` wrappers. Classified in reverse order, each window records its own
    /// wrapper command as a terminal; the unrooted client alone captures nothing of it.
    #[test]
    fn rooted_windows_sharing_a_daemon_capture_their_own_wrapper() {
        let p = FakeProc::new("rooted-resume");
        p.add((500, "cterm", 1), 0, "/c1", &["cterm"], &[]);
        p.add((501, "cterm", 1), 0, "/c2", &["cterm"], &[]);
        p.add((600, "cterm", 1), 0, "/", &["cterm", "daemon"], &[601, 602]);
        p.add(
            (601, "crucible", 600),
            601,
            "/e",
            &["crucible", "resume", "--cwd", "/a", "--", "vim a"],
            &[],
        );
        p.add(
            (602, "crucible", 600),
            602,
            "/e",
            &["crucible", "resume", "--cwd", "/b", "--", "cargo test"],
            &[],
        );
        assert_eq!(rooted_record(&p, 501, 602), term("/b", Some("cargo test")));
        assert_eq!(rooted_record(&p, 500, 601), term("/a", Some("vim a")));
        // Unbound (no root): today's behaviour, the client's own idle cwd.
        assert_eq!(terminal_record(&p, 500, None), term("/c1", None));
    }

    /// A rooted idle shell under the daemon captures the root shell's cwd, never the daemon's;
    /// a rooted foreground command is captured from the root shell's `tpgid`.
    #[test]
    fn rooted_idle_shell_captures_the_root_cwd() {
        let p = FakeProc::new("rooted-idle");
        p.add((500, "cterm", 1), 0, "/c1", &["cterm"], &[]);
        p.add(
            (600, "cterm", 1),
            0,
            "/daemon",
            &["cterm", "daemon"],
            &[603, 604],
        );
        p.add((603, "bash", 600), 603, "/home/x", &["bash"], &[]);
        p.add((604, "bash", 600), 605, "/home/y", &["bash"], &[605]);
        p.add((605, "vim", 604), 605, "/home/y/src", &["vim", "f.rs"], &[]);
        assert_eq!(rooted_record(&p, 500, 603), term("/home/x", None));
        assert_eq!(
            rooted_record(&p, 501, 604),
            term("/home/y/src", Some("vim f.rs"))
        );
    }

    /// `relaunch_wrapper_of` on a root that is itself the `crucible relaunch` wrapper finds it.
    #[test]
    fn relaunch_wrapper_of_finds_a_wrapper_root() {
        let p = FakeProc::new("rooted-relaunch");
        p.add((600, "cterm", 1), 0, "/", &["cterm", "daemon"], &[601, 602]);
        for (pid, cmd) in [(601, "gimp a.png"), (602, "firefox")] {
            p.add(
                (pid, "crucible", 600),
                pid.into(),
                "/",
                &["crucible", "relaunch", "--", cmd],
                &[],
            );
        }
        let wrapper = |root| relaunch_wrapper_of(&p.proc_root(), root).map(|(pid, _)| pid);
        assert_eq!(wrapper(602), Some(602));
        assert_eq!(wrapper(601), Some(601));
    }
}
