//! The single overlay slot (picker / launcher / keys) and the pre-warmed overlay host: slot
//! decisions, the host's mode line and socket, and the open/map/close state transitions (the warm
//! slot's own transitions are pure, in [`super::warm_slot`]).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crucible_layout::Rect;
use smithay::utils::{Point, Rectangle, Size};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
use tracing::{error, info, warn};

use super::close_element;
use super::placement::{Pending, Target};
use super::spawn::{
    SpawnKind, epoch_ms_now, keys_args, launcher_args, picker_args, with_terminal_args,
};
use super::warm_slot::{WarmEffect, WarmHost, WarmSlot};
use super::workspace::PinEvent;
use crate::element::WindowElement;
use crate::focus::KeyboardFocusTarget;
use crate::layout::model_output_id;
use crate::state::{Backend, CrucibleState};

/// The overlay slot and the pre-warmed host (a field of [`super::PersistState`]).
#[derive(Debug, Default)]
pub(super) struct OverlayRuntime {
    pub(super) overlay: Option<Overlay>,
    /// The pre-warmed overlay host's slot.
    pub(super) warm: WarmSlot<WindowElement>,
    /// Counter making each warm host's socket path unique.
    pub(super) warm_counter: u32,
}

/// Which program an overlay slot runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OverlayKind {
    Picker,
    Launcher,
    Keys,
}

/// The single overlay slot's occupant.
#[derive(Debug, Clone)]
pub(super) struct Overlay {
    pub(super) kind: OverlayKind,
    /// The spawned terminal's PID, so the overlay can be ended by killing it (a polite close makes
    /// the terminal ask "close window?" while the TUI runs).
    pub(super) pid: u32,
    /// `None` while launching (waiting for its window to map), else the mapped element.
    pub(super) element: Option<WindowElement>,
}

