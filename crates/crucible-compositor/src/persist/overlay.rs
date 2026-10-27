//! The single overlay slot (picker / launcher / keys) and the pre-warmed overlay host: slot and
//! warm-slot decisions, the host's mode line and socket, and the open/map/close state transitions.

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
    SpawnKind, epoch_ms_now, keys_args, kill_child, launcher_args, picker_args, with_terminal_args,
};
use crate::element::WindowElement;
use crate::focus::KeyboardFocusTarget;
use crate::layout::model_output_id;
use crate::state::{Backend, CrucibleState};

/// The overlay slot and the pre-warmed host (a field of [`super::PersistState`]).
#[derive(Debug, Default)]
pub(super) struct OverlayRuntime {
    pub(super) overlay: Option<Overlay>,
    /// The pre-warmed overlay host, if one is running.
    pub(super) warm: Option<WarmHost>,
    /// Whether a warm host that dies before use may be respawned (once per overlay close).
    pub(super) warm_respawn_ok: bool,
    /// Warming is off for the session (the warm host came up as an X11 window).
    pub(super) warm_disabled: bool,
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

#[derive(Debug, Clone)]
pub(super) enum OverlayState {
    /// Spawned, waiting for its window to map.
    Launching(u32),
    /// Mapped as the overlay.
    Mapped(WindowElement),
}

/// The single overlay slot's occupant.
#[derive(Debug, Clone)]
pub(super) struct Overlay {
    pub(super) kind: OverlayKind,
    pub(super) state: OverlayState,
    /// The spawned terminal's PID, kept once mapped so the overlay can be ended by killing it
    /// (a polite close makes the terminal ask "close window?" while the TUI runs).
    pub(super) pid: Option<u32>,
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

/// A pre-warmed overlay terminal running `crucible overlay-host --socket <socket>`.
#[derive(Debug, Clone)]
pub(super) struct WarmHost {
    /// The spawned terminal's PID (killed to end it, kept as the overlay's PID once used).
    pub(super) pid: u32,
    /// The host's mode socket.
    pub(super) socket: PathBuf,
    /// Its toplevel once it arrived (stored hidden; never in the Space while warm).
    pub(super) element: Option<WindowElement>,
    /// Monotonic spawn time (for the log line when it is used).
    pub(super) spawned_at: Instant,
}

/// What `open_overlay` does with the warm slot when it has to spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WarmUse {
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
pub(super) fn warm_use_decision(warm: Option<bool>) -> WarmUse {
    match warm {
        Some(true) => WarmUse::Use,
        Some(false) => WarmUse::Cold { kill: true },
        None => WarmUse::Cold { kill: false },
    }
}

/// Pure respawn bound for a warm host that died before use: returns `(respawn, respawn_ok after)`.
/// One respawn is allowed per overlay close (and at startup); the flag is re-armed only there.
pub(super) fn warm_after_death(respawn_ok: bool) -> (bool, bool) {
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

impl<B: Backend + 'static> CrucibleState<B> {
    // ---- picker overlay ------------------------------------------------------------------------

    /// Opens the picker (or, with `rename`, the rename prompt); see [`Self::open_overlay`].
    pub fn open_picker(&mut self, rename: bool) {
        let Some(crucible) = Self::crucible_exe("picker") else {
            return;
        };
        if rename {
            self.persist.workspace.pending_pin = None;
        }
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
        self.persist.workspace.pending_pin = None;
        let Some(crucible) = Self::crucible_exe("launcher") else {
            return;
        };
        let args = launcher_args(&self.config.terminal.exec_flag, &crucible);
        self.open_overlay(OverlayKind::Launcher, args);
    }

    /// Opens the read-only settings overlay (`crucible keys`); see [`Self::open_overlay`].
    pub fn open_settings(&mut self) {
        self.persist.workspace.pending_pin = None;
        let Some(crucible) = Self::crucible_exe("settings") else {
            return;
        };
        let args = keys_args(&self.config.terminal.exec_flag, &crucible);
        self.open_overlay(OverlayKind::Keys, args);
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

    /// Opens an overlay of `kind` running the terminal with `args`. A mapped overlay of the same
    /// kind is refocused, one still launching is left alone, and a mapped overlay of a different
    /// kind is asked to close before the new one spawns.
    pub(super) fn open_overlay(&mut self, kind: OverlayKind, args: Vec<OsString>) {
        let current = self.persist.overlay.overlay.clone();
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
            self.persist.placement.pending.insert(
                pid,
                Pending {
                    target: Target::Overlay,
                    since: Instant::now(),
                },
            );
            self.persist.overlay.overlay = Some(Overlay {
                kind,
                state: OverlayState::Launching(pid),
                pid: Some(pid),
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

    pub(super) fn map_overlay(&mut self, element: WindowElement) {
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
            .overlay
            .as_ref()
            .map_or((OverlayKind::Picker, None), |o| (o.kind, o.pid));
        self.persist.overlay.overlay = Some(Overlay {
            kind,
            state: OverlayState::Mapped(element),
            pid,
        });
    }

    /// The overlay's element, only once it is mapped.
    pub(crate) fn mapped_overlay(&self) -> Option<&WindowElement> {
        match &self.persist.overlay.overlay {
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
        }) = self.persist.overlay.overlay.clone()
        else {
            return;
        };
        self.end_overlay(&element, pid);
    }

    /// Ends an overlay by killing its terminal process, so the terminal never asks to confirm
    /// closing a window with a running TUI. Falls back to a polite close when CDE holds no handle.
    pub(super) fn end_overlay(&mut self, element: &WindowElement, pid: Option<u32>) {
        if !pid.is_some_and(|p| kill_child(&mut self.persist.spawn.children, p)) {
            close_element(element);
        }
    }

    /// Called when an unmanaged element was torn down: if it was the mapped overlay, return keyboard
    /// focus to the layout's focused window. A stale close of a replaced overlay is ignored.
    pub(crate) fn unmanaged_closed(&mut self, element: &WindowElement) {
        if matches!(
            &self.persist.overlay.overlay,
            Some(Overlay { state: OverlayState::Mapped(p), .. }) if p == element
        ) {
            self.persist.overlay.overlay = None;
            self.persist.workspace.pending_pin = None;
            self.set_keyboard_focus(None);
            self.reconcile_keyboard_focus();
            self.rewarm_after_overlay_close();
        }
    }

    // ---- pre-warmed overlay host ---------------------------------------------------------------

    /// Starts a warm overlay host at startup (once the compositor can spawn overlays).
    pub(crate) fn start_warm_overlay(&mut self) {
        self.persist.overlay.warm_respawn_ok = true;
        self.spawn_warm();
    }

    /// An overlay closed: re-arm the one-respawn allowance and start a warm host if none runs.
    pub(super) fn rewarm_after_overlay_close(&mut self) {
        self.persist.overlay.warm_respawn_ok = true;
        self.spawn_warm();
    }

    /// Spawns the warm host (terminal + exec flag + `crucible overlay-host --socket …`, with the
    /// overlay terminal args and the overlay env) unless warming is off or one already runs.
    pub(super) fn spawn_warm(&mut self) {
        if self.persist.overlay.warm_disabled || self.persist.overlay.warm.is_some() {
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
            &self.config.overlay.terminal_args,
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
            self.persist.overlay.warm = Some(WarmHost {
                pid,
                socket,
                element: None,
                spawned_at: Instant::now(),
            });
        }
    }

    /// Kills the warm host (if any) and clears the slot; never respawns.
    pub(super) fn discard_warm(&mut self) {
        if let Some(warm) = self.persist.overlay.warm.take() {
            self.persist.placement.pending.remove(&warm.pid);
            if !kill_child(&mut self.persist.spawn.children, warm.pid) {
                if let Some(element) = &warm.element {
                    close_element(element);
                }
            }
            let _ = std::fs::remove_file(&warm.socket);
        }
    }

    /// The warm host died before use (reaped, or its surface was destroyed while hidden): clear the
    /// slot and respawn at most once per overlay close.
    pub(super) fn warm_died(&mut self) {
        self.discard_warm();
        let (respawn, ok) = warm_after_death(self.persist.overlay.warm_respawn_ok);
        self.persist.overlay.warm_respawn_ok = ok;
        if respawn {
            info!("warm overlay host died before use; respawning once");
            self.spawn_warm();
        } else {
            info!("warm overlay host died before use; not respawning until an overlay closes");
        }
    }

    /// Called from the 1 s tick with the reaped PIDs.
    pub(super) fn warm_reaped(&mut self, reaped: &[u32]) {
        if self
            .persist
            .overlay
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
            if let Some(w) = self.persist.overlay.warm.as_mut() {
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
            .overlay
            .warm
            .as_ref()
            .and_then(|w| w.element.as_ref())
            .filter(|e| e.wl_surface().is_some_and(|s| &*s == surface))
            .cloned()
    }

    /// Stores a newly arrived warm-host toplevel hidden in the warm slot (never in the Space). An
    /// X11 window is killed and warming is disabled for the session.
    pub(super) fn store_warm(&mut self, element: WindowElement, pid: Option<u32>) {
        let Some(warm_pid) = self.persist.overlay.warm.as_ref().map(|w| w.pid) else {
            // The slot was discarded meanwhile: end the stray host without showing it.
            if !pid.is_some_and(|p| kill_child(&mut self.persist.spawn.children, p)) {
                close_element(&element);
            }
            return;
        };
        if element.x11_surface().is_some() {
            warn!(
                pid = warm_pid,
                "warm overlay host arrived as an X11 window; overlay pre-warming disabled for this session"
            );
            self.persist.overlay.warm_disabled = true;
            if !kill_child(&mut self.persist.spawn.children, warm_pid) {
                close_element(&element);
            }
            if let Some(w) = self.persist.overlay.warm.take() {
                let _ = std::fs::remove_file(&w.socket);
            }
            return;
        }
        if let Some(toplevel) = element.toplevel() {
            let rect = overlay_rect(self.focused_output_rect());
            // The initial configure goes out on the first commit and carries this pending size.
            toplevel.with_pending_state(|s| s.size = Some(Size::from((rect.width, rect.height))));
        }
        if let Some(w) = self.persist.overlay.warm.as_mut() {
            w.element = Some(element);
        }
    }

    /// Whether the warm host is ready: toplevel stored, initial configure sent, socket bound.
    pub(super) fn warm_ready(warm: &WarmHost) -> bool {
        let Some(toplevel) = warm.element.as_ref().and_then(WindowElement::toplevel) else {
            return false;
        };
        Self::initial_configure_sent(toplevel) && warm.socket.exists()
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
    /// overlay; returns `true` when it did. Otherwise kills any warm host (the caller cold-spawns).
    pub(super) fn try_open_warm(&mut self, kind: OverlayKind, args: &[OsString]) -> bool {
        let ready = self.persist.overlay.warm.as_ref().map(Self::warm_ready);
        match warm_use_decision(ready) {
            WarmUse::Cold { kill } => {
                if kill {
                    info!("warm overlay host not ready; cold spawn");
                    self.discard_warm();
                }
                false
            }
            WarmUse::Use => {
                let Some(warm) = self.persist.overlay.warm.clone() else {
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
                self.persist.overlay.warm = None;
                self.persist.overlay.overlay = Some(Overlay {
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
        if self.persist.overlay.warm.is_some() {
            info!("overlay terminal config changed; restarting the warm overlay host");
            self.discard_warm();
            self.spawn_warm();
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
}
