//! The wire protocol: [`Request`] and [`Response`] lines and the shared field codec.

use std::path::PathBuf;

/// The field codec lives in `crucible_persist::field` (it also encodes `crucible pick`'s
/// argument); re-exported here under the same names for the IPC protocol and its clients.
pub use crucible_persist::field::{decode_field, encode_field};

/// One IPC request (decoded arguments).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// `candidates` — list live, saved and folder candidates.
    Candidates,
    /// `bindings` — list the effective keybindings as `(chord, action)` display pairs.
    Bindings,
    /// `open <name>` — switch to a live workspace or restore a saved one.
    Open(String),
    /// `open-folder <path>` — create (or reopen) a workspace rooted at a folder.
    OpenFolder(PathBuf),
    /// `new <name>` — create and persist an empty named workspace.
    New(String),
    /// `rename <name>` — rename the active workspace (and its saved file).
    Rename(String),
    /// `delete <name>` — delete a *saved, not live* workspace (its record moves to `deleted/` and
    /// its slot pins clear); a live name is refused (use `delete-workspace`), an unknown one errs.
    Delete(String),
    /// `root <path>` / `root -` — set (`Some`) or clear (`None`) the active workspace's root, the
    /// folder its new terminals and apps start in. Not a picker root (`picker { root … }`).
    SetRoot(Option<PathBuf>),
    /// `save` — flush every dirty workspace now.
    Save,
    /// `exec <action>` — run an action text (the `crucible_keys::Action` grammar, e.g.
    /// `run <command>` or `run-in-terminal <command>`) as if its chord was pressed; a failed action
    /// answers `err <reason>`.
    Exec(String),
    /// `run-tui <command>` — like `exec run-in-terminal`, but the terminal closes when the command
    /// exits (`crucible resume --now --exit`), unless the command exits abnormally: `resume` then shows the crash banner first.
    RunTui(String),
    /// `run-term <program>` — spawn the bare emulator `program` (no arguments) as a CDE terminal in
    /// the focused pane; the launcher's TERM rows.
    RunTerm(String),
    /// `relaunch` — swap the calling placeholder window (found by peer PID) for its app.
    Relaunch,
    /// `close-pid <n>` — politely ask every managed window of process `n` to close (xdg `close` /
    /// X11 `WM_DELETE_WINDOW`; an X11 window without it is destroyed); answers `ok <count>`.
    ClosePid(u32),
    /// `capture <path>` — write a PNG of the focused output (no cursor) to `path`; answers
    /// `ok <w>x<h>` once the file is written. Refused while locked. A debug request (plan 34).
    Capture(PathBuf),
    /// `outputs` — list every enabled (mapped) output as one line of `<name>=<x>,<y>,<w>x<h>@<scale>`
    /// records (logical geometry), ordered by x, y, then name (plan 42).
    Outputs,
}

/// Parses the pid argument of `close-pid`: plain decimal digits, a non-zero `u32`.
fn parse_pid(verb: &str, arg: Option<&str>) -> Result<u32, String> {
    let arg = arg.ok_or_else(|| format!("{verb} needs an argument"))?;
    if arg.is_empty() || !arg.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{verb} needs a decimal pid, got {arg:?}"));
    }
    match arg.parse::<u32>() {
        Ok(0) => Err(format!("{verb} needs a non-zero pid")),
        Ok(n) => Ok(n),
        Err(_) => Err(format!("{verb} pid {arg:?} is out of range")),
    }
}

impl Request {
    /// `exec run <cmd>`: run a shell command line into the focused pane.
    #[must_use]
    pub fn run(cmd: impl Into<String>) -> Request {
        Request::Exec(crucible_keys::Action::Run(cmd.into()).to_string())
    }

    /// `exec run-in-terminal <cmd>`: run a shell command line in a new terminal.
    #[must_use]
    pub fn run_in_terminal(cmd: impl Into<String>) -> Request {
        Request::Exec(crucible_keys::Action::RunInTerminal(cmd.into()).to_string())
    }

