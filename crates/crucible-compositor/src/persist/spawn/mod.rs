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
use std::path::{Path, PathBuf};
use std::process::Command;

use crucible_config::Named;
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

/// The `CRUCIBLE_TERM_SESSION` key for a spawn of `kind` running `program`: `None` unless it is a
/// granted `cterm` terminal spawn; otherwise the key of its restored origin terminal record, else
/// a fresh one from `mint`.
#[must_use]
fn session_for_spawn(
    kind: SpawnKind,
    program: &str,
    origin: Option<&WindowRecord>,
    mint: impl FnOnce() -> Option<String>,
) -> Option<String> {
    if !super::identity::wants_grant(kind, program) {
        return None;
    }
    match origin {
        Some(WindowRecord::Terminal {
            session: Some(key), ..
        }) => Some(key.clone()),
        _ => mint(),
    }
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
    /// client PID is the app itself. The origin records `cmd` (without the `exec ` prefix) as an
    /// app with no app id, so the window's saved record is the command that started it.
    #[must_use]
    pub fn shell_app(cmd: &str) -> Self {
        let args = vec![OsString::from("-c"), OsString::from(format!("exec {cmd}"))];
        Self::new("/bin/sh", args, SpawnKind::App).with_origin(Some(WindowRecord::App {
            app_id: None,
            cmdline: Some(cmd.to_string()),
        }))
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

/// Where a workspace-scoped spawn runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DirPolicy {
    /// The workspace root if any, else inherit the compositor's cwd (`run`, `run-term`, a terminal
    /// here, a relaunch).
    Inherit,
    /// The workspace root, else `$HOME`, else `/` (`run-in-terminal`, `run-tui`).
    Terminal,
}

/// The directories one [`DirPolicy`] resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SpawnDir {
    /// The spawn's `current_dir` (`None`: inherit the compositor's).
    pub(super) run_in: Option<PathBuf>,
    /// The directory the command is pre-flighted (resolved) against.
    pub(super) preflight: PathBuf,
}

