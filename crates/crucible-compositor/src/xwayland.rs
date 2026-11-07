//! XWayland integration: spawning the server and managing X11 windows.
//!
//! Brings up XWayland so legacy X11 apps run alongside native Wayland clients in the same
//! [`Space`](smithay::desktop::Space):
//!
//! 1. [`CrucibleState::start_xwayland`] spawns the `Xwayland` server
//!    ([`XWayland::spawn`](smithay::xwayland::XWayland)) and inserts it as a calloop source.
//! 2. On [`XWaylandEvent::Ready`] it attaches an [`X11Wm`] window manager
//!    ([`X11Wm::start_wm`]), sets the per-client scale (X11 clients don't understand `wl_output`
//!    fractional scale), records the X11 `DISPLAY` number, and exports `DISPLAY` so children CDE
//!    spawns can reach the X server.
//! 3. The [`XwmHandler`] impl reparents/maps X11 windows into the `Space` through the same lifecycle
//!    controller as the xdg path ([`crate::layout`]), mirrors override-redirect (OR) window geometry, and bridges the
//!    X11↔Wayland clipboard.
//!
//! X11 *focus* is driven through the seat: focusing a [`KeyboardFocusTarget::Window`] backed by an
//! [`X11Surface`] activates it (the surface's
//! [`KeyboardTarget`](smithay::input::keyboard::KeyboardTarget) impl issues the X11
//! `set_input_focus`/take-focus), and the input path additionally raises it via
//! [`X11Wm::raise_window`] — see [`crate::input`] and [`crate::focus`].
//!
//! ## Scope
//! CDE's tiling model owns window geometry, so interactive move/resize grabs and maximize are not
//! honoured: [`XwmHandler::move_request`]/[`XwmHandler::resize_request`] are deliberate no-ops.
//! Fullscreen (`_NET_WM_STATE_FULLSCREEN`) is honoured through the model: the
//! [`XwmHandler::fullscreen_request`]/[`XwmHandler::unfullscreen_request`] handlers set the
//! window's workspace fullscreen, and placement sets the X state and configures the output rect.
//! Placement comes from the projection, never from the client.

use std::os::unix::{io::OwnedFd, net::UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::{
    reexports::wayland_server::Client,
    utils::{Logical, Rectangle},
    wayland::{compositor::CompositorHandler, selection::SelectionTarget},
    xwayland::{
        X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler,
        xwm::{Reorder, ResizeEdge as X11ResizeEdge, XwmId},
    },
};
use tracing::{debug, error, info, trace, warn};

use crate::{
    autostart::OutcomeSite,
    element::WindowElement,
    layout::{Binding, Teardown},
    selection::{
        claim_x11_selection, x11_cleared_selection, x11_new_selection, x11_send_selection,
    },
    state::{Backend, CrucibleState},
};

/// How long the session start waits for the first XWayland outcome before starting anyway. A
/// healthy XWayland answers in well under a second; 5 s is long enough not to race a slow cold
/// start (the session then restores with a real `DISPLAY`) yet short enough that a hung server
/// delays the restored session by an unnoticeable moment rather than leaving it empty.
pub(crate) const SESSION_START_FALLBACK: Duration = Duration::from_secs(5);

/// The `DISPLAY` CDE's children get: XWayland's once it is serving, else the **empty string**.
/// Unset is not enough (`env -u DISPLAY urxvt` falls back to `:0`, the live desktop; an empty
/// `DISPLAY` aborts "can't open display"). **Pure** so the spawn seam is unit-testable.
#[must_use]
pub(crate) fn child_display(xdisplay: Option<u32>) -> String {
    xdisplay.map_or_else(String::new, |n| format!(":{n}"))
}

/// Environment variable that overrides the scale applied to XWayland clients.
const XWAYLAND_SCALE_ENV: &str = "CDE_XWAYLAND_SCALE";

/// Resolves the scale applied to XWayland clients.
///
/// X11 clients do not understand `wl_output` fractional scale, so CDE applies a per-client buffer
/// scale instead. Reads [`XWAYLAND_SCALE_ENV`] (parsed as a positive `f64`), falling back to
/// `default` when unset, unparseable, or non-positive. The caller passes
/// [`crucible_config::Config::max_output_scale`] as `default`: the largest configured `output`
/// scale, mapped or not, and 1 with no `output` blocks. **Pure** (takes the raw env value and the
/// default) so it is unit-testable without a running compositor.
fn xwayland_scale(env_value: Option<&str>, default: f64) -> f64 {
    env_value
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|scale| *scale > 0.0)
        .unwrap_or(default)
}