    /// Whether this request acts (changes the model, spawns, or touches overlays). Acting requests
    /// are classified here; `candidates`, `bindings` and `save` are not acting. Admission while
    /// locked is `admit_request`'s job, which refuses them all but `exec set-layout`.
    #[must_use]
    pub fn acts(&self) -> bool {
        match self {
            Request::Candidates | Request::Bindings | Request::Save | Request::Outputs => false,
            Request::Open(_)
            | Request::OpenFolder(_)
            | Request::New(_)
            | Request::Rename(_)
            | Request::Delete(_)
            | Request::SetRoot(_)
            | Request::Exec(_)
            | Request::RunTui(_)
            | Request::RunTerm(_)
            | Request::Relaunch
            | Request::ClosePid(_)
            | Request::Capture(_) => true,
        }
    }

    /// Parses one request line (without its trailing newline; a trailing `\r` is not stripped
    /// because CR is always encoded). Errors carry a human-readable reason.
    pub fn parse(line: &str) -> Result<Request, String> {
        let (verb, arg) = match line.split_once(' ') {
            Some((v, a)) => (v, Some(a)),
            None => (line, None),
        };
        let need = |arg: Option<&str>| -> Result<String, String> {
            let arg = arg.ok_or_else(|| format!("{verb} needs an argument"))?;
            let decoded =
                decode_field(arg).ok_or_else(|| "malformed percent-encoding".to_string())?;
            if decoded.is_empty() {
                return Err(format!("{verb} needs a non-empty argument"));
            }
            Ok(decoded)
        };
        let none = |arg: Option<&str>, req: Request| match arg {
            None => Ok(req),
            Some(_) => Err(format!("{verb} takes no argument")),
        };
        match verb {
            "candidates" => none(arg, Request::Candidates),
            "bindings" => none(arg, Request::Bindings),
            "save" => none(arg, Request::Save),
            "outputs" => none(arg, Request::Outputs),
            "relaunch" => none(arg, Request::Relaunch),
            "open" => need(arg).map(Request::Open),
            "open-folder" => need(arg).map(|p| Request::OpenFolder(PathBuf::from(p))),
            "new" => need(arg).map(Request::New),
            "rename" => need(arg).map(Request::Rename),
            "delete" => need(arg).map(Request::Delete),
            "root" => need(arg).map(|p| Request::SetRoot((p != "-").then(|| PathBuf::from(p)))),
            "exec" => need(arg).map(Request::Exec),
            "run-tui" => need(arg).map(Request::RunTui),
            "run-term" => need(arg).map(Request::RunTerm),
            "close-pid" => parse_pid(verb, arg).map(Request::ClosePid),
            "capture" => need(arg).map(|p| Request::Capture(PathBuf::from(p))),
            other => Err(format!("unknown request {other:?}")),
        }
    }

    /// Formats the request as one wire line (no trailing newline). Paths must be UTF-8 (lossy
    /// otherwise).
    #[must_use]
    pub fn to_line(&self) -> String {
        match self {
            Request::Candidates => "candidates".into(),
            Request::Bindings => "bindings".into(),
            Request::Save => "save".into(),
            Request::Outputs => "outputs".into(),
            Request::Relaunch => "relaunch".into(),
            Request::Open(n) => format!("open {}", encode_field(n)),
            Request::OpenFolder(p) => format!("open-folder {}", encode_field(&p.to_string_lossy())),
            Request::New(n) => format!("new {}", encode_field(n)),
            Request::Rename(n) => format!("rename {}", encode_field(n)),
            Request::Delete(n) => format!("delete {}", encode_field(n)),
            Request::SetRoot(None) => "root -".into(),
            Request::SetRoot(Some(p)) => format!("root {}", encode_field(&p.to_string_lossy())),
            Request::Exec(a) => format!("exec {}", encode_field(a)),
            Request::RunTui(c) => format!("run-tui {}", encode_field(c)),
            Request::RunTerm(c) => format!("run-term {}", encode_field(c)),
            Request::ClosePid(n) => format!("close-pid {n}"),
            Request::Capture(p) => format!("capture {}", encode_field(&p.to_string_lossy())),
        }
    }
}

/// One IPC response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// Success; the payload is already wire-formatted (e.g. [`format_candidates`]) and may be
    /// empty (`ok`).
    Ok(String),
    /// Failure with a decoded, human-readable reason.
    Err(String),
}

impl Response {
    /// Formats the response as one wire line (no trailing newline).
    #[must_use]
    pub fn to_line(&self) -> String {
        match self {
            Response::Ok(p) if p.is_empty() => "ok".into(),
            Response::Ok(p) => format!("ok {p}"),
            Response::Err(r) => format!("err {}", encode_field(r)),
        }
    }