/// What opening an overlay of a requested kind does given the slot's current occupant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OpenDecision {
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
pub(super) fn overlay_open_decision(
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
pub(super) fn send_mode_line(socket: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
    stream.set_write_timeout(Some(Duration::from_millis(5)))?;
    stream.write_all(line.as_bytes())
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

/// What the picker overlay is opened for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerMode {
    /// The workspace picker.
    Pick,
    /// The rename prompt for the active workspace.
    Rename,
    /// The workspace-root prompt for the active workspace.
    Root,
}

impl<B: Backend + 'static> CrucibleState<B> {
    // ---- picker overlay ------------------------------------------------------------------------

    /// Opens the picker, the rename prompt or the workspace-root prompt per `mode`; see
    /// [`Self::open_overlay`]. Rename and root prompts do not keep the pin, like each other.
    pub fn open_picker(&mut self, mode: PickerMode) {
        let rename =
            (mode == PickerMode::Rename).then(|| self.layout.world.focus_path().workspace_name);
        let root = (mode == PickerMode::Root).then(|| {
            let ws = self.layout.world.active_workspace_id();
            self.persist
                .workspace
                .roots
                .get(&ws)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
        let exec_flag = self.config.terminal.exec_flag.clone();
        self.open_overlay(OverlayKind::Picker, mode == PickerMode::Pick, |crucible| {
            picker_args(&exec_flag, crucible, rename.as_deref(), root.as_deref())
        });
    }

    /// Opens the program launcher; see [`Self::open_overlay`].
    pub fn open_launcher(&mut self) {
        let exec_flag = self.config.terminal.exec_flag.clone();
        self.open_overlay(OverlayKind::Launcher, false, |crucible| {
            launcher_args(&exec_flag, crucible)
        });
    }

    /// Opens the read-only settings overlay (`crucible keys`); see [`Self::open_overlay`].
    pub fn open_settings(&mut self) {
        let exec_flag = self.config.terminal.exec_flag.clone();
        self.open_overlay(OverlayKind::Keys, false, |crucible| {
            keys_args(&exec_flag, crucible)
        });
    }

    pub(super) fn crucible_exe(what: &str) -> Option<PathBuf> {
        match std::env::current_exe() {
            Ok(p) => Some(p),
            Err(err) => {
                error!(%err, "cannot resolve the crucible binary; no {what}");
                None
            }
        }
    }

    /// Opens an overlay of `kind` running the terminal with `args(crucible)`. Every open first
    /// applies [`PinEvent::OpenOverlay`] (`keeps_pin` only for the plain picker), whatever the
    /// decision. A mapped overlay of the same kind is refocused, one still launching is left alone,
    /// and a mapped overlay of a different kind is asked to close before the new one spawns.
    fn open_overlay(
        &mut self,
        kind: OverlayKind,
        keeps_pin: bool,
        args: impl FnOnce(&Path) -> Vec<OsString>,
    ) {
        // A hover enter held back by the previous overlay moves focus first, so this one opens on
        // the pointer's output.
        self.apply_deferred_hover();
        self.pin_event(PinEvent::OpenOverlay { keeps_pin });
        let what = match kind {
            OverlayKind::Picker => "picker",
            OverlayKind::Launcher => "launcher",
            OverlayKind::Keys => "settings",
        };
        let Some(crucible) = Self::crucible_exe(what) else {
            return;
        };
        let args = args(&crucible);
        let current = self.persist.overlay.overlay.clone();
        let summary = current.as_ref().map(|o| (o.kind, o.element.is_some()));
        let replaced = match (overlay_open_decision(summary, kind), current) {
            (OpenDecision::Ignore, _) => return,
            (
                OpenDecision::Refocus,
                Some(
                    overlay @ Overlay {
                        element: Some(_), ..
                    },
                ),
            ) => {
                // Re-place on the focused output (it may have moved), raise and refocus.
                if let Some(element) = overlay.element.clone() {
                    self.map_overlay(overlay, element);
                }
                return;
            }
            (OpenDecision::Refocus, _) => return,
            (
                OpenDecision::ReplaceThenSpawn,
                Some(Overlay {
                    element: Some(old),
                    pid,
                    ..
                }),
            ) => {
                self.end_overlay(&old, pid);
                true
            }
            (OpenDecision::ReplaceThenSpawn | OpenDecision::Spawn, _) => false,
        };
        // Order matters: the ready warm host is consumed first, then the replaced overlay's close
        // re-arms the warm slot and spawns the next host.
        let used_warm = self.try_open_warm(kind, &args);
        if replaced {
            // The replaced overlay is closing: that is an overlay close for the warm slot too.
            self.rewarm_after_overlay_close();
        }
        if used_warm {
            return;
        }
        let program = self.terminal_program();
        let args = with_terminal_args(&self.config.overlay.effective_terminal_args(&program), args);
        if let Some(pid) = self.spawn_process(&program, &args, None, SpawnKind::Overlay) {
            self.persist.placement.pending.insert(
                pid,
                Pending {
                    target: Target::Overlay,
                    since: Instant::now(),
                },
            );
            self.persist.overlay.overlay = Some(Overlay {
                kind,
                pid,
                element: None,
            });
        }
    }

    pub(super) fn focused_output_rect(&self) -> Rect {
        let focused = self.layout.world.focused_output();
        self.space
            .outputs()
            .find(|o| model_output_id(o) == Some(focused))
            .and_then(|o| self.space.output_geometry(o))
            .map_or(Rect::new(0, 0, 1280, 800), |g| {
                Rect::new(g.loc.x, g.loc.y, g.size.w, g.size.h)
            })
    }

    /// A cold-spawned overlay's window arrived (`pid` from the arrival). It maps as the slot's
    /// overlay; with an empty slot (the overlay was given up or replaced meanwhile) it is a stray and
    /// is ended without being shown, like a stray warm host.
    pub(super) fn overlay_arrived(&mut self, element: WindowElement, pid: Option<u32>) {
        match self.persist.overlay.overlay.take() {
            Some(overlay) => self.map_overlay(overlay, element),
            None => {
                warn!(
                    ?pid,
                    "overlay window arrived after its slot was cleared; ending it"
                );
                self.run_warm_effects(vec![WarmEffect::Kill {
                    pid,
                    element: Some(element),
                }]);
            }
        }
    }

    /// Maps `element` as `overlay` and stores it in the slot.
    pub(super) fn map_overlay(&mut self, overlay: Overlay, element: WindowElement) {
        let loc = self.size_overlay(&element);
        self.map_unmanaged_element(element.clone(), loc);
        self.space.raise_element(&element, true);
        self.set_keyboard_focus(Some(KeyboardFocusTarget::Window(element.clone())));
        self.persist.overlay.overlay = Some(Overlay {
            element: Some(element),
            ..overlay
        });
    }

    /// Sizes `element` to the overlay rect of the focused output (pending toplevel size, or an X11
    /// configure) and returns the location to map it at. Shared by [`Self::map_overlay`] and
    /// [`Self::replace_mapped_overlay`].
    fn size_overlay(&self, element: &WindowElement) -> Point<i32, smithay::utils::Logical> {
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
        loc
    }

    /// Re-places a mapped overlay after an output geometry change (a scale applied or changed).
    /// Overlays are unmanaged, so `apply_layout` never moves them; no-op without a mapped overlay.
    pub(crate) fn replace_mapped_overlay(&mut self) {
        let Some(element) = self.mapped_overlay().cloned() else {
            return;
        };
        let loc = self.size_overlay(&element);
        self.space.map_element(element, loc, false);
    }

    /// The overlay's element, only once it is mapped.
    pub(crate) fn mapped_overlay(&self) -> Option<&WindowElement> {
        self.persist.overlay.overlay.as_ref()?.element.as_ref()
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
            element: Some(element),
            pid,
            ..
        }) = self.persist.overlay.overlay.clone()
        else {
            return;
        };
        self.end_overlay(&element, pid);
    }

    /// Ends an overlay by killing its terminal process, so the terminal never asks to confirm
    /// closing a window with a running TUI. Falls back to a polite close when CDE holds no handle.
    pub(super) fn end_overlay(&mut self, element: &WindowElement, pid: u32) {
        if !self.persist.spawn.kill(pid) {
            close_element(element);
        }
    }

    /// Clears the overlay slot (its overlay closed or was given up) and applies
    /// [`PinEvent::OverlayClosed`].
    pub(super) fn clear_overlay_slot(&mut self) {
        self.persist.overlay.overlay = None;
        self.pin_event(PinEvent::OverlayClosed);
    }

    /// Called when an unmanaged element was torn down: if it was the mapped overlay, return keyboard
    /// focus to the layout's focused window. A stale close of a replaced overlay is ignored.
    pub(crate) fn unmanaged_closed(&mut self, element: &WindowElement) {
        if self.mapped_overlay() == Some(element) {
            self.clear_overlay_slot();
            self.set_keyboard_focus(None);
            self.reconcile_keyboard_focus();
            self.rewarm_after_overlay_close();
        }
    }

    // ---- pre-warmed overlay host ---------------------------------------------------------------

    /// Runs warm-slot effects in order: `Kill` forgets the pending placement and kills the PID,
    /// else politely closes the element; `RemoveSocket` removes the file; `Spawn` spawns a host.
    pub(super) fn run_warm_effects(&mut self, effects: Vec<WarmEffect<WindowElement>>) {
        for effect in effects {
            match effect {
                WarmEffect::Kill { pid, element } => {
                    if let Some(pid) = pid {
                        self.persist.placement.pending.remove(&pid);
                    }
                    if !pid.is_some_and(|p| self.persist.spawn.kill(p)) {
                        if let Some(element) = &element {
                            close_element(element);
                        }
                    }
                }
                WarmEffect::RemoveSocket(socket) => {
                    let _ = std::fs::remove_file(&socket);
                }
                WarmEffect::Spawn => self.spawn_warm(),
            }
        }
    }

    /// Starts a warm overlay host at startup (once the compositor can spawn overlays).
    pub(crate) fn start_warm_overlay(&mut self) {
        let effects = self.persist.overlay.warm.arm();
        self.run_warm_effects(effects);
    }

    /// An overlay closed: re-arm the one-respawn allowance and start a warm host if none runs.
    pub(super) fn rewarm_after_overlay_close(&mut self) {
        self.start_warm_overlay();
    }

    /// Spawns the warm host (terminal + exec flag + `crucible overlay-host --socket …`, with the
    /// overlay terminal args and the overlay env) unless warming is off or one already runs.
    fn spawn_warm(&mut self) {
        if !self.persist.overlay.warm.can_spawn() {
            return;
        }
        let Some(crucible) = Self::crucible_exe("warm overlay host") else {
            return;
        };
        self.persist.overlay.warm_counter = self.persist.overlay.warm_counter.wrapping_add(1);
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
        let socket = warm_socket_path(
            runtime.as_deref(),
            self.persist.ipc_path.as_deref(),
            std::process::id(),
            self.persist.overlay.warm_counter,
        );
        let program = self.terminal_program();
        let args = with_terminal_args(
            &self.config.overlay.effective_terminal_args(&program),
            warm_host_args(&self.config.terminal.exec_flag, &crucible, &socket),
        );
        if let Some(pid) = self.spawn_process(&program, &args, None, SpawnKind::Overlay) {
            self.persist.placement.pending.insert(
                pid,
                Pending {
                    target: Target::WarmOverlay,
                    since: Instant::now(),
                },
            );
            self.persist
                .overlay
                .warm
                .spawned(pid, socket, Instant::now());
        }
    }

    /// Runs the effects of a warm host's death before use, logging whether it respawns.
    fn warm_died(&mut self, effects: Vec<WarmEffect<WindowElement>>) {
        if effects.is_empty() {
            return;
        }
        if effects.contains(&WarmEffect::Spawn) {
            info!("warm overlay host died before use; respawning once");
        } else {
            info!("warm overlay host died before use; not respawning until an overlay closes");
        }
        self.run_warm_effects(effects);
    }

    /// Called from the 1 s tick with the reaped PIDs.
    pub(super) fn warm_reaped(&mut self, reaped: &[u32]) {
        let effects = self.persist.overlay.warm.reaped(reaped);
        self.warm_died(effects);
    }

    /// If `surface` is the hidden warm host's toplevel, treats it as dead and returns `true`.
    pub(crate) fn warm_toplevel_destroyed(
        &mut self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) -> bool {
        let hit = self.warm_element_for(surface).is_some();
        if hit {
            let effects = self.persist.overlay.warm.died(true);
            self.warm_died(effects);
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
            .overlay
            .warm
            .element()
            .filter(|e| e.wl_surface().is_some_and(|s| &*s == surface))
            .cloned()
    }

    /// Stores a newly arrived warm-host toplevel hidden in the warm slot (never in the Space). An
    /// X11 window is killed and warming is disabled for the session; a stray (no live host) is
    /// ended without being shown.
    pub(super) fn store_warm(&mut self, element: WindowElement, pid: Option<u32>) {
        let x11 = element.x11_surface().is_some();
        if let Some(warm_pid) = self.persist.overlay.warm.host().map(|h| h.pid) {
            if x11 {
                warn!(
                    pid = warm_pid,
                    "warm overlay host arrived as an X11 window; overlay pre-warming disabled for this session"
                );
            } else if let Some(toplevel) = element.toplevel() {
                let rect = overlay_rect(self.focused_output_rect());
                // The initial configure goes out on the first commit and carries this pending size.
                toplevel
                    .with_pending_state(|s| s.size = Some(Size::from((rect.width, rect.height))));
            }
        }
        let effects = self.persist.overlay.warm.arrived(element, pid, x11);
        self.run_warm_effects(effects);
    }

    /// Whether the warm host is ready: initial configure of its stored toplevel sent, socket bound.
    fn warm_ready(host: &WarmHost, element: &WindowElement) -> bool {
        element
            .toplevel()
            .is_some_and(|t| Self::initial_configure_sent(t) && host.socket.exists())
    }

    /// Whether `toplevel`'s initial configure has gone out.
    pub(super) fn initial_configure_sent(
        toplevel: &smithay::wayland::shell::xdg::ToplevelSurface,
    ) -> bool {
        with_states(toplevel.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .is_some_and(|d| d.lock().is_ok_and(|d| d.initial_configure_sent))
        })
    }

    /// On `Spawn`/`ReplaceThenSpawn`: hands the mode to a ready warm host and maps it as the
    /// overlay; returns `true` when it did. Otherwise discards any warm host (the caller
    /// cold-spawns).
    fn try_open_warm(&mut self, kind: OverlayKind, args: &[OsString]) -> bool {
        let warm = &self.persist.overlay.warm;
        let ready_socket = match (warm.host(), warm.element()) {
            (Some(host), Some(element)) if Self::warm_ready(host, element) => {
                Some(host.socket.clone())
            }
            _ => None,
        };
        let Some(socket) = ready_socket else {
            let effects = self.persist.overlay.warm.discard();
            if !effects.is_empty() {
                info!("warm overlay host not ready; cold spawn");
                self.run_warm_effects(effects);
            }
            return false;
        };
        let sent = warm_mode_line(epoch_ms_now(), args)
            .ok_or_else(|| "unencodable mode".to_string())
            .and_then(|line| send_mode_line(&socket, &line).map_err(|e| e.to_string()));
        if let Err(err) = sent {
            warn!(err = ?Some(err), "warm overlay host hand-off failed; cold spawn");
            let effects = self.persist.overlay.warm.discard();
            self.run_warm_effects(effects);
            return false;
        }
        let Some((warm, element)) = self.persist.overlay.warm.take_ready() else {
            return false;
        };
        let started = Instant::now();
        let overlay = Overlay {
            kind,
            pid: warm.pid,
            element: None,
        };
        self.map_overlay(overlay, element);
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

    /// Config reload: a changed terminal program, exec flag or overlay terminal args invalidates
    /// the warm host; it is replaced with a fresh one.
    pub(crate) fn warm_config_changed(&mut self) {
        if self.persist.overlay.warm.host().is_some() {
            info!("overlay terminal config changed; restarting the warm overlay host");
            let effects = self.persist.overlay.warm.config_changed();
            self.run_warm_effects(effects);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_is_centred_at_sixty_by_fifty_percent() {
        assert_eq!(
            overlay_rect(Rect::new(1920, 0, 1000, 800)),
            Rect::new(2120, 200, 600, 400)
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
        let pick = picker_args("-e", Path::new("/c"), Some("a:b"), None);
        assert_eq!(
            warm_mode_line(42, &pick).as_deref(),
            Some("42\tpick\t--rename\ta%3Ab\n")
        );
        let root = picker_args("-e", Path::new("/c"), None, Some(""));
        assert_eq!(
            warm_mode_line(3, &root).as_deref(),
            Some("3\tpick\t--root\t\n")
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
}