/// Logs when a config reload changes the XWayland default scale: XWayland's client scale is fixed
/// when it starts, so X11 apps pick the new value up only after a CDE restart.
pub(crate) fn note_xwayland_scale_change(old_default: f64, new_default: f64) {
    if old_default != new_default && std::env::var_os(XWAYLAND_SCALE_ENV).is_none() {
        info!(
            old = old_default,
            new = new_default,
            "largest output scale changed; X11 apps pick it up after a CDE restart"
        );
    }
}

/// The X resources file loaded into XWayland: `$HOME/.Xresources` when it exists. **Pure** (takes
/// `home` and an existence check) so it is unit-testable.
fn xresources_file(home: Option<&Path>, exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let file = home?.join(".Xresources");
    exists(&file).then_some(file)
}

/// Loads `~/.Xresources` into the new X server, as an X session's startup would (`xrdb -merge`),
/// so X11 apps such as urxvt see their resources. Runs only when the file exists; a missing `xrdb`
/// is logged at debug and a failed start as a warning. Returns the running merge, which the caller
/// waits on ([`CrucibleState::start_session_after_merge`]) before anything X11 is spawned: an app
/// reads its resources once, at startup.
fn merge_xresources(display_number: u32) -> Option<(Child, PathBuf)> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let file = xresources_file(home.as_deref(), Path::is_file)?;
    let child = std::process::Command::new("xrdb")
        .arg("-merge")
        .arg(&file)
        .env("DISPLAY", format!(":{display_number}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match child {
        Ok(child) => Some((child, file)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            debug!("xrdb not installed; X resources not loaded");
            None
        }
        Err(err) => {
            warn!(%err, "xrdb -merge could not be started");
            None
        }
    }
}

/// How long the session start waits for `xrdb -merge` (it takes tens of milliseconds).
const XRDB_WAIT: Duration = Duration::from_secs(1);
/// How often the session start polls the running merge.
const XRDB_POLL: Duration = Duration::from_millis(10);

/// What one poll of the running merge decides ([`xrdb_wait_step`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XrdbWait {
    /// Still running and within [`XRDB_WAIT`]: poll again.
    Poll,
    /// Finished (or its status is unreadable): start the session.
    Start,
    /// Still running past [`XRDB_WAIT`]: start the session anyway and leave it to finish.
    GiveUp,
}

/// Decides one poll: `finished` is whether the merge has exited (or cannot be waited on).
/// **Pure.**
fn xrdb_wait_step(finished: bool, elapsed: Duration) -> XrdbWait {
    if finished {
        XrdbWait::Start
    } else if elapsed < XRDB_WAIT {
        XrdbWait::Poll
    } else {
        XrdbWait::GiveUp
    }
}

/// Logs a finished merge's outcome.
fn log_merge(file: &Path, status: std::io::Result<std::process::ExitStatus>) {
    match status {
        Ok(status) if status.success() => {
            info!(file = %file.display(), "merged X resources into XWayland");
        }
        Ok(status) => warn!(file = %file.display(), %status, "xrdb -merge failed"),
        Err(err) => warn!(%err, "xrdb -merge could not be waited on"),
    }
}

