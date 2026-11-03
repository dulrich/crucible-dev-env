//! `crucible msg <request…>` — send one protocol line to the compositor and print the response.

use crucible_compositor::ipc::{Request, SOCKET_ENV, encode_field};

use crate::cli::Parsed;
use crate::client::{Client, socket_from_env};

/// Usage text for `crucible msg --help`.
pub const USAGE: &str = "usage: crucible msg <request...>\n\nSend one request line to the running compositor over $CDE_SOCKET and print the raw response line.\n\n  candidates | bindings | save | open <name> | open-folder <path> | new <name> | rename <name> | root <path>|- | exec <action> | run <command...> | run-in-terminal <command...> | run-tui <command...> | run-term <program> | close-pid <n> | relaunch\n\n`close-pid <n>` politely asks every window of process <n> to close (an X11 window without `WM_DELETE_WINDOW` is destroyed) and answers `ok <count>`, or `err no window for pid <n>`.\n`root <path>` sets the active workspace's root (the folder its new terminals and apps start in; the path must be an absolute existing directory) and `root -` clears it.\n`exec` runs a keybinding action (the `binds` action grammar, for example `crucible msg exec focus left`); a failed action answers `err <reason>`.\n`run` and `run-in-terminal` are shorthands for `exec run <command>` and `exec run-in-terminal <command>`: they take the rest of the arguments as one command line (joined with single spaces) and encode it for you\n(for example `crucible msg run firefox --new-window`); an unresolvable first word is refused with `err unknown command <word>`. `run-tui` encodes its command the same way. For every other request, arguments are sent as typed, joined with single spaces: percent-encode %, :, TAB, LF and CR\nyourself (for example `crucible msg open my%3Aname`). While the session is locked, every request but `candidates`, `bindings` and `save` answers `err session locked`.\n\nExit: 0 on an `ok` response, 1 on `err`, 2 on usage errors or a missing $CDE_SOCKET.\n";

/// Joins the arguments into one protocol line and validates it.
///
/// # Errors
/// A usage message when there are no arguments or the line is not a valid request.
pub fn build_line(args: &[String]) -> Result<String, String> {
    if args.is_empty() {
        return Err("msg needs a request (try `crucible msg --help`)".into());
    }
    let verb = args[0].as_str();
    let line = match verb {
        "run" | "run-in-terminal" | "run-tui" | "run-term" => {
            // The command is free text: encode it so `:`, `%` and friends survive the wire.
            let cmd = args[1..].join(" ");
            if verb == "run-tui" || verb == "run-term" {
                format!("{verb} {}", encode_field(&cmd))
            } else if cmd.trim().is_empty() {
                return Err(format!("{verb} needs a command"));
            } else {
                // `run`/`run-in-terminal` are shorthands for `exec run …`/`exec run-in-terminal …`.
                format!("exec {}", encode_field(&format!("{verb} {cmd}")))
            }
        }
        _ => args.join(" "),
    };
    Request::parse(&line)?;
    Ok(line)
}

/// Maps a raw response line to the process exit code: 0 `ok`, 1 `err`, 1 anything else.
#[must_use]
pub fn exit_code(response: &str) -> i32 {
    if response == "ok" || response.starts_with("ok ") {
        0
    } else {
        1
    }
}

/// Parses the arguments: only a **first** argument of `--help`/`-h` is [`Parsed::Help`] (the rest
/// is the request line, so `msg open --help` sends a request); otherwise the request line.
///
/// # Errors
/// The request-line builder's message.
pub fn parse_args(args: &[String]) -> Result<Parsed<String>, String> {
    if args.first().is_some_and(|a| a == "--help" || a == "-h") {
        return Ok(Parsed::Help);
    }
    build_line(args).map(Parsed::Run)
}

