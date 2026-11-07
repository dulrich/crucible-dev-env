//! The one typed contract for the argv CDE runs `crucible` with: [`Invocation`] — `crucible resume
//! …` (a restored or `run-in-terminal` terminal banner), `crucible relaunch …` (an app
//! placeholder), `crucible pick …`, `crucible launch`, `crucible keys` and `crucible overlay-host
//! --socket <path>` (the pre-warmed overlay host).
//!
//! The compositor builds a terminal's argv with [`Invocation::terminal_argv`], the `crucible`
//! binary parses its own arguments with each payload's `parse` ([`ResumeWrapper::parse`],
//! [`RelaunchWrapper::parse`], [`PickTarget::parse`]), and capture decodes a window's argv with
//! [`CdeWrapper::decode`] (resume/relaunch only; any parse error is `None` there). One grammar,
//! one place.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::field::{decode_field, encode_field};

/// One `crucible <sub> <args…>` invocation CDE builds (and the binary parses).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// `crucible resume …`.
    Resume(ResumeWrapper),
    /// `crucible relaunch …`.
    Relaunch(RelaunchWrapper),
    /// `crucible pick [--rename <encoded> | --root <encoded>]`.
    Pick(PickTarget),
    /// `crucible launch`.
    Launch,
    /// `crucible keys`.
    Keys,
    /// `crucible overlay-host --socket <path>`.
    OverlayHost {
        /// The socket the warm host binds.
        socket: PathBuf,
    },
}

impl Invocation {
    /// The subcommand name (`argv[1]` of the `crucible` process).
    #[must_use]
    pub fn subcommand(&self) -> &'static str {
        match self {
            Self::Resume(_) => "resume",
            Self::Relaunch(_) => "relaunch",
            Self::Pick(_) => "pick",
            Self::Launch => "launch",
            Self::Keys => "keys",
            Self::OverlayHost { .. } => "overlay-host",
        }
    }

    /// The arguments after the subcommand.
    #[must_use]
    pub fn to_args(&self) -> Vec<OsString> {
        match self {
            Self::Resume(w) => w.to_args(),
            Self::Relaunch(w) => w.to_args(),
            Self::Pick(t) => t.to_args(),
            Self::Launch | Self::Keys => Vec::new(),
            Self::OverlayHost { socket } => vec!["--socket".into(), socket.into()],
        }
    }

    /// Parses the arguments after subcommand `sub`.
    ///
    /// # Errors
    /// An unknown subcommand, or the subcommand's usage message.
    pub fn parse(sub: &str, args: &[String]) -> Result<Self, String> {
        match sub {
            "resume" => ResumeWrapper::parse(args).map(Self::Resume),
            "relaunch" => RelaunchWrapper::parse(args).map(Self::Relaunch),
            "pick" => PickTarget::parse(args).map(Self::Pick),
            "launch" | "keys" => match args {
                [] if sub == "launch" => Ok(Self::Launch),
                [] => Ok(Self::Keys),
                [other, ..] => Err(format!("unexpected argument {other:?}")),
            },
            "overlay-host" => match args {
                [flag, path] if flag == "--socket" && !path.is_empty() => Ok(Self::OverlayHost {
                    socket: PathBuf::from(path),
                }),
                _ => Err("expected --socket <path>".into()),
            },
            other => Err(format!("unknown subcommand {other:?}")),
        }
    }

    /// The argv a terminal runs: `[exec_flag, crucible, <sub>, args…]`.
    #[must_use]
    pub fn terminal_argv(&self, exec_flag: &str, crucible: &Path) -> Vec<OsString> {
        let mut out: Vec<OsString> =
            vec![exec_flag.into(), crucible.into(), self.subcommand().into()];
        out.extend(self.to_args());
        out
    }
}

/// When `crucible resume` runs its command: on the wire `Banner` is no flag, `Run` is `--now`,
/// `RunThenExit` is `--now --exit` (in that order), `Lost` is `--lost`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeMode {
    /// Show the banner and wait for Enter (a restored terminal).
    Banner,
    /// `--now`: run the command immediately, then start a shell (`run-in-terminal`).
    Run,
    /// `--now --exit`: run immediately and exit with the command's status instead of starting a
    /// shell (`run-tui`).
    RunThenExit,
    /// `--lost`: `Banner`, plus a line saying the terminal was lost (a loss placeholder: the
    /// window's terminal process died under it). Enter re-runs the command.
    Lost,
}

impl ResumeMode {
    /// Whether the command runs without waiting for Enter.
    #[must_use]
    pub fn runs_now(self) -> bool {
        matches!(self, Self::Run | Self::RunThenExit)
    }

    /// Whether `resume` exits with the command's status instead of starting a shell.
    #[must_use]
    pub fn exits(self) -> bool {
        self == Self::RunThenExit
    }
}