/// Reaps a merge left running on a thread, so it never blocks the event loop or becomes a zombie.
fn reap_merge_later(mut child: Child, file: PathBuf) {
    std::thread::spawn(move || log_merge(&file, child.wait()));
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Spawns the XWayland server and wires its readiness into the event loop.
    ///
    /// Inserts the [`XWayland`] source into calloop; on [`XWaylandEvent::Ready`] it starts the
    /// [`X11Wm`], sets the per-client scale, records the `DISPLAY` number, and exports `DISPLAY`
    /// into the compositor's own environment so any X11 children CDE spawns find the server.
    pub fn start_xwayland(&mut self) {
        // No outcome in `SESSION_START_FALLBACK` starts the session anyway (children keep the
        // empty `DISPLAY`); a later outcome still runs autostart.
        let fallback = self.handle.insert_source(
            Timer::from_duration(SESSION_START_FALLBACK),
            |_, _, data| {
                data.on_xwayland_outcome(OutcomeSite::Timer);
                TimeoutAction::Drop
            },
        );
        if let Err(err) = fallback {
            error!(%err, "cannot arm the session-start fallback timer");
        }
        // The server (and so its X clients' defaults) sees the configured cursor theme and size.
        let env = crate::cursor::child_env(&self.config.cursor);
        let (xwayland, client) = match XWayland::spawn(
            &self.display_handle,
            None,
            env,
            true,
            Stdio::null(),
            Stdio::null(),
            |_| (),
        ) {
            Ok(ret) => ret,
            Err(err) => {
                error!(%err, "Failed to start XWayland");
                self.on_xwayland_outcome(OutcomeSite::StartFailed);
                return;
            }
        };

        let ret = self
            .handle
            .insert_source(xwayland, move |event, _, data| match event {
                XWaylandEvent::Ready {
                    x11_socket,
                    display_number,
                } => {
                    // Success or WM-attach failure alike: the session starts, then autostart
                    // runs, each exactly once, after any `.Xresources` merge has landed.
                    match data.on_xwayland_ready(&client, x11_socket, display_number) {
                        Some(merge) => data.start_session_after_merge(merge),
                        None => data.on_xwayland_outcome(OutcomeSite::Ready),
                    }
                }
                XWaylandEvent::Error => {
                    warn!("XWayland crashed on startup");
                    data.on_xwayland_outcome(OutcomeSite::Crashed);
                }
            });
        if let Err(err) = ret {
            error!(%err, "Failed to insert the XWayland source into the event loop");
            self.on_xwayland_outcome(OutcomeSite::StartFailed);
        }
    }

    /// Handles [`XWaylandEvent::Ready`]: applies the per-client scale, attaches the [`X11Wm`],
    /// claims both selections, records the `DISPLAY` number and exports it.
    ///
    /// A failed WM attach is logged and leaves `xwm` unset. Does **not** run the autostart; the
    /// event-loop callback does that once per outcome. Returns the running `.Xresources` merge, if
    /// one was started.
    fn on_xwayland_ready(
        &mut self,
        client: &Client,
        x11_socket: UnixStream,
        display_number: u32,
    ) -> Option<(Child, PathBuf)> {
        let mut merge = None;
        // X11 clients can't interpret fractional wl_output scale; apply it per-client.
        let scale = xwayland_scale(
            std::env::var(XWAYLAND_SCALE_ENV).ok().as_deref(),
            self.config.max_output_scale(),
        );
        self.client_compositor_state(client).set_client_scale(scale);

        match X11Wm::start_wm(
            self.handle.clone(),
            &self.display_handle,
            x11_socket,
            client.clone(),
        ) {
            Ok(mut wm) => {
                // Own both X11 selections from the start (with no data yet).
                // `X11Wm::new_selection` queues `SetSelectionOwner` without
                // flushing, so an ownership change only reaches the X server on
                // the next X11 event, and an `xclip` started after a Wayland copy
                // loses that race. Owning early means a later Wayland copy only
                // updates the mime types; `claim_x11_selection` flushes each claim
                // (as it does after every later Wayland copy) so the queued
                // ownership goes out immediately.
                for ty in [SelectionTarget::Clipboard, SelectionTarget::Primary] {
                    claim_x11_selection(&mut wm, ty, Some(Vec::new()));
                }
                self.xwm = Some(wm);
                self.xdisplay = Some(display_number);
                // Export DISPLAY so X11 children spawned by CDE reach this server.
                // SAFETY: set during single-threaded event-loop dispatch, before any
                // child is spawned that would read it.
                #[allow(unsafe_code)]
                unsafe {
                    std::env::set_var("DISPLAY", format!(":{display_number}"));
                }
                info!(
                    display = display_number,
                    "XWayland ready (DISPLAY=:{display_number})"
                );
                merge = merge_xresources(display_number);
            }
            Err(err) => {
                error!(%err, "Failed to attach X11 window manager");
            }
        }
        merge
    }

    /// Starts the session (the [`OutcomeSite::Ready`] outcome) once the `.Xresources` merge has
    /// exited, polling it from an event-loop timer so nothing blocks. Without the wait, the warm
    /// overlay host and restored X11 apps are spawned while `xrdb` is still running and start
    /// without their resources. Past [`XRDB_WAIT`] the session starts anyway and the merge is
    /// reaped on a thread.
    fn start_session_after_merge(&mut self, merge: (Child, PathBuf)) {
        let started = Instant::now();
        let mut pending = Some(merge);
        let timer = self
            .handle
            .insert_source(Timer::immediate(), move |_, _, data| {
                let Some((mut child, file)) = pending.take() else {
                    return TimeoutAction::Drop;
                };
                let status = child.try_wait();
                let finished = !matches!(status, Ok(None));
                match xrdb_wait_step(finished, started.elapsed()) {
                    XrdbWait::Poll => {
                        pending = Some((child, file));
                        return TimeoutAction::ToDuration(XRDB_POLL);
                    }
                    XrdbWait::Start => match status {
                        Ok(Some(exit)) => log_merge(&file, Ok(exit)),
                        Err(err) => log_merge(&file, Err(err)),
                        // `finished` is false for `Ok(None)`, so this step never sees it.
                        Ok(None) => {}
                    },
                    XrdbWait::GiveUp => {
                        warn!(
                            file = %file.display(),
                            "xrdb -merge still running after {XRDB_WAIT:?}; starting the session"
                        );
                        reap_merge_later(child, file);
                    }
                }
                data.on_xwayland_outcome(OutcomeSite::Ready);
                TimeoutAction::Drop
            });
        if let Err(err) = timer {
            // The closure (and the merge it owned) was dropped with the failed source; the
            // session must not wait on it.
            error!(%err, "cannot arm the xrdb wait; starting the session now");
            self.on_xwayland_outcome(OutcomeSite::Ready);
        }
    }

    /// Finds the mapped [`WindowElement`] backed by the X11 `surface`, if any.
    fn x11_element(&self, surface: &X11Surface) -> Option<WindowElement> {
        self.space
            .elements()
            .find(|e| e.x11_surface() == Some(surface))
            .cloned()
    }
}

