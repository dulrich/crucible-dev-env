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
//! Crash banner: when the command dies abnormally (a fatal signal other than SIGINT, or a nonzero
//! exit code other than 130), `resume` keeps the pane and shows `<cmd> exited (<label>). Press Enter
//! to run it again, or Ctrl+C for a shell.` instead of starting the shell straight away.
//!
//! `--exit` (only valid with `--now`, as CDE's `run-tui` sends it): when the command finishes, `resume`
//! exits with the command's exit code instead of starting a shell, so the terminal window closes.
//!
//! History: when `$SHELL` is bash, each run of the command is first appended to the bash history file
//! (`$HISTFILE`, else `~/.bash_history`), so the interactive shell that starts afterwards can recall it.

use crucible_persist::ResumeWrapper;
use rustix::stdio::stdin;
use rustix::termios::{OptionalActions, Termios, tcgetattr, tcsetattr};
use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use crate::cli::{Parsed, help_requested};
use crate::tty::{RawGuard, install_noop_handler};

/// Usage text for `crucible resume --help`.
pub const USAGE: &str = "usage: crucible resume [--now [--exit] | --lost] --cwd <dir> [-- <command>]\n\nRestored-terminal banner used by CDE: change into <dir>, show the command that was running and\nwait. Enter runs it (via $SHELL -c) and then starts an interactive $SHELL; Ctrl+C or EOF starts\n$SHELL directly. The command is never run without Enter (or --now, which runs it immediately, as `run-in-terminal` does). With --now, --exit exits with the command's exit code instead of starting a shell afterwards (--exit without --now is a usage error). If the command exits abnormally (a crash signal or a nonzero exit code other than a Ctrl+C), a crash banner offers Enter to run it again or Ctrl+C for a shell (or to close the terminal with --exit). --lost (CDE's placeholder for a terminal whose window was lost when its terminal process died) adds a line to the banner saying so; Enter re-runs the command as usual, and with no command the shell starts at once. --lost excludes --now. Without a command the shell starts\nimmediately. The command is one argument (a shell string); everything after `--` is payload. When $SHELL is bash, each run of the command is first appended to the bash history file ($HISTFILE, else ~/.bash_history), so the shell that starts afterwards can recall it.\n";

/// The banner shown before waiting (or before the immediate shell).
#[must_use]
pub fn banner(args: &ResumeWrapper) -> String {
    if args.mode == crucible_persist::ResumeMode::Lost {
        let line = match &args.command {
            Some(_) => {
                "This terminal was lost (its terminal process died). Enter re-runs the command."
            }
            None => "This terminal was lost (its terminal process died). A new shell starts here.",
        };
        let base = ResumeWrapper {
            mode: crucible_persist::ResumeMode::Banner,
            ..args.clone()
        };
        return format!("{line}\n{}", banner(&base));
    }
    match &args.command {
        Some(cmd) if args.mode.runs_now() => format!("CDE is running: {cmd}"),
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
    let parsed = match crate::cli::parse_or_exit("resume", USAGE, parse_args(args)) {
        Ok(v) => v,
        Err(code) => return code,
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
    let mut run_it = parsed.mode.runs_now() || wait_for_choice() == Wait::Run;
    let mut last = None;
    let saved_termios = tcgetattr(stdin()).ok();
    while run_it {
        record_history(&shell, &cmd);
        last = run_command(&shell, &cmd);
        match last.map_or(Outcome::Clean, exit_outcome) {
            Outcome::Abnormal(label) => {
                restore_terminal(saved_termios.as_ref());
                println!("{}", crash_banner(&cmd, &label, parsed.mode.exits()));
                run_it = wait_for_choice() == Wait::Run;
            }
            Outcome::Clean | Outcome::Interrupted => run_it = false,
        }
    }
    if parsed.mode.exits() {
        return return_code(last);
    }
    exec_shell(&shell)
}

/// True when the basename of `shell` is exactly `bash`.
#[must_use]
pub fn is_bash(shell: &OsStr) -> bool {
    Path::new(shell).file_name() == Some(OsStr::new("bash"))
}

/// The bash history file: `HISTFILE` when set and non-empty, else `$HOME/.bash_history` when `HOME`
/// is set and non-empty, else `None`.
#[must_use]
pub fn history_path(histfile: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    match (histfile, home) {
        (Some(h), _) if !h.is_empty() => Some(PathBuf::from(h)),
        (_, Some(home)) if !home.is_empty() => Some(PathBuf::from(home).join(".bash_history")),
        _ => None,
    }
}

/// The command as a history entry, or `None` when it must be skipped: empty or whitespace-only,
/// containing a newline or carriage return, or starting with a space.
#[must_use]
pub fn history_entry(cmd: &str) -> Option<&str> {
    if cmd.trim().is_empty() || cmd.contains(['\n', '\r']) || cmd.starts_with(' ') {
        None
    } else {
        Some(cmd)
    }
}

/// True for bash's timestamp comment line: `#` and one or more ASCII digits.
fn is_timestamp_line(line: &[u8]) -> bool {
    line.len() > 1 && line[0] == b'#' && line[1..].iter().all(u8::is_ascii_digit)
}

/// Appends `entry` to the history file at `path` (created when missing). A file whose first line is
/// a bash timestamp comment gets `#<now_epoch>` before the entry; a last line without a newline is
/// terminated first. Only the first line and the last byte are read.
///
/// # Errors
/// Any open, read or write error.
pub fn append_history(path: &Path, entry: &str, now_epoch: u64) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(path)?;
    let len = file.metadata()?.len();
    let mut out = String::new();
    if len > 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0u8; 1];
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            out.push('\n');
        }
        file.seek(SeekFrom::Start(0))?;
        let mut head = Vec::new();
        (&mut file).take(32).read_to_end(&mut head)?;
        let first = head.split(|b| *b == b'\n').next().unwrap_or(&[]);
        if is_timestamp_line(first) {
            out.push_str(&format!("#{now_epoch}\n"));
        }
    }
    out.push_str(entry);
    out.push('\n');
    file.write_all(out.as_bytes())
}

