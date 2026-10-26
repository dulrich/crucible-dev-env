//! Workspace persistence wired into the compositor: the 1 s tick (auto-save + config reload),
//! per-window capture by client PID, restore with PID-matched pane placement, the IPC request
//! handlers, process spawning, and the picker overlay.
//!
//! Every decision is a pure function at the top of this module (unit-tested below); the
//! `impl CrucibleState` half only gathers inputs from the model / `/proc` / the store and applies
//! the results.
//!
//! ## Which windows are terminals
//! By **PID provenance**, not app-id matching: every process CDE spawns through
//! [`CrucibleState::spawn_process`] is remembered with its [`SpawnKind`], and a toplevel whose client PID
//! is one of them — or has one among its `/proc` ancestors — of kind [`SpawnKind::Terminal`] is a
//! terminal, captured with [`crucible_persist::capture_terminal`]. Everything else (apps from
//! `run`, the picker overlay, unknown clients) is recorded as an `app`.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use crucible_layout::{OutputId, PaneId, Rect, WindowId, WorkspaceId, World};
use crucible_persist::{WindowRecord, WorkspaceRecord, store};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::utils::{Point, Rectangle, Size};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
use tracing::{error, info, warn};

use crate::config::pick_terminal;
use crate::element::WindowElement;
use crate::focus::KeyboardFocusTarget;
use crate::ipc::{
    Candidate, CandidateKind, Request, Response, SOCKET_ENV, TERMINAL_ENV, TERMINAL_EXEC_FLAG_ENV,
    encode_field,
};
use crate::layout::model_output_id;
use crate::state::{Backend, CrucibleState};

/// Auto-save / config-poll cadence.
pub const TICK: Duration = Duration::from_secs(1);
/// How long a pending placement waits for its window to map.
pub const PENDING_TTL: Duration = Duration::from_secs(30);
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

/// How far up `/proc` a client PID is walked looking for a spawned/pending ancestor.
pub const ANCESTOR_DEPTH: usize = 8;

/// Where a window expected from a spawned PID should land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Into `pane` of a restored workspace, at its saved stack position.
    Pane {
        /// The restored pane.
        pane: PaneId,
        /// The window's index in the saved stack.
        saved_index: usize,
        /// Whether it was the pane's focused window.
        focused: bool,
    },
    /// An overlay (picker / launcher; unmanaged, centred, keyboard-focused).
    Overlay,
}

#[derive(Debug, Clone, Copy)]
struct Pending {
    target: Target,
    since: Instant,
}

#[derive(Debug, Clone, Copy)]
struct WindowInfo {
    pid: Option<u32>,
    terminal: bool,
}

/// Which program an overlay slot runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlayKind {
    Picker,
    Launcher,
    Keys,
}

#[derive(Debug, Clone)]
enum OverlayState {
    /// Spawned, waiting for its window to map.
    Launching(u32),
    /// Mapped as the overlay.
    Mapped(WindowElement),
}

/// The single overlay slot's occupant.
#[derive(Debug, Clone)]
struct Overlay {
    kind: OverlayKind,
    state: OverlayState,
}

/// What opening an overlay of a requested kind does given the slot's current occupant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenDecision {
    /// Same kind already mapped: raise and refocus it.
    Refocus,
    /// One is still launching: ignore the request.
    Ignore,
    /// A different kind is mapped: close it, then spawn.
    ReplaceThenSpawn,
    /// The slot is empty: spawn.
    Spawn,
}

/// Pure slot decision: `current` is `(kind, mapped)` of the occupant, if any.
fn overlay_open_decision(
    current: Option<(OverlayKind, bool)>,
    requested: OverlayKind,
) -> OpenDecision {
    match current {
        None => OpenDecision::Spawn,
        Some((_, false)) => OpenDecision::Ignore,
        Some((kind, true)) if kind == requested => OpenDecision::Refocus,
        Some((_, true)) => OpenDecision::ReplaceThenSpawn,
    }
}

