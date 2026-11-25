//! `crucible url <url>` — the `cde:` scheme handler: focus a window by pid, or a live workspace by
//! name. Registered as the `x-scheme-handler/cde` handler by the deploy.

use crucible_compositor::ipc::{Request, Response};

use crate::client::{Client, discover_socket_from_env};

/// Usage text for `crucible url --help`.
pub const USAGE: &str = "usage: crucible url <url>\n\nOpen a `cde:` link: focus a window or a workspace of the running CDE session. The URL is `cde:focus?pid=<n>` and/or `cde:focus?workspace=<name>` (`cde://focus?...` is accepted too; the scheme is case-insensitive; a #fragment is ignored). Values are percent-encoded (RFC 3986; `+` is a literal plus), so a name containing `& = ? # / %` or a space must encode them.\n\nWith `pid`, the window of that process (or its nearest ancestor) is focused. When that fails, or when only `workspace` is given, the live workspace of that name is switched to (never restored from disk).\n\nThe socket is $CDE_SOCKET, or else the single *.sock file in $XDG_RUNTIME_DIR/cde/ (so a desktop launcher works without the session's environment).\n\nExit: 0 when focused, 1 when nothing could be focused (the reason is on stderr and in a `notify-send` notification when that is installed), 2 on a malformed URL or usage error.\n";

/// A parsed `cde:focus` link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusLink {
    /// The process whose window to focus.
    pub pid: Option<u32>,
    /// The live workspace to switch to.
    pub workspace: Option<String>,
}

/// Percent-decodes `s` (RFC 3986; `+` stays a literal `+`).
fn percent_decode(s: &str) -> Result<String, String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = |c: Option<&u8>| c.and_then(|c| (*c as char).to_digit(16));
            match (hex(b.get(i + 1)), hex(b.get(i + 2))) {
                (Some(h), Some(l)) => {
                    out.push(u8::try_from(h * 16 + l).expect("two hex digits fit a byte"));
                    i += 3;
                }
                _ => return Err(format!("malformed percent-encoding in {s:?}")),
            }
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| format!("percent-decoded value of {s:?} is not UTF-8"))
}

/// The pid rule of the IPC `parse_pid`: plain decimal digits, a non-zero `u32`.
fn parse_pid(arg: &str) -> Result<u32, String> {
    if arg.is_empty() || !arg.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("pid must be decimal digits, got {arg:?}"));
    }
    match arg.parse::<u32>() {
        Ok(0) => Err("pid must be non-zero".to_string()),
        Ok(n) => Ok(n),
        Err(_) => Err(format!("pid {arg:?} is out of range")),
    }
}

/// Parses a `cde:focus?…` (or `cde://focus?…`) URL.
///
/// # Errors
/// A one-line reason: wrong scheme or action, an unknown, duplicate or empty parameter, a bad pid,
/// malformed percent-encoding, or no parameter at all.
pub fn parse(url: &str) -> Result<FocusLink, String> {
    let (scheme, rest) = url
        .split_once(':')
        .ok_or_else(|| format!("not a cde: link: {url:?}"))?;
    if !scheme.eq_ignore_ascii_case("cde") {
        return Err(format!("not a cde: link: {url:?}"));
    }
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    let rest = rest.split_once('#').map_or(rest, |(before, _)| before);
    let (action, query) = rest.split_once('?').unwrap_or((rest, ""));
    if action != "focus" {
        return Err(format!("unknown cde: action {action:?} (only `focus`)"));
    }
    if query.is_empty() {
        return Err("cde:focus needs a pid= or workspace= parameter".to_string());
    }
    let mut link = FocusLink {
        pid: None,
        workspace: None,
    };
    for pair in query.split('&') {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| format!("parameter {pair:?} has no value"))?;
        let key = percent_decode(key)?;
        let value = percent_decode(value)?;
        if value.is_empty() {
            return Err(format!("parameter {key:?} has an empty value"));
        }
        let duplicate = || format!("duplicate parameter {key:?}");
        match key.as_str() {
            "pid" if link.pid.is_some() => return Err(duplicate()),
            "pid" => link.pid = Some(parse_pid(&value)?),
            "workspace" if link.workspace.is_some() => return Err(duplicate()),
            "workspace" => link.workspace = Some(value),
            _ => return Err(format!("unknown parameter {key:?} (pid, workspace)")),
        }
    }
    Ok(link)
}

/// What to do after the pid step answered (or failed).
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    /// Focused; exit 0.
    Done,
    /// Fall back to `focus-live <name>`.
    FocusLive(String),
    /// Give up with this reason; exit 1.
    Fail(String),
}

/// Pure fallback decision over the pid step's result (`Err` is a connection or protocol failure).
#[must_use]
pub fn after_pid(result: &Result<Response, String>, workspace: Option<&str>) -> Next {
    let reason = match result {
        Ok(Response::Ok(_)) => return Next::Done,
        Ok(Response::Err(e)) => e.clone(),
        Err(e) => e.clone(),
    };
    match workspace {
        Some(w) => Next::FocusLive(w.to_string()),
        None => Next::Fail(reason),
    }
}

/// Pure outcome of the final (workspace) step: `None` is success, `Some` the reason.
#[must_use]
pub fn after_live(result: &Result<Response, String>) -> Option<String> {
    match result {
        Ok(Response::Ok(_)) => None,
        Ok(Response::Err(e)) => Some(e.clone()),
        Err(e) => Some(e.clone()),
    }
}

fn send_one(path: &std::path::Path, request: &Request) -> Result<Response, String> {
    let mut client =
        Client::connect(path).map_err(|e| format!("cannot connect to {}: {e}", path.display()))?;
    client.send(request).map_err(|e| e.to_string())
}

