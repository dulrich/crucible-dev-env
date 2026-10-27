//! Workspace persistence wired into the compositor: the 1 s tick (auto-save + config reload),
//! per-window capture by client PID, session record and restore. This module root keeps the
//! tick/save/capture/restore/session code plus the pure helpers those use (`skip_record`,
//! `resolve_cwd`, the picker candidate builders, the capture fallbacks); the rest lives in siblings,
//! each with its own `impl CrucibleState` block and its own slice of [`PersistState`]:
//!
//! - [`spawn`]: spawn registry ([`SpawnKind`]), env/`PATH`, command preflight, the terminal/overlay
//!   argv builders and `spawn_process`.
//! - [`overlay`]: the single overlay slot (picker / launcher / keys) and the pre-warmed overlay host.
//! - [`placement`]: pending placements by PID, the new-toplevel map paths and window classification.
//! - [`workspace`]: workspace verbs (close, close-all, move to output, slot pins) and their names.
//! - [`ipc`]: the IPC request handlers.
//!
//! Decision logic is kept in free functions beside the code that applies it (unit-tested in each
//! module's `tests`); the `impl CrucibleState` blocks gather inputs from the model, `/proc` and the
//! store and apply the results.
//!
//! ## Which windows are terminals
//! By **PID provenance**, not app-id matching: every process CDE spawns through
//! [`CrucibleState::spawn_process`] is remembered with its [`spawn::SpawnKind`], and a toplevel whose client PID
//! is one of them (exact pid, no ancestor walk) of kind [`spawn::SpawnKind::Terminal`] is a terminal,
//! captured with [`crucible_persist::capture_terminal`]. Everything else (apps from `run`, the
//! picker overlay, unknown clients) is recorded as an `app` — with one exception: a non-terminal
//! window whose own argv is a CDE restore wrapper (`crucible relaunch|resume …`, bare or run by
//! the terminal after its exec flag) is recorded as the record the wrapper stands for (see
//! [`crucible_persist::capture::decode_cde_wrapper`]), which may be a `terminal`, so restores
//! never nest wrappers.

mod ipc;
mod overlay;
mod placement;
mod spawn;
mod workspace;

pub use spawn::{SpawnKind, SpawnRecord};
pub use workspace::new_workspace_name;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crucible_layout::{OutputId, WindowId, WorkspaceId};
use crucible_persist::session::{Session, plan_restore, session_path};
use crucible_persist::slots::{Slots, slots_path};
use crucible_persist::{WindowRecord, WorkspaceRecord, store};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
use tracing::{debug, error, info, warn};

use crate::element::WindowElement;
use crate::ipc::{Candidate, CandidateKind};
use crate::state::{Backend, CrucibleState};
use overlay::{Overlay, OverlayState};
use placement::{PENDING_TTL, Pending, Target};
use spawn::{reap_exited, relaunch_args, restore_args};
use workspace::restore_target;

/// Auto-save / config-poll cadence.
pub const TICK: Duration = Duration::from_secs(1);

/// Asks a window to close (xdg `close` or the X11 equivalent).
fn close_element(element: &WindowElement) {
    if let Some(toplevel) = element.toplevel() {
        toplevel.send_close();
    }
    if let Some(x11) = element.x11_surface() {
        let _ = x11.close();
    }
}

/// Compositor-side persistence state (lives in [`CrucibleState::persist`]): the per-concern
/// sub-structs owned by the sibling modules, plus the save/session fields no sibling owns.
#[derive(Debug)]
pub struct PersistState {
    /// Where workspace files live (`$XDG_STATE_HOME/cde/workspaces`).
    pub store_dir: PathBuf,
    /// Named workspace slot pins (`$XDG_STATE_HOME/cde/slots.kdl`, beside the store dir). Empty
    /// until [`CrucibleState::start_persistence`] loads them.
    pub slots: Slots,
    /// The bound IPC socket path, exported to children as `CDE_SOCKET`.
    pub ipc_path: Option<PathBuf>,
    /// Spawned-process registry and child handles (see [`spawn`]).
    spawn: spawn::SpawnState,
    /// The overlay slot and the pre-warmed host (see [`overlay`]).
    overlay: overlay::OverlayRuntime,
    /// Window kinds, pending placements and restore order (see [`placement`]).
    placement: placement::PlacementState,
    /// Workspace roots, last-viewed outputs and the pending slot pin (see [`workspace`]).
    workspace: workspace::WorkspaceState,
    /// Last record written per workspace name, and when.
    saved: HashMap<String, (WorkspaceRecord, Instant)>,
    config_mtime: Option<SystemTime>,
    /// The session record last written (an unchanged session writes nothing).
    session_written: Option<Session>,
    /// Whether the startup session step ran (the record is written only afterwards).
    session_started: bool,
}

