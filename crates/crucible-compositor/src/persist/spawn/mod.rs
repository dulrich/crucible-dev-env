//! Process spawning on `CrucibleState`: the one real spawn ([`CrucibleState::spawn`] on a
//! [`SpawnRequest`]), the child session env and `PATH`, command preflight, and the user-facing
//! spawn verbs (`run`, `run-in-terminal`, `run-tui`, `run-term`, a terminal here).
//!
//! What lives elsewhere: the PID-provenance registry ([`SpawnKind`]/[`SpawnRecord`]) is in
//! [`registry`]; the terminal argv contract is `crucible_persist::Invocation`; program resolution
//! (`child_path`, `preflight_command`, `resolve_program`) is `crucible_persist::program`; restore
//! planning (`plan_spawns`, `prepare_restore`) is `crucible_persist::restore`.

mod registry;

pub use registry::{SpawnKind, SpawnRecord};
pub(super) use registry::{SpawnRegistry, epoch_ms_now};

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

use crucible_layout::WorkspaceId;
use crucible_persist::{
    Invocation, ResumeMode, ResumeWrapper, WindowRecord, child_path, preflight_command,
    resolve_program, same_program,
};
use tracing::{error, info, warn};

use crate::config::pick_terminal;
use crate::ipc::{
    LAUNCHER_TUI_ENV, OVERLAY_STYLE_ENV, SOCKET_ENV, SPAWN_EPOCH_ENV, TERMINAL_ENV,
    TERMINAL_EXEC_FLAG_ENV,
};
use crate::state::{Backend, CrucibleState};

/// Session identity every spawned child gets, overriding inherited values (the compositor's own
/// environment is left untouched).
pub(crate) fn session_env() -> [(&'static str, &'static str); 2] {
    [
        ("XDG_CURRENT_DESKTOP", "CDE"),
        ("XDG_SESSION_TYPE", "wayland"),
    ]
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

/// One spawn: `program args…`, optionally in `cwd`, remembered as a CDE spawn of `kind` with
/// `origin` (the saved record it stands for, if any).
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    /// The program to run.
    pub program: String,
    /// Its arguments.
    pub args: Vec<OsString>,
    /// The working directory (`None`: the compositor's).
    pub cwd: Option<PathBuf>,
    /// What the spawn is.
    pub kind: SpawnKind,
    /// The saved record the spawn stands for (a restored window or a relaunched app).
    pub origin: Option<WindowRecord>,
}

impl SpawnRequest {
    /// `program args…` as `kind`, in the compositor's cwd, with no origin.
    #[must_use]
    pub fn new(program: impl Into<String>, args: Vec<OsString>, kind: SpawnKind) -> Self {
        Self {
            program: program.into(),
            args,
            cwd: None,
            kind,
            origin: None,
        }
    }

    /// `cmd` (a shell command line) via `/bin/sh -c 'exec <cmd>'` (kind `App`), so the window's
    /// client PID is the app itself.
    #[must_use]
    pub fn shell_app(cmd: &str) -> Self {
        let args = vec![OsString::from("-c"), OsString::from(format!("exec {cmd}"))];
        Self::new("/bin/sh", args, SpawnKind::App)
    }

    /// The same request, run in `cwd`.
    #[must_use]
    pub fn in_dir(mut self, cwd: Option<PathBuf>) -> Self {
        self.cwd = cwd;
        self
    }

    /// The same request, remembering `origin`.
    #[must_use]
    pub fn with_origin(mut self, origin: Option<WindowRecord>) -> Self {
        self.origin = origin;
        self
    }
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

