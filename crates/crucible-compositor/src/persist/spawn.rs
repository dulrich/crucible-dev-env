//! Process spawning: the PID-provenance registry ([`SpawnKind`]/[`SpawnRecord`]), child env and
//! `PATH`, command preflight, the terminal/overlay argv builders, child reaping and
//! `CrucibleState::spawn_process`.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Instant, SystemTime};

use crucible_layout::WorkspaceId;
use crucible_persist::{RelaunchWrapper, ResumeWrapper};
use tracing::{error, info, warn};

use crate::config::pick_terminal;
use crate::ipc::{
    LAUNCHER_TUI_ENV, OVERLAY_STYLE_ENV, SOCKET_ENV, SPAWN_EPOCH_ENV, TERMINAL_ENV,
    TERMINAL_EXEC_FLAG_ENV, encode_field,
};
use crate::state::{Backend, CrucibleState};

/// Spawned-process registry and child handles (a field of [`super::PersistState`]).
#[derive(Debug, Default)]
pub(super) struct SpawnState {
    pub(super) spawned: HashMap<u32, SpawnRecord>,
    /// Handles of every CDE-spawned child, kept so exited ones are reaped (never reap-any).
    pub(super) children: Vec<Child>,
}

/// What a CDE-spawned process is, remembered per PID so a mapped window can be classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnKind {
    /// A terminal (captured with its cwd and foreground command).
    Terminal,
    /// An application launched by `run` (recorded as an `app`).
    App,
    /// A CDE overlay such as the picker (never captured).
    Overlay,
}

/// A CDE-spawned process: what it is, what it ran and when (for the spawn-to-map latency line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRecord {
    /// What the process is.
    pub kind: SpawnKind,
    /// The program it was spawned as.
    pub program: String,
    /// Monotonic spawn time.
    pub at: Instant,
    /// Wall-clock spawn time, ms since the UNIX epoch.
    pub epoch_ms: u128,
}

impl SpawnRecord {
    /// A record of `program` spawned as `kind` right now.
    #[must_use]
    pub fn now(kind: SpawnKind, program: &str) -> Self {
        Self {
            kind,
            program: program.to_owned(),
            at: Instant::now(),
            epoch_ms: epoch_ms_now(),
        }
    }

    /// Whole milliseconds from the spawn to `now` (0 if `now` is earlier).
    #[must_use]
    pub fn ms_until(&self, now: Instant) -> u128 {
        now.saturating_duration_since(self.at).as_millis()
    }
}

/// Wall-clock milliseconds since the UNIX epoch (0 if the clock is before it).
pub(super) fn epoch_ms_now() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// Reaps exited children through their own handles (`try_wait`), dropping them from `children`
/// and returning their PIDs. Children still running (or never reaped by anyone else) are kept; a
/// `try_wait` error drops the child too. Never a reap-any call: XWayland's children are not ours.
pub fn reap_exited(children: &mut Vec<Child>) -> Vec<u32> {
    let mut reaped = Vec::new();
    children.retain_mut(|child| match child.try_wait() {
        Ok(None) => true,
        Ok(Some(_)) | Err(_) => {
            reaped.push(child.id());
            false
        }
    });
    reaped
}

/// Kills the held child `pid` (SIGKILL via its own handle; the next [`reap_exited`] collects it).
/// Returns `false` when CDE holds no live handle for `pid`.
pub fn kill_child(children: &mut [Child], pid: u32) -> bool {
    children
        .iter_mut()
        .find(|c| c.id() == pid)
        .is_some_and(|c| c.kill().is_ok())
}

/// Whether `pid` itself was spawned as a [`SpawnKind::Terminal`] (exact pid; ancestors never count, so an app launched from a terminal is not a terminal).
#[must_use]
pub fn is_terminal_spawn(pid: u32, spawned: &HashMap<u32, SpawnRecord>) -> bool {
    spawned
        .get(&pid)
        .is_some_and(|rec| rec.kind == SpawnKind::Terminal)
}

