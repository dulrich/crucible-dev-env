//! `crucible resume [--now] [--exit] --cwd <dir> [-- <command>]` — the restore banner CDE puts in a restored
//! terminal: the old command waits until Enter; Ctrl+C or EOF gives a plain shell.
//!
//! Ctrl+C handling: while waiting, stdin is in raw mode (ISIG off), so Ctrl+C arrives as byte
//! `0x03` and is handled in-process. On Enter, `resume` stays resident: it spawns `$SHELL -c
//! <command>` as a child, waits for it, then execs an interactive `$SHELL`. Staying resident keeps
//! this process's argv (`crucible resume --cwd <dir> -- <command>`) as the terminal's child, so
//! CDE's capture still records the command on the next save. `resume` installs no-op SIGINT and
//! SIGQUIT handlers (caught, not ignored), so a Ctrl+C kills only the command; caught
//! dispositions reset to default across `exec`, so the command keeps normal Ctrl+C semantics.
//!
//! `--exit` (only valid with `--now`, as CDE's `run-tui` sends it): when the command finishes, `resume`
//! exits with the command's exit code instead of starting a shell, so the terminal window closes.

use crucible_persist::ResumeWrapper;
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::Command;

use crate::tty::RawGuard;
use crate::{Parsed, help_requested};

/// Usage text for `crucible resume --help`.
pub const USAGE: &str = "usage: crucible resume [--now [--exit]] --cwd <dir> [-- <command>]\n\nRestored-terminal banner used by CDE: change into <dir>, show the command that was running and\nwait. Enter runs it (via $SHELL -c) and then starts an interactive $SHELL; Ctrl+C or EOF starts\n$SHELL directly. The command is never run without Enter (or --now, which runs it immediately, as `run-in-terminal` does). With --now, --exit exits with the command's exit code instead of starting a shell afterwards (--exit without --now is a usage error). Without a command the shell starts\nimmediately. The command is one argument (a shell string); everything after `--` is payload.\n";

/// The banner shown before waiting (or before the immediate shell).
#[must_use]
pub fn banner(args: &ResumeWrapper) -> String {
    match &args.command {
        Some(cmd) if args.now => format!("CDE is running: {cmd}"),
        Some(cmd) => format!(
            "CDE restored this terminal. Last command: {cmd}. Press Enter to run it, or Ctrl+C for a shell."
        ),
        None => format!("CDE restored this terminal in {}.", args.cwd.display()),
    }
}

/// `$SHELL`, or `/bin/sh` when unset or empty.
#[must_use]
pub fn choose_shell(env: Option<OsString>) -> OsString {
    match env {
        Some(s) if !s.is_empty() => s,
        _ => OsString::from("/bin/sh"),
    }
}

/// What a byte (or EOF) read at the banner means.
#[derive(Debug, PartialEq, Eq)]
pub enum Wait {
    /// Enter: run the command.
    Run,
    /// Ctrl+C, Ctrl+D or EOF: go straight to the shell.
    Shell,
    /// Anything else: keep waiting.
    Ignore,
}

/// Classifies one input byte; `None` is EOF.
#[must_use]
pub fn classify(byte: Option<u8>) -> Wait {
    match byte {
        Some(b'\r' | b'\n') => Wait::Run,
        None | Some(0x03 | 0x04) => Wait::Shell,
        Some(_) => Wait::Ignore,
    }
}

/// Parses the arguments: `--help`/`-h` before `--` (and not as the `--cwd` value) is
/// [`Parsed::Help`]; otherwise the shared wrapper grammar.
///
/// # Errors
/// The wrapper's usage message.
pub fn parse_args(args: &[String]) -> Result<Parsed<ResumeWrapper>, String> {
    if help_requested(args, &["--cwd"], true) {
        return Ok(Parsed::Help);
    }
    ResumeWrapper::parse(args).map(Parsed::Run)
}

/// Runs the subcommand. Only returns (with an exit code) when an `exec` fails.
pub fn run(args: &[String]) -> i32 {
    let parsed = match parse_args(args) {
        Ok(Parsed::Run(p)) => p,
        Ok(Parsed::Help) => {
            print!("{USAGE}");
            return 0;
        }
        Err(e) => {
            eprintln!("crucible resume: {e}\n\n{USAGE}");
            return 2;
        }
    };
    if let Err(e) = std::env::set_current_dir(&parsed.cwd) {
        eprintln!(
            "crucible resume: cannot enter {}: {e}; staying in the current directory",
            parsed.cwd.display()
        );
    }
    let shell = choose_shell(std::env::var_os("SHELL"));
    println!("{}", banner(&parsed));
    let Some(cmd) = parsed.command else {
        return exec_shell(&shell);
    };
    ignore_interrupts_while_resident();
    if parsed.now || wait_for_choice() == Wait::Run {
        let code = run_command(&shell, &cmd);
        if parsed.exit {
            return code.unwrap_or(0);
        }
    }
    exec_shell(&shell)
}