/// Compositor-side persistence state (lives in [`CrucibleState::persist`]).
#[derive(Debug)]
pub struct PersistState {
    /// Where workspace files live (`$XDG_STATE_HOME/cde/workspaces`).
    pub store_dir: PathBuf,
    /// The bound IPC socket path, exported to children as `CDE_SOCKET`.
    pub ipc_path: Option<PathBuf>,
    /// Folder roots of workspaces (new terminals start there).
    pub roots: HashMap<WorkspaceId, PathBuf>,
    /// Last record written per workspace name, and when.
    saved: HashMap<String, (WorkspaceRecord, Instant)>,
    windows: HashMap<WindowId, WindowInfo>,
    spawned: HashMap<u32, SpawnKind>,
    pending: HashMap<u32, Pending>,
    placed: HashMap<PaneId, Vec<(WindowId, usize)>>,
    overlay: Option<Overlay>,
    config_mtime: Option<SystemTime>,
}

impl PersistState {
    /// Empty state saving into `store_dir`.
    #[must_use]
    pub fn new(store_dir: PathBuf) -> Self {
        Self {
            store_dir,
            ipc_path: None,
            roots: HashMap::new(),
            saved: HashMap::new(),
            windows: HashMap::new(),
            spawned: HashMap::new(),
            pending: HashMap::new(),
            placed: HashMap::new(),
            overlay: None,
            config_mtime: None,
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
        self.windows.remove(&id);
    }
}

// ---- pure decisions ---------------------------------------------------------------------------

/// The smallest positive number (as a string) for which `taken` is false.
#[must_use]
pub fn first_free_number(taken: impl Fn(&str) -> bool) -> String {
    (1u64..)
        .map(|n| n.to_string())
        .find(|n| !taken(n))
        .expect("unbounded")
}

/// The auto-name for a new workspace: the first number neither live in `world` nor in `saved`.
#[must_use]
pub fn new_workspace_name(world: &World, saved: &HashSet<String>) -> String {
    first_free_number(|n| world.workspace_by_name(n).is_some() || saved.contains(n))
}

/// The key `has` accepts among `pid` itself and then its `ancestors` (parent first).
#[must_use]
pub fn match_pending(pid: u32, ancestors: &[u32], has: impl Fn(u32) -> bool) -> Option<u32> {
    std::iter::once(pid)
        .chain(ancestors.iter().copied())
        .find(|p| has(*p))
}

/// Whether `pid` (or its nearest spawned ancestor) was spawned as a [`SpawnKind::Terminal`].
#[must_use]
pub fn is_terminal_spawn(pid: u32, ancestors: &[u32], spawned: &HashMap<u32, SpawnKind>) -> bool {
    match_pending(pid, ancestors, |k| spawned.contains_key(&k))
        .and_then(|k| spawned.get(&k))
        .is_some_and(|kind| *kind == SpawnKind::Terminal)
}

/// The stack index a restored window takes: the number of already-placed windows in its pane
/// whose saved index is smaller, so saved order survives any map order.
#[must_use]
pub fn insert_index(placed_saved: impl IntoIterator<Item = usize>, saved_index: usize) -> usize {
    placed_saved
        .into_iter()
        .filter(|i| *i < saved_index)
        .count()
}

/// Whether a record is not worth writing: one pane, no windows, no folder root.
#[must_use]
pub fn skip_record(record: &WorkspaceRecord) -> bool {
    let panes = record.panes();
    record.root.is_none() && panes.len() == 1 && panes[0].windows.is_empty()
}

/// The workspace name for folder `path`: its basename, else `basename (parent)`, else
/// `basename (parent) N`. `None` when the path has no UTF-8 basename.
#[must_use]
pub fn folder_workspace_name(path: &Path, taken: impl Fn(&str) -> bool) -> Option<String> {
    let base = path.file_name()?.to_str()?;
    if !taken(base) {
        return Some(base.to_string());
    }
    let parent = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|p| p.to_str());
    let stem = match parent {
        Some(p) => format!("{base} ({p})"),
        None => base.to_string(),
    };
    if !taken(&stem) {
        return Some(stem);
    }
    (2..10_000)
        .map(|n| format!("{stem} {n}"))
        .find(|c| !taken(c))
}