/// The terminal arguments restoring a terminal: `<exec-flag> <crucible> resume --cwd <dir> --
/// <command>` (the command is ONE element), or none for an idle shell.
#[must_use]
pub fn restore_args(
    exec_flag: &str,
    crucible: &Path,
    cwd: &Path,
    command: Option<&str>,
) -> Vec<OsString> {
    let Some(command) = command else {
        return Vec::new();
    };
    let wrapper = ResumeWrapper {
        cwd: cwd.to_path_buf(),
        command: Some(command.to_string()),
        now: false,
        exit: false,
    };
    wrapper_args(exec_flag, crucible, "resume", wrapper.to_args())
}

/// `<exec-flag> <crucible> <sub> <args…>`: a CDE wrapper run by the terminal.
fn wrapper_args(exec_flag: &str, crucible: &Path, sub: &str, args: Vec<OsString>) -> Vec<OsString> {
    let mut out: Vec<OsString> = vec![exec_flag.into(), crucible.into(), sub.into()];
    out.extend(args);
    out
}

/// The terminal arguments for `run-in-terminal` / `run-tui`: `<exec-flag> <crucible> resume --now
/// [--exit] --cwd <dir> -- <command>` (the command is ONE element); `exit` (`run-tui`) closes the
/// terminal when the command exits.
#[must_use]
pub fn run_in_terminal_args(
    exec_flag: &str,
    crucible: &Path,
    cwd: &Path,
    command: &str,
    exit: bool,
) -> Vec<OsString> {
    let wrapper = ResumeWrapper {
        cwd: cwd.to_path_buf(),
        command: Some(command.to_string()),
        now: true,
        exit,
    };
    wrapper_args(exec_flag, crucible, "resume", wrapper.to_args())
}

/// A spawned child's `PATH`: `exe_dir` prepended to the inherited `PATH`, unless it is already an
/// entry. An unset inherited `PATH` yields just `exe_dir`.
#[must_use]
pub fn child_path(exe_dir: &Path, inherited: Option<&std::ffi::OsStr>) -> OsString {
    let Some(inherited) = inherited else {
        return exe_dir.as_os_str().to_os_string();
    };
    if std::env::split_paths(inherited).any(|p| p == exe_dir) {
        return inherited.to_os_string();
    }
    let mut entries = vec![exe_dir.to_path_buf()];
    entries.extend(std::env::split_paths(inherited));
    std::env::join_paths(entries).unwrap_or_else(|_| inherited.to_os_string())
}

/// POSIX sh special/regular builtins and reserved words the pre-flight lets through.
pub(super) const SH_BUILTINS: &[&str] = &[
    ".", ":", "break", "cd", "command", "continue", "eval", "exec", "exit", "export", "getopts",
    "read", "readonly", "return", "set", "shift", "times", "trap", "type", "ulimit", "umask",
    "unset", "wait", "alias", "bg", "fg", "jobs", "kill", "printf", "echo", "test", "true",
    "false", "if", "case", "for", "while", "until",
];

pub(super) fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Pre-flight for `run`/`run-in-terminal`: does the command's first shell word resolve? Passes
/// when the word holds shell syntax the check cannot judge, is a sh builtin/keyword, is a path
/// (relative to `cwd`) to an existing file, or is an executable file on `path` (relative entries
/// and empty entries resolve against `cwd`). Never spawns or waits.
///
/// # Errors
/// The unresolved first word.
pub fn preflight_command(
    cmd: &str,
    cwd: &Path,
    path: Option<&std::ffi::OsStr>,
) -> Result<(), String> {
    let Some(word) = cmd.split_whitespace().next() else {
        return Ok(());
    };
    let syntax = |c: char| "='\"$`\\~*?[()<>|;&".contains(c);
    if word.contains(syntax) || word.starts_with(['{', '!']) || SH_BUILTINS.contains(&word) {
        return Ok(());
    }
    let resolve = |p: &Path| {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        }
    };
    if word.contains('/') {
        return if resolve(Path::new(word)).is_file() {
            Ok(())
        } else {
            Err(word.to_string())
        };
    }
    let found = path.is_some_and(|path| {
        std::env::split_paths(path).any(|dir| {
            let dir = if dir.as_os_str().is_empty() {
                cwd.to_path_buf()
            } else {
                resolve(&dir)
            };
            is_executable_file(&dir.join(word))
        })
    });
    if found { Ok(()) } else { Err(word.to_string()) }
}

