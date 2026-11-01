//! The one window classifier: turns a managed window into the record to save.

use std::path::{Path, PathBuf};

use super::{
    CaptureEnv, claude,
    procfs::{children_of, read_cmdline, read_cwd},
    shell::join_command,
    terminal::capture_terminal,
};
use crate::invocation::{CdeWrapper, RelaunchWrapper, ResumeWrapper};
use crate::record::WindowRecord;

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
/// - **App:** the client's argv — a CDE wrapper in it is decoded before joining; otherwise the
///   app id and the joined argv. An app whose argv yields no cmdline but whose origin is an app
///   (a relaunched app) records the origin's cmdline.
#[must_use]
pub fn window_record(env: &CaptureEnv, input: WindowRecordInput<'_>) -> WindowRecord {
    match input.class {
        SpawnClass::Terminal => terminal_record(env, input),
        SpawnClass::App => app_record(env, input),
    }
}

/// The [`SpawnClass::Terminal`] arm of [`window_record`].
fn terminal_record(env: &CaptureEnv, input: WindowRecordInput<'_>) -> WindowRecord {
    let WindowRecordInput {
        pid,
        origin,
        exec_flag,
        ..
    } = input;
    let proc_root = env.proc_root.as_path();
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
        if matches!(wrapper, CdeWrapper::Terminal(_))
            && let Some((cwd, command)) = claude::resume_descendant(env, wrapper_pid)
        {
            return WindowRecord::Terminal {
                cwd,
                command: Some(command),
            };
        }
        return origin.cloned().unwrap_or_else(|| wrapper_record(wrapper));
    }
    if let Some(cap) = capture_terminal(env, pid) {
        return WindowRecord::Terminal {
            cwd: cap.cwd,
            command: cap.command,
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
    use crate::capture::fake_proc::{FakeProc, SESSION_ID as ID, session_json};

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
            classify(&p, None, SpawnClass::Terminal, None, None),
            WindowRecord::Terminal {
                cwd: PathBuf::new(),
                command: None
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

    /// Licensed change 2: a relaunched app whose argv is unreadable keeps its origin's cmdline;
    /// any readable argv still wins, and a non-app origin never makes an app cmdline.
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
        assert_eq!(
            rec(101, Some(&origin), Some("elem")),
            app(Some("elem"), Some("gimp b.png"))
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
}