    /// Spawns `req` connected to this compositor (`WAYLAND_DISPLAY`, `CDE_SOCKET`), optionally in
    /// its cwd. The PID is remembered as a CDE-spawn of its kind with its origin. Returns the PID.
    pub fn spawn(&mut self, req: SpawnRequest) -> Option<u32> {
        let SpawnRequest {
            program,
            args,
            cwd,
            kind,
            origin,
        } = req;
        let program = program.as_str();
        let cwd = cwd.as_deref();
        let Some(socket) = self.socket_name.clone() else {
            warn!(program, "no wayland socket; cannot spawn");
            return None;
        };
        let mut cmd = Command::new(program);
        cmd.args(&args)
            .envs(session_env())
            .env("WAYLAND_DISPLAY", &socket);
        if let Some(path) = &self.persist.ipc_path {
            cmd.env(SOCKET_ENV, path);
        }
        cmd.env(TERMINAL_ENV, self.terminal_program())
            .env(TERMINAL_EXEC_FLAG_ENV, &self.config.terminal.exec_flag);
        if kind == SpawnKind::Overlay {
            cmd.env(OVERLAY_STYLE_ENV, self.config.overlay.style.as_str());
            cmd.env(LAUNCHER_TUI_ENV, self.config.launcher.tui.join(":"));
        }
        let record = SpawnRecord {
            origin,
            ..SpawnRecord::now(kind, program)
        };
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
                self.persist.spawn.insert(child, record);
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
        let exe = Self::crucible_exe("child PATH")?;
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

    /// `run`: pre-flights, then runs `cmd` via `/bin/sh -c 'exec <cmd>'` ([`SpawnRequest::shell_app`])
    /// in the active workspace's root, if any. `Err` is the IPC reason.
    ///
    /// # Errors
    /// `unknown command <word>` when the pre-flight fails, `spawn failed` when the spawn does.
    pub fn run_checked(&mut self, cmd: &str) -> Result<u32, String> {
        let ws = self.layout.world.active_workspace_id();
        self.preflight(cmd, ws)
            .map_err(|word| format!("unknown command {word}"))?;
        let root = self.persist.workspace.roots.get(&ws).cloned();
        self.spawn(SpawnRequest::shell_app(cmd).in_dir(root))
            .ok_or_else(|| "spawn failed".to_string())
    }

    /// `run-in-terminal`: pre-flights, then spawns the configured terminal running
    /// `crucible resume --now --cwd <workspace root or $HOME> -- <cmd>`.
    ///
    /// # Errors
    /// As [`Self::run_checked`].
    pub fn run_in_terminal_checked(&mut self, cmd: &str) -> Result<u32, String> {
        self.terminal_command_checked(cmd, "run-in-terminal", ResumeMode::Run)
    }

    /// `run-tui`: as [`Self::run_in_terminal_checked`], but `resume --exit` closes the terminal
    /// when the program exits (unless it exits abnormally: `resume` then shows the crash banner first).
    ///
    /// # Errors
    /// As [`Self::run_checked`].
    pub fn run_tui_checked(&mut self, cmd: &str) -> Result<u32, String> {
        self.terminal_command_checked(cmd, "run-tui", ResumeMode::RunThenExit)
    }

    /// The terminal running `<exec-flag> <crucible> resume --now [--exit] --cwd <dir> -- <cmd>`
    /// (the command is ONE element) in the active workspace's root, else `$HOME`, else `/`.
    pub(super) fn terminal_command_checked(
        &mut self,
        cmd: &str,
        label: &str,
        mode: ResumeMode,
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
        let invocation = Invocation::Resume(ResumeWrapper {
            cwd: dir.clone(),
            command: Some(cmd.to_string()),
            mode,
        });
        let args = self
            .with_tiling_args(invocation.terminal_argv(&self.config.terminal.exec_flag, &crucible));
        let program = self.terminal_program();
        self.spawn(SpawnRequest::new(program, args, SpawnKind::Terminal).in_dir(Some(dir)))
            .ok_or_else(|| "spawn failed".to_string())
    }

    /// Spawns a terminal into the focused pane, starting in the active workspace's root if any.
    pub fn spawn_terminal_here(&mut self, program: &str) {
        let ws = self.layout.world.active_workspace_id();
        let root = self.persist.workspace.roots.get(&ws).cloned();
        let args = self.with_tiling_args(Vec::new());
        self.spawn(SpawnRequest::new(program, args, SpawnKind::Terminal).in_dir(root));
    }

    /// `run-term`: spawns the emulator `program` as a CDE terminal ([`SpawnKind::Terminal`]) in the
    /// active workspace's root. Invariants: `program` is a bare program with NO desktop arguments
    /// (single-instance forwarding would hand the window to another process and defeat the
    /// exact-pid classification in `persist/placement.rs`), and the `tiling.terminal-args` are
    /// applied only when `program` is the configured terminal (same basename). The program is
    /// resolved strictly ([`resolve_program`]) and the resolved path is what is spawned.
    ///
    /// # Errors
    /// `unknown command <word>` when `program` does not resolve to an executable file, `spawn
    /// failed` when the spawn does.
    pub fn run_term_checked(&mut self, program: &str) -> Result<u32, String> {
        let ws = self.layout.world.active_workspace_id();
        let root = self.persist.workspace.roots.get(&ws).cloned();
        let cwd = root
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/"));
        let resolved = resolve_program(program, &cwd, Self::spawn_path().as_deref())
            .map_err(|word| format!("unknown command {word}"))?;
        let args = if same_program(program, &self.terminal_program()) {
            self.with_tiling_args(Vec::new())
        } else {
            Vec::new()
        };
        self.spawn(
            SpawnRequest::new(resolved.to_string_lossy(), args, SpawnKind::Terminal).in_dir(root),
        )
        .ok_or_else(|| "spawn failed".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn session_env_names_cde_on_wayland() {
        let env = session_env();
        assert!(env.contains(&("XDG_CURRENT_DESKTOP", "CDE")));
        assert!(env.contains(&("XDG_SESSION_TYPE", "wayland")));
    }

    #[test]
    fn terminal_args_go_before_the_exec_flag() {
        let base = Invocation::Pick(crucible_persist::PickTarget::Workspace)
            .terminal_argv("-e", Path::new("/c"));
        let got = with_terminal_args(&["--a=1".to_string(), "--b".to_string()], base.clone());
        let got: Vec<&str> = got.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(got, ["--a=1", "--b", "-e", "/c", "pick"]);
        assert_eq!(with_terminal_args(&[], base.clone()), base);
    }

    #[test]
    fn shell_app_is_sh_exec_with_no_cwd_or_origin() {
        let req = SpawnRequest::shell_app("firefox --new-window");
        assert_eq!(req.program, "/bin/sh");
        assert_eq!(
            req.args,
            [
                OsString::from("-c"),
                OsString::from("exec firefox --new-window")
            ]
        );
        assert_eq!(
            (req.cwd, req.kind, req.origin),
            (None, SpawnKind::App, None)
        );
    }
}