/// Before a run: when `shell` is bash, records the command in the bash history so the shell that
/// starts afterwards can recall it. A failure is one stderr line and never stops the command.
fn record_history(shell: &OsStr, cmd: &str) {
    if !is_bash(shell) {
        return;
    }
    let Some(entry) = history_entry(cmd) else {
        return;
    };
    let Some(path) = history_path(std::env::var_os("HISTFILE"), std::env::var_os("HOME")) else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    if let Err(e) = append_history(&path, entry, now) {
        eprintln!("crucible resume: cannot write shell history: {e}");
    }
}

/// Undoes what a TUI that died without cleaning up (e.g. SIGABRT) left behind: SGR attributes,
/// alternate screen, hidden cursor, mouse tracking, bracketed paste, application cursor/keypad keys.
/// Without it the crash banner prints onto the dead TUI's alternate screen and is not visible.
pub const TERMINAL_RESET: &str = "\x1b[0m\x1b[?1049l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1l\x1b>";

/// After an abnormal exit: puts back the termios `resume` saw before the command ran (a crashed
/// TUI leaves its own raw/no-echo modes) and, on a terminal, writes [`TERMINAL_RESET`].
fn restore_terminal(saved: Option<&Termios>) {
    let Some(saved) = saved else { return };
    let _ = tcsetattr(stdin(), OptionalActions::Now, saved);
    let mut out = std::io::stdout();
    if out.is_terminal() {
        let _ = out.write_all(TERMINAL_RESET.as_bytes());
        let _ = out.flush();
    }
}

/// How a command's exit status is treated.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Exit code 0.
    Clean,
    /// SIGINT or exit code 130: the user quit it.
    Interrupted,
    /// A crash: the label names the signal or `code <n>`.
    Abnormal(String),
}

/// Classifies an exit status. A fatal signal other than SIGINT, or a nonzero code other than 130,
/// is abnormal. `$SHELL -c` may report a crashed child as code 128+n; that is just `code <n>`.
#[must_use]
pub fn exit_outcome(status: ExitStatus) -> Outcome {
    if let Some(sig) = status.signal() {
        return match sig {
            libc::SIGINT => Outcome::Interrupted,
            other => Outcome::Abnormal(signal_name(other)),
        };
    }
    match status.code() {
        Some(0) | None => Outcome::Clean,
        Some(130) => Outcome::Interrupted,
        Some(n) => Outcome::Abnormal(format!("code {n}")),
    }
}

fn signal_name(sig: i32) -> String {
    let name = match sig {
        libc::SIGABRT => "SIGABRT",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGILL => "SIGILL",
        libc::SIGKILL => "SIGKILL",
        libc::SIGTERM => "SIGTERM",
        libc::SIGHUP => "SIGHUP",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGSYS => "SIGSYS",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGUSR2 => "SIGUSR2",
        _ => return format!("signal {sig}"),
    };
    name.to_string()
}

/// The crash banner shown after an abnormal exit.
#[must_use]
pub fn crash_banner(cmd: &str, label: &str, exit: bool) -> String {
    let fallback = if exit {
        "to close this terminal"
    } else {
        "for a shell"
    };
    format!("{cmd} exited ({label}). Press Enter to run it again, or Ctrl+C {fallback}.")
}

/// The `--exit` return code: the command's exit code; a signal-killed or unknown status is 0.
#[must_use]
pub fn return_code(status: Option<ExitStatus>) -> i32 {
    status.and_then(|s| s.code()).unwrap_or(0)
}