    /// Parses one response line; `None` if it is neither `ok…` nor `err …`.
    #[must_use]
    pub fn parse(line: &str) -> Option<Response> {
        if line == "ok" {
            return Some(Response::Ok(String::new()));
        }
        if let Some(p) = line.strip_prefix("ok ") {
            return Some(Response::Ok(p.to_string()));
        }
        line.strip_prefix("err ")
            .and_then(decode_field)
            .map(Response::Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_through_the_wire() {
        let reqs = [
            Request::Candidates,
            Request::Bindings,
            Request::Save,
            Request::Outputs,
            Request::Relaunch,
            Request::Open("a:b\tc".into()),
            Request::OpenFolder(PathBuf::from("/tmp/x:y/100%")),
            Request::Capture(PathBuf::from("/tmp/shot:1 100%.png")),
            Request::New("n\nl".into()),
            Request::Rename("r\r".into()),
            Request::Delete("d:\t100%".into()),
            Request::SetRoot(Some(PathBuf::from("/tmp/x:y/100%"))),
            Request::SetRoot(None),
            Request::Exec("run sh -c 'echo 100%: a\tb'".into()),
            Request::Exec("run-in-terminal htop -d 5: 100%".into()),
            Request::RunTui("'/usr/bin/vim' a:b 100%".into()),
            Request::RunTerm("/opt/my term/ghostty".into()),
            Request::Exec("split-right".into()),
            Request::ClosePid(4242),
        ];
        for r in reqs {
            assert_eq!(Request::parse(&r.to_line()), Ok(r.clone()));
        }
    }

    #[test]
    fn close_pid_parse_rejects_bad_input() {
        assert_eq!(Request::parse("close-pid 42"), Ok(Request::ClosePid(42)));
        for bad in [
            "close-pid",
            "close-pid ",
            "close-pid x",
            "close-pid 0",
            "close-pid +5",
            "close-pid -5",
            "close-pid 5 6",
            "close-pid 5x",
            "close-pid 4294967296",
        ] {
            assert!(Request::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn retired_run_requests_no_longer_parse() {
        // `run`/`run-in-terminal` travel as `exec run …`/`exec run-in-terminal …` now.
        assert!(Request::parse("run firefox").is_err());
        assert!(Request::parse("run-in-terminal htop").is_err());
    }

    #[test]
    fn only_read_only_requests_do_not_act() {
        for r in [Request::Candidates, Request::Bindings, Request::Save] {
            assert!(!r.acts(), "{r:?}");
        }
        for r in [
            Request::Exec("quit".into()),
            Request::Open("a".into()),
            Request::OpenFolder(PathBuf::from("/tmp")),
            Request::New("n".into()),
            Request::Rename("r".into()),
            Request::Delete("d".into()),
            Request::SetRoot(None),
            Request::RunTui("htop".into()),
            Request::RunTerm("ghostty".into()),
            Request::Relaunch,
            Request::ClosePid(7),
        ] {
            assert!(r.acts(), "{r:?}");
        }
    }

    #[test]
    fn request_parse_rejects_bad_input() {
        assert!(Request::parse("open").is_err());
        assert!(Request::parse("open ").is_err());
        assert!(Request::parse("save now").is_err());
        assert_eq!(
            Request::parse("bindings x"),
            Err("bindings takes no argument".to_string())
        );
        assert_eq!(
            Request::parse("relaunch x"),
            Err("relaunch takes no argument".to_string())
        );
        assert!(Request::parse("bogus x").is_err());
        // A bare `root` is an error; `root -` clears.
        assert!(Request::parse("root").is_err());
        assert_eq!(Request::parse("root -"), Ok(Request::SetRoot(None)));
        assert!(Request::parse("open %ZZ").is_err());
        // The argument is the rest of the line after ONE space: spaces inside are kept.
        assert_eq!(
            Request::parse("open my  ws"),
            Ok(Request::Open("my  ws".into()))
        );
    }

    #[test]
    fn responses_round_trip() {
        for r in [
            Response::Ok(String::new()),
            Response::Ok("live:1".into()),
            Response::Err("no such: workspace\n".into()),
        ] {
            assert_eq!(Response::parse(&r.to_line()), Some(r.clone()));
        }
        assert_eq!(Response::Ok(String::new()).to_line(), "ok");
        assert_eq!(Response::parse("nope"), None);
    }
}
