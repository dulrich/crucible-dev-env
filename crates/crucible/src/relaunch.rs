//! `crucible relaunch [--app-id <id>] -- <cmdline>` — the placeholder CDE puts in a restored app
//! window's slot: a terminal showing what was there, waiting for Enter.
//!
//! Enter sends `relaunch` over `$CDE_SOCKET`. The compositor identifies this process by its peer
//! PID, decodes the command from this process's own argv, spawns the app into the placeholder's
//! slot and closes the placeholder. This process **never** runs the command itself. Ctrl+C,
//! Ctrl+D or EOF dismisses the placeholder (exit 0; the next save drops the app record).

use std::io::{Read, Write};
use std::path::Path;

use crucible_compositor::ipc::{Request, Response};
use crucible_persist::RelaunchWrapper;

use crate::client::{Client, ClientError, socket_from_env};
use crate::tty::RawGuard;
use crate::{Parsed, help_requested};

/// Usage text for `crucible relaunch --help`.
pub const USAGE: &str = "usage: crucible relaunch [--app-id <id>] -- <cmdline>\n\nRestored-app placeholder used by CDE: shows which app was in this slot and waits. Enter asks the\ncompositor (over $CDE_SOCKET) to relaunch the app in the same pane slot and close this placeholder;\nCtrl+C, Ctrl+D or EOF dismisses it. The command is one argument (a shell string) and is never run\nby this process.\n";

/// The basename of the first shell word of `cmdline` (quotes stripped naively).
fn program_of(cmdline: &str) -> String {
    let first = cmdline.split_whitespace().next().unwrap_or(cmdline);
    let first = first.trim_matches(|c| c == '\'' || c == '"');
    Path::new(first)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(first)
        .to_string()
}

/// The banner shown while waiting.
#[must_use]
pub fn banner(args: &RelaunchWrapper) -> String {
    let what = args
        .app_id
        .clone()
        .unwrap_or_else(|| program_of(&args.cmdline));
    format!("CDE restored {what}. Press Enter to relaunch it, or Ctrl+C to dismiss.")
}

/// Sends `relaunch`; `Err` carries the reason to show.
fn send_relaunch() -> Result<(), String> {
    let path = socket_from_env(std::env::var_os(crucible_compositor::ipc::SOCKET_ENV))?;
    let mut client = Client::connect(&path).map_err(|e| e.to_string())?;
    match client.send(&Request::Relaunch) {
        Ok(Response::Ok(_)) => Ok(()),
        Ok(Response::Err(reason)) => Err(reason),
        Err(ClientError::BadResponse(line)) => Err(format!("malformed response {line:?}")),
        Err(e) => Err(e.to_string()),
    }
}

/// Parses the arguments: `--help`/`-h` before `--` (and not as the `--app-id` value) is
/// [`Parsed::Help`]; otherwise the shared wrapper grammar.
///
/// # Errors
/// The wrapper's usage message.
pub fn parse_args(args: &[String]) -> Result<Parsed<RelaunchWrapper>, String> {
    if help_requested(args, &["--app-id"], true) {
        return Ok(Parsed::Help);
    }
    RelaunchWrapper::parse(args).map(Parsed::Run)
}

/// Runs the subcommand; returns the exit code.
pub fn run(args: &[String]) -> i32 {
    let parsed = match parse_args(args) {
        Ok(Parsed::Run(p)) => p,
        Ok(Parsed::Help) => {
            print!("{USAGE}");
            return 0;
        }
        Err(e) => {
            eprintln!("crucible relaunch: {e}\n\n{USAGE}");
            return 2;
        }
    };
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{}", banner(&parsed));
    let _ = out.flush();
    let _guard = RawGuard::enter(false);
    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 1];
    loop {
        let byte = match stdin.read(&mut buf) {
            Ok(0) => None,
            Ok(_) => Some(buf[0]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => None,
        };
        match byte {
            Some(b'\r' | b'\n') => match send_relaunch() {
                Ok(()) => return 0,
                Err(reason) => {
                    // Raw mode: write an explicit CR LF.
                    let _ = write!(out, "relaunch failed: {reason}\r\n");
                    let _ = out.flush();
                }
            },
            None | Some(0x03 | 0x04) => return 0,
            Some(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn parse_accepts_with_and_without_app_id() {
        assert_eq!(
            RelaunchWrapper::parse(&a(&["--app-id", "gimp", "--", "gimp a.png"])),
            Ok(RelaunchWrapper {
                app_id: Some("gimp".into()),
                cmdline: "gimp a.png".into()
            })
        );
        assert_eq!(
            RelaunchWrapper::parse(&a(&["--", "--help"])),
            Ok(RelaunchWrapper {
                app_id: None,
                cmdline: "--help".into()
            })
        );
    }

    #[test]
    fn help_never_eats_payloads() {
        assert_eq!(parse_args(&a(&["--help"])), Ok(Parsed::Help));
        assert_eq!(parse_args(&a(&["--app-id", "x", "-h"])), Ok(Parsed::Help));
        // `--help` as the `--app-id` value is payload.
        assert_eq!(
            parse_args(&a(&["--app-id", "--help", "--", "x"])),
            Ok(Parsed::Run(RelaunchWrapper {
                app_id: Some("--help".into()),
                cmdline: "x".into()
            }))
        );
        // Anything after `--` is payload.
        assert_eq!(
            parse_args(&a(&["--", "--help"])),
            Ok(Parsed::Run(RelaunchWrapper {
                app_id: None,
                cmdline: "--help".into()
            }))
        );
    }

    #[test]
    fn parse_rejects_malformed() {
        for bad in [
            &[][..],
            &["--"],
            &["--", ""],
            &["--", "a", "b"],
            &["--app-id"],
            &["--app-id", "x", "--app-id", "y", "--", "c"],
            &["--bogus", "--", "c"],
            &["c"],
            &["--app-id", "x"],
        ] {
            assert!(RelaunchWrapper::parse(&a(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn banner_names_app_id_or_program() {
        let with = RelaunchWrapper {
            app_id: Some("org.gimp.GIMP".into()),
            cmdline: "gimp".into(),
        };
        assert_eq!(
            banner(&with),
            "CDE restored org.gimp.GIMP. Press Enter to relaunch it, or Ctrl+C to dismiss."
        );
        let without = RelaunchWrapper {
            app_id: None,
            cmdline: "/usr/lib/firefox-esr/firefox-esr --new-window".into(),
        };
        assert_eq!(
            banner(&without),
            "CDE restored firefox-esr. Press Enter to relaunch it, or Ctrl+C to dismiss."
        );
    }
}
