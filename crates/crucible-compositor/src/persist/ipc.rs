//! The IPC request handlers: `handle_ipc` and its per-request helpers (open, new, rename,
//! relaunch, open-folder).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crucible_layout::{WindowId, WorkspaceId};
use crucible_persist::slots::Slots;
use crucible_persist::{WorkspaceRecord, store};
use tracing::info;

use super::placement::{
    ANCESTOR_DEPTH, Pending, SlotEntry, Target, match_pending, placeholder_cmdline, select_siblings,
};
use super::spawn::{DirPolicy, SpawnRequest};
use super::workspace::folder_workspace_name;
use super::{capture_env, close_element, proc_root};
use crate::input::{ActionError, Origin};
use crate::ipc::{Request, Response};
use crate::state::{Backend, CrucibleState};
use crucible_keys::{Action, BindTarget};

/// The IPC response for a checked spawn: `ok` once spawned, else the refusal reason.
fn run_response(spawned: Result<u32, String>) -> Response {
    match spawned {
        Ok(_) => Response::Ok(String::new()),
        Err(reason) => Response::Err(reason),
    }
}

/// The lock admission for an IPC request: while `locked`, every request that acts
/// ([`Request::acts`]) is refused; `candidates`, `bindings` and `save` stay allowed. No `exec`
/// action is allowed while locked (`vt-switch`, the only one that would be, is not parseable).
fn admit_request(locked: bool, request: &Request) -> Result<(), ActionError> {
    if locked && request.acts() {
        Err(ActionError::Locked)
    } else {
        Ok(())
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
    /// Every acting request passes the lock admission first ([`admit_request`]); `exec` then goes
    /// through the action gate (`handle_key_action`), which answers a failed action with `err`.
    pub fn handle_ipc(&mut self, request: Request, peer: Option<u32>) -> Response {
        if let Err(err) = admit_request(self.is_session_locked(), &request) {
            return Response::Err(err.to_string());
        }
        let result = match request {
            Request::Relaunch => self.ipc_relaunch(peer),
            Request::ClosePid(pid) => return self.ipc_close_pid(pid),
            Request::Candidates => {
                return Response::Ok(crate::ipc::format_candidates(&self.candidates()));
            }
            Request::Bindings => {
                return Response::Ok(crate::ipc::format_bindings(&self.bindings.entries()));
            }
            Request::Save => {
                self.flush_workspaces();
                Ok(())
            }
            Request::Exec(text) => {
                return match parse_exec(&text) {
                    Err(reason) => Response::Err(reason),
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
            Request::SetRoot(root) => self.ipc_set_root(root),
        };
        match result {
            Ok(()) => Response::Ok(String::new()),
            Err(reason) => Response::Err(reason),
        }
    }

    /// `close-pid`: politely closes (`close_element`) every layout-bound window of process `pid`,
    /// including windows on hidden workspaces and lower in a pane stack. Overlay and warm-overlay
    /// windows are outside the layout bindings (`placement.rs`) and are left alone. Answers
    /// `ok <count>`, or `err no window for pid <pid>` when none matches.
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

    /// Swaps the placeholder window run by `peer` for its app: spawn the saved cmdline, register
    /// the placeholder's live slot as pending, close the placeholder.
    pub(super) fn ipc_relaunch(&mut self, peer: Option<u32>) -> Result<(), String> {
        let peer = peer.ok_or_else(|| "not a relaunch placeholder".to_string())?;
        let args = crucible_persist::capture::read_cmdline(proc_root(), peer);
        let capture = crucible_persist::RelaunchWrapper::from_argv(&args)
            .ok_or_else(|| "not a relaunch placeholder".to_string())?;
        let ancestors = crucible_persist::ancestors(proc_root(), peer, ANCESTOR_DEPTH);
        let layout = &self.layout;
        let pid = match_pending(peer, &ancestors, |p| {
            layout.metas().any(|(_, m)| m.pid == Some(p))
        })
        .ok_or_else(|| "no placeholder window".to_string())?;
        let mut ids: Vec<WindowId> = layout
            .metas()
            .filter(|(_, m)| m.pid == Some(pid))
            .map(|(id, _)| id)
            .collect();
        ids.sort();
        // A sibling still queued by a live spawn joins that spawn: no second process. Expired
        // spawns took their queues with their pending entries.
        if let Some(id) = ids
            .iter()
            .copied()
            .find(|id| self.persist.placement.is_queued(*id))
        {
            info!(
                ?id,
                "placeholder is queued on a live relaunch; not respawning"
            );
            return Ok(());
        }
        let world = &self.layout.world;
        let (id, own) = ids
            .into_iter()
            .find_map(|id| Some((id, self.placeholder_slot(id)?)))
            .ok_or_else(|| "no placeholder window".to_string())?;
        let pane = own.pane;
        let ws = self
            .live_workspaces()
            .into_iter()
            .map(|(_, ws)| ws)
            .find(|ws| {
                world
                    .workspace(*ws)
                    .is_some_and(|w| w.panes().contains(&pane))
            })
            .unwrap_or_else(|| world.active_workspace_id());
        let element = self.layout.element(id).cloned();
        // The app stands for the placeholder's saved record: carried as the spawn's origin.
        let origin = crucible_persist::WindowRecord::App {
            app_id: capture.app_id.clone(),
            cmdline: Some(capture.cmdline.clone()),
        };
        let dir = self.spawn_dir(DirPolicy::Inherit, ws);
        let pid = self
            .spawn(
                SpawnRequest::shell_app(&capture.cmdline)
                    .in_dir(dir.run_in)
                    .with_origin(Some(origin)),
            )
            .ok_or_else(|| "spawn failed".to_string())?;
        let siblings = self.sibling_placeholders(id, &capture.cmdline);
        let mut queue = VecDeque::from([SlotEntry::Resolved(own)]);
        queue.extend(siblings.iter().map(|s| SlotEntry::Placeholder(*s)));
        self.persist.placement.pending.insert(
            pid,
            Pending {
                target: Target::Slot(own),
                since: Instant::now(),
                queue: Some(queue),
            },
        );
        // The relaunched placeholder's own window stays a plain close (deliberately not
        // `end_window` / `end_placeholder`): its `crucible relaunch` wrapper is this IPC peer.
        if let Some(element) = element {
            close_element(&element);
        }
        info!(
            pid,
            cmdline = capture.cmdline,
            siblings = siblings.len(),
            "relaunching app into its placeholder slot"
        );
        Ok(())
    }

    /// The other live relaunch placeholders of `own` that stand for `cmdline`, in workspace, pane
    /// and stack order, not already queued by another spawn. A placeholder is recognised from its
    /// spawn-time facts ([`placeholder_cmdline`]), with no `/proc` read.
    fn sibling_placeholders(&self, own: WindowId, cmdline: &str) -> Vec<WindowId> {
        let world = &self.layout.world;
        let mut candidates = Vec::new();
        for (_, ws) in self.live_workspaces() {
            let Some(w) = world.workspace(ws) else {
                continue;
            };
            for pane in w.panes() {
                let Some(pane) = world.pane(pane) else {
                    continue;
                };
                for id in pane.windows() {
                    let placeholder = self
                        .layout
                        .meta(*id)
                        .and_then(placeholder_cmdline)
                        .map(str::to_string);
                    candidates.push((*id, placeholder));
                }
            }
        }
        let placement = &self.persist.placement;
        select_siblings(candidates, own, cmdline, |id| placement.is_queued(id))
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

    pub(super) fn ipc_open(&mut self, name: &str) -> Result<(), String> {
        if let Some(ws) = self.layout.world.workspace_by_name(name) {
            return self.switch_to_live(ws);
        }
        let path = self.persist.file_of(name)?;
        if !path.exists() {
            return Err(format!("no workspace named {name}"));
        }
        let record = store::load(&path).map_err(|e| e.to_string())?;
        self.restore(record)
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
            return self.switch_to_live(ws);
        }
        let saved = self.persist.saved_names();
        if let Some(base) = path.file_name().and_then(|b| b.to_str()) {
            if saved.contains(base) && self.layout.world.workspace_by_name(base).is_none() {
                let file = self.persist.file_of(base)?;
                if let Ok(record) = store::load(&file) {
                    if record.root.as_deref() == Some(path) {
                        return self.restore(record);
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
        let pid = terminal_pid(self.layout.meta(id))?;
        let cwd = crucible_persist::capture_terminal(&capture_env(), pid)
            .ok_or_else(|| "cannot read the terminal's working directory".to_string())?
            .cwd;
        self.ipc_set_root(Some(cwd))
    }

    pub(super) fn ipc_new(&mut self, name: &str) -> Result<(), String> {
        if self.name_taken(name, &self.persist.saved_names()) {
            return Err(format!("a workspace named {name} already exists"));
        }
        self.persist.file_of(name)?;
        self.create_named(name.to_string(), None)
    }

    pub(super) fn ipc_rename(&mut self, new: &str) -> Result<(), String> {
        let ws = self.layout.world.active_workspace_id();
        let old = self
            .layout
            .world
            .workspace(ws)
            .map(|w| w.name.clone())
            .ok_or_else(|| "no active workspace".to_string())?;
        if old == new {
            return Ok(());
        }
        if self.name_taken(new, &self.persist.saved_names()) {
            return Err(format!("a workspace named {new} already exists"));
        }
        self.persist.file_of(new)?;
        let old_file = self.persist.file_of(&old).ok();
        // Step 1, the only fallible write: the record under the new name. Nothing else has
        // changed yet, so a failure here leaves the model, the pins, `saved` and the old file.
        let renamed = match old_file.as_ref().is_some_and(|f| f.exists()) {
            true => {
                let record = self
                    .capture_record(ws)
                    .ok_or_else(|| "workspace vanished".to_string())?;
                Some(save_renamed(&self.persist.store_dir, record, new)?)
            }
            false => None,
        };
        if let Err(e) = self.with_model(|w| w.rename_workspace(ws, new)) {
            if renamed.is_some() {
                if let Ok(f) = self.persist.file_of(new) {
                    let _ = std::fs::remove_file(f);
                }
            }
            return Err(e.to_string());
        }
        // Pins follow a user-driven rename (and only that one).
        if finish_rename(
            &mut self.persist.slots,
            &mut self.persist.saved,
            &old,
            new,
            renamed,
        ) {
            self.save_slots();
        }
        if let Some(f) = old_file {
            let _ = std::fs::remove_file(f);
        }
        info!(from = old, to = new, "workspace renamed");
        Ok(())
    }
}

/// Writes `record` under the name `new` and returns it renamed; the one fallible step of a rename.
fn save_renamed(
    store_dir: &Path,
    mut record: WorkspaceRecord,
    new: &str,
) -> Result<WorkspaceRecord, String> {
    record.name = new.to_string();
    store::save(store_dir, &record).map_err(|e| e.to_string())?;
    Ok(record)
}

/// The infallible tail of a rename, run once the new record is on disk and the model is renamed:
/// moves the slot pins and the `saved` entry from `old` to `new`. `renamed` is the record that
/// was written under `new` (`None` when the workspace had no file). Returns whether any pin moved.
fn finish_rename(
    slots: &mut Slots,
    saved: &mut HashMap<String, (WorkspaceRecord, Instant)>,
    old: &str,
    new: &str,
    renamed: Option<WorkspaceRecord>,
) -> bool {
    saved.remove(old);
    if let Some(record) = renamed {
        saved.insert(new.to_string(), (record, Instant::now()));
    }
    slots.rename(old, new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_persist::{PaneRecord, RecordNode};

    #[test]
    fn locked_refuses_every_acting_request() {
        for r in [
            Request::Exec("quit".into()),
            Request::Exec("run firefox".into()),
            Request::Exec("run-in-terminal htop".into()),
            Request::Open("a".into()),
            Request::OpenFolder("/tmp".into()),
            Request::New("n".into()),
            Request::Rename("r".into()),
            Request::SetRoot(Some("/tmp".into())),
            Request::SetRoot(None),
            Request::RunTui("htop".into()),
            Request::RunTerm("ghostty".into()),
            Request::Relaunch,
            Request::ClosePid(7),
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
        for r in [Request::Candidates, Request::Bindings, Request::Save] {
            assert_eq!(admit_request(true, &r), Ok(()), "{r:?}");
        }
    }

    #[test]
    fn exec_parses_actions_and_refuses_none_and_garbage() {
        assert_eq!(
            parse_exec("run firefox --new"),
            Ok(Action::Run("firefox --new".into()))
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

    fn record(name: &str) -> WorkspaceRecord {
        WorkspaceRecord {
            name: name.into(),
            root: Some("/r".into()),
            focused_pane: 0,
            output: None,
            tree: RecordNode::Pane(PaneRecord {
                focused: 0,
                windows: vec![],
            }),
        }
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("cde-rename-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_failed_save_under_the_new_name_changes_nothing() {
        // A regular file where the store dir should be: every save fails.
        let dir = scratch("fail");
        let blocked = dir.join("store");
        std::fs::write(&blocked, b"x").unwrap();
        let old_file = dir.join("old-file");
        std::fs::write(&old_file, b"x").unwrap();
        let mut slots = Slots::new();
        slots.pin(1, "old");
        let saved = HashMap::from([("old".to_string(), (record("old"), Instant::now()))]);
        let err = save_renamed(&blocked, record("old"), "new");
        // `ipc_rename` stops at this `Err` before the model, pins, `saved` or old file are
        // touched; the Smithay-bound method itself is not constructible in a test.
        assert!(err.is_err());
        assert_eq!(slots.get(1), Some("old"));
        assert!(saved.contains_key("old") && old_file.exists());
        assert!(blocked.is_file(), "the store path is untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_successful_rename_writes_the_new_file_and_moves_pins_and_saved() {
        let dir = scratch("ok");
        let old_path = store::save(&dir, &record("old")).unwrap();
        let mut slots = Slots::new();
        slots.pin(2, "old");
        let mut saved = HashMap::from([("old".to_string(), (record("old"), Instant::now()))]);
        let renamed = save_renamed(&dir, record("old"), "new").unwrap();
        let new_path = dir.join(store::encode_file_name("new").unwrap());
        assert!(finish_rename(
            &mut slots,
            &mut saved,
            "old",
            "new",
            Some(renamed)
        ));
        std::fs::remove_file(&old_path).unwrap();
        assert!(new_path.exists() && !old_path.exists());
        assert_eq!(slots.get(2), Some("new"));
        assert!(!saved.contains_key("old") && saved["new"].0.name == "new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rename_without_a_file_moves_pins_but_saves_nothing() {
        let mut slots = Slots::new();
        slots.pin(3, "old");
        let mut saved = HashMap::from([("old".to_string(), (record("old"), Instant::now()))]);
        assert!(finish_rename(&mut slots, &mut saved, "old", "new", None));
        assert_eq!(slots.get(3), Some("new"));
        assert!(saved.is_empty());
    }
}