impl<B: Backend + 'static> XwmHandler for CrucibleState<B> {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().unwrap()
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        // Accepting the map is protocol I/O and may fail recoverably: log and decline the map,
        // leaving the model untouched. Then hand the element to the same lifecycle controller a
        // Wayland toplevel goes through, which lands it in the focused pane and lets the projection
        // drive its tiled geometry, visibility, and focus. No centered placeholder.
        if let Err(err) = window.set_mapped(true) {
            warn!(%err, "Failed to mark X11 window mapped");
            return;
        }
        let pid = window.pid();
        self.map_new_window(WindowElement::new_x11_window(window), pid, None);
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        // OR windows (menus, tooltips, DnD) place themselves, so they stay outside the `World` —
        // but the controller tracks them so `unmapped_window`/`destroyed_window` below can tear them
        // down symmetrically. Honor their last configured location.
        let location = window.last_configure().loc;
        self.map_unmanaged_element(WindowElement::new_x11_window(window), location);
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        // One controller call covers both kinds: a managed window is closed in the model before its
        // binding and `Space` mapping are released; an override-redirect element is untracked and
        // unmapped. An element that is already gone resolves to `Teardown::Absent` — no error.
        // A managed close also runs the managed-close hook (recovery record, ephemeral prune).
        self.unmap_matching(|e| e.x11_surface() == Some(&window));
        if !window.is_override_redirect() {
            // Protocol I/O: best-effort, and harmless to repeat.
            let _ = window.set_mapped(false);
        }
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        // Symmetry with the map paths: an X11 window can be destroyed without an `unmapped_window`
        // first (notably override-redirect surfaces, which never route through the model). The same
        // controller call tears down whichever owner still holds it, and is a no-op after an unmap
        // already ran.
        match self.unmap_matching(|e| e.x11_surface() == Some(&window)) {
            Teardown::Absent => {
                trace!("X11 window destroyed after its unmap; nothing left to tear down");
            }
            teardown => debug!(?teardown, "Tore down X11 window on destroy"),
        }
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        _x: Option<i32>,
        _y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        // Managed windows are sized by the projection alone (ICCCM 4.1.5: the client is told its
        // real geometry via a synthetic ConfigureNotify); only unmanaged ones honour the size.
        let placement = match self.layout.resolve(|e| e.x11_surface() == Some(&window)) {
            Binding::Managed(id) => match self.projection.rect_of(id) {
                Some(tile) => X11Placement::ManagedVisible(self.placed_rect(id, tile)),
                None => X11Placement::ManagedHidden,
            },
            Binding::Unmanaged | Binding::None => X11Placement::Unmanaged,
        };
        let geo = x11_configure_geometry(placement, w, h, window.last_configure());
        let _ = window.configure(geo);
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        // Mirror the X11 geometry into the Space so OR windows track where the X server put them.
        // Managed windows are placed by the projection alone: echoing their X geometry back would
        // snap them to wherever the X server last had them (originally 0,0 — the left output).
        if !window.is_override_redirect() {
            return;
        }
        let Some(elem) = self.x11_element(&window) else {
            return;
        };
        self.space.map_element(elem, geometry.loc, false);
    }

    fn resize_request(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _button: u32,
        _edges: X11ResizeEdge,
    ) {
        // CDE tiling owns geometry; interactive resize grabs are deliberately not honoured.
        trace!("Ignoring X11 resize_request (tiling owns geometry)");
    }

    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {
        // CDE tiling owns geometry; interactive move grabs are deliberately not honoured.
        trace!("Ignoring X11 move_request (tiling owns geometry)");
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        // Placement (`apply_layout`) sets `_NET_WM_STATE_FULLSCREEN` and configures the output rect.
        self.request_fullscreen(|e| e.x11_surface() == Some(&window), true);
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.request_fullscreen(|e| e.x11_surface() == Some(&window), false);
    }

    /// Lets every XWayland client read the bridged Wayland selection. Smithay's default (`false`)
    /// refuses all of them, and Anvil's "an X11 window has keyboard focus" policy would still
    /// refuse a Wayland-native app that shells out to `xclip` (cvault-editor). XWayland clients
    /// are the user's own apps, which X11 under i3 already lets read the clipboard.
    fn allow_selection_access(&mut self, _xwm: XwmId, _selection: SelectionTarget) -> bool {
        true
    }

    fn send_selection(
        &mut self,
        _xwm: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
    ) {
        x11_send_selection(self, selection, mime_type, fd);
    }

    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        x11_new_selection(self, selection, mime_types);
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        x11_cleared_selection(self, selection);
    }

    fn disconnected(&mut self, _xwm: XwmId) {
        self.xwm = None;
    }
}