/// `crucible resume [--now [--exit] | --lost] --cwd <dir> [-- <command>]`.
///
/// Grammar: `--cwd <dir>`, `--now`, `--exit`, `--lost` in any order; repeated flags are
/// idempotent, a repeated `--cwd` is rejected, `--exit` needs `--now`, `--lost` excludes `--now`, `--cwd` is required. An
/// optional `--` is followed by at most one element (the command, one shell string); an empty one
/// means no command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeWrapper {
    /// Directory to change into.
    pub cwd: PathBuf,
    /// The command string, if any (never empty).
    pub command: Option<String>,
    /// When the command runs (`--now` / `--exit`).
    pub mode: ResumeMode,
}

impl ResumeWrapper {
    /// The arguments after `resume`: `[--now] [--exit] [--lost] --cwd <dir> [-- <command>]`.
    #[must_use]
    pub fn to_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::new();
        if self.mode.runs_now() {
            args.push("--now".into());
        }
        if self.mode.exits() {
            args.push("--exit".into());
        }
        if self.mode == ResumeMode::Lost {
            args.push("--lost".into());
        }
        args.push("--cwd".into());
        args.push(self.cwd.clone().into_os_string());
        if let Some(command) = &self.command {
            args.push("--".into());
            args.push(command.into());
        }
        args
    }

    /// Parses the arguments after `resume`.
    ///
    /// # Errors
    /// A usage message for a missing `--cwd` value, unknown flags or extra positional arguments.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut cwd = None;
        let mut now = false;
        let mut exit = false;
        let mut lost = false;
        let mut i = 0;
        let mut command = None;
        while i < args.len() {
            match args[i].as_str() {
                "--cwd" => {
                    let v = args.get(i + 1).ok_or("--cwd needs a directory")?;
                    if cwd.replace(PathBuf::from(v)).is_some() {
                        return Err("--cwd given twice".into());
                    }
                    i += 2;
                }
                "--now" => {
                    now = true;
                    i += 1;
                }
                "--exit" => {
                    exit = true;
                    i += 1;
                }
                "--lost" => {
                    lost = true;
                    i += 1;
                }
                "--" => {
                    match &args[i + 1..] {
                        [] => {}
                        [cmd] => command = Some(cmd.clone()).filter(|c| !c.is_empty()),
                        _ => return Err("expected at most one command argument after `--`".into()),
                    }
                    break;
                }
                other => return Err(format!("unexpected argument {other:?}")),
            }
        }
        let mode = match (now, exit, lost) {
            (false, false, false) => ResumeMode::Banner,
            (false, false, true) => ResumeMode::Lost,
            (true, false, false) => ResumeMode::Run,
            (true, true, false) => ResumeMode::RunThenExit,
            (false, true, _) => return Err("--exit requires --now".into()),
            (true, _, true) => return Err("--lost excludes --now".into()),
        };
        Ok(Self {
            cwd: cwd.ok_or("--cwd <dir> is required")?,
            command,
            mode,
        })
    }

    /// Decodes a whole argv `[<…/crucible>, "resume", …]` (`argv[0]` named `crucible` or `own`, the
    /// running binary's basename); anything else is `None`.
    #[must_use]
    pub fn from_argv(argv: &[String], own: &str) -> Option<Self> {
        Self::parse(subcommand_args(argv, "resume", own)?).ok()
    }
}

/// `crucible relaunch [--app-id <id>] -- <cmdline>`: an optional `--app-id` (a repeat is
/// rejected), then a required `--` followed by exactly one non-empty element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelaunchWrapper {
    /// The saved app id, if any.
    pub app_id: Option<String>,
    /// The saved command line (one shell string).
    pub cmdline: String,
}

impl RelaunchWrapper {
    /// The arguments after `relaunch`: `[--app-id <id>] -- <cmdline>`.
    #[must_use]
    pub fn to_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::new();
        if let Some(id) = &self.app_id {
            args.push("--app-id".into());
            args.push(id.into());
        }
        args.push("--".into());
        args.push((&self.cmdline).into());
        args
    }

    /// Parses the arguments after `relaunch`.
    ///
    /// # Errors
    /// A usage message for a repeated or valueless `--app-id`, unknown arguments, or anything but
    /// exactly one non-empty element after `--`.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut app_id = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--app-id" => {
                    let v = args.get(i + 1).ok_or("--app-id needs a value")?;
                    if app_id.replace(v.clone()).is_some() {
                        return Err("--app-id given twice".into());
                    }
                    i += 2;
                }
                "--" => {
                    return match &args[i + 1..] {
                        [cmd] if !cmd.is_empty() => Ok(Self {
                            app_id,
                            cmdline: cmd.clone(),
                        }),
                        _ => Err("expected exactly one non-empty command line after `--`".into()),
                    };
                }
                other => return Err(format!("unexpected argument {other:?}")),
            }
        }
        Err("`-- <cmdline>` is required".into())
    }

    /// Decodes a whole argv `[<…/crucible>, "relaunch", …]` (`argv[0]` named `crucible` or `own`,
    /// the running binary's basename); anything else is `None`.
    #[must_use]
    pub fn from_argv(argv: &[String], own: &str) -> Option<Self> {
        Self::parse(subcommand_args(argv, "relaunch", own)?).ok()
    }
}

