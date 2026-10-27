//! The one typed contract for CDE's own wrapper argv: `crucible resume …` (a restored or
//! `run-in-terminal` terminal banner) and `crucible relaunch …` (an app placeholder).
//!
//! The compositor encodes with [`ResumeWrapper::to_args`] / [`RelaunchWrapper::to_args`], the
//! `crucible` binary parses its own arguments with [`ResumeWrapper::parse`] /
//! [`RelaunchWrapper::parse`], and capture decodes a window's argv with [`CdeWrapper::decode`]
//! (any parse error is `None` there). One grammar, one place.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// `crucible resume [--now [--exit]] --cwd <dir> [-- <command>]`.
///
/// Grammar: `--cwd <dir>`, `--now`, `--exit` in any order; repeated `--now`/`--exit` are
/// idempotent, a repeated `--cwd` is rejected, `--exit` needs `--now`, `--cwd` is required. An
/// optional `--` is followed by at most one element (the command, one shell string); an empty one
/// means no command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeWrapper {
    /// Directory to change into.
    pub cwd: PathBuf,
    /// The command string, if any (never empty).
    pub command: Option<String>,
    /// `--now`: run the command immediately instead of waiting for Enter.
    pub now: bool,
    /// `--exit`: exit with the command's status instead of starting a shell (needs `--now`).
    pub exit: bool,
}

impl ResumeWrapper {
    /// The arguments after `resume`: `[--now] [--exit] --cwd <dir> [-- <command>]`.
    #[must_use]
    pub fn to_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::new();
        if self.now {
            args.push("--now".into());
        }
        if self.exit {
            args.push("--exit".into());
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
        if exit && !now {
            return Err("--exit requires --now".into());
        }
        Ok(Self {
            cwd: cwd.ok_or("--cwd <dir> is required")?,
            command,
            now,
            exit,
        })
    }

    /// Decodes a whole argv `[<…/crucible>, "resume", …]`; anything else is `None`.
    #[must_use]
    pub fn from_argv(argv: &[String]) -> Option<Self> {
        Self::parse(subcommand_args(argv, "resume")?).ok()
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

    /// Decodes a whole argv `[<…/crucible>, "relaunch", …]`; anything else is `None`.
    #[must_use]
    pub fn from_argv(argv: &[String]) -> Option<Self> {
        Self::parse(subcommand_args(argv, "relaunch")?).ok()
    }
}

/// The args after `sub` when `argv` is `[<…/crucible>, sub, …]`.
fn subcommand_args<'a>(argv: &'a [String], sub: &str) -> Option<&'a [String]> {
    let exe = Path::new(argv.first()?).file_name()?;
    (exe == "crucible" && argv.get(1)? == sub).then(|| &argv[2..])
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
    #[must_use]
    pub fn decode(argv: &[String], exec_flag: &str) -> Option<Self> {
        let start = if is_wrapper_start(argv) {
            0
        } else {
            argv.iter().position(|a| a == exec_flag)? + 1
        };
        let wrapper = argv.get(start..)?;
        if !is_wrapper_start(wrapper) {
            return None;
        }
        match wrapper.get(1).map(String::as_str) {
            Some("relaunch") => RelaunchWrapper::from_argv(wrapper).map(Self::App),
            _ => ResumeWrapper::from_argv(wrapper).map(Self::Terminal),
        }
    }
}