/// Runs `$shell -c <cmd>` as a child (the command string is one argument, never re-split) and
/// waits for it. Failures are reported; the caller execs the shell either way (unless `--exit`).
/// Returns the command's exit code, or `None` when it could not be determined.
fn run_command(shell: &OsString, cmd: &str) -> Option<i32> {
    match Command::new(shell).arg("-c").arg(cmd).spawn() {
        Ok(mut child) => loop {
            match child.wait() {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    eprintln!("crucible resume: waiting for the command failed: {e}");
                    break None;
                }
                Ok(status) => break status.code(),
            }
        },
        Err(e) => {
            eprintln!(
                "crucible resume: cannot run {}: {e}",
                shell.to_string_lossy()
            );
            None
        }
    }
}

extern "C" fn noop_handler(_: libc::c_int) {}

/// Installs a do-nothing handler for SIGINT and SIGQUIT so a Ctrl+C aimed at the restored command
/// does not kill `resume`. A handler (not `SIG_IGN`) is used because caught dispositions reset to
/// default across `exec`, so the spawned command and the final shell get normal signal semantics.
fn ignore_interrupts_while_resident() {
    for sig in [libc::SIGINT, libc::SIGQUIT] {
        // SAFETY: `sigaction` is called with a zero-initialised (valid) `sigaction` struct whose
        // handler is an `extern "C"` function that does nothing, which is trivially
        // async-signal-safe; the mask is emptied via `sigemptyset` on a valid pointer and the old
        // action pointer is null, which `sigaction` permits.
        #[allow(unsafe_code)]
        unsafe {
            let mut act: libc::sigaction = std::mem::zeroed();
            act.sa_sigaction = noop_handler as extern "C" fn(libc::c_int) as libc::sighandler_t;
            act.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&raw mut act.sa_mask);
            if libc::sigaction(sig, &raw const act, std::ptr::null_mut()) != 0 {
                eprintln!(
                    "crucible resume: cannot install signal handler: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
    }
}

fn exec_shell(shell: &OsString) -> i32 {
    let err = Command::new(shell).exec();
    eprintln!(
        "crucible resume: cannot exec {}: {err}",
        shell.to_string_lossy()
    );
    1
}

/// Blocks until Enter, Ctrl+C or EOF. On a terminal, raw mode delivers Ctrl+C as a byte.
fn wait_for_choice() -> Wait {
    let guard = RawGuard::enter(false);
    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 1];
    let outcome = loop {
        let byte = match stdin.read(&mut buf) {
            Ok(0) => None,
            Ok(_) => Some(buf[0]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => None,
        };
        match classify(byte) {
            Wait::Ignore => {}
            other => break other,
        }
    };
    drop(guard);
    println!();
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn help_never_eats_payloads() {
        assert_eq!(parse_args(&v(&["--help"])), Ok(Parsed::Help));
        assert_eq!(parse_args(&v(&["--cwd", "/x", "-h"])), Ok(Parsed::Help));
        // After `--` everything is the command payload.
        assert!(matches!(
            parse_args(&v(&["--cwd", "/x", "--", "--help"])),
            Ok(Parsed::Run(w)) if w.command.as_deref() == Some("--help")
        ));
        // `--help` as the `--cwd` value is payload.
        assert!(matches!(
            parse_args(&v(&["--cwd", "--help"])),
            Ok(Parsed::Run(w)) if w.cwd.as_os_str() == "--help"
        ));
    }

    #[test]
    fn parses_cwd_and_command() {
        let a = ResumeWrapper::parse(&v(&["--cwd", "/p", "--", "cargo test"])).unwrap();
        assert_eq!(a.cwd, PathBuf::from("/p"));
        assert_eq!(a.command.as_deref(), Some("cargo test"));
    }

    #[test]
    fn command_may_look_like_a_flag() {
        let a = ResumeWrapper::parse(&v(&["--cwd", "/p", "--", "--help"])).unwrap();
        assert_eq!(a.command.as_deref(), Some("--help"));
    }

    #[test]
    fn no_command_forms() {
        assert_eq!(
            ResumeWrapper::parse(&v(&["--cwd", "/p"])).unwrap().command,
            None
        );
        assert_eq!(
            ResumeWrapper::parse(&v(&["--cwd", "/p", "--"]))
                .unwrap()
                .command,
            None
        );
        assert_eq!(
            ResumeWrapper::parse(&v(&["--cwd", "/p", "--", ""]))
                .unwrap()
                .command,
            None
        );
    }

    #[test]
    fn now_flag_parses_before_the_separator_only() {
        let a = ResumeWrapper::parse(&v(&["--now", "--cwd", "/p", "--", "htop"])).unwrap();
        assert!(a.now);
        assert_eq!(a.command.as_deref(), Some("htop"));
        let a = ResumeWrapper::parse(&v(&["--cwd", "/p", "--now"])).unwrap();
        assert!(a.now && a.command.is_none());
        let a = ResumeWrapper::parse(&v(&["--cwd", "/p", "--", "--now"])).unwrap();
        assert!(!a.now);
        assert_eq!(a.command.as_deref(), Some("--now"));
        assert!(!ResumeWrapper::parse(&v(&["--cwd", "/p"])).unwrap().now);
    }

    #[test]
    fn exit_flag_parses_like_now_and_needs_now() {
        let a =
            ResumeWrapper::parse(&v(&["--now", "--exit", "--cwd", "/p", "--", "htop"])).unwrap();
        assert!(a.now && a.exit);
        let a = ResumeWrapper::parse(&v(&["--exit", "--now", "--exit", "--cwd", "/p"])).unwrap();
        assert!(a.exit && a.command.is_none());
        let a = ResumeWrapper::parse(&v(&["--now", "--cwd", "/p", "--", "--exit"])).unwrap();
        assert!(!a.exit);
        assert_eq!(a.command.as_deref(), Some("--exit"));
        assert!(
            !ResumeWrapper::parse(&v(&["--now", "--cwd", "/p"]))
                .unwrap()
                .exit
        );
        assert!(ResumeWrapper::parse(&v(&["--exit", "--cwd", "/p", "--", "htop"])).is_err());
    }

    #[test]
    fn now_banner_does_not_offer_enter() {
        let a = ResumeWrapper {
            cwd: "/p".into(),
            command: Some("htop".into()),
            now: true,
            exit: false,
        };
        assert_eq!(banner(&a), "CDE is running: htop");
    }

    #[test]
    fn rejects_bad_usage() {
        assert!(ResumeWrapper::parse(&[]).is_err());
        assert!(ResumeWrapper::parse(&v(&["--cwd"])).is_err());
        assert!(ResumeWrapper::parse(&v(&["--cwd", "/a", "--cwd", "/b"])).is_err());
        assert!(ResumeWrapper::parse(&v(&["--cwd", "/a", "x"])).is_err());
        assert!(ResumeWrapper::parse(&v(&["--cwd", "/a", "--", "a", "b"])).is_err());
        assert!(ResumeWrapper::parse(&v(&["--", "x"])).is_err());
    }

    #[test]
    fn banner_text_is_exact() {
        let with = ResumeWrapper {
            cwd: "/p".into(),
            command: Some("cargo test".into()),
            now: false,
            exit: false,
        };
        assert_eq!(
            banner(&with),
            "CDE restored this terminal. Last command: cargo test. Press Enter to run it, or Ctrl+C for a shell."
        );
        let without = ResumeWrapper {
            cwd: "/p".into(),
            command: None,
            now: false,
            exit: false,
        };
        assert_eq!(banner(&without), "CDE restored this terminal in /p.");
    }

    #[test]
    fn shell_choice() {
        assert_eq!(choose_shell(None), OsString::from("/bin/sh"));
        assert_eq!(choose_shell(Some("".into())), OsString::from("/bin/sh"));
        assert_eq!(
            choose_shell(Some("/bin/zsh".into())),
            OsString::from("/bin/zsh")
        );
    }

    #[test]
    fn only_enter_runs_the_command() {
        assert_eq!(classify(Some(b'\r')), Wait::Run);
        assert_eq!(classify(Some(b'\n')), Wait::Run);
        assert_eq!(classify(Some(0x03)), Wait::Shell);
        assert_eq!(classify(Some(0x04)), Wait::Shell);
        assert_eq!(classify(None), Wait::Shell);
        assert_eq!(classify(Some(b'x')), Wait::Ignore);
    }
}