#[cfg(test)]
mod tests {
    use super::{xresources_file, xwayland_scale};
    use std::path::{Path, PathBuf};

    #[test]
    fn the_session_waits_for_the_xrdb_merge_but_never_past_the_limit() {
        use super::{XRDB_WAIT, XrdbWait, xrdb_wait_step};
        use std::time::Duration;
        // A finished merge starts the session at once, even at the limit.
        assert_eq!(xrdb_wait_step(true, Duration::ZERO), XrdbWait::Start);
        assert_eq!(xrdb_wait_step(true, XRDB_WAIT * 2), XrdbWait::Start);
        // A running merge is polled until the limit, then given up on.
        assert_eq!(xrdb_wait_step(false, Duration::ZERO), XrdbWait::Poll);
        assert_eq!(
            xrdb_wait_step(false, XRDB_WAIT - Duration::from_millis(1)),
            XrdbWait::Poll
        );
        assert_eq!(xrdb_wait_step(false, XRDB_WAIT), XrdbWait::GiveUp);
    }

    #[test]
    fn xresources_file_is_home_xresources_when_present() {
        let home = Path::new("/home/u");
        assert_eq!(
            xresources_file(Some(home), |_| true),
            Some(PathBuf::from("/home/u/.Xresources"))
        );
        assert_eq!(xresources_file(Some(home), |_| false), None);
        assert_eq!(xresources_file(None, |_| true), None);
    }