fn is_wrapper_start(argv: &[String]) -> bool {
    argv.first()
        .is_some_and(|a| Path::new(a).file_name().is_some_and(|n| n == "crucible"))
        && matches!(argv.get(1).map(String::as_str), Some("relaunch" | "resume"))
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

    // --- Round trip ---

    #[test]
    fn round_trip_over_a_grid() {
        let cwds = ["/p", "/with space/dir", "/ünïcødé/路径", "relative"];
        let commands = [
            None,
            Some("htop"),
            Some("cargo test -- --nocapture"),
            Some("echo 'a b' \"c\" it's"),
            Some("--help"),
        ];
        let flags = [(false, false), (true, false), (true, true)];
        for cwd in cwds {
            for command in commands {
                for (now, exit) in flags {
                    let w = ResumeWrapper {
                        cwd: cwd.into(),
                        command: command.map(String::from),
                        now,
                        exit,
                    };
                    assert_eq!(ResumeWrapper::parse(&strings(w.to_args())), Ok(w.clone()));
                }
            }
        }
        for app_id in [None, Some("org.gimp.GIMP"), Some("with space")] {
            for cmdline in ["gimp", "gimp 'a b.png'", "--help", "é \"q\""] {
                let w = RelaunchWrapper {
                    app_id: app_id.map(String::from),
                    cmdline: cmdline.into(),
                };
                assert_eq!(RelaunchWrapper::parse(&strings(w.to_args())), Ok(w.clone()));
            }
        }
    }

    // --- Grammar points ---

    #[test]
    fn resume_flags_in_any_order() {
        let a = ResumeWrapper::parse(&v(&["--cwd", "/p", "--exit", "--now", "--", "x"])).unwrap();
        assert!(a.now && a.exit);
        assert_eq!(a.command.as_deref(), Some("x"));
    }

    #[test]
    fn resume_repeated_now_and_exit_are_idempotent() {
        let a = ResumeWrapper::parse(&v(&["--now", "--now", "--exit", "--exit", "--cwd", "/p"]))
            .unwrap();
        assert!(a.now && a.exit);
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
        assert!(ResumeWrapper::from_argv(&v(&["/bin/crucible", "pick"])).is_none());
        assert!(ResumeWrapper::from_argv(&v(&["/bin/other", "resume", "--cwd", "/p"])).is_none());
        assert!(
            ResumeWrapper::from_argv(&v(&["/bin/crucible", "resume", "--cwd", "/p", "x"]))
                .is_none()
        );
        assert!(
            ResumeWrapper::from_argv(&v(&[
                "/bin/crucible",
                "resume",
                "--cwd",
                "/p",
                "--",
                "a",
                "b"
            ]))
            .is_none()
        );
        assert!(ResumeWrapper::from_argv(&[]).is_none());
        let b = ResumeWrapper::from_argv(&v(&["/bin/crucible", "resume", "--cwd", "/p", "--"]))
            .unwrap();
        assert_eq!(b.command, None);
    }

    #[test]
    fn banner_parser_accepts_the_now_form() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let plain = ResumeWrapper::from_argv(&v(&[
            "/bin/crucible",
            "resume",
            "--cwd",
            "/p",
            "--",
            "htop",
        ]))
        .unwrap();
        let now = ResumeWrapper::from_argv(&v(&[
            "/bin/crucible",
            "resume",
            "--now",
            "--cwd",
            "/p",
            "--",
            "htop",
        ]))
        .unwrap();
        // The typed wrapper carries `now`; the decoded record (cwd, command) is the same.
        assert_eq!((&plain.cwd, &plain.command), (&now.cwd, &now.command));
        assert!(now.now && !plain.now);
        assert_eq!(now.command.as_deref(), Some("htop"));
        assert!(ResumeWrapper::from_argv(&v(&["/bin/crucible", "resume", "--now"])).is_none());
        // Shared grammar: repeated `--now` is idempotent, so this now decodes (was `None`).
        assert!(
            ResumeWrapper::from_argv(&v(&[
                "/bin/crucible",
                "resume",
                "--now",
                "--now",
                "--cwd",
                "/p"
            ]))
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
            let b = ResumeWrapper::from_argv(&v(&a)).unwrap();
            assert_eq!(b.cwd, PathBuf::from("/p"));
            assert_eq!(b.command.as_deref(), Some("htop"));
        }
        // Still `None`, now because `--exit` without `--now` is rejected (repeats are idempotent).
        let twice = ["/bin/crucible", "resume", "--exit", "--exit", "--cwd", "/p"];
        assert!(ResumeWrapper::from_argv(&v(&twice)).is_none());
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
            CdeWrapper::decode(&wrapped, "-e"),
            app(Some("a"), "firefox")
        );
        assert_eq!(
            CdeWrapper::decode(&v(&["/x/crucible", "relaunch", "--", "x"]), "-e"),
            app(None, "x")
        );
        for flags in [&[][..], &["--now", "--exit"][..]] {
            let mut a = vec!["ghostty", "-e", "/x/crucible", "resume"];
            a.extend(flags);
            a.extend(["--cwd", "/p", "--", "htop"]);
            assert_eq!(
                as_term(CdeWrapper::decode(&v(&a), "-e")),
                term(Some("htop"))
            );
        }
        assert_eq!(
            CdeWrapper::decode(&v(&["firefox", "--new-window"]), "-e"),
            None
        );
        assert_eq!(
            CdeWrapper::decode(&v(&["ghostty", "-e", "/x/crucible", "pick"]), "-e"),
            None
        );
        assert_eq!(
            CdeWrapper::decode(
                &v(&["gui", "--", "crucible", "relaunch", "--", "firefox"]),
                "-e"
            ),
            None
        );
        // Malformed wrapper the grammar rejects.
        assert_eq!(
            CdeWrapper::decode(&v(&["/x/crucible", "relaunch", "--", ""]), "-e"),
            None
        );
    }

    #[test]
    fn relaunch_banner_round_trips() {
        let c = RelaunchWrapper::from_argv(&v(&[
            "/usr/bin/crucible",
            "relaunch",
            "--app-id",
            "firefox",
            "--",
            "firefox --new-window",
        ]))
        .unwrap();
        assert_eq!(c.app_id.as_deref(), Some("firefox"));
        assert_eq!(c.cmdline, "firefox --new-window");
        let c = RelaunchWrapper::from_argv(&v(&["crucible", "relaunch", "--", "gimp"])).unwrap();
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
            assert_eq!(RelaunchWrapper::from_argv(&v(bad)), None, "{bad:?}");
        }
    }
}
