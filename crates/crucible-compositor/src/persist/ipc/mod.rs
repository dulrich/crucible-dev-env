//! The IPC request handlers: `handle_ipc` and its per-request helpers (open, new, rename,
//! relaunch, open-folder).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crucible_layout::{WindowId, WorkspaceId};
use crucible_persist::slots::Slots;
use crucible_persist::{ProcAnchor, WorkspaceRecord, store};
use tracing::info;

use super::placement::{
    ANCESTOR_DEPTH, Pending, SlotEntry, Target, placeholder_cmdline, relaunch_window_pid,
    select_siblings,
};
use super::spawn::{DirPolicy, SpawnRequest};
use super::workspace::{
    BringOutcome, LiveOpenPlan, bring_outcome, folder_workspace_name, live_open_plan, run_on_target,
};
use super::{capture_env, close_element, proc_root};
use crate::input::{ActionError, Origin};
use crate::ipc::{Reply, Request, Response};
use crate::layout::WindowMeta;
use crate::state::{Backend, CrucibleState};
use crucible_keys::{Action, BindTarget};
use crucible_layout::WorkspaceLayout;

mod relaunch;
mod rename;

/// The IPC response for a checked spawn: `ok` once spawned, else the refusal reason.
fn run_response(spawned: Result<u32, String>) -> Response {
    match spawned {
        Ok(_) => Response::Ok(String::new()),
        Err(reason) => Response::Err(reason),
    }
}

/// The lock admission for an IPC request: while `locked`, every request that acts
/// ([`Request::acts`]) is refused; `candidates`, `bindings` and `save` stay allowed. `exec` passes
/// through: its lock rule is the action gate's ([`crate::input::admit`] via `handle_key_action`),
/// with [`exec_action`] refusing what never reaches that gate.
fn admit_request(locked: bool, request: &Request) -> Result<(), ActionError> {
    if !locked || !request.acts() || matches!(request, Request::Exec(_)) {
        return Ok(());
    }
    Err(ActionError::Locked)
}

/// Parses an `exec` for running; the error is the reply reason. While `locked`, an unparsable
/// text and a `run-on` (which runs outside the action gate) are refused as `session locked`,
/// without parse detail; every other action goes on to the action gate, which applies the lock
/// rule ([`Action::allowed_while_locked`]).
fn exec_action(locked: bool, text: &str) -> Result<Action, String> {
    match parse_exec(text) {
        Err(_) | Ok(Action::RunOn { .. }) if locked => Err(ActionError::Locked.to_string()),
        parsed => parsed,
    }
}

/// The ids (sorted) of every window in `metas` whose process is exactly `pid`.
fn windows_of_pid<'a>(
    metas: impl Iterator<Item = (WindowId, &'a crate::layout::WindowMeta)>,
    pid: u32,
) -> Vec<WindowId> {
    let mut ids: Vec<WindowId> = metas
        .filter(|(_, m)| m.pid == Some(pid))
        .map(|(id, _)| id)
        .collect();
    ids.sort();
    ids
}

/// Validates a workspace root: it must be an absolute path to an existing directory.
pub(super) fn check_root(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("{} is not an absolute path", path.display()));
    }
    if !path.is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    Ok(())
}

/// The pid of a terminal window for the root capture; an app, an unbound window or a terminal
/// without a known pid is refused.
fn terminal_pid(meta: Option<&crate::layout::WindowMeta>) -> Result<u32, String> {
    let meta = meta.ok_or_else(|| "the focused window is not a terminal".to_string())?;
    if meta.class != crate::layout::WindowClass::Terminal {
        return Err("the focused window is not a terminal".to_string());
    }
    meta.pid
        .ok_or_else(|| "the focused terminal has no known pid".to_string())
}

