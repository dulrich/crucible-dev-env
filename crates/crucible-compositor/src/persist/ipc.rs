//! The IPC request handlers: `handle_ipc` and its per-request helpers (open, new, rename,
//! relaunch, open-folder).

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use crucible_layout::{WindowId, WorkspaceId};
use crucible_persist::slots::Slots;
use crucible_persist::{WorkspaceRecord, store};
use tracing::info;

use super::placement::{ANCESTOR_DEPTH, Pending, Target, match_pending, slot_for};
use super::workspace::folder_workspace_name;
use super::{close_element, proc_root};
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
        };
        match result {
            Ok(()) => Response::Ok(String::new()),
            Err(reason) => Response::Err(reason),
        }
    }

    /// Swaps the placeholder window run by `peer` for its app: spawn the saved cmdline, register
    /// the placeholder's live slot as pending, close the placeholder.
    pub(super) fn ipc_relaunch(&mut self, peer: Option<u32>) -> Result<(), String> {
        let peer = peer.ok_or_else(|| "not a relaunch placeholder".to_string())?;
        let args = std::fs::read(proc_root().join(peer.to_string()).join("cmdline"))
            .map(|b| crucible_persist::capture::parse_cmdline(&b))
            .unwrap_or_default();
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
        let world = &self.layout.world;
        let (id, pane, index, focused) = ids
            .into_iter()
            .find_map(|id| {
                let pane_id = world.pane_of_window(id)?;
                let pane = world.pane(pane_id)?;
                let (index, focused) = slot_for(pane.windows(), pane.focused_index(), id)?;
                Some((id, pane_id, index, focused))
            })
            .ok_or_else(|| "no placeholder window".to_string())?;
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
        let saved_index = self
            .persist
            .placement
            .placed
            .get(&pane)
            .and_then(|v| v.iter().find(|(w, _)| *w == id).map(|(_, i)| *i));
        let element = self.layout.element(id).cloned();
        // The app stands for the placeholder's saved record: carried as the spawn's origin.
        let origin = crucible_persist::WindowRecord::App {
            app_id: capture.app_id.clone(),
            cmdline: Some(capture.cmdline.clone()),
        };
        let pid = self
            .spawn_command_with(&capture.cmdline, ws, Some(origin))
            .ok_or_else(|| "spawn failed".to_string())?;
        self.persist.placement.pending.insert(
            pid,
            Pending {
                target: Target::Slot {
                    pane,
                    index,
                    focused,
                    saved_index,
                },
                since: Instant::now(),
            },
        );
        if let Some(element) = element {
            close_element(&element);
        }
        info!(
            pid,
            cmdline = capture.cmdline,
            "relaunching app into its placeholder slot"
        );
        Ok(())
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
            .roots
            .iter()
            .find(|(ws, root)| {
                root.as_path() == path && self.layout.world.workspace(**ws).is_some()
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
            Request::RunTui("htop".into()),
            Request::RunTerm("ghostty".into()),
            Request::Relaunch,
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