/// The args after `sub` when `argv` is `[<…/crucible>, sub, …]` (`crucible` or `own`).
fn subcommand_args<'a>(argv: &'a [String], sub: &str, own: &str) -> Option<&'a [String]> {
    (crucible_subcommand(argv, own)? == sub).then(|| &argv[2..])
}

/// `argv[1]` when `argv[0]`'s basename is `crucible` or the running binary's basename: the CDE
/// subcommand the process runs. `own` is the caller-resolved basename ([`crate::own_exe_name`]).
#[must_use]
pub fn crucible_subcommand<'a>(argv: &'a [String], own: &str) -> Option<&'a str> {
    is_own_name(Path::new(argv.first()?).file_name()?, own).then_some(())?;
    argv.get(1).map(String::as_str)
}

/// `name` is `crucible` or `own`.
fn is_own_name(name: &std::ffi::OsStr, own: &str) -> bool {
    name == "crucible" || name == own
}

/// Whether a process is one of CDE's own processes: `argv[0]`'s basename is `crucible` or the
/// running binary's basename, or (argv unreadable or empty) its `comm` is one of them. The own
/// name `own` is resolved by the caller ([`crate::own_exe_name`]). The `comm`
/// fallback compares each name truncated to 15 bytes (the kernel's `TASK_COMM_LEN - 1`).
#[must_use]
pub fn is_cde_process(argv: &[String], comm: &str, own: &str) -> bool {
    match argv.first() {
        Some(a) => Path::new(a)
            .file_name()
            .is_some_and(|n| is_own_name(n, own)),
        None => ["crucible", own]
            .iter()
            .any(|name| comm == truncate_comm(name)),
    }
}

/// `name` cut to the kernel's 15-byte `comm` limit (on a char boundary).
fn truncate_comm(name: &str) -> &str {
    let mut end = name.len().min(15);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

/// What a CDE restore wrapper found in a window's own argv stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CdeWrapper {
    /// `crucible relaunch …`: an app placeholder.
    App(RelaunchWrapper),
    /// `crucible resume …`: a terminal banner.
    Terminal(ResumeWrapper),
}

impl CdeWrapper {
    /// Decodes a window argv that is CDE's own wrapper (`crucible relaunch|resume …`), either
    /// bare at `argv[0]` or run by a terminal right after the first `exec_flag` element. A
    /// wrapper-shaped run anywhere else is ordinary argument data and yields `None`, as does
    /// anything the shared grammar rejects. Decodes one level only.
    /// `own` is the running binary's basename, resolved by the caller.
    #[must_use]
    pub fn decode(argv: &[String], exec_flag: &str, own: &str) -> Option<Self> {
        let start = if is_wrapper_start(argv, own) {
            0
        } else {
            argv.iter().position(|a| a == exec_flag)? + 1
        };
        let wrapper = argv.get(start..)?;
        if !is_wrapper_start(wrapper, own) {
            return None;
        }
        match wrapper.get(1).map(String::as_str) {
            Some("relaunch") => RelaunchWrapper::from_argv(wrapper, own).map(Self::App),
            _ => ResumeWrapper::from_argv(wrapper, own).map(Self::Terminal),
        }
    }
}

fn is_wrapper_start(argv: &[String], own: &str) -> bool {
    matches!(crucible_subcommand(argv, own), Some("relaunch" | "resume"))
}

/// What `crucible pick` is asked to do: `--rename` and `--root` are alternatives, so both cannot
/// be built. On the wire the value is field-encoded ([`encode_field`]); an empty root (no current
/// root) is an empty value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickTarget {
    /// The fuzzy workspace picker (no arguments).
    Workspace,
    /// `--rename <encoded>`: the rename prompt prefilled with the (decoded) name.
    Rename(String),
    /// `--root <encoded>`: the workspace-root prompt prefilled with the (decoded) current root
    /// (empty = none).
    Root(String),
}

impl PickTarget {
    /// The arguments after `pick`: nothing, `--rename <encoded>` or `--root <encoded>`.
    #[must_use]
    pub fn to_args(&self) -> Vec<OsString> {
        match self {
            Self::Workspace => Vec::new(),
            Self::Rename(v) => vec!["--rename".into(), encode_field(v).into()],
            Self::Root(v) => vec!["--root".into(), encode_field(v).into()],
        }
    }