/// The picker overlay rect: ~60% × 50% of `output`, centred.
#[must_use]
pub fn overlay_rect(output: Rect) -> Rect {
    let w = output.width * 3 / 5;
    let h = output.height / 2;
    Rect::new(
        output.x + (output.width - w) / 2,
        output.y + (output.height - h) / 2,
        w,
        h,
    )
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
    vec![
        exec_flag.into(),
        crucible.into(),
        "resume".into(),
        "--cwd".into(),
        cwd.into(),
        "--".into(),
        command.into(),
    ]
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
            }),
    );
    out.extend(folders.into_iter().map(|(name, path)| Candidate {
        kind: CandidateKind::Folder,
        name,
        path: Some(path),
    }));
    out
}

fn proc_root() -> &'static Path {
    Path::new("/proc")
}

fn read_cmdline(pid: u32) -> Option<String> {
    let bytes = std::fs::read(proc_root().join(pid.to_string()).join("cmdline")).ok()?;
    let args = crucible_persist::capture::parse_cmdline(&bytes);
    (!args.is_empty()).then(|| crucible_persist::capture::join_command(&args))
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
    /// One-time persistence startup, called by both backends right after the startup config is
    /// applied: renames the seed workspace off a saved name, binds the IPC socket, arms the 1 s
    /// tick, and opens the picker when saved workspaces exist.
    pub fn start_persistence(&mut self) {
        let saved = self.persist.saved_names();
        let seed = self.layout.world.active_workspace_id();
        let seed_name = self
            .layout
            .world
            .workspace(seed)
            .map(|w| w.name.clone())
            .unwrap_or_default();
        if saved.contains(&seed_name) {
            let name = new_workspace_name(&self.layout.world, &saved);
            info!(
                from = seed_name,
                to = name,
                "seed workspace name is saved; renaming"
            );
            if let Err(err) = self.with_model(|w| w.rename_workspace(seed, name)) {
                warn!(%err, "seed rename failed");
            }
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
        if !saved.is_empty() {
            self.open_picker(false);
        }
    }

    /// The 1 s tick: expire pending placements, auto-save dirty workspaces, poll the config.
    pub fn persist_tick(&mut self) {
        let now = Instant::now();
        self.persist
            .pending
            .retain(|_, p| now.saturating_duration_since(p.since) < PENDING_TTL);
        if let Some(Overlay {
            state: OverlayState::Launching(pid),
            kind,
        }) = self.persist.overlay
        {
            if !self.persist.pending.contains_key(&pid) {
                warn!(pid, ?kind, "overlay never mapped; giving up on it");
                self.persist.overlay = None;
            }
        }
        self.save_dirty(now, TICK);
        self.poll_config();
    }

    /// Writes every dirty workspace now, bypassing the interval (`save`, `Super+Shift+Q`).
    pub fn flush_workspaces(&mut self) {
        self.save_dirty(Instant::now(), Duration::ZERO);
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

    fn workspace_has_pending(&self, ws: WorkspaceId) -> bool {
        let Some(w) = self.layout.world.workspace(ws) else {
            return false;
        };
        let panes = w.panes();
        self.persist.pending.values().any(|p| match p.target {
            Target::Pane { pane, .. } => panes.contains(&pane),
            Target::Overlay => false,
        })
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
        WorkspaceRecord::from_world(world, ws, self.persist.roots.get(&ws).cloned(), |id| {
            records.get(&id).cloned().unwrap_or(WindowRecord::App {
                app_id: None,
                cmdline: None,
            })
        })
    }

    fn window_record(&self, id: WindowId) -> WindowRecord {
        let info = self.persist.windows.get(&id).copied();
        let pid = info.and_then(|i| i.pid);
        if let (Some(pid), true) = (pid, info.is_some_and(|i| i.terminal)) {
            if let Some(cap) = crucible_persist::capture_terminal(proc_root(), pid) {
                return WindowRecord::Terminal {
                    cwd: cap.cwd,
                    command: cap.command,
                };
            }
        }
        WindowRecord::App {
            app_id: self.layout.element(id).and_then(element_app_id),
            cmdline: pid.and_then(read_cmdline),
        }
    }

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
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id();
                info!(program, ?args, ?cwd, pid, "spawned");
                self.persist.spawned.insert(pid, kind);
                Some(pid)
            }
            Err(err) => {
                error!(program, %err, "spawn failed");
                None
            }
        }
    }

    /// Spawns a terminal into the focused pane, starting in the active workspace's root if any.
    pub fn spawn_terminal_here(&mut self, program: &str) {
        let ws = self.layout.world.active_workspace_id();
        let root = self.persist.roots.get(&ws).cloned();
        self.spawn_process(program, &[], root.as_deref(), SpawnKind::Terminal);
    }

    /// Runs `cmd` (a shell command line) into the focused pane via `/bin/sh -c 'exec <cmd>'`, so the
    /// window's client PID is the app itself. Starts in the active workspace's root if any.
    /// Returns the PID.
    pub fn spawn_command_here(&mut self, cmd: &str) -> Option<u32> {
        let ws = self.layout.world.active_workspace_id();
        let root = self.persist.roots.get(&ws).cloned();
        let args = [OsString::from("-c"), OsString::from(format!("exec {cmd}"))];
        self.spawn_process("/bin/sh", &args, root.as_deref(), SpawnKind::App)
    }

    // ---- picker overlay ------------------------------------------------------------------------

    /// Opens the picker (or, with `rename`, the rename prompt); see [`Self::open_overlay`].
    pub fn open_picker(&mut self, rename: bool) {
        let Some(crucible) = Self::crucible_exe("picker") else {
            return;
        };
        let current = rename.then(|| self.layout.world.focus_path().workspace_name);
        let args = picker_args(
            &self.config.terminal.exec_flag,
            &crucible,
            current.as_deref(),
        );
        self.open_overlay(OverlayKind::Picker, args);
    }

    /// Opens the program launcher; see [`Self::open_overlay`].
    pub fn open_launcher(&mut self) {
        let Some(crucible) = Self::crucible_exe("launcher") else {
            return;
        };
        let args = launcher_args(&self.config.terminal.exec_flag, &crucible);
        self.open_overlay(OverlayKind::Launcher, args);
    }

    /// Opens the read-only settings overlay (`crucible keys`); see [`Self::open_overlay`].
    pub fn open_settings(&mut self) {
        let Some(crucible) = Self::crucible_exe("settings") else {
            return;
        };
        let args = keys_args(&self.config.terminal.exec_flag, &crucible);
        self.open_overlay(OverlayKind::Keys, args);
    }

    fn crucible_exe(what: &str) -> Option<PathBuf> {
        match std::env::current_exe() {
            Ok(p) => Some(p),
            Err(err) => {
                error!(%err, "cannot resolve the crucible binary; no {what}");
                None
            }
        }
    }

    /// Opens an overlay of `kind` running the terminal with `args`. A mapped overlay of the same
    /// kind is refocused, one still launching is left alone, and a mapped overlay of a different
    /// kind is asked to close before the new one spawns.
    fn open_overlay(&mut self, kind: OverlayKind, args: Vec<OsString>) {
        let current = self.persist.overlay.clone();
        let summary = current
            .as_ref()
            .map(|o| (o.kind, matches!(o.state, OverlayState::Mapped(_))));
        match overlay_open_decision(summary, kind) {
            OpenDecision::Ignore => return,
            OpenDecision::Refocus => {
                if let Some(Overlay {
                    state: OverlayState::Mapped(element),
                    ..
                }) = current
                {
                    self.space.raise_element(&element, true);
                    self.set_keyboard_focus(Some(KeyboardFocusTarget::Window(element)));
                }
                return;
            }
            OpenDecision::ReplaceThenSpawn => {
                if let Some(Overlay {
                    state: OverlayState::Mapped(old),
                    ..
                }) = current
                {
                    if let Some(toplevel) = old.toplevel() {
                        toplevel.send_close();
                    }
                    if let Some(x11) = old.x11_surface() {
                        let _ = x11.close();
                    }
                }
            }
            OpenDecision::Spawn => {}
        }
        let program = self.terminal_program();
        if let Some(pid) = self.spawn_process(&program, &args, None, SpawnKind::Overlay) {
            self.persist.pending.insert(
                pid,
                Pending {
                    target: Target::Overlay,
                    since: Instant::now(),
                },
            );
            self.persist.overlay = Some(Overlay {
                kind,
                state: OverlayState::Launching(pid),
            });
        }
    }

    fn focused_output_rect(&self) -> Rect {
        let focused = self.layout.world.focused_output();
        self.space
            .outputs()
            .find(|o| model_output_id(o) == Some(focused))
            .and_then(|o| self.space.output_geometry(o))
            .map_or(Rect::new(0, 0, 1280, 800), |g| {
                Rect::new(g.loc.x, g.loc.y, g.size.w, g.size.h)
            })
    }

    fn map_overlay(&mut self, element: WindowElement) {
        let rect = overlay_rect(self.focused_output_rect());
        let size = Size::from((rect.width, rect.height));
        let loc = Point::from((rect.x, rect.y));
        if let Some(toplevel) = element.toplevel() {
            // The initial configure goes out on the first commit and carries this pending size.
            toplevel.with_pending_state(|s| s.size = Some(size));
        }
        if let Some(x11) = element.x11_surface() {
            let _ = x11.configure(Rectangle::new(loc, size));
        }
        self.map_unmanaged_element(element.clone(), loc);
        self.space.raise_element(&element, true);
        self.set_keyboard_focus(Some(KeyboardFocusTarget::Window(element.clone())));
        let kind = self
            .persist
            .overlay
            .as_ref()
            .map_or(OverlayKind::Picker, |o| o.kind);
        self.persist.overlay = Some(Overlay {
            kind,
            state: OverlayState::Mapped(element),
        });
    }

    /// Called when an unmanaged element was torn down: if it was the mapped overlay, return keyboard
    /// focus to the layout's focused window. A stale close of a replaced overlay is ignored.
    pub(crate) fn unmanaged_closed(&mut self, element: &WindowElement) {
        if matches!(
            &self.persist.overlay,
            Some(Overlay { state: OverlayState::Mapped(p), .. }) if p == element
        ) {
            self.persist.overlay = None;
            self.set_keyboard_focus(None);
            self.reconcile_keyboard_focus();
        }
    }

    // ---- mapping new windows -------------------------------------------------------------------

    /// The client PID of an xdg toplevel's Wayland client.
    #[must_use]
    pub fn wayland_client_pid(
        &self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) -> Option<u32> {
        use smithay::reexports::wayland_server::Resource;
        let client = surface.client()?;
        let creds = client.get_credentials(&self.display_handle).ok()?;
        u32::try_from(creds.pid).ok()
    }

    /// Maps a new toplevel (xdg or X11 managed) from client `pid`: into its pending restore
    /// placement, as the picker overlay, or through the normal focused-pane path. Records whether
    /// it is a CDE-spawned terminal.
    pub fn map_new_window(&mut self, element: WindowElement, pid: Option<u32>) {
        let ancestors = pid
            .map(|p| crucible_persist::ancestors(proc_root(), p, ANCESTOR_DEPTH))
            .unwrap_or_default();
        let pending = pid
            .and_then(|p| match_pending(p, &ancestors, |k| self.persist.pending.contains_key(&k)))
            .and_then(|k| self.persist.pending.remove(&k));
        let id = match pending.map(|p| p.target) {
            Some(Target::Overlay) => {
                self.map_overlay(element);
                return;
            }
            Some(Target::Pane {
                pane,
                saved_index,
                focused,
            }) => self.map_placed(element, pane, saved_index, focused),
            None => self.map_managed_window(element),
        };
        let terminal = pid.is_some_and(|p| is_terminal_spawn(p, &ancestors, &self.persist.spawned));
        self.persist
            .windows
            .insert(id, WindowInfo { pid, terminal });
    }

    fn map_placed(
        &mut self,
        element: WindowElement,
        pane: PaneId,
        saved_index: usize,
        focused: bool,
    ) -> WindowId {
        let live: Vec<WindowId> = self
            .layout
            .world
            .pane(pane)
            .map(|p| p.windows.clone())
            .unwrap_or_default();
        let placed = self.persist.placed.entry(pane).or_default();
        placed.retain(|(w, _)| live.contains(w));
        let index = insert_index(placed.iter().map(|(_, i)| *i), saved_index);
        match self.map_managed_window_at(element.clone(), pane, index) {
            Ok(id) => {
                self.persist
                    .placed
                    .entry(pane)
                    .or_default()
                    .push((id, saved_index));
                if focused {
                    let pos = self
                        .layout
                        .world
                        .pane(pane)
                        .and_then(|p| p.windows.iter().position(|w| *w == id));
                    if let Some(pos) = pos {
                        let _ = self.with_model(|w| w.set_stack_focus(pane, pos));
                    }
                }
                id
            }
            Err(err) => {
                warn!(%err, "restore placement failed; using the focused pane");
                self.map_managed_window(element)
            }
        }
    }

    // ---- IPC -----------------------------------------------------------------------------------

    /// Answers one IPC request.
    pub fn handle_ipc(&mut self, request: Request) -> Response {
        let result = match request {
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
                return match self.spawn_command_here(&cmd) {
                    Some(_) => Response::Ok(String::new()),
                    None => Response::Err(encode_field("spawn failed")),
                };
            }
            Request::Open(name) => self.ipc_open(&name),
            Request::OpenFolder(path) => self.ipc_open_folder(&path),
            Request::New(name) => self.ipc_new(&name),
            Request::Rename(name) => self.ipc_rename(&name),
        };
        match result {
            Ok(()) => Response::Ok(String::new()),
            Err(reason) => Response::Err(reason),
        }
    }

    fn candidates(&self) -> Vec<Candidate> {
        let world = &self.layout.world;
        let live = self
            .live_workspaces()
            .into_iter()
            .filter_map(|(_, ws)| {
                let name = world.workspace(ws)?.name.clone();
                Some((name, self.persist.roots.get(&ws).cloned()))
            })
            .collect();
        let saved = store::list_saved(&self.persist.store_dir)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        build_candidates(live, saved, folder_entries(&self.config.picker_roots))
    }

    fn switch_to_live(&mut self, ws: WorkspaceId) -> Result<(), String> {
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

    fn ipc_open(&mut self, name: &str) -> Result<(), String> {
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

    fn restore(&mut self, record: WorkspaceRecord) -> Result<(), String> {
        let output = self.layout.world.focused_output();
        let mut created = None;
        self.with_model(|w| {
            let (ws, panes) = w.insert_workspace(output, record.name.clone(), &record.layout())?;
            w.switch_workspace(ws)?;
            created = Some((ws, panes));
            Ok(())
        })
        .map_err(|e| e.to_string())?;
        let (ws, panes) = created.expect("set on success");
        if let Some(root) = &record.root {
            self.persist.roots.insert(ws, root.clone());
        }
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let crucible = std::env::current_exe().map_err(|e| e.to_string())?;
        let program = self.terminal_program();
        let exec_flag = self.config.terminal.exec_flag.clone();
        let mut jobs = Vec::new();
        for (pane, pane_record) in panes.iter().zip(record.panes()) {
            for (i, window) in pane_record.windows.iter().enumerate() {
                if let WindowRecord::Terminal { cwd, command } = window {
                    let dir =
                        resolve_cwd(cwd, record.root.as_deref(), home.as_deref(), Path::is_dir);
                    let args = restore_args(&exec_flag, &crucible, &dir, command.as_deref());
                    let target = Target::Pane {
                        pane: *pane,
                        saved_index: i,
                        focused: pane_record.focused == i,
                    };
                    jobs.push((dir, args, target));
                }
            }
        }
        let now = Instant::now();
        for (dir, args, target) in jobs {
            if let Some(pid) = self.spawn_process(&program, &args, Some(&dir), SpawnKind::Terminal)
            {
                self.persist
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
            self.persist.roots.insert(ws, root);
        }
        self.save_now(ws)
    }

    fn ipc_open_folder(&mut self, path: &Path) -> Result<(), String> {
        if !path.is_dir() {
            return Err(format!("{} is not a directory", path.display()));
        }
        // Already open (live) or saved for this exact folder: open that instead of a duplicate.
        let live = self
            .persist
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

    fn ipc_new(&mut self, name: &str) -> Result<(), String> {
        if self.name_taken(name, &self.persist.saved_names()) {
            return Err(format!("a workspace named {name} already exists"));
        }
        self.persist.file_of(name)?;
        self.create_named(name.to_string(), None)
    }

    fn ipc_rename(&mut self, new: &str) -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_persist::{PaneRecord, RecordNode};

    #[test]
    fn first_free_number_skips_taken() {
        assert_eq!(first_free_number(|_| false), "1");
        assert_eq!(first_free_number(|n| n == "1" || n == "2"), "3");
    }

    #[test]
    fn new_workspace_name_skips_live_and_saved() {
        let world = World::new(); // live "1"
        let saved: HashSet<String> = ["2".to_string()].into();
        assert_eq!(new_workspace_name(&world, &saved), "3");
        assert_eq!(new_workspace_name(&world, &HashSet::new()), "2");
    }

    #[test]
    fn match_pending_prefers_the_pid_then_the_nearest_ancestor() {
        let pending: HashSet<u32> = [10, 20].into();
        let has = |k| pending.contains(&k);
        assert_eq!(match_pending(10, &[20], has), Some(10));
        assert_eq!(match_pending(5, &[7, 20, 10], has), Some(20));
        assert_eq!(match_pending(5, &[7, 8], has), None);
    }

    #[test]
    fn is_terminal_spawn_depends_on_the_matched_kind() {
        let spawned: HashMap<u32, SpawnKind> = [
            (10, SpawnKind::Terminal),
            (20, SpawnKind::App),
            (30, SpawnKind::Overlay),
        ]
        .into();
        assert!(is_terminal_spawn(10, &[], &spawned));
        assert!(is_terminal_spawn(5, &[7, 10], &spawned));
        assert!(!is_terminal_spawn(20, &[], &spawned));
        assert!(!is_terminal_spawn(5, &[20], &spawned));
        assert!(!is_terminal_spawn(30, &[], &spawned));
        assert!(!is_terminal_spawn(5, &[7, 8], &spawned));
        // the nearest spawned ancestor decides, not any terminal further up
        assert!(!is_terminal_spawn(5, &[20, 10], &spawned));
    }

    #[test]
    fn insert_index_restores_saved_order_in_any_map_order() {
        // Saved stack [a0, a1, a2] maps as a2, a0, a1.
        let mut stack: Vec<usize> = Vec::new();
        let mut placed: Vec<usize> = Vec::new();
        for saved in [2, 0, 1] {
            let at = insert_index(placed.iter().copied(), saved);
            stack.insert(at, saved);
            placed.push(saved);
        }
        assert_eq!(stack, vec![0, 1, 2]);
        assert_eq!(insert_index([], 3), 0);
    }

    fn record(root: Option<&str>, tree: RecordNode) -> WorkspaceRecord {
        WorkspaceRecord {
            name: "w".into(),
            root: root.map(PathBuf::from),
            focused_pane: 0,
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
    fn folder_names_disambiguate_with_the_parent() {
        let p = Path::new("/code/proj");
        assert_eq!(folder_workspace_name(p, |_| false).as_deref(), Some("proj"));
        assert_eq!(
            folder_workspace_name(p, |n| n == "proj").as_deref(),
            Some("proj (code)")
        );
        assert_eq!(
            folder_workspace_name(p, |n| n == "proj" || n == "proj (code)").as_deref(),
            Some("proj (code) 2")
        );
        assert_eq!(folder_workspace_name(Path::new("/"), |_| false), None);
    }

    #[test]
    fn overlay_is_centred_at_sixty_by_fifty_percent() {
        assert_eq!(
            overlay_rect(Rect::new(1920, 0, 1000, 800)),
            Rect::new(2120, 200, 600, 400)
        );
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
    fn overlay_open_decision_covers_all_outcomes() {
        use OverlayKind::{Keys, Launcher, Picker};
        assert_eq!(overlay_open_decision(None, Launcher), OpenDecision::Spawn);
        assert_eq!(
            overlay_open_decision(Some((Picker, true)), Picker),
            OpenDecision::Refocus
        );
        assert_eq!(
            overlay_open_decision(Some((Picker, false)), Launcher),
            OpenDecision::Ignore
        );
        assert_eq!(
            overlay_open_decision(Some((Launcher, false)), Launcher),
            OpenDecision::Ignore
        );
        assert_eq!(
            overlay_open_decision(Some((Picker, true)), Launcher),
            OpenDecision::ReplaceThenSpawn
        );
        assert_eq!(
            overlay_open_decision(Some((Launcher, true)), Keys),
            OpenDecision::ReplaceThenSpawn
        );
        assert_eq!(
            overlay_open_decision(Some((Keys, true)), Keys),
            OpenDecision::Refocus
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
}