/// Runs the subcommand; returns the exit code.
pub fn run(args: &[String]) -> i32 {
    let line = match crate::cli::parse_or_exit_terse("msg", USAGE, parse_args(args)) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let path = match socket_from_env(std::env::var_os(SOCKET_ENV)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("crucible msg: {e}");
            return 2;
        }
    };
    let mut client = match Client::connect(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("crucible msg: cannot connect to {}: {e}", path.display());
            return 1;
        }
    };
    match client.request_raw(&line) {
        Ok(resp) => {
            println!("{resp}");
            exit_code(&resp)
        }
        Err(e) => {
            eprintln!("crucible msg: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn joins_with_single_spaces_and_validates() {
        assert_eq!(build_line(&v(&["candidates"])).unwrap(), "candidates");
        assert_eq!(
            build_line(&v(&["open", "my%3Aname"])).unwrap(),
            "open my%3Aname"
        );
        assert_eq!(build_line(&v(&["new", "a", "b"])).unwrap(), "new a b");
    }

    #[test]
    fn run_encodes_the_command_line() {
        let line = build_line(&v(&["run", "firefox", "--new-window"])).unwrap();
        assert_eq!(line, "exec run firefox --new-window");
        assert_eq!(
            Request::parse(&line),
            Ok(Request::run("firefox --new-window"))
        );
        let line = build_line(&v(&["run", "echo", "50%:x"])).unwrap();
        assert_eq!(line, "exec run echo 50%25%3Ax");
        assert_eq!(
            Request::parse(&line),
            Ok(Request::Exec("run echo 50%:x".into()))
        );
        assert!(build_line(&v(&["run"])).is_err());
        assert!(build_line(&v(&["run", " "])).is_err());
    }

    #[test]
    fn run_tui_encodes_the_command_line() {
        let line = build_line(&v(&["run-tui", "htop", "50%:x"])).unwrap();
        assert_eq!(line, "run-tui htop 50%25%3Ax");
        assert_eq!(
            Request::parse(&line),
            Ok(Request::RunTui("htop 50%:x".into()))
        );
        assert!(build_line(&v(&["run-tui"])).is_err());
    }

    #[test]
    fn run_term_encodes_the_program() {
        let line = build_line(&v(&["run-term", "/opt/my", "term"])).unwrap();
        assert_eq!(line, "run-term /opt/my term");
        assert_eq!(
            Request::parse(&line),
            Ok(Request::RunTerm("/opt/my term".into()))
        );
        assert!(build_line(&v(&["run-term"])).is_err());
    }

    #[test]
    fn run_in_terminal_encodes_the_command_line() {
        let line = build_line(&v(&["run-in-terminal", "htop", "50%:x"])).unwrap();
        assert_eq!(line, "exec run-in-terminal htop 50%25%3Ax");
        assert_eq!(
            Request::parse(&line),
            Ok(Request::run_in_terminal("htop 50%:x"))
        );
        assert!(build_line(&v(&["run-in-terminal"])).is_err());
    }

    #[test]
    fn close_pid_builds_and_rejects_non_numbers() {
        assert_eq!(
            build_line(&v(&["close-pid", "42"])).unwrap(),
            "close-pid 42"
        );
        assert!(build_line(&v(&["close-pid", "x"])).is_err());
    }

    #[test]
    fn invalid_requests_are_rejected() {
        assert!(build_line(&[]).is_err());
        assert!(build_line(&v(&["bogus"])).is_err());
        assert!(build_line(&v(&["open"])).is_err());
        assert!(build_line(&v(&["open", "bad%zz"])).is_err());
        assert!(build_line(&v(&["save", "extra"])).is_err());
    }

    #[test]
    fn help_only_as_the_first_argument() {
        assert_eq!(parse_args(&v(&["--help"])), Ok(Parsed::Help));
        assert_eq!(parse_args(&v(&["-h"])), Ok(Parsed::Help));
        // `msg open --help` is a request, not help.
        assert_eq!(
            parse_args(&v(&["open", "--help"])),
            Ok(Parsed::Run("open --help".into()))
        );
    }

    #[test]
    fn exit_codes() {
        assert_eq!(exit_code("ok"), 0);
        assert_eq!(exit_code("ok live:a"), 0);
        assert_eq!(exit_code("err nope"), 1);
        assert_eq!(exit_code("garbage"), 1);
    }
}