/// The terminal arguments for an app relaunch placeholder: `<exec-flag> <crucible> relaunch
/// [--app-id <id>] -- <cmdline>` (the cmdline is ONE element).
#[must_use]
pub fn relaunch_args(
    exec_flag: &str,
    crucible: &Path,
    app_id: Option<&str>,
    cmdline: &str,
) -> Vec<OsString> {
    let wrapper = RelaunchWrapper {
        app_id: app_id.map(str::to_string),
        cmdline: cmdline.to_string(),
    };
    wrapper_args(exec_flag, crucible, "relaunch", wrapper.to_args())
}

/// The terminal arguments for the picker: `<exec-flag> <crucible> pick [--rename <encoded>]`.
#[must_use]
pub fn picker_args(exec_flag: &str, crucible: &Path, rename: Option<&str>) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![exec_flag.into(), crucible.into(), "pick".into()];
    if let Some(current) = rename {
        args.push("--rename".into());
        args.push(encode_field(current).into());
    }
    args
}

/// The terminal arguments for the launcher: `<exec-flag> <crucible> launch`.
#[must_use]
pub fn launcher_args(exec_flag: &str, crucible: &Path) -> Vec<OsString> {
    vec![exec_flag.into(), crucible.into(), "launch".into()]
}

/// The terminal arguments for the settings overlay: `<exec-flag> <crucible> keys`.
#[must_use]
pub fn keys_args(exec_flag: &str, crucible: &Path) -> Vec<OsString> {
    vec![exec_flag.into(), crucible.into(), "keys".into()]
}

/// Prepends the configured overlay `terminal-args` to the overlay terminal arguments, i.e. before
/// the exec flag: `[terminal-args…, exec-flag, crucible, sub…]`.
#[must_use]
pub fn with_terminal_args(terminal_args: &[String], args: Vec<OsString>) -> Vec<OsString> {
    terminal_args
        .iter()
        .map(OsString::from)
        .chain(args)
        .collect()
}