impl PersistState {
    /// Empty state saving into `store_dir`.
    #[must_use]
    pub fn new(store_dir: PathBuf) -> Self {
        Self {
            store_dir,
            slots: Slots::new(),
            ipc_path: None,
            spawn: spawn::SpawnState::default(),
            overlay: overlay::OverlayRuntime::default(),
            placement: placement::PlacementState::default(),
            workspace: workspace::WorkspaceState::default(),
            saved: HashMap::new(),
            config_mtime: None,
            session_written: None,
            session_started: false,
        }
    }

    fn saved_names(&self) -> HashSet<String> {
        store::list_saved(&self.store_dir)
            .into_iter()
            .map(|(n, _)| n)
            .collect()
    }

    fn file_of(&self, name: &str) -> Result<PathBuf, String> {
        store::encode_file_name(name)
            .map(|f| self.store_dir.join(f))
            .map_err(|e| e.to_string())
    }

    /// Forgets a closed managed window's kind.
    pub fn forget_window(&mut self, id: WindowId) {
        self.placement.windows.remove(&id);
    }
}

/// Whether a record is not worth writing: one pane, no windows, no folder root.
#[must_use]
pub fn skip_record(record: &WorkspaceRecord) -> bool {
    let panes = record.panes();
    record.root.is_none() && panes.len() == 1 && panes[0].windows.is_empty()
}

/// A restored terminal's working directory: the saved cwd, else the workspace root, else `$HOME`,
/// else `/`.
#[must_use]
pub fn resolve_cwd(
    saved: &Path,
    root: Option<&Path>,
    home: Option<&Path>,
    is_dir: impl Fn(&Path) -> bool,
) -> PathBuf {
    [Some(saved), root, home]
        .into_iter()
        .flatten()
        .find(|p| is_dir(p))
        .map_or_else(|| PathBuf::from("/"), Path::to_path_buf)
}

/// Immediate, non-hidden subdirectories of each root as `(basename, path)`, each root's entries
/// sorted by name, roots in order. Unreadable roots and non-UTF-8 names are skipped.
#[must_use]
pub fn folder_entries(roots: &[PathBuf]) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        let mut dirs: Vec<(String, PathBuf)> = entries
            .filter_map(Result::ok)
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let path = e.path();
                (!name.starts_with('.') && path.is_dir()).then_some((name, path))
            })
            .collect();
        dirs.sort();
        out.extend(dirs);
    }
    out
}

/// The `candidates` list: live (in the given order), then saved not live, then folders.
#[must_use]
pub fn build_candidates(
    live: Vec<(String, Option<PathBuf>)>,
    saved: Vec<String>,
    folders: Vec<(String, PathBuf)>,
) -> Vec<Candidate> {
    let live_names: HashSet<String> = live.iter().map(|(n, _)| n.clone()).collect();
    let mut out: Vec<Candidate> = live
        .into_iter()
        .map(|(name, path)| Candidate {
            kind: CandidateKind::Live,
            name,
            path,
            slot: None,
        })
        .collect();
    out.extend(
        saved
            .into_iter()
            .filter(|n| !live_names.contains(n))
            .map(|name| Candidate {
                kind: CandidateKind::Saved,
                name,
                path: None,
                slot: None,
            }),
    );
    out.extend(folders.into_iter().map(|(name, path)| Candidate {
        kind: CandidateKind::Folder,
        name,
        path: Some(path),
        slot: None,
    }));
    out
}

/// Fills `slot` on live and saved candidates from `slot_of` (folders never carry one).
pub fn apply_slots(candidates: &mut [Candidate], slot_of: impl Fn(&str) -> Option<u8>) {
    for c in candidates
        .iter_mut()
        .filter(|c| c.kind != CandidateKind::Folder)
    {
        c.slot = slot_of(&c.name);
    }
}

fn proc_root() -> &'static Path {
    Path::new("/proc")
}

