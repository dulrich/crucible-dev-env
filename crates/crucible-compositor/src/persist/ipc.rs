//! The IPC request handlers: `handle_ipc` and its per-request helpers (open, new, rename,
//! relaunch, open-folder).

use std::path::Path;
use std::time::Instant;

use crucible_layout::{WindowId, WorkspaceId};
use crucible_persist::store;
use tracing::info;

use super::placement::{ANCESTOR_DEPTH, Pending, Target, match_pending, slot_for};
use super::workspace::folder_workspace_name;
use super::{close_element, proc_root};
use crate::ipc::{Request, Response};
use crate::state::{Backend, CrucibleState};

/// The IPC response for a checked spawn: `ok` once spawned, else the refusal reason.
fn run_response(spawned: Result<u32, String>) -> Response {
    match spawned {
        Ok(_) => Response::Ok(String::new()),
        Err(reason) => Response::Err(reason),
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    // ---- IPC -----------------------------------------------------------------------------------

    /// Answers one IPC request. `peer` is the connecting process's PID (`SO_PEERCRED`); only
    /// `relaunch` uses it.
    pub fn handle_ipc(&mut self, request: Request, peer: Option<u32>) -> Response {
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
            Request::Run(cmd) => {
                return run_response(self.run_checked(&cmd));
            }
            Request::Exec(text) => {
                return match crucible_keys::bindings::parse_action_text(&text) {
                    Err(e) => Response::Err(e.to_string()),
                    Ok(None) => Response::Err("`none` is not a runnable action".to_string()),
                    Ok(Some(action)) => {
                        self.close_mapped_overlay();
                        self.handle_key_action(action);
                        Response::Ok(String::new())
                    }
                };
            }
            Request::RunInTerminal(cmd) => {
                return run_response(self.run_in_terminal_checked(&cmd));
            }
            Request::RunTui(cmd) => {
                return run_response(self.run_tui_checked(&cmd));
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
        let capture = crucible_persist::parse_relaunch_banner(&args)
            .ok_or_else(|| "not a relaunch placeholder".to_string())?;
        let ancestors = crucible_persist::ancestors(proc_root(), peer, ANCESTOR_DEPTH);
        let windows = &self.persist.placement.windows;
        let pid = match_pending(peer, &ancestors, |p| {
            windows.values().any(|i| i.pid == Some(p))
        })
        .ok_or_else(|| "no placeholder window".to_string())?;
        let mut ids: Vec<WindowId> = windows
            .iter()
            .filter(|(_, i)| i.pid == Some(pid))
            .map(|(id, _)| *id)
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
        let pid = self
            .spawn_command_in(&capture.cmdline, ws)
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
        self.with_model(|w| w.rename_workspace(ws, new))
            .map_err(|e| e.to_string())?;
        // Pins follow a user-driven rename (and only that one).
        if self.persist.slots.rename(&old, new) {
            self.save_slots();
        }
        let old_file = self.persist.file_of(&old).ok();
        let had_file = old_file.as_ref().is_some_and(|f| f.exists());
        self.persist.saved.remove(&old);
        if had_file {
            // Save under the new name first, then drop the old file.
            self.save_now(ws)?;
            if let Some(f) = old_file {
                let _ = std::fs::remove_file(f);
            }
        }
        info!(from = old, to = new, "workspace renamed");
        Ok(())
    }
}