    #[test]
    fn scale_defaults_to_one_when_unset() {
        assert_eq!(xwayland_scale(None, 1.0), 1.0);
    }

    #[test]
    fn scale_parses_a_positive_value() {
        assert_eq!(xwayland_scale(Some("2"), 1.0), 2.0);
        assert_eq!(xwayland_scale(Some(" 1.5 "), 1.0), 1.5);
    }

    #[test]
    fn scale_rejects_garbage_and_non_positive() {
        assert_eq!(xwayland_scale(Some("not-a-number"), 1.0), 1.0);
        assert_eq!(xwayland_scale(Some("0"), 1.0), 1.0);
        assert_eq!(xwayland_scale(Some("-3"), 1.0), 1.0);
        assert_eq!(xwayland_scale(Some(""), 1.0), 1.0);
    }

    fn config_with(text: &str) -> crucible_config::Config {
        crucible_config::parse(text).unwrap()
    }

    #[test]
    fn default_follows_the_largest_configured_output_and_env_wins() {
        // DP-9 need not be connected: the rule reads the config, not the mapped outputs.
        let config =
            config_with("output \"DP-1\" { scale 1.4167; }\noutput \"DP-9\" { scale 2; }\n");
        assert_eq!(xwayland_scale(None, config.max_output_scale()), 2.0);
        assert_eq!(
            xwayland_scale(Some("1.25"), config.max_output_scale()),
            1.25
        );
        assert_eq!(xwayland_scale(Some("junk"), config.max_output_scale()), 2.0);
        let none = config_with("");
        assert_eq!(xwayland_scale(None, none.max_output_scale()), 1.0);
    }
}

/// Where a window that sent an X11 `ConfigureRequest` stands in the layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum X11Placement {
    /// Managed and in the current projection, at this rect.
    ManagedVisible(Rectangle<i32, Logical>),
    /// Managed but hidden (absent from the projection): its last configured geometry stands.
    ManagedHidden,
    /// Not bound in the `World` (yet).
    Unmanaged,
}

/// The geometry to answer a `ConfigureRequest` with. **Pure** so the rule is unit-testable.
///
/// Managed windows ignore the requested size and get their projected rect (visible) or their
/// last configured geometry (hidden) re-sent. Unmanaged ones keep today's rule: honour the size,
/// never the position.
fn x11_configure_geometry(
    placement: X11Placement,
    w: Option<u32>,
    h: Option<u32>,
    last: Rectangle<i32, Logical>,
) -> Rectangle<i32, Logical> {
    match placement {
        X11Placement::ManagedVisible(rect) => rect,
        X11Placement::ManagedHidden => last,
        X11Placement::Unmanaged => {
            let mut geo = last;
            if let Some(w) = w {
                geo.size.w = w as i32;
            }
            if let Some(h) = h {
                geo.size.h = h as i32;
            }
            geo
        }
    }
}

#[cfg(test)]
mod configure_tests {
    use super::*;

    fn r(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    #[test]
    fn managed_visible_gets_projected_rect_ignoring_request() {
        let proj = r(10, 20, 300, 200);
        let got = x11_configure_geometry(
            X11Placement::ManagedVisible(proj),
            Some(999),
            Some(888),
            r(0, 0, 50, 50),
        );
        assert_eq!(got, proj);
    }

    #[test]
    fn managed_hidden_resends_last_geometry_ignoring_request() {
        let last = r(5, 6, 70, 80);
        let got = x11_configure_geometry(X11Placement::ManagedHidden, Some(999), Some(888), last);
        assert_eq!(got, last);
    }

    #[test]
    fn unmanaged_honours_size_but_never_position() {
        let last = r(5, 6, 70, 80);
        let got = x11_configure_geometry(X11Placement::Unmanaged, Some(400), None, last);
        assert_eq!(got, r(5, 6, 400, 80));
    }
}

#[cfg(test)]
mod display_tests {
    use super::child_display;

    #[test]
    fn child_display_is_empty_until_xwayland_serves() {
        // Before readiness and after a failed start: the empty string, never unset or inherited.
        assert_eq!(child_display(None), "");
        assert_eq!(child_display(Some(3)), ":3");
    }
}