    /// Parses (and decodes) the arguments after `pick`.
    ///
    /// # Errors
    /// `"{flag} needs a value"`, `"--rename and --root cannot be combined"`, `"malformed
    /// percent-encoding in {flag} value"` or `"unexpected argument {other:?}"`.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        Self::parse_with(args, |flag, value| {
            decode_field(value).ok_or_else(|| format!("malformed percent-encoding in {flag} value"))
        })
    }

    /// The shape check of [`Self::parse`] without decoding: the value is kept as sent (still
    /// encoded). The warm overlay host validates a mode line with this; the decode (and its error)
    /// happens when the picker runs.
    ///
    /// # Errors
    /// As [`Self::parse`], minus the percent-encoding error.
    pub fn parse_encoded(args: &[String]) -> Result<Self, String> {
        Self::parse_with(args, |_, value| Ok(value.to_string()))
    }

    fn parse_with(
        args: &[String],
        decode: impl Fn(&str, &str) -> Result<String, String>,
    ) -> Result<Self, String> {
        match args {
            [] => Ok(Self::Workspace),
            [flag, value] if flag == "--rename" => decode(flag, value).map(Self::Rename),
            [flag, value] if flag == "--root" => decode(flag, value).map(Self::Root),
            [flag] if flag == "--rename" || flag == "--root" => {
                Err(format!("{flag} needs a value"))
            }
            [a, _, b, ..]
                if (a == "--rename" && b == "--root") || (a == "--root" && b == "--rename") =>
            {
                Err("--rename and --root cannot be combined".into())
            }
            [other, ..] => Err(format!("unexpected argument {other:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_string()).collect()
    }

    fn strings(args: Vec<OsString>) -> Vec<String> {
        args.into_iter().map(|a| a.into_string().unwrap()).collect()
    }

    #[test]
    fn crucible_subcommand_and_cde_process() {
        assert_eq!(
            crucible_subcommand(&v(&["/x/crucible", "mem"]), "crucible"),
            Some("mem")
        );
        assert_eq!(crucible_subcommand(&v(&["crucible"]), "crucible"), None);
        assert_eq!(
            crucible_subcommand(&v(&["/x/other", "mem"]), "crucible"),
            None
        );
        assert_eq!(crucible_subcommand(&[], "crucible"), None);
        assert!(is_cde_process(&v(&["/x/crucible"]), "x", "crucible"));
        assert!(!is_cde_process(&v(&["htop"]), "crucible", "crucible"));
        assert!(is_cde_process(&[], "crucible", "crucible"));
        assert!(!is_cde_process(&[], "bash", "crucible"));
    }

    // --- Round trip ---

    const MODES: [ResumeMode; 4] = [
        ResumeMode::Banner,
        ResumeMode::Run,
        ResumeMode::RunThenExit,
        ResumeMode::Lost,
    ];

    fn resume_grid() -> Vec<ResumeWrapper> {
        let cwds = ["/p", "/with space/dir", "/ünïcødé/路径", "relative"];
        let commands = [
            None,
            Some("htop"),
            Some("cargo test -- --nocapture"),
            Some("echo 'a b' \"c\" it's"),
            Some("--help"),
        ];
        let mut out = Vec::new();
        for cwd in cwds {
            for command in commands {
                for mode in MODES {
                    out.push(ResumeWrapper {
                        cwd: cwd.into(),
                        command: command.map(String::from),
                        mode,
                    });
                }
            }
        }
        out
    }

    fn relaunch_grid() -> Vec<RelaunchWrapper> {
        let mut out = Vec::new();
        for app_id in [None, Some("org.gimp.GIMP"), Some("with space")] {
            for cmdline in ["gimp", "gimp 'a b.png'", "--help", "é \"q\""] {
                out.push(RelaunchWrapper {
                    app_id: app_id.map(String::from),
                    cmdline: cmdline.into(),
                });
            }
        }
        out
    }

    fn pick_grid() -> Vec<PickTarget> {
        let mut out = vec![PickTarget::Workspace];
        for v in ["", "a:b", "100%", "tab\there", "--help", "/h/a b", "--root"] {
            out.push(PickTarget::Rename(v.into()));
            out.push(PickTarget::Root(v.into()));
        }
        out
    }

    #[test]
    fn round_trip_over_a_grid() {
        for w in resume_grid() {
            assert_eq!(ResumeWrapper::parse(&strings(w.to_args())), Ok(w.clone()));
        }
        for w in relaunch_grid() {
            assert_eq!(RelaunchWrapper::parse(&strings(w.to_args())), Ok(w.clone()));
        }
    }

    #[test]
    fn pick_target_round_trips_and_parse_encoded_keeps_the_value() {
        for t in pick_grid() {
            let args = strings(t.to_args());
            assert_eq!(PickTarget::parse(&args), Ok(t.clone()));
            let raw = PickTarget::parse_encoded(&args).unwrap();
            let expect = match &t {
                PickTarget::Workspace => PickTarget::Workspace,
                PickTarget::Rename(v) => PickTarget::Rename(encode_field(v)),
                PickTarget::Root(v) => PickTarget::Root(encode_field(v)),
            };
            assert_eq!(raw, expect);
        }
    }

    #[test]
    fn every_invocation_round_trips_through_its_terminal_argv() {
        let mut all: Vec<Invocation> = vec![
            Invocation::Launch,
            Invocation::Keys,
            Invocation::OverlayHost {
                socket: "/run/u/cde-overlay-host-1-1.sock".into(),
            },
        ];
        all.extend(resume_grid().into_iter().map(Invocation::Resume));
        all.extend(relaunch_grid().into_iter().map(Invocation::Relaunch));
        all.extend(pick_grid().into_iter().map(Invocation::Pick));
        for inv in all {
            assert_eq!(
                Invocation::parse(inv.subcommand(), &strings(inv.to_args())),
                Ok(inv.clone())
            );
            let argv = strings(inv.terminal_argv("-e", Path::new("/c")));
            assert_eq!(argv[..3], ["-e", "/c", inv.subcommand()]);
            assert_eq!(Invocation::parse(&argv[2], &argv[3..]), Ok(inv));
        }
        assert!(Invocation::parse("launch", &v(&["x"])).is_err());
        assert!(Invocation::parse("overlay-host", &v(&["--socket", ""])).is_err());
        assert!(Invocation::parse("nope", &[]).is_err());
    }

    #[test]
    fn resume_mode_is_the_flag_pair_on_the_wire() {
        let w = |mode| ResumeWrapper {
            cwd: "/p".into(),
            command: None,
            mode,
        };
        assert_eq!(strings(w(ResumeMode::Banner).to_args()), ["--cwd", "/p"]);
        assert_eq!(
            strings(w(ResumeMode::Run).to_args()),
            ["--now", "--cwd", "/p"]
        );
        assert_eq!(
            strings(w(ResumeMode::RunThenExit).to_args()),
            ["--now", "--exit", "--cwd", "/p"]
        );
        let m = |a: &[&str]| ResumeWrapper::parse(&v(a)).map(|r| r.mode);
        assert_eq!(
            m(&["--exit", "--now", "--exit", "--cwd", "/p"]),
            Ok(ResumeMode::RunThenExit)
        );
        assert_eq!(m(&["--cwd", "/p", "--now", "--now"]), Ok(ResumeMode::Run));
        assert_eq!(m(&["--cwd", "/p"]), Ok(ResumeMode::Banner));
        assert_eq!(
            m(&["--exit", "--cwd", "/p"]),
            Err("--exit requires --now".into())
        );
    }

    #[test]
    fn pick_parse_errors_are_exact() {
        let p = |a: &[&str]| PickTarget::parse(&v(a));
        assert_eq!(p(&["--rename"]), Err("--rename needs a value".into()));
        assert_eq!(p(&["--root"]), Err("--root needs a value".into()));
        assert_eq!(
            p(&["--rename", "a", "--root", "b"]),
            Err("--rename and --root cannot be combined".into())
        );
        assert_eq!(
            p(&["--root", "a", "--rename", "b"]),
            Err("--rename and --root cannot be combined".into())
        );
        assert_eq!(
            p(&["--rename", "bad%zz"]),
            Err("malformed percent-encoding in --rename value".into())
        );
        assert_eq!(p(&["--nope"]), Err("unexpected argument \"--nope\"".into()));
        assert_eq!(
            p(&["--rename", "a", "--rename", "b"]),
            Err("unexpected argument \"--rename\"".into())
        );
        assert_eq!(
            PickTarget::parse_encoded(&v(&["--rename", "bad%zz"])),
            Ok(PickTarget::Rename("bad%zz".into()))
        );
    }

    // --- Literal argv oracles (ported from the compositor's former argv builders) ---

    fn term_argv(inv: Invocation, exec: &str, crucible: &str) -> Vec<String> {
        strings(inv.terminal_argv(exec, Path::new(crucible)))
    }

    #[test]
    fn relaunch_argv_keeps_the_cmdline_as_one_element() {
        let rl = |id: Option<&str>, c: &str| {
            Invocation::Relaunch(RelaunchWrapper {
                app_id: id.map(str::to_string),
                cmdline: c.to_string(),
            })
        };
        assert_eq!(
            term_argv(rl(Some("gimp"), "gimp 'a b.png'"), "-e", "/bin/crucible"),
            [
                "-e",
                "/bin/crucible",
                "relaunch",
                "--app-id",
                "gimp",
                "--",
                "gimp 'a b.png'"
            ]
        );
        assert_eq!(
            term_argv(rl(None, "xterm"), "-e", "/bin/crucible"),
            ["-e", "/bin/crucible", "relaunch", "--", "xterm"]
        );
        // The placeholder argv (minus the exec flag) decodes back to the record.
        let argv = term_argv(rl(Some("x"), "x --y"), "-e", "/bin/crucible");
        let cap = RelaunchWrapper::from_argv(&argv[1..], "crucible").unwrap();
        assert_eq!(cap.app_id.as_deref(), Some("x"));
        assert_eq!(cap.cmdline, "x --y");
    }

    #[test]
    fn restore_argv_keeps_the_command_as_one_element() {
        let inv = Invocation::Resume(ResumeWrapper {
            cwd: "/w d".into(),
            command: Some("cargo test 'a b'".into()),
            mode: ResumeMode::Banner,
        });
        assert_eq!(
            term_argv(inv, "-e", "/bin/crucible"),
            [
                "-e",
                "/bin/crucible",
                "resume",
                "--cwd",
                "/w d",
                "--",
                "cargo test 'a b'"
            ]
        );
    }

    #[test]
    fn launcher_argv_runs_crucible_launch() {
        let args = Invocation::Launch.terminal_argv("-e", Path::new("/c"));
        assert_eq!(
            args,
            vec![OsString::from("-e"), "/c".into(), "launch".into()]
        );
    }

    #[test]
    fn keys_argv_runs_the_keys_subcommand() {
        let args = Invocation::Keys.terminal_argv("-e", Path::new("/c"));
        assert_eq!(args, vec![OsString::from("-e"), "/c".into(), "keys".into()]);
    }

    #[test]
    fn picker_argv_encodes_the_rename_target() {
        let pick = |t| Invocation::Pick(t).terminal_argv("-e", Path::new("/c"));
        assert_eq!(
            strings(pick(PickTarget::Rename("a:b".into()))),
            ["-e", "/c", "pick", "--rename", "a%3Ab"]
        );
        assert_eq!(pick(PickTarget::Workspace).len(), 3);
    }

    #[test]
    fn picker_argv_encodes_the_root_value_even_when_empty() {
        let pick = |t| strings(Invocation::Pick(t).terminal_argv("-e", Path::new("/c")));
        assert_eq!(
            pick(PickTarget::Root("/h/a:b".into())),
            ["-e", "/c", "pick", "--root", "/h/a%3Ab"]
        );
        assert_eq!(
            pick(PickTarget::Root(String::new())),
            ["-e", "/c", "pick", "--root", ""]
        );
    }

    #[test]
    fn run_in_terminal_args_keep_the_command_one_element() {
        let inv = |mode| {
            Invocation::Resume(ResumeWrapper {
                cwd: "/w".into(),
                command: Some("a b; c".into()),
                mode,
            })
            .terminal_argv("-e", Path::new("/x/crucible"))
        };
        let args = inv(ResumeMode::Run);
        let want: Vec<OsString> = [
            "-e",
            "/x/crucible",
            "resume",
            "--now",
            "--cwd",
            "/w",
            "--",
            "a b; c",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        assert_eq!(args, want);
        let tui = inv(ResumeMode::RunThenExit);
        assert_eq!(
            tui[3..5],
            [OsString::from("--now"), OsString::from("--exit")]
        );
        assert_eq!(tui.len(), want.len() + 1);
    }

    #[test]
    fn overlay_host_argv_is_the_socket_flag() {
        assert_eq!(
            Invocation::OverlayHost {
                socket: "/s.sock".into()
            }
            .terminal_argv("-e", Path::new("/c")),
            vec![
                OsString::from("-e"),
                "/c".into(),
                "overlay-host".into(),
                "--socket".into(),
                "/s.sock".into()
            ]
        );
    }

    // --- Grammar points ---

    #[test]
    fn resume_flags_in_any_order() {
        let a = ResumeWrapper::parse(&v(&["--cwd", "/p", "--exit", "--now", "--", "x"])).unwrap();
        assert_eq!(a.mode, ResumeMode::RunThenExit);
        assert_eq!(a.command.as_deref(), Some("x"));
    }

    #[test]
    fn resume_repeated_now_and_exit_are_idempotent() {
        let a = ResumeWrapper::parse(&v(&["--now", "--now", "--exit", "--exit", "--cwd", "/p"]))
            .unwrap();
        assert_eq!(a.mode, ResumeMode::RunThenExit);
    }

    #[test]
    fn resume_exit_alone_is_rejected() {
        assert_eq!(
            ResumeWrapper::parse(&v(&["--exit", "--cwd", "/p"])),
            Err("--exit requires --now".into())
        );
    }

    #[test]
    fn repeated_cwd_and_app_id_are_rejected() {
        assert_eq!(
            ResumeWrapper::parse(&v(&["--cwd", "/a", "--cwd", "/b"])),
            Err("--cwd given twice".into())
        );
        assert_eq!(
            RelaunchWrapper::parse(&v(&["--app-id", "x", "--app-id", "y", "--", "c"])),
            Err("--app-id given twice".into())
        );
    }

    #[test]
    fn resume_empty_command_is_none() {
        let a = ResumeWrapper::parse(&v(&["--cwd", "/p", "--", ""])).unwrap();
        assert_eq!(a.command, None);
    }

    // --- Moved from crucible-persist `capture` (banner parsers / decoder) ---

    #[test]
    fn banner_parser_rejects_lookalikes() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(ResumeWrapper::from_argv(&v(&["/bin/crucible", "pick"]), "crucible").is_none());
        assert!(
            ResumeWrapper::from_argv(&v(&["/bin/other", "resume", "--cwd", "/p"]), "crucible")
                .is_none()
        );
        assert!(
            ResumeWrapper::from_argv(
                &v(&["/bin/crucible", "resume", "--cwd", "/p", "x"]),
                "crucible"
            )
            .is_none()
        );
        assert!(
            ResumeWrapper::from_argv(
                &v(&["/bin/crucible", "resume", "--cwd", "/p", "--", "a", "b"]),
                "crucible"
            )
            .is_none()
        );
        assert!(ResumeWrapper::from_argv(&[], "crucible").is_none());
        let b = ResumeWrapper::from_argv(
            &v(&["/bin/crucible", "resume", "--cwd", "/p", "--"]),
            "crucible",
        )
        .unwrap();
        assert_eq!(b.command, None);
    }

    #[test]
    fn banner_parser_accepts_the_now_form() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let plain = ResumeWrapper::from_argv(
            &v(&["/bin/crucible", "resume", "--cwd", "/p", "--", "htop"]),
            "crucible",
        )
        .unwrap();
        let now = ResumeWrapper::from_argv(
            &v(&[
                "/bin/crucible",
                "resume",
                "--now",
                "--cwd",
                "/p",
                "--",
                "htop",
            ]),
            "crucible",
        )
        .unwrap();
        // The typed wrapper carries `now`; the decoded record (cwd, command) is the same.
        assert_eq!((&plain.cwd, &plain.command), (&now.cwd, &now.command));
        assert_eq!(
            (now.mode, plain.mode),
            (ResumeMode::Run, ResumeMode::Banner)
        );
        assert_eq!(now.command.as_deref(), Some("htop"));
        assert!(
            ResumeWrapper::from_argv(&v(&["/bin/crucible", "resume", "--now"]), "crucible")
                .is_none()
        );
        // Shared grammar: repeated `--now` is idempotent, so this now decodes (was `None`).
        assert!(
            ResumeWrapper::from_argv(
                &v(&["/bin/crucible", "resume", "--now", "--now", "--cwd", "/p"]),
                "crucible"
            )
            .is_some()
        );
    }

    #[test]
    fn banner_parser_accepts_now_and_exit_in_either_order() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for flags in [["--now", "--exit"], ["--exit", "--now"]] {
            let mut a = vec!["/bin/crucible", "resume"];
            a.extend(flags);
            a.extend(["--cwd", "/p", "--", "htop"]);
            let b = ResumeWrapper::from_argv(&v(&a), "crucible").unwrap();
            assert_eq!(b.cwd, PathBuf::from("/p"));
            assert_eq!(b.command.as_deref(), Some("htop"));
        }
        // Still `None`, now because `--exit` without `--now` is rejected (repeats are idempotent).
        let twice = ["/bin/crucible", "resume", "--exit", "--exit", "--cwd", "/p"];
        assert!(ResumeWrapper::from_argv(&v(&twice), "crucible").is_none());
    }

    #[test]
    fn wrapper_decoder_recognises_only_executable_positions() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let app = |id: Option<&str>, c: &str| {
            Some(CdeWrapper::App(RelaunchWrapper {
                app_id: id.map(str::to_string),
                cmdline: c.to_string(),
            }))
        };
        // Projects a decoded terminal wrapper onto the (cwd, command) the record keeps.
        let term = |c: Option<&str>| Some((PathBuf::from("/p"), c.map(str::to_string)));
        let as_term = |w: Option<CdeWrapper>| match w {
            Some(CdeWrapper::Terminal(r)) => Some((r.cwd, r.command)),
            _ => None,
        };
        let wrapped = v(&[
            "ghostty",
            "--gtk-single-instance=false",
            "-e",
            "/x/crucible",
            "relaunch",
            "--app-id",
            "a",
            "--",
            "firefox",
        ]);
        assert_eq!(
            CdeWrapper::decode(&wrapped, "-e", "crucible"),
            app(Some("a"), "firefox")
        );
        assert_eq!(
            CdeWrapper::decode(
                &v(&["/x/crucible", "relaunch", "--", "x"]),
                "-e",
                "crucible"
            ),
            app(None, "x")
        );
        for flags in [&[][..], &["--now", "--exit"][..]] {
            let mut a = vec!["ghostty", "-e", "/x/crucible", "resume"];
            a.extend(flags);
            a.extend(["--cwd", "/p", "--", "htop"]);
            assert_eq!(
                as_term(CdeWrapper::decode(&v(&a), "-e", "crucible")),
                term(Some("htop"))
            );
        }
        assert_eq!(
            CdeWrapper::decode(&v(&["firefox", "--new-window"]), "-e", "crucible"),
            None
        );
        assert_eq!(
            CdeWrapper::decode(
                &v(&["ghostty", "-e", "/x/crucible", "pick"]),
                "-e",
                "crucible"
            ),
            None
        );
        assert_eq!(
            CdeWrapper::decode(
                &v(&["gui", "--", "crucible", "relaunch", "--", "firefox"]),
                "-e",
                "crucible"
            ),
            None
        );
        // Malformed wrapper the grammar rejects.
        assert_eq!(
            CdeWrapper::decode(&v(&["/x/crucible", "relaunch", "--", ""]), "-e", "crucible"),
            None
        );
    }

    #[test]
    fn relaunch_banner_round_trips() {
        let c = RelaunchWrapper::from_argv(
            &v(&[
                "/usr/bin/crucible",
                "relaunch",
                "--app-id",
                "firefox",
                "--",
                "firefox --new-window",
            ]),
            "crucible",
        )
        .unwrap();
        assert_eq!(c.app_id.as_deref(), Some("firefox"));
        assert_eq!(c.cmdline, "firefox --new-window");
        let c = RelaunchWrapper::from_argv(&v(&["crucible", "relaunch", "--", "gimp"]), "crucible")
            .unwrap();
        assert_eq!(c.app_id, None);
        assert_eq!(c.cmdline, "gimp");
    }

    #[test]
    fn relaunch_banner_rejects_lookalikes() {
        for bad in [
            &["/usr/bin/crucible", "resume", "--cwd", "/p", "--", "x"][..],
            &["/usr/bin/notcrucible", "relaunch", "--", "x"],
            &["crucible", "relaunch", "--"],
            &["crucible", "relaunch", "--", ""],
            &["crucible", "relaunch", "--", "x", "y"],
            &["crucible", "relaunch", "--app-id"],
            &["crucible", "relaunch", "--app-id", "a", "x"],
            &["crucible", "relaunch", "--bogus", "--", "x"],
            &["crucible", "relaunch", "x"],
        ] {
            assert_eq!(
                RelaunchWrapper::from_argv(&v(bad), "crucible"),
                None,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn own_name_predicates() {
        let own = "crucible-trixie";
        assert_eq!(
            crucible_subcommand(&v(&["/x/crucible-trixie", "relaunch"]), own),
            Some("relaunch")
        );
        assert_eq!(
            crucible_subcommand(&v(&["/x/crucible", "mem"]), own),
            Some("mem")
        );
        assert_eq!(
            crucible_subcommand(&v(&["/x/crucible.prev", "mem"]), "crucible.prev"),
            Some("mem")
        );
        // A look-alike is not CDE's own.
        assert_eq!(
            crucible_subcommand(&v(&["/x/crucible-foo", "mem"]), "crucible"),
            None
        );
        assert!(!is_cde_process(&v(&["/x/crucible-foo"]), "x", "crucible"));
        assert!(is_cde_process(&v(&["/x/crucible-trixie"]), "x", own));
        // comm fallback: each name truncated to 15 bytes.
        assert!(is_cde_process(&[], "crucible-trixie", own));
        assert!(is_cde_process(&[], "crucible", own));
        // A longer own name (17 bytes) shows as its first 15 bytes.
        let long = "crucible-bookworm";
        assert!(is_cde_process(&[], "crucible-bookwo", long));
        assert!(!is_cde_process(&[], long, long));
        assert!(!is_cde_process(&[], "bash", own));
    }

    #[test]
    fn wrappers_decode_under_a_non_default_own_name() {
        let own = "crucible-trixie";
        let relaunch = v(&[
            "/x/crucible-trixie",
            "relaunch",
            "--app-id",
            "a",
            "--",
            "gimp",
        ]);
        let resume = v(&["/x/crucible-trixie", "resume", "--cwd", "/p"]);
        assert!(RelaunchWrapper::from_argv(&relaunch, own).is_some());
        assert!(RelaunchWrapper::from_argv(&relaunch, "crucible").is_none());
        assert!(ResumeWrapper::from_argv(&resume, own).is_some());
        assert!(matches!(
            CdeWrapper::decode(&relaunch, "-e", own),
            Some(CdeWrapper::App(_))
        ));
        assert!(matches!(
            CdeWrapper::decode(&resume, "-e", own),
            Some(CdeWrapper::Terminal(_))
        ));
        let mut wrapped = v(&["ghostty", "-e"]);
        wrapped.extend(relaunch.clone());
        assert!(matches!(
            CdeWrapper::decode(&wrapped, "-e", own),
            Some(CdeWrapper::App(_))
        ));
        assert_eq!(CdeWrapper::decode(&wrapped, "-e", "crucible"), None);
    }
}
