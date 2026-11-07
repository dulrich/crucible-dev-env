//! Workspace persistence wired into the compositor: the 1 s tick (auto-save + config reload),
//! per-window capture by client PID, session record and restore. This module root keeps the
//! tick/save/capture/restore/session code plus the pure helpers those use (`skip_record`,
//! the picker candidate builders); the rest lives in siblings,
//! each with its own `impl CrucibleState` block and its own slice of [`PersistState`]:
//!
//! - [`spawn`]: the one spawn (`spawn(SpawnRequest)`), child env/`PATH`, preflight and the spawn
//!   verbs; its `registry` holds the PID-provenance registry ([`SpawnKind`]). Terminal argv is
//!   `crucible_persist::Invocation`; restore planning is `crucible_persist::restore`.
//! - [`overlay`]: the single overlay slot (picker / launcher / keys) and the pre-warmed overlay host.
//! - [`placement`]: pending placements by PID, the new-toplevel map paths and window classification.
//! - [`workspace`]: workspace verbs (close, close-all, move to output, slot pins) and their names.
//! - [`ipc`]: the IPC request handlers.
//!
//! Decision logic is kept in free functions beside the code that applies it (unit-tested in each
//! module's `tests`); the `impl CrucibleState` blocks gather inputs from the model, `/proc` and the
//! store and apply the results.
//!
//! ## How windows are classified (origin first)
//! By **PID provenance**, not app-id matching: every process CDE spawns through
//! [`CrucibleState::spawn`] is held in the `spawn::SpawnRegistry` with its
//! [`spawn::SpawnKind`] and, for a restored window or a relaunched app, its **origin** — the saved
//! record it stands for. A reaped child leaves the registry with its record. At map time a
//! toplevel whose client PID is itself (exact pid) a live [`spawn::SpawnKind::Terminal`] spawn is
//! a terminal; the spawn matched by pid or ancestor hands its origin to the window's meta. Capture
//! is one pure classifier, [`crucible_persist::window_record`]: the origin wins whenever the
//! foreground is CDE's own wrapper or `/proc` finds nothing, a live shell/foreground capture wins
//! otherwise, and a wrapper with no origin (one that outlived a compositor restart) is decoded —
//! so saved records never nest `crucible resume|relaunch`, and an uncaptured terminal is never an
//! app.

mod candidates;
pub(crate) mod identity;
mod ipc;
mod overlay;
mod placement;
mod recovery;
mod spawn;
mod warm_slot;
mod workspace;

pub use identity::{ClientLink, GRANT_ENV, WindowIdentity};
pub use spawn::{SpawnKind, SpawnRecord, SpawnRequest};
pub use workspace::{new_workspace_name, startup_seed_name};

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crucible_layout::{OutputId, PaneId, WindowId, WorkspaceId};
use crucible_persist::session::{Session, plan_restore, session_path};
use crucible_persist::slots::{Slots, slots_path};
use crucible_persist::{CaptureEnv, WindowRecord, WorkspaceRecord, store};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
use tracing::{debug, error, info, warn};

use crate::element::WindowElement;
use crate::state::{Backend, CrucibleState};
use candidates::SavedRootCache;
use overlay::Overlay;
pub(crate) use overlay::PickerMode;
use placement::{PENDING_TTL, Pending, Target};
use workspace::restore_target;

/// Auto-save / config-poll cadence.
pub const TICK: Duration = Duration::from_secs(1);

/// Asks a window to close (xdg `close` or the X11 equivalent).
pub(crate) fn close_element(element: &WindowElement) {
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
    spawn: spawn::SpawnRegistry,
    /// The overlay slot and the pre-warmed host (see [`overlay`]).
    overlay: overlay::OverlayRuntime,
    /// Window kinds, pending placements and restore order (see [`placement`]).
    placement: placement::PlacementState,
    /// Workspace roots, last-viewed outputs and the pending slot pin (see [`workspace`]).
    workspace: workspace::WorkspaceState,
    /// Last record written per workspace name, and when.
    saved: HashMap<String, (WorkspaceRecord, Instant)>,
    /// Roots read from saved files for the picker, keyed by file and mtime.
    picker_roots: SavedRootCache,
    config_mtime: Option<SystemTime>,
    /// The session record last written (an unchanged session writes nothing).
    session_written: Option<Session>,
    /// Whether the startup session step ran (the record is written only afterwards).
    session_started: bool,
    /// Surface-keyed window identities (see [`identity`]); dropped on surface destroy.
    identities: identity::SurfaceIdentities,
    /// The recovery set (crucible-term plan 03): the last record of every bound window, set at map
    /// and refreshed every tick whatever the save gate says; a lost window's placeholder is built
    /// from it. Dropped when the window closes.
    recovery: HashMap<WindowId, WindowRecord>,
}

