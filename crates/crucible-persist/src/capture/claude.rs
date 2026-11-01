//! The `claude` CLI: recognising it, reading its per-pid session records and rewriting its argv
//! into a resume command. Its [`POLICY`] is the one program-specific row of the terminal's
//! foreground table; [`resume_descendant`] reuses that row to find claude below a resume wrapper.

use std::{
    fs,
    path::{Path, PathBuf},
};

use super::{
    CaptureEnv,
    procfs::{children_of, read_cmdline, read_cwd},
    shell::join_command,
    terminal::{Foreground, ForegroundPolicy, Verdict},
};

/// The foreground-table row for claude: argv0 is `claude` (basename) and the pid has a session
/// record → `claude --resume <id>` plus the remaining arguments. Anything else does not match.
pub(super) const POLICY: ForegroundPolicy = ForegroundPolicy {
    name: "claude",
    apply: resume,
};

/// Whether `argv0` names the `claude` CLI (basename).
fn is_claude_program(argv0: &str) -> bool {
    Path::new(argv0).file_name().and_then(|n| n.to_str()) == Some("claude")
}

/// The [`POLICY`] body: the resume command for `fg` when it is `claude` with a session record.
fn resume(env: &CaptureEnv, fg: &Foreground<'_>) -> Option<Verdict> {
    if !is_claude_program(fg.argv.first()?) {
        return None;
    }
    let id = claude_session_id(&env.sessions_dir(), fg.pid)?;
    Some(Verdict::Command(join_command(&claude_resume_argv(
        fg.argv, &id,
    ))))
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

/// Max depth searched below a resume wrapper for its `claude` process.
const CLAUDE_SEARCH_DEPTH: usize = 4;

/// Breadth-first search below `wrapper_pid` for a process the claude [`POLICY`] matches (a
/// `claude` with a session record): its cwd and resume command.
pub(super) fn resume_descendant(env: &CaptureEnv, wrapper_pid: u32) -> Option<(PathBuf, String)> {
    let mut level = vec![wrapper_pid];
    for _ in 0..CLAUDE_SEARCH_DEPTH {
        level = level
            .into_iter()
            .flat_map(|p| children_of(&env.proc_root, p))
            .collect();
        for pid in &level {
            let argv = read_cmdline(&env.proc_root, *pid);
            let fg = Foreground {
                pid: *pid,
                argv: &argv,
                direct_child_of_shell: false,
            };
            if let Some(Verdict::Command(command)) = (POLICY.apply)(env, &fg)
                && let Some(cwd) = read_cwd(&env.proc_root, *pid)
            {
                return Some((cwd, command));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{
        capture_terminal,
        fake_proc::{FakeProc, SESSION_ID as ID, session_json},
    };

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_string()).collect()
    }

    /// A terminal 100 -> shell 200 -> foreground `fg` (pid 300).
    fn live(d: &FakeProc, fg: &[&str]) {
        d.add_anon(100, 1, 0, "/", &["ghostty"], &[200]);
        d.add_anon(200, 100, 300, "/sh", &["bash"], &[300]);
        d.add_anon(300, 200, 300, "/proj", fg, &[]);
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
        let d = FakeProc::new("claude-reader");
        d.record(1, &session_json(1));
        assert_eq!(claude_session_id(&d.sessions(), 1).as_deref(), Some(ID));
        assert_eq!(claude_session_id(&d.sessions(), 2), None);
        for (pid, body) in [
            (3, "not json"),
            (4, r#"{"sessionId":""}"#),
            (5, r#"{"sessionId":42}"#),
            (6, r#"{"sessionId":"abc"#),
            (7, r#"{"name":"sessionId","pid":1}"#),
            (8, ""),
        ] {
            d.record(pid, body);
            assert_eq!(claude_session_id(&d.sessions(), pid), None, "{body}");
        }
        d.record(9, r#"{ "name" : "sessionId", "sessionId" : "ab-1" }"#);
        assert_eq!(claude_session_id(&d.sessions(), 9).as_deref(), Some("ab-1"));
    }

    #[test]
    fn live_claude_foreground_is_captured_as_resume() {
        let d = FakeProc::new("claude-live");
        live(&d, &["claude", "--continue", "-n", "Lush Egret"]);
        d.record(300, &session_json(300));
        let c = capture_terminal(&d.env(), 100).unwrap();
        assert_eq!(c.cwd, PathBuf::from("/proj"));
        assert_eq!(
            c.command.as_deref(),
            Some(format!("claude --resume {ID} -n 'Lush Egret'").as_str())
        );
    }

    #[test]
    fn missing_or_malformed_record_falls_back_to_plain_capture() {
        let d = FakeProc::new("claude-fallback");
        live(&d, &["claude", "-n", "Lush Egret"]);
        let plain = Some("claude -n 'Lush Egret'");
        let command = |d: &FakeProc| capture_terminal(&d.env(), 100).unwrap().command;
        assert_eq!(command(&d).as_deref(), plain);
        d.record(300, "{garbage");
        assert_eq!(command(&d).as_deref(), plain);
        // A claude home with no `sessions/` directory at all is also today's output.
        fs::remove_dir_all(d.sessions()).unwrap();
        assert_eq!(command(&d).as_deref(), plain);
    }

    #[test]
    fn non_claude_program_is_untouched() {
        let d = FakeProc::new("claude-other");
        live(&d, &["htop", "-p", "1"]);
        d.record(300, &session_json(300));
        assert_eq!(
            capture_terminal(&d.env(), 100).unwrap().command.as_deref(),
            Some("htop -p 1")
        );
    }
}