/// Parses an `exec` action text; the error is the reply reason. `none` (bind-only) is refused.
fn parse_exec(text: &str) -> Result<Action, String> {
    match text.parse::<BindTarget>() {
        Ok(BindTarget::Action(action)) => Ok(action),
        Ok(BindTarget::Unbind) => Err("`none` is not a runnable action".to_string()),
        Err(reason) => Err(format!("bad action `{text}`: {reason}")),
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    // ---- IPC -----------------------------------------------------------------------------------

    /// Answers one IPC request. `peer` is the connecting process's PID (`SO_PEERCRED`); only
    /// `relaunch` uses it.
    ///
    /// Every acting request but `exec` passes the lock admission first ([`admit_request`]); `exec`
    /// goes through [`exec_action`] and the action gate (`handle_key_action`), which answers a
    /// refused action (`session locked` while locked) with `err`.
    pub fn handle_ipc(&mut self, request: Request, peer: Option<u32>) -> Reply {
        if let Err(err) = admit_request(self.is_session_locked(), &request) {
            return Reply::Now(Response::Err(err.to_string()));
        }
        if let Request::Capture(path) = request {
            // Checked once at request time; a held output (plan 38) is captured after its release
            // so the capture shows what the user sees, and is admitted again then. Encoded,
            // written and answered (`ok <w>x<h>`) off the main thread.
            return match self.request_capture(path) {
                Ok(rx) => Reply::Later(rx),
                Err(reason) => Reply::Now(Response::Err(reason)),
            };
        }
        Reply::Now(self.answer_ipc(request, peer))
    }

    /// [`Self::handle_ipc`]'s immediate requests, after the lock admission.
    fn answer_ipc(&mut self, request: Request, peer: Option<u32>) -> Response {
        let result = match request {
            Request::Relaunch => self.ipc_relaunch(peer),
            Request::ClosePid(pid) => return self.ipc_close_pid(pid),
            Request::Candidates => {
                return Response::Ok(crate::ipc::format_candidates(&self.candidates()));
            }
            Request::Bindings => {
                return Response::Ok(crate::ipc::format_bindings(&self.bindings.entries()));
            }
            Request::Outputs => {
                return Response::Ok(crate::ipc::format_outputs(&self.output_infos()));
            }
            Request::Save => {
                self.flush_workspaces();
                Ok(())
            }
            Request::Exec(text) => {
                return match exec_action(self.is_session_locked(), &text) {
                    Err(reason) => Response::Err(reason),
                    // `run-on` answers `ok <pid>`. The lock was refused above, and it is exempt
                    // from IPC overlay dismissal (`closes_overlay`), so nothing else in the action
                    // gate applies to it.
                    Ok(Action::RunOn { workspace, command }) => {
                        match self.run_on_checked(&workspace, &command) {
                            Ok(pid) => Response::Ok(pid.to_string()),
                            Err(reason) => Response::Err(reason),
                        }
                    }
                    Ok(action) => match self.handle_key_action(action, Origin::Ipc) {
                        Ok(()) => Response::Ok(String::new()),
                        Err(err) => Response::Err(err.to_string()),
                    },
                };
            }
            Request::RunTui(cmd) => {
                return run_response(self.run_tui_checked(&cmd));
            }
            Request::RunTerm(program) => {
                return run_response(self.run_term_checked(&program));
            }
            Request::Open(name) => {
                let r = self.ipc_open(&name);
                self.complete_pending_pin(r)
            }
            Request::OpenFolder(path) => {
                let r = self.ipc_open_folder(&path);
                self.complete_pending_pin(r)
            }
            Request::New(name) => {
                let r = self.ipc_new(&name);
                self.complete_pending_pin(r)
            }
            Request::Rename(name) => self.ipc_rename(&name),
            Request::Delete(name) => self.ipc_delete(&name),
            Request::SetRoot(root) => self.ipc_set_root(root),
            // Deferred by `handle_ipc` before this point.
            Request::Capture(_) => Err("capture is answered by handle_ipc".to_string()),
        };
        match result {
            Ok(()) => Response::Ok(String::new()),
            Err(reason) => Response::Err(reason),
        }
    }

    /// Every mapped output's name, logical geometry and scale (the `outputs` request).
    fn output_infos(&self) -> Vec<crate::ipc::OutputInfo> {
        self.space
            .outputs()
            .filter_map(|output| {
                let geo = self.space.output_geometry(output)?;
                Some(crate::ipc::OutputInfo {
                    name: output.name(),
                    x: geo.loc.x,
                    y: geo.loc.y,
                    w: geo.size.w,
                    h: geo.size.h,
                    scale: output.current_scale().fractional_scale(),
                })
            })
            .collect()
    }

    /// `close-pid`: politely closes (`close_element`) every layout-bound window of process `pid`,
    /// including windows on hidden workspaces and lower in a pane stack. Overlay and warm-overlay
    /// windows are outside the layout bindings (`placement.rs`) and are left alone. Answers
    /// `ok <count>`, or `err no window for pid <pid>` when none matches.
    /// A bound cterm window's `meta.pid` is its thin client's pid, so `close-pid <cterm client pid>`
    /// closes exactly that window (never the shared daemon's other windows).
    fn ipc_close_pid(&mut self, pid: u32) -> Response {
        let ids = windows_of_pid(self.layout.metas(), pid);
        if ids.is_empty() {
            return Response::Err(format!("no window for pid {pid}"));
        }
        for id in &ids {
            if let Some(element) = self.layout.element(*id) {
                close_element(element);
            }
        }
        Response::Ok(ids.len().to_string())
    }

    pub(super) fn switch_to_live(&mut self, ws: WorkspaceId) -> Result<(), String> {
        let output = self
            .live_workspaces()
            .into_iter()
            .find(|(_, w)| *w == ws)
            .map(|(o, _)| o)
            .ok_or_else(|| "workspace has no output".to_string())?;
        self.with_model(|w| {
            w.set_focused_output(output)?;
            w.switch_workspace(ws)
        })
        .map_err(|e| e.to_string())
    }

    /// Opens the live workspace `ws` as `workspace-open { live }` says: jump to its output, or
    /// bring it onto the focused output (the model move activates it; a refusal jumps instead).
    fn open_live(&mut self, ws: WorkspaceId) -> Result<(), String> {
        let owner = self
            .live_workspaces()
            .into_iter()
            .find(|(_, w)| *w == ws)
            .map(|(o, _)| o);
        let plan = live_open_plan(
            self.config.workspace_open.live,
            owner,
            self.layout.world.focused_output(),
        );
        let LiveOpenPlan::Bring(target) = plan else {
            return self.switch_to_live(ws);
        };
        let moved = self.with_model(|w| w.move_workspace_to_output(ws, target));
        match bring_outcome(&moved) {
            BringOutcome::Done => Ok(()),
            BringOutcome::Jump => {
                info!(
                    ?ws,
                    "workspace-open bring: not moved; jumping to it instead"
                );
                self.switch_to_live(ws)
            }
            BringOutcome::Failed(err) => Err(err),
        }
    }

    pub(super) fn ipc_open(&mut self, name: &str) -> Result<(), String> {
        if let Some(ws) = self.layout.world.workspace_by_name(name) {
            return self.open_live(ws);
        }
        let path = self.persist.file_of(name)?;
        if !path.exists() {
            return Err(format!("no workspace named {name}"));
        }
        let record = store::load(&path).map_err(|e| e.to_string())?;
        self.open_saved(record)
    }

    /// `delete <name>`: only a saved, not live workspace; a live one is refused.
    pub(super) fn ipc_delete(&mut self, name: &str) -> Result<(), String> {
        if self.layout.world.workspace_by_name(name).is_some() {
            return Err(format!(
                "{name} is open: refused (use delete-workspace on it)"
            ));
        }
        if self.delete_saved(name)? {
            info!(name, "saved workspace deleted");
            Ok(())
        } else {
            Err(format!("no saved workspace named {name}"))
        }
    }

    pub(super) fn ipc_open_folder(&mut self, path: &Path) -> Result<(), String> {
        if !path.is_dir() {
            return Err(format!("{} is not a directory", path.display()));
        }
        // Already open (live) or saved for this exact folder: open that instead of a duplicate.
        let live = self
            .persist
            .workspace
            .meta
            .iter()
            .find(|(ws, meta)| {
                meta.root.as_deref() == Some(path) && self.layout.world.workspace(**ws).is_some()
            })
            .map(|(ws, _)| *ws);
        if let Some(ws) = live {
            return self.open_live(ws);
        }
        let saved = self.persist.saved_names();
        if let Some(base) = path.file_name().and_then(|b| b.to_str()) {
            if saved.contains(base) && self.layout.world.workspace_by_name(base).is_none() {
                let file = self.persist.file_of(base)?;
                if let Ok(record) = store::load(&file) {
                    if record.root.as_deref() == Some(path) {
                        return self.open_saved(record);
                    }
                }
            }
        }
        let name = folder_workspace_name(path, |n| self.name_taken(n, &saved))
            .ok_or_else(|| format!("cannot name a workspace after {}", path.display()))?;
        self.create_named(name, Some(path.to_path_buf()))
    }

    /// Sets (`Some`, validated by [`check_root`]) or clears (`None`) the active workspace's root.
    /// Only later spawns are affected; the next auto-save sees the changed record content.
    pub(super) fn ipc_set_root(&mut self, root: Option<PathBuf>) -> Result<(), String> {
        let ws = self.layout.world.active_workspace_id();
        let name = self
            .layout
            .world
            .workspace(ws)
            .map(|w| w.name.clone())
            .ok_or_else(|| "no active workspace".to_string())?;
        match root {
            Some(path) => {
                check_root(&path)?;
                info!(workspace = name, root = %path.display(), "workspace root set");
                self.persist.workspace.set_root(ws, Some(path));
            }
            None => {
                self.persist.workspace.set_root(ws, None);
                info!(workspace = name, "workspace root cleared");
            }
        }
        Ok(())
    }

    /// `capture-workspace-root`: the focused pane's active terminal's current directory becomes
    /// the active workspace's root (through [`Self::ipc_set_root`], so it shares its validation).
    /// A missing window, a non-terminal, an unknown pid or a failed capture changes nothing.
    pub(crate) fn capture_workspace_root(&mut self) -> Result<(), String> {
        let id = self
            .layout
            .world
            .focused_window()
            .ok_or_else(|| "no focused window".to_string())?;
        let meta = self.layout.meta(id);
        terminal_pid(meta)?;
        // A bound cterm window's terminal is the shared daemon: capture from its root shell.
        let cwd = meta
            .and_then(WindowMeta::anchor)
            .and_then(|anchor| crucible_persist::capture_anchor(&capture_env(), anchor))
            .ok_or_else(|| "cannot read the terminal's working directory".to_string())?
            .cwd;
        self.ipc_set_root(Some(cwd))
    }

    /// `run-on <name> <cmd>`: resolves the target ([`run_on_target`]), creating a missing one on
    /// the focused output (appended, not activated, ephemeral), then spawns `cmd` as `run` does
    /// (verbatim, `PATH` pre-flight) and registers a pid-keyed [`Target::Workspace`] placement.
    /// No output's active workspace and no focus changes here. Returns the spawned pid.
    ///
    /// # Errors
    /// `saved workspace <name> exists`, the name check `new` applies, `unknown command <word>`
    /// or `spawn failed`; a workspace created for a refused spawn is removed again.
    pub(crate) fn run_on_checked(&mut self, name: &str, cmd: &str) -> Result<u32, String> {
        let live = self.layout.world.workspace_by_name(name);
        let valid = self.persist.file_of(name).map(drop);
        let target = run_on_target(name, live, &self.persist.saved_names(), valid)?;
        let ws = match target {
            Some(ws) => ws,
            None => {
                let output = self.layout.world.focused_output();
                let mut created = None;
                self.with_model(|w| {
                    created =
                        Some(w.insert_workspace(output, name, &WorkspaceLayout::single(()))?);
                    Ok(())
                })
                .map_err(|e| e.to_string())?;
                let (ws, _) = created.ok_or_else(|| "workspace was not created".to_string())?;
                self.persist.workspace.set_ephemeral(ws);
                info!(name, ?ws, "run-on: created an ephemeral workspace");
                ws
            }
        };
        let dir = self.spawn_dir(DirPolicy::Inherit, ws);
        let spawned = Self::preflight(cmd, &dir.preflight)
            .map_err(|word| format!("unknown command {word}"))
            .and_then(|()| {
                self.spawn(SpawnRequest::shell_app(cmd).in_dir(dir.run_in))
                    .ok_or_else(|| "spawn failed".to_string())
            });
        match spawned {
            Ok(pid) => {
                self.persist
                    .placement
                    .pending
                    .insert(pid, Pending::new(Target::Workspace(ws), Instant::now()));
                info!(
                    pid,
                    name, cmd, "run-on: spawned into a background workspace"
                );
                Ok(pid)
            }
            Err(reason) => {
                self.prune_ephemeral();
                Err(reason)
            }
        }
    }

    pub(super) fn ipc_new(&mut self, name: &str) -> Result<(), String> {
        if self.name_taken(name, &self.persist.saved_names()) {
            return Err(format!("a workspace named {name} already exists"));
        }
        self.persist.file_of(name)?;
        self.create_named(name.to_string(), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locked_capture_is_refused_by_admission_with_the_capture_text() {
        // `handle_ipc` answers this `err` at once, before `request_capture` or any worker runs.
        let refused = admit_request(true, &Request::Capture("/tmp/x.png".into()));
        assert_eq!(refused, Err(ActionError::Locked));
        assert_eq!(
            ActionError::Locked.to_string(),
            crate::capture::CaptureError::Locked.to_string()
        );
    }

    #[test]
    fn locked_refuses_every_acting_request() {
        for r in [
            Request::Open("a".into()),
            Request::OpenFolder("/tmp".into()),
            Request::New("n".into()),
            Request::Rename("r".into()),
            Request::Delete("d".into()),
            Request::SetRoot(Some("/tmp".into())),
            Request::SetRoot(None),
            Request::RunTui("htop".into()),
            Request::RunTerm("ghostty".into()),
            Request::Relaunch,
            Request::ClosePid(7),
            Request::Capture("/tmp/x.png".into()),
        ] {
            assert_eq!(admit_request(true, &r), Err(ActionError::Locked), "{r:?}");
            assert_eq!(admit_request(false, &r), Ok(()), "{r:?}");
        }
        assert_eq!(
            Response::Err(ActionError::Locked.to_string()),
            Response::Err("session locked".into())
        );
    }

    #[test]
    fn exec_passes_the_admission_to_the_action_gate() {
        for line in [
            "set-layout us",
            "quit",
            "run-on scratch foot",
            "garbage ~~",
            "none",
            "",
        ] {
            let r = Request::Exec(line.into());
            assert_eq!(admit_request(true, &r), Ok(()), "{line}");
            assert_eq!(admit_request(false, &r), Ok(()), "{line}");
        }
    }

    #[test]
    fn locked_exec_replies_session_locked_unless_the_lock_allows_it() {
        // End to end: `exec_action`, then the action gate's `admit` in the mode a locked session
        // resolves to (`Locked`, or `Greeter`), exactly as `answer_ipc` and `handle_key_action`.
        let reply = |locked: bool, mode: &crate::input::TestInputMode<()>, line: &str| {
            exec_action(locked, line)
                .and_then(|a| crate::input::admit(mode, &a).map_err(|e| e.to_string()))
        };
        let locked_modes = [
            crate::input::TestInputMode::<()>::Locked,
            crate::input::TestInputMode::<()>::Greeter,
        ];
        for mode in &locked_modes {
            assert!(reply(true, mode, "set-layout us").is_ok());
            for line in [
                "set-layout",
                "quit",
                "focus left",
                "run firefox",
                "run-in-terminal htop",
                "run-on scratch foot",
                "vt-switch 2",
                "garbage ~~",
                "none",
                "",
            ] {
                assert_eq!(
                    reply(true, mode, line),
                    Err("session locked".into()),
                    "{line}"
                );
            }
        }
        // Unlocked, a parse failure keeps its detail.
        assert!(
            exec_action(false, "garbage ~~")
                .unwrap_err()
                .starts_with("bad action")
        );
        assert!(exec_action(false, "run-on scratch foot").is_ok());
    }

    #[test]
    fn check_root_wants_an_absolute_existing_directory() {
        let scratch = std::env::temp_dir().join(format!("cde-checkroot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        let dir = scratch.join("dir");
        let file = scratch.join("file");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, b"").unwrap();
        assert_eq!(check_root(&dir), Ok(()));
        assert!(check_root(Path::new("tmp")).is_err());
        assert!(check_root(&scratch.join("missing")).is_err());
        assert!(check_root(&file).is_err());
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn terminal_pid_wants_a_terminal_with_a_pid() {
        use crate::layout::{WindowClass, WindowMeta};
        let meta = |class, pid| WindowMeta {
            pid,
            class,
            origin: None,
            identity: None,
        };
        assert_eq!(
            terminal_pid(Some(&meta(WindowClass::Terminal, Some(7)))),
            Ok(7)
        );
        assert!(terminal_pid(Some(&meta(WindowClass::Terminal, None))).is_err());
        assert!(terminal_pid(Some(&meta(WindowClass::App, Some(7)))).is_err());
        assert!(terminal_pid(None).is_err());
    }

    #[test]
    fn windows_of_pid_selects_exact_matches_only() {
        use crate::layout::{WindowClass, WindowMeta};
        let meta = |pid| WindowMeta {
            pid,
            class: WindowClass::App,
            origin: None,
            identity: None,
        };
        let metas = [
            (WindowId::from_raw(1), meta(Some(10))),
            (WindowId::from_raw(2), meta(Some(100))),
            (WindowId::from_raw(3), meta(Some(10))),
            (WindowId::from_raw(4), meta(None)),
            (WindowId::from_raw(5), meta(Some(1))),
        ];
        let pick = |pid| windows_of_pid(metas.iter().map(|(i, m)| (*i, m)), pid);
        assert_eq!(pick(10), vec![WindowId::from_raw(1), WindowId::from_raw(3)]);
        assert_eq!(pick(100), vec![WindowId::from_raw(2)]);
        assert_eq!(pick(1), vec![WindowId::from_raw(5)]);
        assert!(pick(11).is_empty());
    }

    #[test]
    fn locked_allows_read_only_requests() {
        for r in [
            Request::Candidates,
            Request::Bindings,
            Request::Save,
            Request::Outputs,
        ] {
            assert_eq!(admit_request(true, &r), Ok(()), "{r:?}");
        }
    }

    #[test]
    fn exec_parses_actions_and_refuses_none_and_garbage() {
        assert_eq!(
            parse_exec("run firefox --new"),
            Ok(Action::Run("firefox --new".into()))
        );
        // The IPC round trip `crucible msg run-on` relies on: decoded `exec` text to the action.
        let line = crate::ipc::Request::parse("exec run-on scratch-a-wp-4 echo 50%25%3Ax").unwrap();
        let Request::Exec(text) = line else {
            panic!("not an exec request");
        };
        assert_eq!(
            parse_exec(&text),
            Ok(Action::RunOn {
                workspace: "scratch-a-wp-4".into(),
                command: "echo 50%:x".into(),
            })
        );
        assert_eq!(
            parse_exec("none"),
            Err("`none` is not a runnable action".to_string())
        );
        assert_eq!(
            parse_exec("bogus"),
            Err("bad action `bogus`: unknown action `bogus`".to_string())
        );
        assert!(parse_exec("vt-switch 3").is_err());
    }
}