impl PersistState {
    /// Empty state saving into `store_dir`.
    #[must_use]
    pub fn new(store_dir: PathBuf) -> Self {
        Self {
            store_dir,
            slots: Slots::new(),
            ipc_path: None,
            spawn: spawn::SpawnRegistry::default(),
            overlay: overlay::OverlayRuntime::default(),
            placement: placement::PlacementState::default(),
            workspace: workspace::WorkspaceState::default(),
            saved: HashMap::new(),
            identities: identity::SurfaceIdentities::default(),
            recovery: HashMap::new(),
            picker_roots: SavedRootCache::default(),
            config_mtime: None,
            session_written: None,
            session_started: false,
        }
    }

    pub(crate) fn saved_names(&self) -> HashSet<String> {
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
}

/// Whether a record is not worth writing: one pane, no windows, no folder root.
#[must_use]
pub fn skip_record(record: &WorkspaceRecord) -> bool {
    let panes = record.panes();
    record.root.is_none() && panes.len() == 1 && panes[0].windows.is_empty()
}

/// Whether `save_dirty` may write `record`: a skippable (empty) record is still written when an
/// earlier save of that workspace exists, so a workspace emptied by a move does not keep a stale
/// file that restores its moved windows a second time. Never-saved empty seeds stay unsaved.
#[must_use]
pub fn should_write(record: &WorkspaceRecord, previously_saved: bool) -> bool {
    !skip_record(record) || previously_saved
}

fn proc_root() -> &'static Path {
    Path::new("/proc")
}

/// What capture reads: `/proc` and the claude home, where claude keeps its per-pid session
/// records (`<claude-home>/sessions`). The home is `$CLAUDE_CONFIG_DIR`, else `$HOME/.claude`.
/// Read per capture (cheap), with the running binary's basename resolved once for it; a missing
/// home yields a relative path that simply reads nothing.
pub(super) fn capture_env() -> CaptureEnv {
    let home = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude")))
        .unwrap_or_default();
    CaptureEnv {
        proc_root: proc_root().to_path_buf(),
        claude_home: home,
        own_exe: crucible_persist::own_exe_name(),
    }
}