/// The record for a non-terminal window: a CDE restore wrapper in its raw argv is decoded into the
/// record it stands for (decode before joining); otherwise the plain `app` with the joined argv.
fn app_fallback_record(
    element_app_id: Option<String>,
    argv: Option<&[String]>,
    exec_flag: &str,
) -> WindowRecord {
    use crucible_persist::capture::{CdeWrapper, decode_cde_wrapper, join_command};
    let argv = argv.filter(|a| !a.is_empty());
    match argv.and_then(|a| decode_cde_wrapper(a, exec_flag)) {
        Some(CdeWrapper::App(cap)) => WindowRecord::App {
            app_id: cap.app_id.or(element_app_id),
            cmdline: Some(cap.cmdline),
        },
        Some(CdeWrapper::Terminal(cap)) => WindowRecord::Terminal {
            cwd: cap.cwd,
            command: cap.command,
        },
        None => WindowRecord::App {
            app_id: element_app_id,
            cmdline: argv.map(join_command),
        },
    }
}

/// The record for a CDE-spawned **terminal** whose `/proc` capture found nothing (e.g. saved in
/// the moment between the window mapping and the terminal starting its shell). It never becomes
/// a plain `app` — that record would relaunch the terminal as an app forever. A CDE restore
/// wrapper in its argv decodes as for apps; otherwise it is an idle terminal in the terminal
/// process's own cwd (empty when unreadable, which restore resolves to the root or `$HOME`).
fn terminal_fallback_record(
    argv: Option<&[String]>,
    exec_flag: &str,
    terminal_cwd: Option<PathBuf>,
) -> WindowRecord {
    use crucible_persist::capture::{CdeWrapper, decode_cde_wrapper};
    match argv.and_then(|a| decode_cde_wrapper(a, exec_flag)) {
        Some(CdeWrapper::App(cap)) => WindowRecord::App {
            app_id: cap.app_id,
            cmdline: Some(cap.cmdline),
        },
        Some(CdeWrapper::Terminal(cap)) => WindowRecord::Terminal {
            cwd: cap.cwd,
            command: cap.command,
        },
        None => WindowRecord::Terminal {
            cwd: terminal_cwd.unwrap_or_default(),
            command: None,
        },
    }
}

fn read_argv(pid: u32) -> Option<Vec<String>> {
    let bytes = std::fs::read(proc_root().join(pid.to_string()).join("cmdline")).ok()?;
    Some(crucible_persist::capture::parse_cmdline(&bytes))
}

fn element_app_id(element: &WindowElement) -> Option<String> {
    if let Some(toplevel) = element.toplevel() {
        return with_states(toplevel.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().ok().and_then(|d| d.app_id.clone()))
        });
    }
    element
        .x11_surface()
        .map(|x| x.class())
        .filter(|c| !c.is_empty())
}

