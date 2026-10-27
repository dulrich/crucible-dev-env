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
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime};

use crucible_layout::{OutputId, PaneId, Rect, WindowId, WorkspaceId, World};
use crucible_persist::slots::{Slots, slots_path};
use crucible_persist::{WindowRecord, WorkspaceRecord, store};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::utils::{Point, Rectangle, Size};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
use tracing::{debug, error, info, warn};

use crate::config::pick_terminal;
use crate::element::WindowElement;
use crate::focus::KeyboardFocusTarget;
use crate::ipc::{
    Candidate, CandidateKind, OVERLAY_STYLE_ENV, Request, Response, SOCKET_ENV, SPAWN_EPOCH_ENV,
    TERMINAL_ENV, TERMINAL_EXEC_FLAG_ENV, encode_field,
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
fn epoch_ms_now() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
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
    /// Into the live slot a relaunch placeholder held: `pane` at `index` (its position in the
    /// pane's stack when Enter was pressed, never the saved order).
    Slot {
        /// The placeholder's pane.
        pane: PaneId,
        /// The placeholder's current index in that pane's stack.
        index: usize,
        /// Whether the placeholder was the pane's focused window.
        focused: bool,
        /// The placeholder's restore placement entry (`placed`), inherited by the app.
        saved_index: Option<usize>,
    },
    /// An overlay (picker / launcher; unmanaged, centred, keyboard-focused).
    Overlay,
    /// A pre-warmed overlay host: stored hidden in the warm slot, never mapped until requested.
    WarmOverlay,
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
    /// The spawned terminal's PID, kept once mapped so the overlay can be ended by killing it
    /// (a polite close makes the terminal ask "close window?" while the TUI runs).
    pid: Option<u32>,
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

/// A pre-warmed overlay terminal running `crucible overlay-host --socket <socket>`.
#[derive(Debug, Clone)]
struct WarmHost {
    /// The spawned terminal's PID (killed to end it, kept as the overlay's PID once used).
    pid: u32,
    /// The host's mode socket.
    socket: PathBuf,
    /// Its toplevel once it arrived (stored hidden; never in the Space while warm).
    element: Option<WindowElement>,
    /// Monotonic spawn time (for the log line when it is used).
    spawned_at: Instant,
}

/// What `open_overlay` does with the warm slot when it has to spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WarmUse {
    /// The warm host is ready: hand it the mode and map it.
    Use,
    /// No ready warm host: kill any warm host (`kill`) and cold-spawn as before.
    Cold {
        /// Whether a (not ready) warm host exists and must be killed first.
        kill: bool,
    },
}

/// Pure warm-slot decision. `warm` is `None` without a warm host, else whether it is ready: its
/// toplevel is stored, its initial configure has been sent, and its socket exists.
fn warm_use_decision(warm: Option<bool>) -> WarmUse {
    match warm {
        Some(true) => WarmUse::Use,
        Some(false) => WarmUse::Cold { kill: true },
        None => WarmUse::Cold { kill: false },
    }
}

/// Pure respawn bound for a warm host that died before use: returns `(respawn, respawn_ok after)`.
/// One respawn is allowed per overlay close (and at startup); the flag is re-armed only there.
fn warm_after_death(respawn_ok: bool) -> (bool, bool) {
    (respawn_ok, false)
}

