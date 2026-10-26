//! `crucible msg <request…>` — send one protocol line to the compositor and print the response.

use crucible_compositor::ipc::{Request, SOCKET_ENV};

use crate::client::{Client, socket_from_env};

/// Usage text for `crucible msg --help`.
pub const USAGE: &str = "usage: crucible msg <request...>\n\nSend one request line to the running compositor over $CDE_SOCKET and print the raw response line.\n\n  candidates | save | open <name> | open-folder <path> | new <name> | rename <name>\n\nArguments are sent as typed, joined with single spaces: percent-encode %, :, TAB, LF and CR\nyourself (for example `crucible msg open my%3Aname`).\n\nExit: 0 on an `ok` response, 1 on `err`, 2 on usage errors or a missing $CDE_SOCKET.\n";

/// Joins the arguments into one protocol line and validates it.
///
/// # Errors
/// A usage message when there are no arguments or the line is not a valid request.
pub fn build_line(args: &[String]) -> Result<String, String> {
    if args.is_empty() {
        return Err("msg needs a request (try `crucible msg --help`)".into());
    }
    let line = args.join(" ");
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

/// Runs the subcommand; returns the exit code.
pub fn run(args: &[String]) -> i32 {
    let line = match build_line(args) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("crucible msg: {e}");
            return 2;
        }
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
    match client.request(&line) {
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
    fn invalid_requests_are_rejected() {
        assert!(build_line(&[]).is_err());
        assert!(build_line(&v(&["bogus"])).is_err());
        assert!(build_line(&v(&["open"])).is_err());
        assert!(build_line(&v(&["open", "bad%zz"])).is_err());
        assert!(build_line(&v(&["save", "extra"])).is_err());
    }

    #[test]
    fn exit_codes() {
        assert_eq!(exit_code("ok"), 0);
        assert_eq!(exit_code("ok live:a"), 0);
        assert_eq!(exit_code("err nope"), 1);
        assert_eq!(exit_code("garbage"), 1);
    }
}