fn config_mtime() -> Option<SystemTime> {
    let path = crucible_config::config_path()?;
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// One-time persistence init, called by both backends right after the startup config is
    /// applied (before any output exists): loads the slot pins, binds the IPC socket and arms the
    /// 1 s tick. The output-dependent half is [`Self::start_session`].
    pub fn start_persistence(&mut self) {
        match Slots::load(&slots_path(&self.persist.store_dir)) {
            Ok(slots) => self.persist.slots = slots,
            Err(err) => warn!(%err, "cannot read the slot pins; starting with none"),
        }
        self.start_ipc();
        self.persist.config_mtime = config_mtime();
        let timer = self
            .handle
            .insert_source(Timer::from_duration(TICK), |_, _, data| {
                data.persist_tick();
                TimeoutAction::ToDuration(TICK)
            });
        if let Err(err) = timer {
            error!(%err, "cannot arm the persistence timer; auto-save disabled");
        }
    }

    /// The startup session step, called once by each backend after its initial outputs are
    /// registered: renames the seed off a saved name, then reopens the last session when
    /// `restore-session` is on and the record yields a plan; otherwise (cold start) opens the
    /// picker when saved workspaces exist. Then pre-warms the overlay host. Idempotent.
    pub fn start_session(&mut self) {
        if self.persist.session_started {
            return;
        }
        self.persist.session_started = true;
        let saved = self.persist.saved_names();
        let seed = self.layout.world.active_workspace_id();
        self.rename_seed_off_saved(seed);
        let restored = self.config.restore_session && self.restore_session(&saved);
        if !restored && !saved.is_empty() {
            self.open_picker(false);
        }
        self.start_warm_overlay();
        self.write_session();
    }

    /// Reopens the workspaces named by the session record. `false` (cold start) when there is no
    /// record, it cannot be read, or none of its workspaces could be restored.
    fn restore_session(&mut self, saved: &HashSet<String>) -> bool {
        let record = match Session::load(&session_path(&self.persist.store_dir)) {
            Ok(Some(record)) => record,
            Ok(None) => return false,
            Err(err) => {
                warn!(%err, "cannot read the session record; cold start");
                return false;
            }
        };
        let connected = self.connected_outputs();
        let names: Vec<String> = connected.iter().map(|(n, _)| n.clone()).collect();
        let Some(plan) = plan_restore(&record, saved, &names) else {
            if record.outputs.iter().any(|o| !o.workspaces.is_empty()) {
                warn!("no workspace of the last session is saved; cold start");
            }
            return false;
        };
        let seeds: Vec<WorkspaceId> = self.live_workspaces().into_iter().map(|(_, w)| w).collect();
        let mut restored = false;
        for out in &plan.outputs {
            for name in &out.workspaces {
                let outcome = self
                    .persist
                    .file_of(name)
                    .and_then(|p| store::load(&p).map_err(|e| e.to_string()))
                    .and_then(|r| self.restore_on(r, Some(&out.output)));
                match outcome {
                    Ok(()) => restored = true,
                    Err(reason) => warn!(name, reason, "session workspace not restored"),
                }
            }
        }
        if !restored {
            warn!("no workspace of the last session could be restored; cold start");
            return false;
        }
        // Drop the (empty) seeds the restore replaced; an output's last workspace stays.
        for ws in seeds {
            let empty = self.layout.world.workspace(ws).is_some_and(|w| {
                w.panes()
                    .iter()
                    .filter_map(|p| self.layout.world.pane(*p))
                    .all(|p| p.windows.is_empty())
            });
            if empty
                && self
                    .with_model(|w| w.remove_workspace(ws).map(|_| ()))
                    .is_ok()
            {
                self.persist.workspace.roots.remove(&ws);
                self.persist.workspace.last_output.remove(&ws);
            }
        }
        let id_of = |name: &str| connected.iter().find(|(n, _)| n == name).map(|(_, id)| *id);
        for out in &plan.outputs {
            let (Some(oid), Some(ws)) = (
                id_of(&out.output),
                out.focused
                    .as_deref()
                    .and_then(|n| self.layout.world.workspace_by_name(n)),
            ) else {
                continue;
            };
            if let Err(err) = self.with_model(|w| {
                w.set_focused_output(oid)?;
                w.switch_workspace(ws)
            }) {
                warn!(%err, "cannot restore an output's focused workspace");
            }
        }
        let focus = plan
            .focused_output
            .as_deref()
            .or(plan.outputs.first().map(|o| o.output.as_str()))
            .and_then(id_of);
        if let Some(oid) = focus {
            if let Err(err) = self.with_model(|w| w.set_focused_output(oid)) {
                warn!(%err, "cannot restore the focused output");
            }
        }
        info!("last session restored");
        true
    }

    /// Connected (Space-mapped, model-bound) outputs as `(connector name, model id)`.
    fn connected_outputs(&self) -> Vec<(String, OutputId)> {
        self.space
            .outputs()
            .filter_map(|o| Some((o.name(), crate::layout::model_output_id(o)?)))
            .collect()
    }

    /// The live session: each connected output's workspaces in stack order and its active one,
    /// plus the focused output.
    fn capture_session(&self) -> Session {
        let world = &self.layout.world;
        let mut connected = self.connected_outputs();
        connected.sort_by_key(|(_, id)| *id);
        let name_of = |ws: WorkspaceId| world.workspace(ws).map(|w| w.name.clone());
        let outputs = connected
            .iter()
            .filter_map(|(name, id)| {
                let o = world.output(*id)?;
                Some(crucible_persist::session::OutputSession {
                    output: name.clone(),
                    workspaces: o.workspaces.iter().filter_map(|w| name_of(*w)).collect(),
                    focused: name_of(o.active_workspace()),
                })
            })
            .collect();
        let focused = world.focused_output();
        Session {
            outputs,
            focused_output: connected
                .into_iter()
                .find(|(_, id)| *id == focused)
                .map(|(n, _)| n),
        }
    }

    /// Writes `session.kdl` when the live session differs from the last written one. A no-op
    /// before [`Self::start_session`] (the startup seed must never replace the last record).
    fn write_session(&mut self) {
        if !self.persist.session_started {
            return;
        }
        let session = self.capture_session();
        if self.persist.session_written.as_ref() == Some(&session) {
            return;
        }
        match session.save(&session_path(&self.persist.store_dir)) {
            Ok(()) => self.persist.session_written = Some(session),
            Err(err) => warn!(%err, "cannot write the session record"),
        }
    }

    /// The 1 s tick: expire pending placements, auto-save dirty workspaces, poll the config.
    pub fn persist_tick(&mut self) {
        let now = Instant::now();
        self.persist
            .placement
            .pending
            .retain(|_, p| now.saturating_duration_since(p.since) < PENDING_TTL);
        if let Some(Overlay {
            state: OverlayState::Launching(pid),
            kind,
            ..
        }) = self.persist.overlay.overlay
        {
            if !self.persist.placement.pending.contains_key(&pid) {
                warn!(pid, ?kind, "overlay never mapped; giving up on it");
                self.persist.overlay.overlay = None;
                self.persist.workspace.pending_pin = None;
            }
        }
        let reaped = reap_exited(&mut self.persist.spawn.children);
        if !reaped.is_empty() {
            debug!(?reaped, "reaped exited children");
        }
        self.warm_reaped(&reaped);
        self.save_dirty(now, TICK);
        self.write_session();
        self.poll_config();
    }

    /// Writes every dirty workspace now, bypassing the interval (`save`, `Super+Shift+Q`).
    pub fn flush_workspaces(&mut self) {
        self.save_dirty(Instant::now(), Duration::ZERO);
        self.write_session();
    }

    /// Removes the IPC socket file (clean quit).
    pub fn remove_ipc_socket(&mut self) {
        if let Some(path) = self.persist.ipc_path.take() {
            let _ = std::fs::remove_file(path);
        }
    }

    fn live_workspaces(&self) -> Vec<(OutputId, WorkspaceId)> {
        let mut outs: Vec<_> = self.layout.world.outputs().collect();
        outs.sort_by_key(|(id, _)| *id);
        outs.into_iter()
            .flat_map(|(oid, o)| o.workspaces.iter().map(move |ws| (oid, *ws)))
            .collect()
    }

    fn save_dirty(&mut self, now: Instant, min_interval: Duration) {
        for (_, ws) in self.live_workspaces() {
            // A restore in progress would overwrite the saved commands with half-mapped panes.
            if self.workspace_has_pending(ws) {
                continue;
            }
            let Some(record) = self.capture_record(ws) else {
                continue;
            };
            if skip_record(&record) {
                continue;
            }
            let last = self.persist.saved.get(&record.name);
            let dirty = last.map(|(r, _)| r) != Some(&record);
            if crucible_persist::should_save(dirty, last.map(|(_, t)| *t), now, min_interval) {
                self.write_record(record, now);
            }
        }
    }

    fn write_record(&mut self, record: WorkspaceRecord, now: Instant) -> bool {
        match store::save(&self.persist.store_dir, &record) {
            Ok(path) => {
                info!(name = record.name, path = %path.display(), "workspace saved");
                self.persist
                    .saved
                    .insert(record.name.clone(), (record, now));
                true
            }
            Err(err) => {
                warn!(name = record.name, %err, "workspace save failed");
                false
            }
        }
    }

    fn save_now(&mut self, ws: WorkspaceId) -> Result<(), String> {
        let record = self
            .capture_record(ws)
            .ok_or_else(|| "workspace vanished".to_string())?;
        if self.write_record(record, Instant::now()) {
            Ok(())
        } else {
            Err("save failed (see the compositor log)".into())
        }
    }

    fn poll_config(&mut self) {
        let mtime = config_mtime();
        if mtime == self.persist.config_mtime {
            return;
        }
        self.persist.config_mtime = mtime;
        let Some(path) = crucible_config::config_path() else {
            return;
        };
        if mtime.is_none() {
            return;
        }
        match crucible_config::load(&path) {
            Ok(config) => {
                info!(path = %path.display(), "config changed; reloading");
                self.apply_config(config);
            }
            Err(err) => crate::config::log_config_error(&path, &err),
        }
    }

    fn capture_record(&self, ws: WorkspaceId) -> Option<WorkspaceRecord> {
        let world = &self.layout.world;
        let mut records = HashMap::new();
        for pane in world.workspace(ws)?.panes() {
            for win in &world.pane(pane)?.windows {
                records.insert(*win, self.window_record(*win));
            }
        }
        let mut record = WorkspaceRecord::from_world(
            world,
            ws,
            self.persist.workspace.roots.get(&ws).cloned(),
            |id| {
                records.get(&id).cloned().unwrap_or(WindowRecord::App {
                    app_id: None,
                    cmdline: None,
                })
            },
        )?;
        record.output = self.persist.workspace.last_output.get(&ws).cloned();
        Some(record)
    }

    fn window_record(&self, id: WindowId) -> WindowRecord {
        let info = self.persist.placement.windows.get(&id).copied();
        let pid = info.and_then(|i| i.pid);
        if let (Some(pid), true) = (pid, info.is_some_and(|i| i.terminal)) {
            // A relaunch placeholder still waiting for Enter keeps its original app record.
            if let Some(cap) = crucible_persist::capture_relaunch(proc_root(), pid) {
                return WindowRecord::App {
                    app_id: cap.app_id,
                    cmdline: Some(cap.cmdline),
                };
            }
            if let Some(cap) = crucible_persist::capture_terminal(proc_root(), pid) {
                return WindowRecord::Terminal {
                    cwd: cap.cwd,
                    command: cap.command,
                };
            }
            return terminal_fallback_record(
                read_argv(pid).as_deref(),
                &self.config.terminal.exec_flag,
                std::fs::read_link(proc_root().join(pid.to_string()).join("cwd")).ok(),
            );
        }
        let argv = pid.and_then(read_argv);
        app_fallback_record(
            self.layout.element(id).and_then(element_app_id),
            argv.as_deref(),
            &self.config.terminal.exec_flag,
        )
    }

    fn candidates(&self) -> Vec<Candidate> {
        let world = &self.layout.world;
        let live = self
            .live_workspaces()
            .into_iter()
            .filter_map(|(_, ws)| {
                let name = world.workspace(ws)?.name.clone();
                Some((name, self.persist.workspace.roots.get(&ws).cloned()))
            })
            .collect();
        let saved = store::list_saved(&self.persist.store_dir)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        let mut out = build_candidates(live, saved, folder_entries(&self.config.picker_roots));
        if self.config.picker_slots {
            apply_slots(&mut out, |n| self.persist.slots.slot_of(n));
        }
        out
    }

    fn restore(&mut self, record: WorkspaceRecord) -> Result<(), String> {
        let wanted = record.output.clone();
        self.restore_on(record, wanted.as_deref())
    }

    /// Restores `record` onto the connected output named `wanted` (else the focused output).
    fn restore_on(&mut self, record: WorkspaceRecord, wanted: Option<&str>) -> Result<(), String> {
        let connected = self.connected_outputs();
        let output = restore_target(wanted, &connected, self.layout.world.focused_output());
        let mut created = None;
        self.with_model(|w| {
            // `switch_workspace` only searches the focused output, so focus the target first.
            w.set_focused_output(output)?;
            let (ws, panes) = w.insert_workspace(output, record.name.clone(), &record.layout())?;
            w.switch_workspace(ws)?;
            created = Some((ws, panes));
            Ok(())
        })
        .map_err(|e| e.to_string())?;
        let (ws, panes) = created.expect("set on success");
        // Seed the last-viewed output from the record; a viewing event (apply_layout) wins.
        if let Some(name) = &record.output {
            self.persist
                .workspace
                .last_output
                .entry(ws)
                .or_insert_with(|| name.clone());
        }
        if let Some(root) = &record.root {
            self.persist.workspace.roots.insert(ws, root.clone());
        }
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let crucible = std::env::current_exe().map_err(|e| e.to_string())?;
        let program = self.terminal_program();
        let exec_flag = self.config.terminal.exec_flag.clone();
        let mut jobs = Vec::new();
        for (pane, pane_record) in panes.iter().zip(record.panes()) {
            for (i, window) in pane_record.windows.iter().enumerate() {
                let target = Target::Pane {
                    pane: *pane,
                    saved_index: i,
                    focused: pane_record.focused == i,
                };
                match window {
                    WindowRecord::Terminal { cwd, command } => {
                        let dir =
                            resolve_cwd(cwd, record.root.as_deref(), home.as_deref(), Path::is_dir);
                        let args = restore_args(&exec_flag, &crucible, &dir, command.as_deref());
                        jobs.push((dir, args, target));
                    }
                    WindowRecord::App {
                        app_id,
                        cmdline: Some(cmdline),
                    } => {
                        let root = record.root.as_deref();
                        let dir = resolve_cwd(
                            root.unwrap_or(Path::new("")),
                            root,
                            home.as_deref(),
                            Path::is_dir,
                        );
                        let args = relaunch_args(&exec_flag, &crucible, app_id.as_deref(), cmdline);
                        jobs.push((dir, args, target));
                    }
                    WindowRecord::App { cmdline: None, .. } => {}
                }
            }
        }
        let now = Instant::now();
        for (dir, args, target) in jobs {
            let args = self.with_tiling_args(args);
            if let Some(pid) = self.spawn_process(&program, &args, Some(&dir), SpawnKind::Terminal)
            {
                self.persist
                    .placement
                    .pending
                    .insert(pid, Pending { target, since: now });
            }
        }
        info!(name = record.name, "workspace restored");
        self.persist
            .saved
            .insert(record.name.clone(), (record, now));
        Ok(())
    }

    fn name_taken(&self, name: &str, saved: &HashSet<String>) -> bool {
        self.layout.world.workspace_by_name(name).is_some() || saved.contains(name)
    }

    fn create_named(&mut self, name: String, root: Option<PathBuf>) -> Result<(), String> {
        let mut created = None;
        self.update_model(|w| created = Some(w.new_workspace(name)));
        let ws = created.expect("set");
        if let Some(root) = root {
            self.persist.workspace.roots.insert(ws, root);
        }
        self.save_now(ws)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_persist::{PaneRecord, RecordNode};

    fn record(root: Option<&str>, tree: RecordNode) -> WorkspaceRecord {
        WorkspaceRecord {
            name: "w".into(),
            root: root.map(PathBuf::from),
            focused_pane: 0,
            output: None,
            tree,
        }
    }

    fn pane(windows: Vec<WindowRecord>) -> RecordNode {
        RecordNode::Pane(PaneRecord {
            focused: 0,
            windows,
        })
    }

    #[test]
    fn skip_only_the_empty_single_pane_without_root() {
        assert!(skip_record(&record(None, pane(vec![]))));
        assert!(!skip_record(&record(Some("/r"), pane(vec![]))));
        let term = WindowRecord::Terminal {
            cwd: "/".into(),
            command: None,
        };
        assert!(!skip_record(&record(None, pane(vec![term]))));
        let split = RecordNode::Split {
            axis: crucible_layout::Axis::Horizontal,
            ratio: 0.5,
            first: Box::new(pane(vec![])),
            second: Box::new(pane(vec![])),
        };
        assert!(!skip_record(&record(None, split)));
    }

    #[test]
    fn cwd_falls_back_to_root_then_home() {
        let ok =
            |dirs: &'static [&'static str]| move |p: &Path| dirs.iter().any(|d| Path::new(d) == p);
        let (saved, root, home) = (
            Path::new("/s"),
            Some(Path::new("/r")),
            Some(Path::new("/h")),
        );
        assert_eq!(
            resolve_cwd(saved, root, home, ok(&["/s", "/r"])),
            PathBuf::from("/s")
        );
        assert_eq!(
            resolve_cwd(saved, root, home, ok(&["/r", "/h"])),
            PathBuf::from("/r")
        );
        assert_eq!(
            resolve_cwd(saved, None, home, ok(&["/h"])),
            PathBuf::from("/h")
        );
        assert_eq!(resolve_cwd(saved, root, home, ok(&[])), PathBuf::from("/"));
    }

    #[test]
    fn candidates_order_live_saved_folder_and_dedupe_live() {
        let c = build_candidates(
            vec![("b".into(), None), ("a".into(), Some("/r".into()))],
            vec!["a".into(), "z".into()],
            vec![("proj".into(), "/code/proj".into())],
        );
        let got: Vec<(CandidateKind, &str)> = c.iter().map(|c| (c.kind, c.name.as_str())).collect();
        assert_eq!(
            got,
            [
                (CandidateKind::Live, "b"),
                (CandidateKind::Live, "a"),
                (CandidateKind::Saved, "z"),
                (CandidateKind::Folder, "proj"),
            ]
        );
    }

    #[test]
    fn apply_slots_fills_live_and_saved_but_never_folders() {
        let mut c = build_candidates(
            vec![("a".into(), None)],
            vec!["z".into()],
            vec![("a".into(), "/code/a".into())],
        );
        assert!(c.iter().all(|c| c.slot.is_none()));
        apply_slots(&mut c, |n| match n {
            "a" => Some(2),
            "z" => Some(9),
            _ => None,
        });
        let got: Vec<(CandidateKind, Option<u8>)> = c.iter().map(|c| (c.kind, c.slot)).collect();
        assert_eq!(
            got,
            [
                (CandidateKind::Live, Some(2)),
                (CandidateKind::Saved, Some(9)),
                (CandidateKind::Folder, None),
            ]
        );
    }

    #[test]
    fn folder_entries_lists_sorted_visible_subdirectories() {
        let root = std::env::temp_dir().join(format!("cde-folders-{}", std::process::id()));
        for d in ["b", "a", ".hidden"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("file"), b"").unwrap();
        let got = folder_entries(&[root.clone(), root.join("missing")]);
        let names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(got[0].1, root.join("a"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn app_fallback_decodes_wrappers_before_joining() {
        let v = |a: &[&str]| a.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        let id = || Some("elem".to_string());
        let wrapped = v(&[
            "ghostty",
            "-e",
            "/x/crucible",
            "relaunch",
            "--",
            "firefox -p",
        ]);
        assert_eq!(
            app_fallback_record(id(), Some(&wrapped), "-e"),
            WindowRecord::App {
                app_id: id(),
                cmdline: Some("firefox -p".into())
            }
        );
        let tui = v(&[
            "g",
            "-e",
            "/x/crucible",
            "resume",
            "--now",
            "--exit",
            "--cwd",
            "/p",
            "--",
            "vim",
        ]);
        assert_eq!(
            app_fallback_record(id(), Some(&tui), "-e"),
            WindowRecord::Terminal {
                cwd: "/p".into(),
                command: Some("vim".into())
            }
        );
        let plain = v(&["firefox", "--new"]);
        assert_eq!(
            app_fallback_record(id(), Some(&plain), "-e"),
            WindowRecord::App {
                app_id: id(),
                cmdline: Some("firefox --new".into())
            }
        );
        assert_eq!(
            app_fallback_record(None, None, "-e"),
            WindowRecord::App {
                app_id: None,
                cmdline: None
            }
        );
    }

    #[test]
    fn uncaptured_terminal_never_becomes_an_app() {
        let v = |a: &[&str]| a.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        // A bare terminal saved before its shell started (the host-b 2026-10-27 regression).
        let bare = v(&["ghostty", "--gtk-single-instance=false"]);
        assert_eq!(
            terminal_fallback_record(Some(&bare), "-e", Some("/w".into())),
            WindowRecord::Terminal {
                cwd: "/w".into(),
                command: None
            }
        );
        assert_eq!(
            terminal_fallback_record(None, "-e", None),
            WindowRecord::Terminal {
                cwd: PathBuf::new(),
                command: None
            }
        );
        // A pending restore banner still decodes to its command.
        let resume = v(&[
            "g",
            "-e",
            "/x/crucible",
            "resume",
            "--cwd",
            "/p",
            "--",
            "htop",
        ]);
        assert_eq!(
            terminal_fallback_record(Some(&resume), "-e", Some("/w".into())),
            WindowRecord::Terminal {
                cwd: "/p".into(),
                command: Some("htop".into())
            }
        );
        // A relaunch placeholder whose child already exited keeps its app record.
        let relaunch = v(&["g", "-e", "/x/crucible", "relaunch", "--", "firefox"]);
        assert_eq!(
            terminal_fallback_record(Some(&relaunch), "-e", Some("/w".into())),
            WindowRecord::App {
                app_id: None,
                cmdline: Some("firefox".into())
            }
        );
    }
}