/// Reports a failure on stderr and as a desktop notification when `notify-send` is installed.
fn report(reason: &str) {
    eprintln!("crucible url: {reason}");
    let _ = std::process::Command::new("notify-send")
        .args(["CDE link", reason])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Runs the subcommand; returns the exit code.
pub fn run(args: &[String]) -> i32 {
    if args.first().is_some_and(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return 0;
    }
    let [url] = args else {
        eprintln!("crucible url: needs exactly one URL\n\n{USAGE}");
        return 2;
    };
    let link = match parse(url) {
        Ok(l) => l,
        Err(e) => {
            report(&e);
            return 2;
        }
    };
    let path = match discover_socket_from_env() {
        Ok(p) => p,
        Err(e) => {
            report(&e);
            return 1;
        }
    };
    let mut live = link.workspace.clone();
    if let Some(pid) = link.pid {
        let result = send_one(&path, &Request::FocusPid(pid));
        match after_pid(&result, link.workspace.as_deref()) {
            Next::Done => return 0,
            Next::FocusLive(w) => live = Some(w),
            Next::Fail(reason) => {
                report(&reason);
                return 1;
            }
        }
    }
    let Some(name) = live else {
        report("nothing to focus");
        return 1;
    };
    match after_live(&send_one(&path, &Request::FocusLive(name))) {
        None => 0,
        Some(reason) => {
            report(&reason);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(url: &str) -> String {
        parse(url).unwrap().workspace.unwrap()
    }

    #[test]
    fn valid_forms() {
        let pid = |n| FocusLink {
            pid: Some(n),
            workspace: None,
        };
        assert_eq!(parse("cde:focus?pid=42"), Ok(pid(42)));
        assert_eq!(parse("cde://focus?pid=42"), Ok(pid(42)));
        assert!(
            parse("CDE:Focus?pid=1").is_err(),
            "the action is case-sensitive"
        );
        assert_eq!(parse("CDE:focus?pid=1"), Ok(pid(1)));
        assert_eq!(parse("Cde://focus?pid=1"), Ok(pid(1)));
        assert_eq!(
            parse("cde:focus?workspace=main"),
            Ok(FocusLink {
                pid: None,
                workspace: Some("main".into())
            })
        );
        assert_eq!(
            parse("cde:focus?pid=7&workspace=a"),
            Ok(FocusLink {
                pid: Some(7),
                workspace: Some("a".into())
            })
        );
        assert_eq!(
            parse("cde:focus?workspace=a&pid=7"),
            parse("cde:focus?pid=7&workspace=a")
        );
        assert_eq!(parse("cde:focus?pid=9#frag&x=1"), Ok(pid(9)));
    }

    #[test]
    fn rejected_forms() {
        for bad in [
            "http:focus?pid=1",
            "focus?pid=1",
            "cde:open?pid=1",
            "cde:focus",
            "cde:focus?",
            "cde:focus?foo=1",
            "cde:focus?pid=1&foo=1",
            "cde:focus?pid=1&pid=2",
            "cde:focus?workspace=a&workspace=b",
            "cde:focus?pid=",
            "cde:focus?workspace=",
            "cde:focus?pid",
            "cde:focus?pid=1&",
            "cde:focus?pid=0",
            "cde:focus?pid=-1",
            "cde:focus?pid=12a",
            "cde:focus?pid=+5",
            "cde:focus?pid=4294967296",
            "cde:focus?workspace=%zz",
            "cde:focus?workspace=%2",
            "cde:focus?workspace=a%",
            "cde:focus?workspace=%FF",
            "cde:focus?workspace=%C3",
        ] {
            let r = parse(bad);
            assert!(r.is_err(), "{bad} parsed as {r:?}");
        }
        // The fragment hides everything after it, including a `?`.
        assert!(parse("cde:focus#?pid=1").is_err());
        assert_eq!(
            parse("cde:focus?pid=4294967295").unwrap().pid,
            Some(u32::MAX)
        );
    }

    #[test]
    fn workspace_names_decode_and_round_trip_the_wire() {
        for (enc, name) in [
            ("a%26b", "a&b"),
            ("a%3Db", "a=b"),
            ("a%3Fb", "a?b"),
            ("a%23b", "a#b"),
            ("a%2Fb", "a/b"),
            ("my%20ws", "my ws"),
            ("100%25", "100%"),
            ("caf%C3%A9", "café"),
            ("a+b", "a+b"),
            ("a%3Ab", "a:b"),
        ] {
            let got = ws(&format!("cde:focus?workspace={enc}"));
            assert_eq!(got, name, "{enc}");
            let line = Request::FocusLive(got.clone()).to_line().unwrap();
            assert_eq!(Request::parse(&line), Ok(Request::FocusLive(got)), "{line}");
        }
    }

    #[test]
    fn fallback_decision() {
        let ok = Ok(Response::Ok("w".into()));
        let err = Ok(Response::Err("no window for pid 3".into()));
        let down = Err("cannot connect".to_string());
        assert_eq!(after_pid(&ok, Some("w")), Next::Done);
        assert_eq!(after_pid(&ok, None), Next::Done);
        assert_eq!(after_pid(&err, Some("w")), Next::FocusLive("w".into()));
        assert_eq!(after_pid(&down, Some("w")), Next::FocusLive("w".into()));
        assert_eq!(
            after_pid(&err, None),
            Next::Fail("no window for pid 3".into())
        );
        assert_eq!(after_pid(&down, None), Next::Fail("cannot connect".into()));
        assert_eq!(after_live(&ok), None);
        assert_eq!(
            after_live(&Ok(Response::Err("no live workspace w".into()))),
            Some("no live workspace w".into())
        );
        assert_eq!(after_live(&down), Some("cannot connect".into()));
    }
}