/// Runs `$shell -c <cmd>` as a child (the command string is one argument, never re-split) and
/// waits for it. Failures are reported; the caller execs the shell either way (unless `--exit`).
/// Returns the command's exit status, or `None` when it could not be determined.
fn run_command(shell: &OsString, cmd: &str) -> Option<ExitStatus> {
    match Command::new(shell).arg("-c").arg(cmd).spawn() {
        Ok(mut child) => loop {
            match child.wait() {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    eprintln!("crucible resume: waiting for the command failed: {e}");
                    break None;
                }
                Ok(status) => break Some(status),
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

/// Installs a do-nothing handler for SIGINT and SIGQUIT so a Ctrl+C aimed at the restored command
/// does not kill `resume`. A handler (not `SIG_IGN`) is used because caught dispositions reset to
/// default across `exec`, so the spawned command and the final shell get normal signal semantics.
fn ignore_interrupts_while_resident() {
    for sig in [libc::SIGINT, libc::SIGQUIT] {
        if let Err(e) = install_noop_handler(sig, true) {
            eprintln!("crucible resume: cannot install signal handler: {e}");
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
    use crucible_persist::ResumeMode;

    #[test]
    fn is_bash_matches_basename_only() {
        for (s, want) in [
            ("/bin/bash", true),
            ("/usr/bin/bash", true),
            ("bash", true),
            ("/bin/sh", false),
            ("/usr/bin/zsh", false),
            ("/opt/bashful", false),
        ] {
            assert_eq!(is_bash(OsStr::new(s)), want, "{s}");
        }
    }

    #[test]
    fn history_path_prefers_histfile_then_home() {
        let o = |s: &str| Some(OsString::from(s));
        assert_eq!(
            history_path(o("/h/f"), o("/home")),
            Some(PathBuf::from("/h/f"))
        );
        assert_eq!(
            history_path(o(""), o("/home")),
            Some(PathBuf::from("/home/.bash_history"))
        );
        assert_eq!(
            history_path(None, o("/home")),
            Some(PathBuf::from("/home/.bash_history"))
        );
        assert_eq!(history_path(None, o("")), None);
        assert_eq!(history_path(None, None), None);
    }

    #[test]
    fn history_entry_skips_unrecordable_commands() {
        assert_eq!(history_entry("ls -l"), Some("ls -l"));
        assert_eq!(history_entry("ls  "), Some("ls  "));
        for bad in ["", "   ", "a\nb", "a\rb", " ls"] {
            assert_eq!(history_entry(bad), None, "{bad:?}");
        }
    }

    fn temp_hist(name: &str, content: Option<&str>) -> PathBuf {
        let p = std::env::temp_dir().join(format!("crucible-hist-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        if let Some(c) = content {
            std::fs::write(&p, c).unwrap();
        }
        p
    }

    #[test]
    fn append_history_new_and_plain_files() {
        let p = temp_hist("new", None);
        append_history(&p, "ls", 5).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "ls\n");
        std::fs::remove_file(&p).unwrap();
        let p = temp_hist("plain", Some("a\nb\n"));
        append_history(&p, "ls", 5).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nb\nls\n");
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn append_history_terminates_unfinished_last_line() {
        let p = temp_hist("nonl", Some("a\nb"));
        append_history(&p, "ls", 5).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nb\nls\n");
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn append_history_timestamped_file_gets_timestamp() {
        let p = temp_hist("ts", Some("#1700000000\nls\n"));
        append_history(&p, "pwd", 1_700_000_100).unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "#1700000000\nls\n#1700000100\npwd\n"
        );
        std::fs::remove_file(&p).unwrap();
        let p = temp_hist("notts", Some("#comment\nls\n"));
        append_history(&p, "pwd", 9).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "#comment\nls\npwd\n");
        std::fs::remove_file(&p).unwrap();
    }

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
        assert_eq!(a.mode, ResumeMode::Run);
        assert_eq!(a.command.as_deref(), Some("htop"));
        let a = ResumeWrapper::parse(&v(&["--cwd", "/p", "--now"])).unwrap();
        assert!(a.mode == ResumeMode::Run && a.command.is_none());
        let a = ResumeWrapper::parse(&v(&["--cwd", "/p", "--", "--now"])).unwrap();
        assert_eq!(a.mode, ResumeMode::Banner);
        assert_eq!(a.command.as_deref(), Some("--now"));
        assert_eq!(
            ResumeWrapper::parse(&v(&["--cwd", "/p"])).unwrap().mode,
            ResumeMode::Banner
        );
    }

    #[test]
    fn exit_flag_parses_like_now_and_needs_now() {
        let a =
            ResumeWrapper::parse(&v(&["--now", "--exit", "--cwd", "/p", "--", "htop"])).unwrap();
        assert_eq!(a.mode, ResumeMode::RunThenExit);
        let a = ResumeWrapper::parse(&v(&["--exit", "--now", "--exit", "--cwd", "/p"])).unwrap();
        assert!(a.mode == ResumeMode::RunThenExit && a.command.is_none());
        let a = ResumeWrapper::parse(&v(&["--now", "--cwd", "/p", "--", "--exit"])).unwrap();
        assert_eq!(a.mode, ResumeMode::Run);
        assert_eq!(a.command.as_deref(), Some("--exit"));
        assert_eq!(
            ResumeWrapper::parse(&v(&["--now", "--cwd", "/p"]))
                .unwrap()
                .mode,
            ResumeMode::Run
        );
        assert!(ResumeWrapper::parse(&v(&["--exit", "--cwd", "/p", "--", "htop"])).is_err());
    }

    #[test]
    fn now_banner_does_not_offer_enter() {
        let a = ResumeWrapper {
            cwd: "/p".into(),
            command: Some("htop".into()),
            mode: ResumeMode::Run,
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
            mode: ResumeMode::Banner,
        };
        assert_eq!(
            banner(&with),
            "CDE restored this terminal. Last command: cargo test. Press Enter to run it, or Ctrl+C for a shell."
        );
        let without = ResumeWrapper {
            cwd: "/p".into(),
            command: None,
            mode: ResumeMode::Banner,
        };
        assert_eq!(banner(&without), "CDE restored this terminal in /p.");
    }

    #[test]
    fn lost_banner_and_parsing() {
        let a = ResumeWrapper::parse(&v(&["--lost", "--cwd", "/p", "--", "cargo test"])).unwrap();
        assert_eq!(a.mode, ResumeMode::Lost);
        assert!(!a.mode.runs_now() && !a.mode.exits());
        assert_eq!(
            banner(&a),
            "This terminal was lost (its terminal process died). Enter re-runs the command.\nCDE restored this terminal. Last command: cargo test. Press Enter to run it, or Ctrl+C for a shell."
        );
        let idle = ResumeWrapper::parse(&v(&["--cwd", "/p", "--lost", "--lost"])).unwrap();
        assert_eq!(
            banner(&idle),
            "This terminal was lost (its terminal process died). A new shell starts here.\nCDE restored this terminal in /p."
        );
        assert!(ResumeWrapper::parse(&v(&["--lost", "--now", "--cwd", "/p"])).is_err());
        assert!(matches!(
            parse_args(&v(&["--lost", "--help"])),
            Ok(Parsed::Help)
        ));
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

    fn sig(n: i32) -> ExitStatus {
        ExitStatus::from_raw(n)
    }

    fn code(c: i32) -> ExitStatus {
        ExitStatus::from_raw(c << 8)
    }

    #[test]
    fn exit_outcome_classifies() {
        assert_eq!(
            exit_outcome(sig(libc::SIGABRT)),
            Outcome::Abnormal("SIGABRT".into())
        );
        assert_eq!(
            exit_outcome(sig(libc::SIGSEGV)),
            Outcome::Abnormal("SIGSEGV".into())
        );
        assert_eq!(exit_outcome(sig(libc::SIGINT)), Outcome::Interrupted);
        assert_eq!(exit_outcome(code(0)), Outcome::Clean);
        assert_eq!(exit_outcome(code(130)), Outcome::Interrupted);
        assert_eq!(exit_outcome(code(1)), Outcome::Abnormal("code 1".into()));
        assert_eq!(exit_outcome(sig(50)), Outcome::Abnormal("signal 50".into()));
    }

    #[test]
    fn crash_banner_variants() {
        assert_eq!(
            crash_banner("htop", "SIGSEGV", false),
            "htop exited (SIGSEGV). Press Enter to run it again, or Ctrl+C for a shell."
        );
        assert_eq!(
            crash_banner("htop", "code 1", true),
            "htop exited (code 1). Press Enter to run it again, or Ctrl+C to close this terminal."
        );
    }

    #[test]
    fn terminal_reset_leaves_the_alternate_screen_and_shows_the_cursor() {
        for seq in [
            "\x1b[0m",
            "\x1b[?1049l",
            "\x1b[?25h",
            "\x1b[?1000l",
            "\x1b[?2004l",
        ] {
            assert!(TERMINAL_RESET.contains(seq), "{seq:?}");
        }
    }

    #[test]
    fn exit_return_code_mapping() {
        assert_eq!(return_code(Some(code(3))), 3);
        assert_eq!(return_code(Some(sig(libc::SIGABRT))), 0);
        assert_eq!(return_code(None), 0);
    }
}