/// The warm host's socket path: under `$XDG_RUNTIME_DIR`, else the IPC socket's directory, else the
/// temp dir; `cde-overlay-host-<compositor pid>-<counter>.sock`.
#[must_use]
pub fn warm_socket_path(
    runtime_dir: Option<&Path>,
    ipc_path: Option<&Path>,
    pid: u32,
    counter: u32,
) -> PathBuf {
    let dir = runtime_dir
        .filter(|d| !d.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .or_else(|| ipc_path.and_then(Path::parent).map(Path::to_path_buf))
        .unwrap_or_else(std::env::temp_dir);
    dir.join(format!("cde-overlay-host-{pid}-{counter}.sock"))
}

/// The overlay host's mode line for overlay terminal `args` (`[exec-flag, crucible, sub, rest…]`,
/// as built by [`picker_args`] and friends): `<epoch_ms>\t<sub>[\t<rest>…]\n`. `None` when the argv
/// is too short or a field is not UTF-8 or holds a tab/newline (the caller then spawns cold).
#[must_use]
pub fn warm_mode_line(epoch_ms: u128, args: &[OsString]) -> Option<String> {
    let mode = args.get(2..).filter(|m| !m.is_empty())?;
    let mut line = epoch_ms.to_string();
    for field in mode {
        let field = field.to_str()?;
        if field.contains(['\t', '\n', '\r']) {
            return None;
        }
        line.push('\t');
        line.push_str(field);
    }
    line.push('\n');
    Some(line)
}

/// The terminal arguments for a warm host: `<exec-flag> <crucible> overlay-host --socket <path>`.
#[must_use]
pub fn warm_host_args(exec_flag: &str, crucible: &Path, socket: &Path) -> Vec<OsString> {
    vec![
        exec_flag.into(),
        crucible.into(),
        "overlay-host".into(),
        "--socket".into(),
        socket.into(),
    ]
}

/// Sends `line` to the warm host's socket, bounded to a few ms (never blocks the event loop).
fn send_mode_line(socket: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
    stream.set_write_timeout(Some(Duration::from_millis(5)))?;
    stream.write_all(line.as_bytes())
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

/// Asks a window to close (xdg `close` or the X11 equivalent).
fn close_element(element: &WindowElement) {
    if let Some(toplevel) = element.toplevel() {
        toplevel.send_close();
    }
    if let Some(x11) = element.x11_surface() {
        let _ = x11.close();
    }
}

/// Compositor-side persistence state (lives in [`CrucibleState::persist`]).
#[derive(Debug)]
pub struct PersistState {
    /// Where workspace files live (`$XDG_STATE_HOME/cde/workspaces`).
    pub store_dir: PathBuf,
    /// Named workspace slot pins (`$XDG_STATE_HOME/cde/slots.kdl`, beside the store dir). Empty
    /// until [`CrucibleState::start_persistence`] loads them.
    pub slots: Slots,
    /// The bound IPC socket path, exported to children as `CDE_SOCKET`.
    pub ipc_path: Option<PathBuf>,
    /// Folder roots of workspaces (new terminals start there).
    pub roots: HashMap<WorkspaceId, PathBuf>,
    /// Connector name of the output each workspace was last viewed on (see [`note_last_viewed`]).
    last_output: HashMap<WorkspaceId, String>,
    /// Last record written per workspace name, and when.
    saved: HashMap<String, (WorkspaceRecord, Instant)>,
    windows: HashMap<WindowId, WindowInfo>,
    spawned: HashMap<u32, SpawnRecord>,
    pending: HashMap<u32, Pending>,
    placed: HashMap<PaneId, Vec<(WindowId, usize)>>,
    overlay: Option<Overlay>,
    /// The pre-warmed overlay host, if one is running.
    warm: Option<WarmHost>,
    /// Whether a warm host that dies before use may be respawned (once per overlay close).
    warm_respawn_ok: bool,
    /// Warming is off for the session (the warm host came up as an X11 window).
    warm_disabled: bool,
    /// Counter making each warm host's socket path unique.
    warm_counter: u32,
    /// Handles of every CDE-spawned child, kept so exited ones are reaped (never reap-any).
    children: Vec<Child>,
    config_mtime: Option<SystemTime>,
}

impl PersistState {
    /// Empty state saving into `store_dir`.
    #[must_use]
    pub fn new(store_dir: PathBuf) -> Self {
        Self {
            store_dir,
            slots: Slots::new(),
            ipc_path: None,
            roots: HashMap::new(),
            last_output: HashMap::new(),
            saved: HashMap::new(),
            windows: HashMap::new(),
            spawned: HashMap::new(),
            pending: HashMap::new(),
            placed: HashMap::new(),
            overlay: None,
            warm: None,
            warm_respawn_ok: false,
            warm_disabled: false,
            warm_counter: 0,
            children: Vec::new(),
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
pub fn is_terminal_spawn(pid: u32, ancestors: &[u32], spawned: &HashMap<u32, SpawnRecord>) -> bool {
    match_pending(pid, ancestors, |k| spawned.contains_key(&k))
        .and_then(|k| spawned.get(&k))
        .is_some_and(|rec| rec.kind == SpawnKind::Terminal)
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

/// The live slot of `target` in a pane stack `windows` whose focused index is `focused`:
/// `(index, was_focused)`, or `None` when it is not in the stack.
#[must_use]
pub fn slot_for(windows: &[WindowId], focused: usize, target: WindowId) -> Option<(usize, bool)> {
    let index = windows.iter().position(|w| *w == target)?;
    Some((index, index == focused))
}

/// The index a relaunched app maps at: the recorded slot, clamped to the live stack length.
#[must_use]
pub fn slot_index(index: usize, live_len: usize) -> usize {
    index.min(live_len)
}

/// Records each output's **active** workspace as last viewed on that output's connector name.
/// `viewed` holds one `(model output, active workspace, connector name)` triple per connected,
/// bound output. Workspaces not active anywhere (e.g. migrated inactive by an unplug) keep their
/// entry untouched.
pub fn note_last_viewed(
    map: &mut HashMap<WorkspaceId, String>,
    viewed: &[(OutputId, WorkspaceId, String)],
) {
    for (_, ws, name) in viewed {
        if map.get(ws) != Some(name) {
            map.insert(*ws, name.clone());
        }
    }
}

/// The output a restored workspace lands on: the connected output named `wanted`, else `focused`.
#[must_use]
pub fn restore_target(
    wanted: Option<&str>,
    connected: &[(String, OutputId)],
    focused: OutputId,
) -> OutputId {
    wanted
        .and_then(|w| connected.iter().find(|(name, _)| name == w))
        .map_or(focused, |(_, id)| *id)
}

/// The output `delta` steps from `current` in `order` (wraparound). `None` when there is only one
/// output, `current` is not in `order`, or the step lands back on `current`.
#[must_use]
pub fn shifted_output(order: &[OutputId], current: OutputId, delta: i64) -> Option<OutputId> {
    let len = i64::try_from(order.len()).ok()?;
    if len < 2 {
        return None;
    }
    let at = i64::try_from(order.iter().position(|o| *o == current)?).ok()?;
    let target = order[usize::try_from((at + delta).rem_euclid(len)).ok()?];
    (target != current).then_some(target)
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

/// The terminal arguments for `run-in-terminal`: `<exec-flag> <crucible> resume --now --cwd <dir>
/// -- <command>` (the command is ONE element).
#[must_use]
pub fn run_in_terminal_args(
    exec_flag: &str,
    crucible: &Path,
    cwd: &Path,
    command: &str,
) -> Vec<OsString> {
    vec![
        exec_flag.into(),
        crucible.into(),
        "resume".into(),
        "--now".into(),
        "--cwd".into(),
        cwd.into(),
        "--".into(),
        command.into(),
    ]
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
const SH_BUILTINS: &[&str] = &[
    ".", ":", "break", "cd", "command", "continue", "eval", "exec", "exit", "export", "getopts",
    "read", "readonly", "return", "set", "shift", "times", "trap", "type", "ulimit", "umask",
    "unset", "wait", "alias", "bg", "fg", "jobs", "kill", "printf", "echo", "test", "true",
    "false", "if", "case", "for", "while", "until",
];

fn is_executable_file(path: &Path) -> bool {
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
    let mut args: Vec<OsString> = vec![exec_flag.into(), crucible.into(), "relaunch".into()];
    if let Some(id) = app_id {
        args.push("--app-id".into());
        args.push(id.into());
    }
    args.push("--".into());
    args.push(cmdline.into());
    args
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
        match Slots::load(&slots_path(&self.persist.store_dir)) {
            Ok(slots) => self.persist.slots = slots,
            Err(err) => warn!(%err, "cannot read the slot pins; starting with none"),
        }
        let saved = self.persist.saved_names();
        let seed = self.layout.world.active_workspace_id();
        self.rename_seed_off_saved(seed);
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
        self.start_warm_overlay();
    }

    /// Renames seed workspace `ws` to a fresh auto-name when its name is already saved on disk, so
    /// a later save never overwrites the saved workspace. Called at startup and for every freshly
    /// allocated output's seed.
    pub(crate) fn rename_seed_off_saved(&mut self, ws: WorkspaceId) {
        let saved = self.persist.saved_names();
        let Some(seed_name) = self.layout.world.workspace(ws).map(|w| w.name.clone()) else {
            return;
        };
        if !saved.contains(&seed_name) {
            return;
        }
        let name = new_workspace_name(&self.layout.world, &saved);
        info!(
            from = seed_name,
            to = name,
            "seed workspace name is saved; renaming"
        );
        if let Err(err) = self.with_model(|w| w.rename_workspace(ws, name)) {
            warn!(%err, "seed rename failed");
        }
    }

    /// Notes every connected, bound output's active workspace as last viewed there. Called by
    /// `apply_layout`, the only model-to-Space path.
    pub(crate) fn note_last_viewed_outputs(&mut self) {
        let world = &self.layout.world;
        let viewed: Vec<(OutputId, WorkspaceId, String)> = self
            .space
            .outputs()
            .filter_map(|o| {
                let id = crate::layout::model_output_id(o)?;
                let ws = world.output(id)?.active_workspace();
                Some((id, ws, o.name()))
            })
            .collect();
        note_last_viewed(&mut self.persist.last_output, &viewed);
    }

    /// Closes the focused output's active workspace (`close-workspace`): refuse while a restore is
    /// pending or on the output's last workspace; flush its record (refuse if the write fails);
    /// drop it from the model; then end each of its windows (kill the held child, else a polite
    /// close) after releasing its binding so no late teardown lands elsewhere.
    pub(crate) fn close_workspace(&mut self) {
        let world = &self.layout.world;
        let ws = world.active_workspace_id();
        // (a) A restore in flight would race the teardown; refuse before any flush.
        if self.workspace_has_pending(ws) {
            info!(?ws, "close-workspace: a restore is still pending; refusing");
            return;
        }
        // (b) Never the output's last workspace (checked before flushing anything).
        let output = world.focused_output();
        if world.output(output).is_none_or(|o| o.workspaces.len() < 2) {
            info!(
                ?ws,
                "close-workspace: last workspace on this output; refusing"
            );
            return;
        }
        // (c) Flush first; a failed write refuses the close (never tear down unsaved state).
        let Some(record) = self.capture_record(ws) else {
            warn!(?ws, "close-workspace: workspace vanished; refusing");
            return;
        };
        if !skip_record(&record) && !self.write_record(record, Instant::now()) {
            warn!(?ws, "close-workspace: save failed; refusing to close");
            return;
        }
        // (d) Snapshot what to end, and the panes, before the model forgets them.
        let world = &self.layout.world;
        let panes = world.workspace(ws).map(|w| w.panes()).unwrap_or_default();
        let ends: Vec<(WindowId, Option<WindowElement>, Option<u32>)> = panes
            .iter()
            .filter_map(|p| world.pane(*p))
            .flat_map(|p| p.windows.iter().copied())
            .map(|id| {
                let element = self.layout.element(id).cloned();
                let pid = self.persist.windows.get(&id).and_then(|i| i.pid);
                (id, element, pid)
            })
            .collect();
        // (e) The model drops the workspace; apply_layout hides its elements.
        if let Err(err) = self.with_model(|w| w.remove_workspace(ws).map(|_| ())) {
            info!(%err, ?ws, "close-workspace refused by the model");
            return;
        }
        // (f) Release each binding first, then end the window.
        for (id, element, pid) in ends {
            let bound = self.layout.release_binding(id);
            self.persist.forget_window(id);
            let Some(element) = bound.or(element) else {
                continue;
            };
            self.space.unmap_elem(&element);
            if !pid.is_some_and(|p| kill_child(&mut self.persist.children, p)) {
                close_element(&element);
            }
        }
        // (g) Drop per-workspace bookkeeping.
        self.persist.roots.remove(&ws);
        self.persist.last_output.remove(&ws);
        self.persist.pending.retain(|_, p| match p.target {
            Target::Pane { pane, .. } | Target::Slot { pane, .. } => !panes.contains(&pane),
            Target::Overlay | Target::WarmOverlay => true,
        });
        for pane in &panes {
            self.persist.placed.remove(pane);
        }
        info!(?ws, "workspace closed");
    }

    /// Moves the focused output's active workspace `delta` outputs along the model output order
    /// (`move-workspace-to-output`). No-op with one output; refused on the source's last workspace.
    pub(crate) fn move_active_workspace_to_output(&mut self, delta: i64) {
        let world = &self.layout.world;
        let order: Vec<OutputId> = world.output_ids().collect();
        let source = world.focused_output();
        let Some(target) = shifted_output(&order, source, delta) else {
            debug!(delta, "move-workspace-to-output: no other output; ignoring");
            return;
        };
        let ws = world.active_workspace_id();
        if let Err(err) = self.with_model(|w| w.move_workspace_to_output(ws, target)) {
            info!(%err, ?ws, "move-workspace-to-output refused");
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
            ..
        }) = self.persist.overlay
        {
            if !self.persist.pending.contains_key(&pid) {
                warn!(pid, ?kind, "overlay never mapped; giving up on it");
                self.persist.overlay = None;
            }
        }
        let reaped = reap_exited(&mut self.persist.children);
        if !reaped.is_empty() {
            debug!(?reaped, "reaped exited children");
        }
        self.warm_reaped(&reaped);
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
            Target::Pane { pane, .. } | Target::Slot { pane, .. } => panes.contains(&pane),
            Target::Overlay | Target::WarmOverlay => false,
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
        let mut record =
            WorkspaceRecord::from_world(world, ws, self.persist.roots.get(&ws).cloned(), |id| {
                records.get(&id).cloned().unwrap_or(WindowRecord::App {
                    app_id: None,
                    cmdline: None,
                })
            })?;
        record.output = self.persist.last_output.get(&ws).cloned();
        Some(record)
    }

    fn window_record(&self, id: WindowId) -> WindowRecord {
        let info = self.persist.windows.get(&id).copied();
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
        if kind == SpawnKind::Overlay {
            cmd.env(OVERLAY_STYLE_ENV, self.config.overlay.style.as_str());
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
                self.persist.spawned.insert(pid, record);
                self.persist.children.push(child);
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
    fn with_tiling_args(&self, args: Vec<OsString>) -> Vec<OsString> {
        with_terminal_args(&self.config.tiling.substituted_terminal_args(), args)
    }

    /// The `PATH` every spawned child gets: this binary's directory (so `crucible` resolves) in
    /// front of the compositor's own `PATH`. `None` when the executable path is unknown.
    fn spawn_path() -> Option<OsString> {
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?;
        Some(child_path(dir, std::env::var_os("PATH").as_deref()))
    }

    /// Pre-flights `cmd` for workspace `ws`: its first word must resolve against the child's
    /// `PATH`, relative to the spawn's effective cwd (the workspace root, else the compositor's).
    fn preflight(&self, cmd: &str, ws: WorkspaceId) -> Result<(), String> {
        let cwd = self
            .persist
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
        let ws = self.layout.world.active_workspace_id();
        self.preflight(cmd, ws)
            .map_err(|word| format!("unknown command {word}"))?;
        let crucible = Self::crucible_exe("run-in-terminal").ok_or("spawn failed")?;
        let dir = self
            .persist
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
        ));
        let program = self.terminal_program();
        self.spawn_process(&program, &args, Some(&dir), SpawnKind::Terminal)
            .ok_or_else(|| "spawn failed".to_string())
    }

    /// Spawns a terminal into the focused pane, starting in the active workspace's root if any.
    pub fn spawn_terminal_here(&mut self, program: &str) {
        let ws = self.layout.world.active_workspace_id();
        let root = self.persist.roots.get(&ws).cloned();
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
    fn spawn_command_in(&mut self, cmd: &str, ws: WorkspaceId) -> Option<u32> {
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
                    pid,
                    ..
                }) = current
                {
                    self.end_overlay(&old, pid);
                }
            }
            OpenDecision::Spawn => {}
        }
        let replaced = matches!(
            overlay_open_decision(summary, kind),
            OpenDecision::ReplaceThenSpawn
        );
        let used_warm = self.try_open_warm(kind, &args);
        if replaced {
            // The replaced overlay is closing: that is an overlay close for the warm slot too.
            self.rewarm_after_overlay_close();
        }
        if used_warm {
            return;
        }
        let program = self.terminal_program();
        let args = with_terminal_args(&self.config.overlay.terminal_args, args);
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
                pid: Some(pid),
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
            // A pre-warmed host was configured while hidden: re-send for the current output.
            if Self::initial_configure_sent(toplevel) {
                toplevel.send_pending_configure();
            }
        }
        if let Some(x11) = element.x11_surface() {
            let _ = x11.configure(Rectangle::new(loc, size));
        }
        self.map_unmanaged_element(element.clone(), loc);
        self.space.raise_element(&element, true);
        self.set_keyboard_focus(Some(KeyboardFocusTarget::Window(element.clone())));
        let (kind, pid) = self
            .persist
            .overlay
            .as_ref()
            .map_or((OverlayKind::Picker, None), |o| (o.kind, o.pid));
        self.persist.overlay = Some(Overlay {
            kind,
            state: OverlayState::Mapped(element),
            pid,
        });
    }

    /// The overlay's element, only once it is mapped.
    pub(crate) fn mapped_overlay(&self) -> Option<&WindowElement> {
        match &self.persist.overlay {
            Some(Overlay {
                state: OverlayState::Mapped(element),
                ..
            }) => Some(element),
            _ => None,
        }
    }

    /// The mapped overlay's actual global Space geometry, `None` while no overlay is mapped.
    pub(crate) fn mapped_overlay_rect(&self) -> Option<Rect> {
        let element = self.mapped_overlay()?;
        let geo = self.space.element_geometry(element)?;
        Some(Rect::new(geo.loc.x, geo.loc.y, geo.size.w, geo.size.h))
    }

    /// Ends the mapped overlay (if any); the slot is cleared by [`Self::unmanaged_closed`] when its
    /// surface goes away.
    pub(crate) fn close_mapped_overlay(&mut self) {
        let Some(Overlay {
            state: OverlayState::Mapped(element),
            pid,
            ..
        }) = self.persist.overlay.clone()
        else {
            return;
        };
        self.end_overlay(&element, pid);
    }

    /// Ends an overlay by killing its terminal process, so the terminal never asks to confirm
    /// closing a window with a running TUI. Falls back to a polite close when CDE holds no handle.
    fn end_overlay(&mut self, element: &WindowElement, pid: Option<u32>) {
        if !pid.is_some_and(|p| kill_child(&mut self.persist.children, p)) {
            close_element(element);
        }
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
            self.rewarm_after_overlay_close();
        }
    }

    // ---- pre-warmed overlay host ---------------------------------------------------------------

    /// Starts a warm overlay host at startup (once the compositor can spawn overlays).
    pub(crate) fn start_warm_overlay(&mut self) {
        self.persist.warm_respawn_ok = true;
        self.spawn_warm();
    }

    /// An overlay closed: re-arm the one-respawn allowance and start a warm host if none runs.
    fn rewarm_after_overlay_close(&mut self) {
        self.persist.warm_respawn_ok = true;
        self.spawn_warm();
    }

    /// Spawns the warm host (terminal + exec flag + `crucible overlay-host --socket …`, with the
    /// overlay terminal args and the overlay env) unless warming is off or one already runs.
    fn spawn_warm(&mut self) {
        if self.persist.warm_disabled || self.persist.warm.is_some() {
            return;
        }
        let Some(crucible) = Self::crucible_exe("warm overlay host") else {
            return;
        };
        self.persist.warm_counter = self.persist.warm_counter.wrapping_add(1);
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
        let socket = warm_socket_path(
            runtime.as_deref(),
            self.persist.ipc_path.as_deref(),
            std::process::id(),
            self.persist.warm_counter,
        );
        let program = self.terminal_program();
        let args = with_terminal_args(
            &self.config.overlay.terminal_args,
            warm_host_args(&self.config.terminal.exec_flag, &crucible, &socket),
        );
        if let Some(pid) = self.spawn_process(&program, &args, None, SpawnKind::Overlay) {
            self.persist.pending.insert(
                pid,
                Pending {
                    target: Target::WarmOverlay,
                    since: Instant::now(),
                },
            );
            self.persist.warm = Some(WarmHost {
                pid,
                socket,
                element: None,
                spawned_at: Instant::now(),
            });
        }
    }

    /// Kills the warm host (if any) and clears the slot; never respawns.
    fn discard_warm(&mut self) {
        if let Some(warm) = self.persist.warm.take() {
            self.persist.pending.remove(&warm.pid);
            if !kill_child(&mut self.persist.children, warm.pid) {
                if let Some(element) = &warm.element {
                    close_element(element);
                }
            }
            let _ = std::fs::remove_file(&warm.socket);
        }
    }

    /// The warm host died before use (reaped, or its surface was destroyed while hidden): clear the
    /// slot and respawn at most once per overlay close.
    fn warm_died(&mut self) {
        self.discard_warm();
        let (respawn, ok) = warm_after_death(self.persist.warm_respawn_ok);
        self.persist.warm_respawn_ok = ok;
        if respawn {
            info!("warm overlay host died before use; respawning once");
            self.spawn_warm();
        } else {
            info!("warm overlay host died before use; not respawning until an overlay closes");
        }
    }

    /// Called from the 1 s tick with the reaped PIDs.
    fn warm_reaped(&mut self, reaped: &[u32]) {
        if self
            .persist
            .warm
            .as_ref()
            .is_some_and(|w| reaped.contains(&w.pid))
        {
            self.warm_died();
        }
    }

    /// If `surface` is the hidden warm host's toplevel, treats it as dead and returns `true`.
    pub(crate) fn warm_toplevel_destroyed(
        &mut self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) -> bool {
        let hit = self.warm_element_for(surface).is_some();
        if hit {
            // Already gone: drop the element so `discard_warm` does not try to close it.
            if let Some(w) = self.persist.warm.as_mut() {
                w.element = None;
            }
            self.warm_died();
        }
        hit
    }

    /// The hidden warm toplevel whose root surface is `surface` (E7: it is not in the Space, so
    /// the commit and initial-configure lookups ask for it here too).
    pub(crate) fn warm_element_for(
        &self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) -> Option<WindowElement> {
        self.persist
            .warm
            .as_ref()
            .and_then(|w| w.element.as_ref())
            .filter(|e| e.wl_surface().is_some_and(|s| &*s == surface))
            .cloned()
    }

    /// Stores a newly arrived warm-host toplevel hidden in the warm slot (never in the Space). An
    /// X11 window is killed and warming is disabled for the session.
    fn store_warm(&mut self, element: WindowElement, pid: Option<u32>) {
        let Some(warm_pid) = self.persist.warm.as_ref().map(|w| w.pid) else {
            // The slot was discarded meanwhile: end the stray host without showing it.
            if !pid.is_some_and(|p| kill_child(&mut self.persist.children, p)) {
                close_element(&element);
            }
            return;
        };
        if element.x11_surface().is_some() {
            warn!(
                pid = warm_pid,
                "warm overlay host arrived as an X11 window; overlay pre-warming disabled for this session"
            );
            self.persist.warm_disabled = true;
            if !kill_child(&mut self.persist.children, warm_pid) {
                close_element(&element);
            }
            if let Some(w) = self.persist.warm.take() {
                let _ = std::fs::remove_file(&w.socket);
            }
            return;
        }
        if let Some(toplevel) = element.toplevel() {
            let rect = overlay_rect(self.focused_output_rect());
            // The initial configure goes out on the first commit and carries this pending size.
            toplevel.with_pending_state(|s| s.size = Some(Size::from((rect.width, rect.height))));
        }
        if let Some(w) = self.persist.warm.as_mut() {
            w.element = Some(element);
        }
    }

    /// Whether the warm host is ready: toplevel stored, initial configure sent, socket bound.
    fn warm_ready(warm: &WarmHost) -> bool {
        let Some(toplevel) = warm.element.as_ref().and_then(WindowElement::toplevel) else {
            return false;
        };
        Self::initial_configure_sent(toplevel) && warm.socket.exists()
    }

    /// Whether `toplevel`'s initial configure has gone out.
    fn initial_configure_sent(toplevel: &smithay::wayland::shell::xdg::ToplevelSurface) -> bool {
        with_states(toplevel.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .is_some_and(|d| d.lock().is_ok_and(|d| d.initial_configure_sent))
        })
    }

    /// On `Spawn`/`ReplaceThenSpawn`: hands the mode to a ready warm host and maps it as the
    /// overlay; returns `true` when it did. Otherwise kills any warm host (the caller cold-spawns).
    fn try_open_warm(&mut self, kind: OverlayKind, args: &[OsString]) -> bool {
        let ready = self.persist.warm.as_ref().map(Self::warm_ready);
        match warm_use_decision(ready) {
            WarmUse::Cold { kill } => {
                if kill {
                    info!("warm overlay host not ready; cold spawn");
                    self.discard_warm();
                }
                false
            }
            WarmUse::Use => {
                let Some(warm) = self.persist.warm.clone() else {
                    return false;
                };
                let sent = warm_mode_line(epoch_ms_now(), args)
                    .ok_or_else(|| "unencodable mode".to_string())
                    .and_then(|line| {
                        send_mode_line(&warm.socket, &line).map_err(|e| e.to_string())
                    });
                let (Ok(()), Some(element)) = (sent.clone(), warm.element.clone()) else {
                    warn!(err = ?sent.err(), "warm overlay host hand-off failed; cold spawn");
                    self.discard_warm();
                    return false;
                };
                let started = Instant::now();
                self.persist.warm = None;
                self.persist.overlay = Some(Overlay {
                    kind,
                    state: OverlayState::Launching(warm.pid),
                    pid: Some(warm.pid),
                });
                self.map_overlay(element);
                info!(
                    kind = "warm-overlay",
                    overlay = ?kind,
                    pid = warm.pid,
                    warm_age_ms = u64::try_from(warm.spawned_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                    ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    "latency: request→map"
                );
                // Pre-warm the next host now, not at close: an overlay replaced directly by
                // another (keys → launch) would otherwise always take the cold path.
                self.start_warm_overlay();
                true
            }
        }
    }

    /// Config reload: a changed terminal program, exec flag or overlay terminal args invalidates
    /// the warm host; it is replaced with a fresh one.
    pub(crate) fn warm_config_changed(&mut self) {
        if self.persist.warm.is_some() {
            info!("overlay terminal config changed; restarting the warm overlay host");
            self.discard_warm();
            self.spawn_warm();
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
        if let Some(rec) = pid
            .and_then(|p| match_pending(p, &ancestors, |k| self.persist.spawned.contains_key(&k)))
            .and_then(|k| self.persist.spawned.get(&k).map(|rec| (k, rec)))
            .map(|(k, rec)| (k, rec.clone()))
        {
            info!(
                kind = ?rec.1.kind,
                program = %rec.1.program,
                pid = rec.0,
                spawn_to_map_ms = u64::try_from(rec.1.ms_until(Instant::now())).unwrap_or(u64::MAX),
                "latency: spawn→map"
            );
        }
        // The class must be known before the first placement (every mapping path applies the
        // layout), so the window's first rect already honours terminal vs app.
        let terminal = pid.is_some_and(|p| is_terminal_spawn(p, &ancestors, &self.persist.spawned));
        let info = WindowInfo { pid, terminal };
        match pending.map(|p| p.target) {
            Some(Target::Overlay) => self.map_overlay(element),
            Some(Target::WarmOverlay) => self.store_warm(element, pid),
            Some(Target::Pane {
                pane,
                saved_index,
                focused,
            }) => {
                self.map_placed(element, pane, saved_index, focused, info);
            }
            Some(Target::Slot {
                pane,
                index,
                focused,
                saved_index,
            }) => {
                self.map_slot(element, pane, index, focused, saved_index, info);
            }
            None => {
                self.map_managed_window(element, Self::register_info(info));
            }
        }
    }

    /// An `on_open` hook recording `info` for the new window id before its first placement.
    fn register_info(info: WindowInfo) -> impl FnOnce(&mut Self, WindowId) {
        move |this, id| {
            this.persist.windows.insert(id, info);
        }
    }

    /// Every managed window classified as a CDE-spawned terminal.
    pub(crate) fn terminal_window_ids(&self) -> std::collections::HashSet<WindowId> {
        self.persist
            .windows
            .iter()
            .filter(|(_, i)| i.terminal)
            .map(|(id, _)| *id)
            .collect()
    }

    /// The placement class of window `id`: a CDE-spawned terminal, else an app.
    pub(crate) fn window_class(&self, id: WindowId) -> crate::layout::WindowClass {
        if self.persist.windows.get(&id).is_some_and(|i| i.terminal) {
            crate::layout::WindowClass::Terminal
        } else {
            crate::layout::WindowClass::App
        }
    }

    fn map_placed(
        &mut self,
        element: WindowElement,
        pane: PaneId,
        saved_index: usize,
        focused: bool,
        info: WindowInfo,
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
        match self.map_managed_window_at(element.clone(), pane, index, Self::register_info(info)) {
            Ok(id) => {
                self.finish_placement(pane, id, Some(saved_index), focused);
                id
            }
            Err(err) => {
                warn!(%err, "restore placement failed; using the focused pane");
                self.map_managed_window(element, Self::register_info(info))
            }
        }
    }

    /// Records a placed window's saved index (if any) and applies its stack focus.
    fn finish_placement(
        &mut self,
        pane: PaneId,
        id: WindowId,
        saved_index: Option<usize>,
        focused: bool,
    ) {
        if let Some(saved_index) = saved_index {
            self.persist
                .placed
                .entry(pane)
                .or_default()
                .push((id, saved_index));
        }
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
    }

    /// Maps a relaunched app into the slot its placeholder held (clamped to the live stack).
    fn map_slot(
        &mut self,
        element: WindowElement,
        pane: PaneId,
        index: usize,
        focused: bool,
        saved_index: Option<usize>,
        info: WindowInfo,
    ) -> WindowId {
        let Some(live_len) = self.layout.world.pane(pane).map(|p| p.windows.len()) else {
            warn!(?pane, "relaunch pane is gone; using the focused pane");
            return self.map_managed_window(element, Self::register_info(info));
        };
        match self.map_managed_window_at(
            element.clone(),
            pane,
            slot_index(index, live_len),
            Self::register_info(info),
        ) {
            Ok(id) => {
                self.finish_placement(pane, id, saved_index, focused);
                id
            }
            Err(err) => {
                warn!(%err, "relaunch placement failed; using the focused pane");
                self.map_managed_window(element, Self::register_info(info))
            }
        }
    }

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
                return match self.run_checked(&cmd) {
                    Ok(_) => Response::Ok(String::new()),
                    Err(reason) => Response::Err(reason),
                };
            }
            Request::RunInTerminal(cmd) => {
                return match self.run_in_terminal_checked(&cmd) {
                    Ok(_) => Response::Ok(String::new()),
                    Err(reason) => Response::Err(reason),
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

    /// Swaps the placeholder window run by `peer` for its app: spawn the saved cmdline, register
    /// the placeholder's live slot as pending, close the placeholder.
    fn ipc_relaunch(&mut self, peer: Option<u32>) -> Result<(), String> {
        let peer = peer.ok_or_else(|| "not a relaunch placeholder".to_string())?;
        let args = std::fs::read(proc_root().join(peer.to_string()).join("cmdline"))
            .map(|b| crucible_persist::capture::parse_cmdline(&b))
            .unwrap_or_default();
        let capture = crucible_persist::parse_relaunch_banner(&args)
            .ok_or_else(|| "not a relaunch placeholder".to_string())?;
        let ancestors = crucible_persist::ancestors(proc_root(), peer, ANCESTOR_DEPTH);
        let windows = &self.persist.windows;
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
                let (index, focused) = slot_for(&pane.windows, pane.focused, id)?;
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
            .placed
            .get(&pane)
            .and_then(|v| v.iter().find(|(w, _)| *w == id).map(|(_, i)| *i));
        let element = self.layout.element(id).cloned();
        let pid = self
            .spawn_command_in(&capture.cmdline, ws)
            .ok_or_else(|| "spawn failed".to_string())?;
        self.persist.pending.insert(
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
        let connected: Vec<(String, OutputId)> = self
            .space
            .outputs()
            .filter_map(|o| Some((o.name(), crate::layout::model_output_id(o)?)))
            .collect();
        let output = restore_target(
            record.output.as_deref(),
            &connected,
            self.layout.world.focused_output(),
        );
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
                .last_output
                .entry(ws)
                .or_insert_with(|| name.clone());
        }
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

    fn save_slots(&self) {
        let path = slots_path(&self.persist.store_dir);
        if let Err(err) = self.persist.slots.save(&path) {
            warn!(%err, path = %path.display(), "cannot save the slot pins");
        }
    }

    /// `Action::PinSlot(n)`: pins the active workspace's name to slot `n` and saves the pins.
    pub(crate) fn pin_slot(&mut self, n: u8) {
        let ws = self.layout.world.active_workspace_id();
        let Some(name) = self.layout.world.workspace(ws).map(|w| w.name.clone()) else {
            return;
        };
        if !self.persist.slots.pin(n, &name) {
            return;
        }
        info!(slot = n, workspace = name, "slot pinned");
        self.save_slots();
    }

    /// `Action::Slot(n)`: opens the workspace pinned to slot `n` (live, else restored from disk
    /// on its last-viewed output). An unpinned slot or a failed open is an `info!` no-op; a stale
    /// pin is kept.
    pub(crate) fn slot_open(&mut self, n: u8) {
        let Some(name) = self.persist.slots.get(n).map(str::to_string) else {
            info!(slot = n, "slot is not pinned");
            return;
        };
        if let Err(reason) = self.ipc_open(&name) {
            info!(
                slot = n,
                workspace = name,
                reason,
                "slot workspace not opened"
            );
        }
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

#[cfg(test)]
mod tests {
    use super::{kill_child, reap_exited};

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
    fn spawn_record_measures_elapsed_ms_and_saturates() {
        let rec = SpawnRecord::now(SpawnKind::App, "x");
        assert_eq!(rec.ms_until(rec.at + Duration::from_millis(250)), 250);
        assert_eq!(rec.ms_until(rec.at), 0);
        assert!(rec.epoch_ms > 0);
    }

    #[test]
    fn is_terminal_spawn_depends_on_the_matched_kind() {
        let spawned: HashMap<u32, SpawnRecord> = [
            (10, SpawnRecord::now(SpawnKind::Terminal, "t")),
            (20, SpawnRecord::now(SpawnKind::App, "a")),
            (30, SpawnRecord::now(SpawnKind::Overlay, "o")),
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
        let cap = crucible_persist::parse_relaunch_banner(&argv[1..]).unwrap();
        assert_eq!(cap.app_id.as_deref(), Some("x"));
        assert_eq!(cap.cmdline, "x --y");
    }

    #[test]
    fn slot_for_reports_current_index_and_focus() {
        let mut w = World::new();
        let a = w.open_window();
        let b = w.open_window();
        let pane = w.focused_pane_id();
        let windows = w.pane(pane).unwrap().windows.clone();
        let focused = w.pane(pane).unwrap().focused;
        let ia = windows.iter().position(|x| *x == a).unwrap();
        let ib = windows.iter().position(|x| *x == b).unwrap();
        assert_eq!(slot_for(&windows, focused, a), Some((ia, ia == focused)));
        assert_eq!(slot_for(&windows, focused, b), Some((ib, ib == focused)));
        assert_eq!(slot_for(&[], 0, a), None);
        assert_eq!(slot_index(5, 2), 2);
        assert_eq!(slot_index(1, 2), 1);
    }

    /// Audit E1: the slot is the placeholder's live index, not its saved-order index.
    #[test]
    fn relaunch_slot_uses_live_index_not_saved_order() {
        let mut w = World::new();
        let pane = w.focused_pane_id();
        let t1 = w.open_window_at(pane, 0).unwrap();
        let p = w.open_window_at(pane, 1).unwrap();
        let t2 = w.open_window_at(pane, 2).unwrap();
        // L: an ordinary window opened after restore, no `placed` entry, lands before P.
        let l = w.open_window_at(pane, 1).unwrap();
        let windows = w.pane(pane).unwrap().windows.clone();
        assert_eq!(windows, vec![t1, l, p, t2]);
        let placed = [(t1, 0usize), (p, 1), (t2, 2)];
        // Saved order would put P at 1 (wrong: before L).
        let saved_order = insert_index(placed.iter().filter(|(w, _)| *w != p).map(|(_, i)| *i), 1);
        assert_eq!(saved_order, 1);
        w.set_stack_focus(pane, 2).unwrap();
        let (index, focused) = slot_for(&windows, w.pane(pane).unwrap().focused, p).unwrap();
        assert_eq!((index, focused), (2, true));
        w.close_window(p).unwrap();
        let live_len = w.pane(pane).unwrap().windows.len();
        let app = w.open_window_at(pane, slot_index(index, live_len)).unwrap();
        assert_eq!(w.pane(pane).unwrap().windows, vec![t1, l, app, t2]);
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
    fn warm_slot_uses_only_a_ready_host_and_respawns_once() {
        assert_eq!(warm_use_decision(Some(true)), WarmUse::Use);
        assert_eq!(warm_use_decision(Some(false)), WarmUse::Cold { kill: true });
        assert_eq!(warm_use_decision(None), WarmUse::Cold { kill: false });
        // Dead before use: one respawn after a close/startup, then none until re-armed.
        let (respawn, ok) = warm_after_death(true);
        assert!(respawn);
        assert_eq!(warm_after_death(ok), (false, false));
    }

    #[test]
    fn warm_socket_path_prefers_runtime_dir_then_ipc_dir() {
        assert_eq!(
            warm_socket_path(Some(Path::new("/run/u")), Some(Path::new("/i/s")), 7, 2),
            PathBuf::from("/run/u/cde-overlay-host-7-2.sock")
        );
        assert_eq!(
            warm_socket_path(Some(Path::new("")), Some(Path::new("/i/s")), 7, 3),
            PathBuf::from("/i/cde-overlay-host-7-3.sock")
        );
        assert_eq!(
            warm_socket_path(None, None, 1, 1),
            std::env::temp_dir().join("cde-overlay-host-1-1.sock")
        );
    }

    #[test]
    fn warm_mode_line_carries_the_cold_subcommand_argv() {
        let pick = picker_args("-e", Path::new("/c"), Some("a:b"));
        assert_eq!(
            warm_mode_line(42, &pick).as_deref(),
            Some("42\tpick\t--rename\ta%3Ab\n")
        );
        assert_eq!(
            warm_mode_line(1, &launcher_args("-e", Path::new("/c"))).as_deref(),
            Some("1\tlaunch\n")
        );
        assert_eq!(
            warm_mode_line(1, &keys_args("-e", Path::new("/c"))).as_deref(),
            Some("1\tkeys\n")
        );
        assert_eq!(warm_mode_line(1, &[OsString::from("-e")]), None);
        let bad = vec![OsString::from("-e"), "/c".into(), "pick\tx".into()];
        assert_eq!(warm_mode_line(1, &bad), None);
        assert_eq!(
            warm_host_args("-e", Path::new("/c"), Path::new("/s.sock")),
            vec![
                OsString::from("-e"),
                "/c".into(),
                "overlay-host".into(),
                "--socket".into(),
                "/s.sock".into()
            ]
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
        let args = run_in_terminal_args("-e", Path::new("/x/crucible"), Path::new("/w"), "a b; c");
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

    fn oid(n: u64) -> OutputId {
        OutputId::from_raw(n)
    }

    #[test]
    fn shifted_output_wraps_both_ways_and_skips_single_output() {
        let order = [oid(1), oid(2), oid(3)];
        assert_eq!(shifted_output(&order, oid(1), 1), Some(oid(2)));
        assert_eq!(shifted_output(&order, oid(3), 1), Some(oid(1)));
        assert_eq!(shifted_output(&order, oid(1), -1), Some(oid(3)));
        assert_eq!(shifted_output(&order, oid(2), 7), Some(oid(3)));
        assert_eq!(shifted_output(&order, oid(2), -8), Some(oid(3)));
        // A full lap lands back on the source: no-op.
        assert_eq!(shifted_output(&order, oid(2), 3), None);
        assert_eq!(shifted_output(&[oid(1)], oid(1), 1), None);
        assert_eq!(shifted_output(&order, oid(9), 1), None);
    }

    #[test]
    fn restore_target_prefers_the_connected_last_viewed_output() {
        let connected = vec![("HDMI-1".to_string(), oid(1)), ("DP-1".to_string(), oid(2))];
        assert_eq!(restore_target(Some("DP-1"), &connected, oid(1)), oid(2));
        assert_eq!(restore_target(Some("DP-9"), &connected, oid(1)), oid(1));
        assert_eq!(restore_target(None, &connected, oid(2)), oid(2));
    }

    #[test]
    fn note_last_viewed_records_only_active_workspaces() {
        let mut map = HashMap::new();
        let (a, b) = (WorkspaceId::from_raw(1), WorkspaceId::from_raw(2));
        note_last_viewed(&mut map, &[(oid(1), a, "DP-1".into())]);
        note_last_viewed(&mut map, &[(oid(1), b, "HDMI-1".into())]);
        assert_eq!(map.get(&a).map(String::as_str), Some("DP-1"));
        assert_eq!(map.get(&b).map(String::as_str), Some("HDMI-1"));
    }

    /// Audit E2: a workspace last viewed on A, migrated inactive onto B by A's unplug, still says A
    /// and returns to A once it reconnects.
    #[test]
    fn unplug_migration_keeps_the_last_viewed_output() {
        let mut world = World::new();
        let a = world.focused_output();
        let b = world.add_output(crucible_layout::Rect::new(1920, 0, 1920, 1080));
        let on_a = world.active_workspace_id();
        let on_b = world.output(b).unwrap().active_workspace();
        let mut map = HashMap::new();
        let viewed =
            |w: &World, outs: &[(OutputId, &str)]| -> Vec<(OutputId, WorkspaceId, String)> {
                outs.iter()
                    .map(|(o, n)| {
                        (
                            *o,
                            w.output(*o).unwrap().active_workspace(),
                            (*n).to_string(),
                        )
                    })
                    .collect()
            };
        note_last_viewed(&mut map, &viewed(&world, &[(a, "DP-1"), (b, "HDMI-1")]));
        world.remove_output(a).unwrap();
        assert!(world.output(b).unwrap().workspaces.contains(&on_a));
        assert_eq!(world.output(b).unwrap().active_workspace(), on_b);
        note_last_viewed(&mut map, &viewed(&world, &[(b, "HDMI-1")]));
        assert_eq!(map.get(&on_a).map(String::as_str), Some("DP-1"));
        // A reconnects as a fresh model output; the record (output = DP-1) targets it.
        let a2 = world.add_output(crucible_layout::Rect::new(0, 0, 1920, 1080));
        let connected = vec![("HDMI-1".to_string(), b), ("DP-1".to_string(), a2)];
        assert_eq!(
            restore_target(map.get(&on_a).map(String::as_str), &connected, b),
            a2
        );
    }
}