/// The one spawn-dir policy table, pure: `root` is the workspace root, `home` is `$HOME`, `cwd` is
/// the compositor's current directory. The pre-flight directory is always the one the command
/// will really run in (for `Inherit` without a root, that is the compositor's cwd, else `/`).
#[must_use]
pub(super) fn spawn_dir(
    policy: DirPolicy,
    root: Option<&Path>,
    home: Option<&Path>,
    cwd: Option<&Path>,
) -> SpawnDir {
    match policy {
        DirPolicy::Inherit => SpawnDir {
            run_in: root.map(Path::to_path_buf),
            preflight: root.or(cwd).unwrap_or(Path::new("/")).to_path_buf(),
        },
        DirPolicy::Terminal => {
            let dir = root.or(home).unwrap_or(Path::new("/")).to_path_buf();
            SpawnDir {
                run_in: Some(dir.clone()),
                preflight: dir,
            }
        }
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
            .env("WAYLAND_DISPLAY", &socket)
            .env("DISPLAY", crate::xwayland::child_display(self.xdisplay));
        if let Some(path) = &self.persist.ipc_path {
            cmd.env(SOCKET_ENV, path);
        }
        cmd.env(TERMINAL_ENV, self.terminal_program())
            .env(TERMINAL_EXEC_FLAG_ENV, &self.config.terminal.exec_flag);
        // Only what the config sets; otherwise the child inherits the session env unchanged.
        cmd.envs(crate::cursor::child_env(&self.config.cursor));
        if kind == SpawnKind::Overlay {
            cmd.env(OVERLAY_STYLE_ENV, self.config.overlay.style.as_str());
            cmd.env(LAUNCHER_TUI_ENV, self.config.launcher.tui.join(":"));
        }
        let mut record = SpawnRecord {
            origin,
            ..SpawnRecord::now(kind, program)
        };
        if super::identity::wants_grant(kind, program) {
            match (Self::grant_exe(program, cwd), super::identity::mint_grant()) {
                (Some(exe), Some(grant)) => {
                    cmd.env(super::identity::GRANT_ENV, &grant);
                    record.grant = Some(grant);
                    record.exe = Some(exe);
                    // A restore of a keyed terminal re-exports its saved key; any other granted
                    // spawn gets a fresh one. Without randomness the spawn simply has no key.
                    let session = session_for_spawn(
                        kind,
                        program,
                        record.origin.as_ref(),
                        super::identity::mint_grant,
                    );
                    if let Some(key) = &session {
                        cmd.env(super::identity::SESSION_ENV, key);
                    }
                    record.session = session;
                }
                _ => warn_no_grant(program),
            }
        }
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

    /// The canonical executable `program` resolves to on the child's `PATH` (relative to `cwd`,
    /// else the compositor's cwd), recorded at grant mint.
    fn grant_exe(program: &str, cwd: Option<&Path>) -> Option<PathBuf> {
        let here = std::env::current_dir().ok();
        let dir = cwd.or(here.as_deref())?;
        let hit = resolve_program(program, dir, Self::spawn_path().as_deref()).ok()?;
        std::fs::canonicalize(hit).ok()
    }

    /// The `PATH` every spawned child gets: this binary's directory (so `crucible` resolves) in
    /// front of the compositor's own `PATH`. `None` when the executable path is unknown.
    pub(super) fn spawn_path() -> Option<OsString> {
        let exe = Self::crucible_exe("child PATH")?;
        let dir = exe.parent()?;
        Some(child_path(dir, std::env::var_os("PATH").as_deref()))
    }

    /// The root of workspace `ws`, if it has one (the one spawn-side root lookup).
    pub(super) fn workspace_root(&self, ws: WorkspaceId) -> Option<PathBuf> {
        self.persist.workspace.root(ws).cloned()
    }

    /// The spawn directories of `policy` for workspace `ws`, from its root, `$HOME` and the
    /// compositor's cwd.
    pub(super) fn spawn_dir(&self, policy: DirPolicy, ws: WorkspaceId) -> SpawnDir {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let cwd = std::env::current_dir().ok();
        spawn_dir(
            policy,
            self.workspace_root(ws).as_deref(),
            home.as_deref(),
            cwd.as_deref(),
        )
    }

    /// Pre-flights `cmd`: its first word must resolve against the child's `PATH`, relative to the
    /// directory the spawn will run in (`dir`, from [`spawn_dir`]).
    pub(super) fn preflight(cmd: &str, dir: &Path) -> Result<(), String> {
        preflight_command(cmd, dir, Self::spawn_path().as_deref())
    }

    /// `run`: pre-flights, then runs `cmd` via `/bin/sh -c 'exec <cmd>'` ([`SpawnRequest::shell_app`])
    /// in the active workspace's root, if any. `Err` is the IPC reason.
    ///
    /// # Errors
    /// `unknown command <word>` when the pre-flight fails, `spawn failed` when the spawn does.
    pub fn run_checked(&mut self, cmd: &str) -> Result<u32, String> {
        let ws = self.layout.world.active_workspace_id();
        let dir = self.spawn_dir(DirPolicy::Inherit, ws);
        Self::preflight(cmd, &dir.preflight).map_err(|word| format!("unknown command {word}"))?;
        self.spawn(SpawnRequest::shell_app(cmd).in_dir(dir.run_in))
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
        let dir = self.spawn_dir(DirPolicy::Terminal, ws);
        Self::preflight(cmd, &dir.preflight).map_err(|word| format!("unknown command {word}"))?;
        let crucible = Self::crucible_exe(label).ok_or("spawn failed")?;
        let invocation = Invocation::Resume(ResumeWrapper {
            cwd: dir.preflight,
            command: Some(cmd.to_string()),
            mode,
        });
        let args = self
            .with_tiling_args(invocation.terminal_argv(&self.config.terminal.exec_flag, &crucible));
        let program = self.terminal_program();
        self.spawn(SpawnRequest::new(program, args, SpawnKind::Terminal).in_dir(dir.run_in))
            .ok_or_else(|| "spawn failed".to_string())
    }

    /// Spawns a terminal into the focused pane, starting in the active workspace's root if any.
    pub fn spawn_terminal_here(&mut self, program: &str) {
        let ws = self.layout.world.active_workspace_id();
        let dir = self.spawn_dir(DirPolicy::Inherit, ws);
        let args = self.with_tiling_args(Vec::new());
        self.spawn(SpawnRequest::new(program, args, SpawnKind::Terminal).in_dir(dir.run_in));
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
        let dir = self.spawn_dir(DirPolicy::Inherit, ws);
        let resolved = resolve_program(program, &dir.preflight, Self::spawn_path().as_deref())
            .map_err(|word| format!("unknown command {word}"))?;
        let args = if same_program(program, &self.terminal_program()) {
            self.with_tiling_args(Vec::new())
        } else {
            Vec::new()
        };
        self.spawn(
            SpawnRequest::new(resolved.to_string_lossy(), args, SpawnKind::Terminal)
                .in_dir(dir.run_in),
        )
        .ok_or_else(|| "spawn failed".to_string())
    }
}

/// Warns, once per compositor run, that a `cterm` spawn got no grant (its exe did not resolve or
/// `/dev/urandom` was unreadable); the spawn still runs, its windows stay unbound.
fn warn_no_grant(program: &str) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        warn!(
            program,
            "cannot mint a window-identity grant; its windows stay unbound"
        );
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

    fn keyed(key: Option<&str>) -> WindowRecord {
        WindowRecord::Terminal {
            cwd: "/".into(),
            command: None,
            session: key.map(String::from),
        }
    }

    #[test]
    fn fresh_granted_spawns_get_distinct_32_hex_keys() {
        let mint = || super::super::identity::mint_grant();
        let a = session_for_spawn(SpawnKind::Terminal, "cterm", None, mint).expect("key");
        let b = session_for_spawn(SpawnKind::Terminal, "/usr/bin/cterm", None, mint).expect("key");
        assert!(crucible_persist::is_session_key(&a));
        assert!(crucible_persist::is_session_key(&b));
        assert_ne!(a, b);
        // A restore of a key-less terminal record is a fresh spawn too.
        let c = session_for_spawn(SpawnKind::Terminal, "cterm", Some(&keyed(None)), mint);
        assert!(c.is_some_and(|k| crucible_persist::is_session_key(&k)));
    }

    #[test]
    fn restore_of_a_keyed_terminal_reexports_its_key() {
        let key = "0123456789abcdef0123456789abcdef";
        let got = session_for_spawn(
            SpawnKind::Terminal,
            "cterm",
            Some(&keyed(Some(key))),
            || panic!("must not mint"),
        );
        assert_eq!(got.as_deref(), Some(key));
    }

    #[test]
    fn non_cterm_terminals_and_apps_never_get_a_key() {
        let key = Some("0123456789abcdef0123456789abcdef");
        for (kind, program) in [
            (SpawnKind::Terminal, "foot"),
            (SpawnKind::Terminal, "/usr/bin/alacritty"),
            (SpawnKind::App, "cterm"),
            (SpawnKind::Overlay, "cterm"),
        ] {
            let got = session_for_spawn(kind, program, Some(&keyed(key)), || Some("x".into()));
            assert_eq!(got, None, "{kind:?} {program}");
        }
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

    fn p(s: &str) -> Option<&Path> {
        Some(Path::new(s))
    }

    #[test]
    fn the_root_wins_for_both_policies() {
        for policy in [DirPolicy::Inherit, DirPolicy::Terminal] {
            let got = spawn_dir(policy, p("/root"), p("/home"), p("/cwd"));
            assert_eq!(got.run_in.as_deref(), p("/root"), "{policy:?}");
            assert_eq!(got.preflight, Path::new("/root"), "{policy:?}");
        }
    }

    #[test]
    fn without_a_root_inherit_keeps_the_compositor_cwd() {
        let got = spawn_dir(DirPolicy::Inherit, None, p("/home"), p("/cwd"));
        assert_eq!(got.run_in, None);
        assert_eq!(got.preflight, Path::new("/cwd"));
        let got = spawn_dir(DirPolicy::Inherit, None, p("/home"), None);
        assert_eq!((got.run_in, got.preflight), (None, PathBuf::from("/")));
    }

    #[test]
    fn without_a_root_terminal_runs_in_home_else_slash() {
        let got = spawn_dir(DirPolicy::Terminal, None, p("/home"), p("/cwd"));
        assert_eq!(got.run_in.as_deref(), p("/home"));
        assert_eq!(got.preflight, Path::new("/home"));
        let got = spawn_dir(DirPolicy::Terminal, None, None, p("/cwd"));
        assert_eq!(got.run_in.as_deref(), p("/"));
        assert_eq!(got.preflight, Path::new("/"));
    }

    /// C9: a relative `./x` that exists only in the home directory passes the `Terminal`
    /// pre-flight (it runs in home), while the `Inherit` pre-flight dir is the compositor cwd.
    #[test]
    fn terminal_preflight_resolves_relative_commands_against_home() {
        use std::os::unix::fs::PermissionsExt;
        let home = std::env::temp_dir().join(format!("cde-spawndir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let exe = home.join("x");
        std::fs::write(
            &exe,
            b"#!/bin/sh
",
        )
        .unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cwd = Path::new("/");
        let terminal = spawn_dir(DirPolicy::Terminal, None, Some(&home), Some(cwd));
        assert_eq!(terminal.preflight, home);
        assert_eq!(preflight_command("./x", &terminal.preflight, None), Ok(()));
        let inherit = spawn_dir(DirPolicy::Inherit, None, Some(&home), Some(cwd));
        assert_eq!(inherit.preflight, cwd);
        assert_ne!(inherit.preflight, home);
        assert!(preflight_command("./x", &inherit.preflight, None).is_err());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn shell_app_is_sh_exec_with_no_cwd_and_an_app_origin() {
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
            (
                None,
                SpawnKind::App,
                Some(WindowRecord::App {
                    app_id: None,
                    cmdline: Some("firefox --new-window".to_string())
                })
            )
        );
    }
}