pub(super) fn element_app_id(element: &WindowElement) -> Option<String> {
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

/// What [`CrucibleState::capture_record`] yields for a workspace.
pub(super) enum Capture {
    /// The record to write.
    Record(WorkspaceRecord),
    /// An ephemeral (`run-on`) workspace: never written.
    Ephemeral,
    /// No such workspace.
    Gone,
}

impl Capture {
    /// The record, if there is one to write.
    pub(super) fn record(self) -> Option<WorkspaceRecord> {
        match self {
            Capture::Record(record) => Some(record),
            Capture::Ephemeral | Capture::Gone => None,
        }
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Ends a window: kill its CDE-spawned child `pid` (so a terminal never asks to confirm
    /// closing a running process), else politely ask `element` to close.
    fn end_window(&mut self, pid: Option<u32>, element: Option<&WindowElement>) {
        if !pid.is_some_and(|p| self.persist.spawn.kill(p)) {
            if let Some(element) = element {
                close_element(element);
            }
        }
    }

    /// Ends a relaunch placeholder whose slot an app took. Its `crucible relaunch` is still
    /// waiting for Enter, so asking the terminal window to close would make the terminal confirm
    /// killing a running process (ghostty's "Close window?" dialog). End the wrapper instead
    /// (found by one direct-child lookup from the terminal pid); its terminal then closes on its
    /// own. Only when no wrapper is found is the window asked to close. A bound cterm window is
    /// looked up from its identity's root (see [`crate::layout::WindowMeta::anchor`]).
    fn end_placeholder(&mut self, id: WindowId) {
        let wrapper = self
            .layout
            .meta(id)
            .and_then(crate::layout::WindowMeta::anchor)
            .and_then(|anchor| crucible_persist::relaunch_wrapper_of(&capture_env(), anchor));
        let ended = wrapper.is_some_and(|(pid, _)| placement::terminate(pid));
        if !ended && let Some(element) = self.layout.element(id) {
            close_element(element);
        }
    }

    /// One-time persistence init, called by both backends right after the startup config is
    /// applied (before any output exists): loads the slot pins, binds the IPC socket and arms the
    /// 1 s tick. The output-dependent half is [`Self::start_session`].
    pub fn start_persistence(&mut self) {
        match Slots::load(&slots_path(&self.persist.store_dir)) {
            Ok(slots) => self.persist.slots = slots,
            Err(err) => warn!(%err, "cannot read the slot pins; starting with none"),
        }
        // The header preferences come from the raw record, independent of workspace restore
        // (and of `restore-session`); a missing or unreadable record just means none.
        match Session::load(&session_path(&self.persist.store_dir)) {
            Ok(Some(record)) => {
                self.header.prefs = crucible_persist::session::header_prefs(&record)
            }
            Ok(None) => {}
            Err(err) => warn!(%err, "cannot read the session record; no header preferences"),
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
        self.start_idle_timer();
    }

    /// The startup session step, called once by each backend after its initial outputs are
    /// registered (the seed was already named off the saved names when `LayoutState` was built):
    /// reopens the last session when `restore-session` is on and the record yields a plan;
    /// otherwise (cold start) opens the picker when saved workspaces exist. Then pre-warms the
    /// overlay host. Idempotent.
    pub fn start_session(&mut self) {
        if self.persist.session_started {
            return;
        }
        self.persist.session_started = true;
        let saved = self.persist.saved_names();
        let restored = self.config.restore_session && self.restore_session(&saved);
        if !restored && !saved.is_empty() {
            self.open_picker(PickerMode::Pick);
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
            let panes = self
                .layout
                .world
                .workspace(ws)
                .map(|w| w.panes())
                .unwrap_or_default();
            let empty = !panes.is_empty()
                && panes
                    .iter()
                    .filter_map(|p| self.layout.world.pane(*p))
                    .all(|p| p.windows().is_empty());
            if empty
                && self
                    .with_model(|w| w.remove_workspace(ws).map(|_| ()))
                    .is_ok()
            {
                // Every pane is empty, so no restore can be pending on it; the prune is a no-op.
                self.forget_workspace(ws, &panes);
            }
        }
        let id_of = |name: &str| connected.iter().find(|(n, _)| n == name).map(|(_, id)| *id);
        for out in &plan.outputs {
            let (Some(_), Some(ws)) = (
                id_of(&out.output),
                out.focused
                    .as_deref()
                    .and_then(|n| self.layout.world.workspace_by_name(n)),
            ) else {
                continue;
            };
            // `switch_workspace` focuses the owning output itself.
            if let Err(err) = self.with_model(|w| w.switch_workspace(ws)) {
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
        let outputs = session_outputs(world, &connected, |ws| {
            self.persist.workspace.is_ephemeral(ws)
        });
        let outputs = crucible_persist::session::with_header_prefs(outputs, &self.header.prefs);
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
    pub(crate) fn write_session(&mut self) {
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
            kind,
            pid,
            element: None,
        }) = self.persist.overlay.overlay
        {
            if !self.persist.placement.pending.contains_key(&pid) {
                warn!(pid, ?kind, "overlay never mapped; giving up on it");
                self.clear_overlay_slot();
            }
        }
        let reaped = self.persist.spawn.reap();
        if !reaped.is_empty() {
            debug!(?reaped, "reaped exited children");
        }
        self.warm_reaped(&reaped);
        // One `/proc` capture per tick feeds both the recovery set and the auto-save.
        let captured = self.tick_capture();
        self.refresh_recovery(&captured);
        self.prune_reservations();
        // Also catches a `run-on` whose window never mapped (its pending entry just expired).
        self.prune_ephemeral();
        self.save_dirty(now, TICK, &captured);
        self.write_session();
        self.poll_config();
    }

    /// Writes every dirty workspace now, bypassing the interval (`save`, `Super+Shift+Q`).
    pub fn flush_workspaces(&mut self) {
        self.save_dirty(Instant::now(), Duration::ZERO, &HashMap::new());
        self.write_session();
    }

    /// Removes the IPC socket file (clean quit).
    pub fn remove_ipc_socket(&mut self) {
        if let Some(path) = self.persist.ipc_path.take() {
            let _ = std::fs::remove_file(path);
        }
    }

    pub(super) fn live_workspaces(&self) -> Vec<(OutputId, WorkspaceId)> {
        let mut outs: Vec<_> = self.layout.world.outputs().collect();
        outs.sort_by_key(|(id, _)| *id);
        outs.into_iter()
            .flat_map(|(oid, o)| o.workspaces().iter().map(move |ws| (oid, *ws)))
            .collect()
    }

    /// Writes the dirty live workspaces whose interval passed. `captured` holds this tick's window
    /// records ([`Self::tick_capture`]); a window missing from it is captured fresh.
    fn save_dirty(
        &mut self,
        now: Instant,
        min_interval: Duration,
        captured: &HashMap<WindowId, WindowRecord>,
    ) {
        for (_, ws) in self.live_workspaces() {
            // A restore in progress would overwrite the saved commands with half-mapped panes.
            if self.workspace_has_pending(ws) {
                continue;
            }
            let Capture::Record(record) = self.capture_record_from(ws, captured) else {
                continue;
            };
            if !should_write(&record, self.persist.saved.contains_key(&record.name)) {
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
            .record()
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

    /// The record of `ws` to write ([`Capture`]): never one for an unknown or **ephemeral** workspace. This is the
    /// single store-save choke point: every workspace-store write (`save_dirty`, `save_now`,
    /// close's flush, rename) captures through here, so an ephemeral workspace is never written.
    fn capture_record(&self, ws: WorkspaceId) -> Capture {
        self.capture_record_from(ws, &HashMap::new())
    }

    /// [`Self::capture_record`] taking each window's record from `captured` (this tick's capture),
    /// a window missing from it captured fresh. Same ephemeral rule.
    fn capture_record_from(
        &self,
        ws: WorkspaceId,
        captured: &HashMap<WindowId, WindowRecord>,
    ) -> Capture {
        if self.persist.workspace.is_ephemeral(ws) {
            return Capture::Ephemeral;
        }
        let env = capture_env();
        let record = WorkspaceRecord::from_world(
            &self.layout.world,
            ws,
            self.persist.workspace.root(ws).cloned(),
            |id| {
                captured
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| self.window_record(&env, id))
            },
        );
        let Some(mut record) = record else {
            return Capture::Gone;
        };
        record.output = self.persist.workspace.last_output(ws).cloned();
        Capture::Record(record)
    }

    fn restore(&mut self, record: WorkspaceRecord) -> Result<(), String> {
        let wanted = record.output.clone();
        self.restore_on(record, wanted.as_deref())
    }

    /// Restores `record` onto the connected output named `wanted` (else the focused output).
    fn restore_on(&mut self, record: WorkspaceRecord, wanted: Option<&str>) -> Result<(), String> {
        // Every fallible precondition and the spawn plan come first: a failure leaves the model alone.
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let exec_flag = self.config.terminal.exec_flag.clone();
        let jobs = crucible_persist::prepare_restore(
            &record,
            Self::crucible_exe("restore")
                .ok_or_else(|| std::io::Error::other("crucible binary unavailable")),
            home.as_deref(),
            &exec_flag,
            Path::is_dir,
        )?;
        let connected = self.connected_outputs();
        let output = restore_target(wanted, &connected, self.layout.world.focused_output());
        let mut created = None;
        self.with_model(|w| {
            // Focus the target output, then show the new workspace there.
            w.set_focused_output(output)?;
            let (ws, root) = w.insert_workspace(output, record.name.clone(), &record.layout())?;
            w.switch_workspace(ws)?;
            // Tree-order pane ids: `job.pane_index` indexes `record.panes()` in the same order.
            let panes: Vec<PaneId> = root.leaves().into_iter().copied().collect();
            created = Some((ws, panes));
            Ok(())
        })
        .map_err(|e| e.to_string())?;
        let (ws, panes) = created.expect("set on success");
        // Seed the last-viewed output from the record; a viewing event (apply_layout) wins.
        let meta = self.persist.workspace.meta.entry(ws).or_default();
        if let Some(name) = &record.output {
            meta.last_output.get_or_insert_with(|| name.clone());
        }
        if let Some(root) = &record.root {
            meta.root = Some(root.clone());
        }
        let program = self.terminal_program();
        let now = Instant::now();
        for job in jobs {
            let target = Target::Pane {
                pane: panes[job.pane_index],
                saved_index: job.saved_index,
                focused: job.focused,
            };
            let (dir, args, origin) = (job.dir, job.args, job.origin);
            let args = self.with_tiling_args(args);
            let req = SpawnRequest::new(program.clone(), args, SpawnKind::Terminal)
                .in_dir(Some(dir))
                .with_origin(Some(origin));
            if let Some(pid) = self.spawn(req) {
                self.persist
                    .placement
                    .pending
                    .insert(pid, Pending::new(target, now));
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
        self.with_model(|w| {
            created = Some(w.new_workspace(name)?);
            Ok(())
        })
        .map_err(|err| err.to_string())?;
        let Some(ws) = created else {
            return Err("workspace was not created".into());
        };
        if root.is_some() {
            self.persist.workspace.set_root(ws, root);
        }
        self.save_now(ws)
    }
}

/// The session's `output` records for the connected `(connector, id)` pairs: each output's
/// workspace names in stack order and its active workspace. Pure over the world, so the
/// recorded order is unit-tested without a compositor. An `ephemeral` workspace is never listed
/// (nor recorded as an output's focused one).
fn session_outputs(
    world: &crucible_layout::World,
    connected: &[(String, OutputId)],
    ephemeral: impl Fn(WorkspaceId) -> bool,
) -> Vec<crucible_persist::session::OutputSession> {
    let name_of = |ws: WorkspaceId| {
        (!ephemeral(ws))
            .then(|| world.workspace(ws).map(|w| w.name.clone()))
            .flatten()
    };
    connected
        .iter()
        .filter_map(|(name, id)| {
            let o = world.output(*id)?;
            Some(crucible_persist::session::OutputSession {
                output: name.clone(),
                workspaces: o.workspaces().iter().filter_map(|w| name_of(*w)).collect(),
                focused: name_of(o.active_workspace()),
                header: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_persist::{PaneRecord, RecordNode};

    pub(super) fn record(root: Option<&str>, tree: RecordNode) -> WorkspaceRecord {
        WorkspaceRecord {
            name: "w".into(),
            root: root.map(PathBuf::from),
            focused_pane: 0,
            output: None,
            tree,
        }
    }

    pub(super) fn pane(windows: Vec<WindowRecord>) -> RecordNode {
        RecordNode::Pane(PaneRecord {
            focused: 0,
            windows,
        })
    }

    #[test]
    fn reordered_world_produces_the_reordered_output_node() {
        let mut world = crucible_layout::World::new("one");
        let one = world.active_workspace_id();
        world.new_workspace("two").unwrap();
        let out = world.focused_output();
        let connected = vec![("DP-1".to_string(), out)];
        let names = |w: &crucible_layout::World| {
            session_outputs(w, &connected, |_| false)[0]
                .workspaces
                .clone()
        };
        assert_eq!(names(&world), ["one", "two"]);
        world.reorder_workspace(one, 1).unwrap();
        let rec = session_outputs(&world, &connected, |_| false);
        assert_eq!(rec[0].workspaces, ["two", "one"]);
        assert_eq!(
            rec[0].focused.as_deref(),
            Some("two"),
            "active is unchanged"
        );
    }

    #[test]
    fn an_ephemeral_workspace_is_never_in_the_session_record() {
        let mut world = crucible_layout::World::new("one");
        let out = world.focused_output();
        let (scratch, _) = world
            .insert_workspace(
                out,
                "scratch",
                &crucible_layout::WorkspaceLayout::single(()),
            )
            .unwrap();
        let connected = vec![("DP-1".to_string(), out)];
        let rec = session_outputs(&world, &connected, |ws| ws == scratch);
        assert_eq!(rec[0].workspaces, ["one"]);
        assert_eq!(rec[0].focused.as_deref(), Some("one"));
        // Viewing it does not make it recordable: the output records no focused name.
        world.switch_workspace(scratch).unwrap();
        let rec = session_outputs(&world, &connected, |ws| ws == scratch);
        assert_eq!(rec[0].workspaces, ["one"]);
        assert_eq!(rec[0].focused, None);
    }

    #[test]
    fn skip_only_the_empty_single_pane_without_root() {
        assert!(skip_record(&record(None, pane(vec![]))));
        assert!(!skip_record(&record(Some("/r"), pane(vec![]))));
        let term = WindowRecord::Terminal {
            cwd: "/".into(),
            command: None,
            session: None,
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
    fn emptied_saved_workspace_is_still_written_but_never_saved_seed_is_not() {
        let term = WindowRecord::Terminal {
            cwd: "/".into(),
            command: None,
            session: None,
        };
        let with_window = record(None, pane(vec![term]));
        let emptied = record(None, pane(vec![]));
        assert!(should_write(&with_window, false));
        assert!(should_write(&with_window, true));
        // Saved once, then its last window moved away: the empty record must overwrite the file.
        assert!(should_write(&emptied, true));
        // A fresh empty seed that was never saved stays unsaved.
        assert!(!should_write(&emptied, false));
    }
}