impl<B: Backend + 'static> CrucibleState<B> {
    // ---- spawning ------------------------------------------------------------------------------

    /// The terminal program: the configured one, else `$TERMINAL`/the keys crate's fallback.
    #[must_use]
    pub fn terminal_program(&self) -> String {
        pick_terminal(
            self.config.terminal.command.as_deref(),
            crucible_keys::terminal_command(),
        )
    }

    /// Spawns `program args…` connected to this compositor (`WAYLAND_DISPLAY`, `CDE_SOCKET`),
    /// optionally in `cwd`. The PID is remembered as a CDE-spawn of `kind`. Returns the PID.
    pub fn spawn_process(
        &mut self,
        program: &str,
        args: &[OsString],
        cwd: Option<&Path>,
        kind: SpawnKind,
    ) -> Option<u32> {
        let Some(socket) = self.socket_name.clone() else {
            warn!(program, "no wayland socket; cannot spawn");
            return None;
        };
        let mut cmd = Command::new(program);
        cmd.args(args).env("WAYLAND_DISPLAY", &socket);
        if let Some(path) = &self.persist.ipc_path {
            cmd.env(SOCKET_ENV, path);
        }
        cmd.env(TERMINAL_ENV, self.terminal_program())
            .env(TERMINAL_EXEC_FLAG_ENV, &self.config.terminal.exec_flag);
        if kind == SpawnKind::Overlay {
            cmd.env(OVERLAY_STYLE_ENV, self.config.overlay.style.as_str());
            cmd.env(LAUNCHER_TUI_ENV, self.config.launcher_tui.join(":"));
        }
        let record = SpawnRecord::now(kind, program);
        if kind == SpawnKind::Overlay {
            cmd.env(SPAWN_EPOCH_ENV, record.epoch_ms.to_string());
        }
        if let Some(path) = Self::spawn_path() {
            cmd.env("PATH", path);
        }
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id();
                info!(program, ?args, ?cwd, pid, "spawned");
                self.persist.spawn.spawned.insert(pid, record);
                self.persist.spawn.children.push(child);
                Some(pid)
            }
            Err(err) => {
                error!(program, %err, "spawn failed");
                None
            }
        }
    }

    /// Prepends the substituted `tiling.terminal-args` (before the exec flag) to the arguments of a
    /// CDE-spawned **terminal**. Overlays use `overlay.terminal-args` only; apps get none.
    pub(super) fn with_tiling_args(&self, args: Vec<OsString>) -> Vec<OsString> {
        with_terminal_args(&self.config.tiling.substituted_terminal_args(), args)
    }

    /// The `PATH` every spawned child gets: this binary's directory (so `crucible` resolves) in
    /// front of the compositor's own `PATH`. `None` when the executable path is unknown.
    pub(super) fn spawn_path() -> Option<OsString> {
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?;
        Some(child_path(dir, std::env::var_os("PATH").as_deref()))
    }

    /// Pre-flights `cmd` for workspace `ws`: its first word must resolve against the child's
    /// `PATH`, relative to the spawn's effective cwd (the workspace root, else the compositor's).
    pub(super) fn preflight(&self, cmd: &str, ws: WorkspaceId) -> Result<(), String> {
        let cwd = self
            .persist
            .workspace
            .roots
            .get(&ws)
            .cloned()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/"));
        preflight_command(cmd, &cwd, Self::spawn_path().as_deref())
    }

    /// `run`: pre-flights, then [`Self::spawn_command_here`]. `Err` is the IPC reason.
    ///
    /// # Errors
    /// `unknown command <word>` when the pre-flight fails, `spawn failed` when the spawn does.
    pub fn run_checked(&mut self, cmd: &str) -> Result<u32, String> {
        let ws = self.layout.world.active_workspace_id();
        self.preflight(cmd, ws)
            .map_err(|word| format!("unknown command {word}"))?;
        self.spawn_command_in(cmd, ws)
            .ok_or_else(|| "spawn failed".to_string())
    }

    /// `run-in-terminal`: pre-flights, then spawns the configured terminal running
    /// `crucible resume --now --cwd <workspace root or $HOME> -- <cmd>`.
    ///
    /// # Errors
    /// As [`Self::run_checked`].
    pub fn run_in_terminal_checked(&mut self, cmd: &str) -> Result<u32, String> {
        self.terminal_command_checked(cmd, "run-in-terminal", false)
    }

    /// `run-tui`: as [`Self::run_in_terminal_checked`], but `resume --exit` closes the terminal
    /// when the program exits.
    ///
    /// # Errors
    /// As [`Self::run_checked`].
    pub fn run_tui_checked(&mut self, cmd: &str) -> Result<u32, String> {
        self.terminal_command_checked(cmd, "run-tui", true)
    }

    pub(super) fn terminal_command_checked(
        &mut self,
        cmd: &str,
        label: &str,
        exit: bool,
    ) -> Result<u32, String> {
        let ws = self.layout.world.active_workspace_id();
        self.preflight(cmd, ws)
            .map_err(|word| format!("unknown command {word}"))?;
        let crucible = Self::crucible_exe(label).ok_or("spawn failed")?;
        let dir = self
            .persist
            .workspace
            .roots
            .get(&ws)
            .cloned()
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("/"));
        let args = self.with_tiling_args(run_in_terminal_args(
            &self.config.terminal.exec_flag,
            &crucible,
            &dir,
            cmd,
            exit,
        ));
        let program = self.terminal_program();
        self.spawn_process(&program, &args, Some(&dir), SpawnKind::Terminal)
            .ok_or_else(|| "spawn failed".to_string())
    }

    /// Spawns a terminal into the focused pane, starting in the active workspace's root if any.
    pub fn spawn_terminal_here(&mut self, program: &str) {
        let ws = self.layout.world.active_workspace_id();
        let root = self.persist.workspace.roots.get(&ws).cloned();
        let args = self.with_tiling_args(Vec::new());
        self.spawn_process(program, &args, root.as_deref(), SpawnKind::Terminal);
    }

    /// Runs `cmd` (a shell command line) into the focused pane via `/bin/sh -c 'exec <cmd>'`, so the
    /// window's client PID is the app itself. Starts in the active workspace's root if any.
    /// Returns the PID.
    pub fn spawn_command_here(&mut self, cmd: &str) -> Option<u32> {
        let ws = self.layout.world.active_workspace_id();
        self.spawn_command_in(cmd, ws)
    }

    /// Runs `cmd` via `/bin/sh -c 'exec <cmd>'` (kind `App`) in workspace `ws`'s root, if any.
    pub(super) fn spawn_command_in(&mut self, cmd: &str, ws: WorkspaceId) -> Option<u32> {
        let root = self.persist.workspace.roots.get(&ws).cloned();
        let args = [OsString::from("-c"), OsString::from(format!("exec {cmd}"))];
        self.spawn_process("/bin/sh", &args, root.as_deref(), SpawnKind::App)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn kill_child_ends_only_a_held_child() {
        let child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let mut children = vec![child];
        assert!(!kill_child(&mut children, pid.wrapping_add(1_000_000)));
        assert!(kill_child(&mut children, pid));
        let status = children[0].wait().unwrap();
        assert!(!status.success(), "killed, not exited cleanly");
    }

    #[test]
    fn reap_exited_drops_only_exited_children() {
        let quick = std::process::Command::new("/bin/true").spawn().unwrap();
        let quick_pid = quick.id();
        let slow = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let slow_pid = slow.id();
        let mut children = vec![quick, slow];
        let mut reaped = Vec::new();
        for _ in 0..40 {
            reaped = reap_exited(&mut children);
            if !reaped.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(reaped, vec![quick_pid]);
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].id(), slow_pid);
        for c in &mut children {
            c.kill().unwrap();
            c.wait().unwrap();
        }
    }

    #[test]
    fn spawn_record_measures_elapsed_ms_and_saturates() {
        let rec = SpawnRecord::now(SpawnKind::App, "x");
        assert_eq!(rec.ms_until(rec.at + Duration::from_millis(250)), 250);
        assert_eq!(rec.ms_until(rec.at), 0);
        assert!(rec.epoch_ms > 0);
    }

    #[test]
    fn is_terminal_spawn_is_the_exact_pid_kind_only() {
        let spawned: HashMap<u32, SpawnRecord> = [
            (10, SpawnRecord::now(SpawnKind::Terminal, "t")),
            (20, SpawnRecord::now(SpawnKind::App, "a")),
            (30, SpawnRecord::now(SpawnKind::Overlay, "o")),
        ]
        .into();
        assert!(is_terminal_spawn(10, &spawned));
        assert!(!is_terminal_spawn(20, &spawned));
        assert!(!is_terminal_spawn(30, &spawned));
        assert!(!is_terminal_spawn(99, &spawned));
        // a child of a terminal spawn (firefox from ghostty) is not a terminal; the
        // pid 5 child of App 20 is not one either — ancestors never decide the class
        assert!(!is_terminal_spawn(5, &spawned));
    }

    #[test]
    fn relaunch_argv_keeps_the_cmdline_as_one_element() {
        let c = Path::new("/bin/crucible");
        let strs = |a: Vec<OsString>| -> Vec<String> {
            a.iter().map(|s| s.to_str().unwrap().to_string()).collect()
        };
        assert_eq!(
            strs(relaunch_args("-e", c, Some("gimp"), "gimp 'a b.png'")),
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
            strs(relaunch_args("-e", c, None, "xterm")),
            ["-e", "/bin/crucible", "relaunch", "--", "xterm"]
        );
        // The placeholder argv (minus the exec flag) decodes back to the record.
        let argv = strs(relaunch_args("-e", c, Some("x"), "x --y"));
        let cap = crucible_persist::RelaunchWrapper::from_argv(&argv[1..]).unwrap();
        assert_eq!(cap.app_id.as_deref(), Some("x"));
        assert_eq!(cap.cmdline, "x --y");
    }

    #[test]
    fn restore_argv_keeps_the_command_as_one_element() {
        let args = restore_args(
            "-e",
            Path::new("/bin/crucible"),
            Path::new("/w d"),
            Some("cargo test 'a b'"),
        );
        let args: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
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
        assert!(restore_args("-e", Path::new("/c"), Path::new("/"), None).is_empty());
    }

    #[test]
    fn launcher_argv_runs_crucible_launch() {
        let args = launcher_args("-e", Path::new("/c"));
        assert_eq!(
            args,
            vec![OsString::from("-e"), "/c".into(), "launch".into()]
        );
    }

    #[test]
    fn keys_argv_runs_the_keys_subcommand() {
        let args = keys_args("-e", Path::new("/c"));
        assert_eq!(args, vec![OsString::from("-e"), "/c".into(), "keys".into()]);
    }

    #[test]
    fn picker_argv_encodes_the_rename_target() {
        let args = picker_args("-e", Path::new("/c"), Some("a:b"));
        let args: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(args, ["-e", "/c", "pick", "--rename", "a%3Ab"]);
        assert_eq!(picker_args("-e", Path::new("/c"), None).len(), 3);
    }

    #[test]
    fn terminal_args_go_before_the_exec_flag() {
        let base = picker_args("-e", Path::new("/c"), None);
        let got = with_terminal_args(&["--a=1".to_string(), "--b".to_string()], base.clone());
        let got: Vec<&str> = got.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(got, ["--a=1", "--b", "-e", "/c", "pick"]);
        assert_eq!(with_terminal_args(&[], base.clone()), base);
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cde-pre-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn touch(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn child_path_prepends_once() {
        let dir = Path::new("/opt/cde");
        assert_eq!(child_path(dir, None), OsString::from("/opt/cde"));
        assert_eq!(
            child_path(dir, Some("/usr/bin:/bin".as_ref())),
            OsString::from("/opt/cde:/usr/bin:/bin")
        );
        assert_eq!(
            child_path(dir, Some("/usr/bin:/opt/cde".as_ref())),
            OsString::from("/usr/bin:/opt/cde")
        );
    }

    #[test]
    fn run_in_terminal_args_keep_the_command_one_element() {
        let args = run_in_terminal_args(
            "-e",
            Path::new("/x/crucible"),
            Path::new("/w"),
            "a b; c",
            false,
        );
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
        let tui = run_in_terminal_args(
            "-e",
            Path::new("/x/crucible"),
            Path::new("/w"),
            "a b; c",
            true,
        );
        assert_eq!(
            tui[3..5],
            [OsString::from("--now"), OsString::from("--exit")]
        );
        assert_eq!(tui.len(), want.len() + 1);
    }

    #[test]
    fn preflight_finds_path_commands_and_names_misses() {
        let bin = scratch("bin");
        touch(&bin.join("tool"), 0o755);
        touch(&bin.join("data"), 0o644);
        let path = bin.clone().into_os_string();
        let cwd = Path::new("/");
        assert_eq!(preflight_command("tool --x", cwd, Some(&path)), Ok(()));
        assert_eq!(
            preflight_command("nosuch --x", cwd, Some(&path)),
            Err("nosuch".to_string())
        );
        assert_eq!(
            preflight_command("data", cwd, Some(&path)),
            Err("data".to_string())
        );
        assert_eq!(
            preflight_command("tool", cwd, None),
            Err("tool".to_string())
        );
        let _ = std::fs::remove_dir_all(&bin);
    }

    #[test]
    fn preflight_resolves_paths_against_the_cwd() {
        let cwd = scratch("cwd");
        touch(&cwd.join("x"), 0o755);
        let abs = cwd.join("x");
        assert_eq!(
            preflight_command(abs.to_str().unwrap(), Path::new("/"), None),
            Ok(())
        );
        assert_eq!(preflight_command("./x", &cwd, None), Ok(()));
        assert_eq!(preflight_command("./y", &cwd, None), Err("./y".to_string()));
        // A relative PATH entry and an empty one both resolve against the cwd.
        assert_eq!(preflight_command("x", &cwd, Some(".".as_ref())), Ok(()));
        assert_eq!(preflight_command("x", &cwd, Some("".as_ref())), Ok(()));
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn preflight_passes_what_it_cannot_judge() {
        let cwd = Path::new("/");
        for cmd in [
            "'quoted prog' a",
            "\"q\" a",
            "FOO=1 nosuch",
            "$EDITOR f",
            "~/bin/x",
            "(cd /; nosuch)",
            "cd /tmp && nosuch",
            "{ nosuch; }",
            "! nosuch",
            "echo hi",
            "exec nosuch",
            "if true; then :; fi",
            "",
        ] {
            assert_eq!(preflight_command(cmd, cwd, None), Ok(()), "{cmd:?}");
        }
    }
}
