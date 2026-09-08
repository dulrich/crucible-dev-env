//! XWayland integration: spawning the server and managing X11 windows.
//!
//! WS-5 brings up XWayland so legacy X11 apps run alongside native Wayland clients in the same
//! [`Space`](smithay::desktop::Space):
//!
//! 1. [`CrucibleState::start_xwayland`] spawns the `Xwayland` server
//!    ([`XWayland::spawn`](smithay::xwayland::XWayland)) and inserts it as a calloop source.
//! 2. On [`XWaylandEvent::Ready`] it attaches an [`X11Wm`] window manager
//!    ([`X11Wm::start_wm`]), sets the per-client scale (X11 clients don't understand `wl_output`
//!    fractional scale), records the X11 `DISPLAY` number, and exports `DISPLAY` so children CDE
//!    spawns can reach the X server.
//! 3. The [`XwmHandler`] impl reparents/maps X11 windows into the `Space` with the same placeholder
//!    placement as the xdg path, mirrors override-redirect (OR) window geometry, and bridges the
//!    X11↔Wayland clipboard.
//!
//! X11 *focus* is driven through the seat: focusing a [`KeyboardFocusTarget::Window`] backed by an
//! [`X11Surface`] activates it (the surface's
//! [`KeyboardTarget`](smithay::input::keyboard::KeyboardTarget) impl issues the X11
//! `set_input_focus`/take-focus), and the input path additionally raises it via
//! [`X11Wm::raise_window`] — see [`crate::input`] and [`crate::focus`].
//!
//! ## Scope (WS-5)
//! Interactive move/resize grabs and maximize/fullscreen are deferred to Phase 2 (CDE's tiling
//! model owns window geometry), so [`XwmHandler::move_request`]/[`XwmHandler::resize_request`] are
//! intentionally minimal no-ops here. Placement stays the WS-3 placeholder (centered); no udev/DRM
//! work is involved.

use std::os::unix::io::OwnedFd;
use std::process::Stdio;

use smithay::{
    utils::{Logical, Rectangle},
    wayland::{
        compositor::CompositorHandler,
        selection::{
            SelectionTarget,
            data_device::{
                clear_data_device_selection, current_data_device_selection_userdata,
                request_data_device_client_selection, set_data_device_selection,
            },
            primary_selection::{
                clear_primary_selection, current_primary_selection_userdata,
                request_primary_client_selection, set_primary_selection,
            },
        },
    },
    xwayland::{
        X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler,
        xwm::{Reorder, ResizeEdge as X11ResizeEdge, XwmId},
    },
};
use tracing::{debug, error, info, trace, warn};

use crate::{
    element::WindowElement,
    layout::Teardown,
    state::{Backend, CrucibleState},
};

/// Environment variable that overrides the scale applied to XWayland clients.
const XWAYLAND_SCALE_ENV: &str = "CDE_XWAYLAND_SCALE";

/// Resolves the scale applied to XWayland clients.
///
/// X11 clients do not understand `wl_output` fractional scale, so CDE applies a per-client buffer
/// scale instead. Reads [`XWAYLAND_SCALE_ENV`] (parsed as a positive `f64`), falling back to `1.0`
/// when unset, unparseable, or non-positive. **Pure** (takes the raw env value) so it is
/// unit-testable without a running compositor.
fn xwayland_scale(env_value: Option<&str>) -> f64 {
    env_value
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|scale| *scale > 0.0)
        .unwrap_or(1.0)
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Spawns the XWayland server and wires its readiness into the event loop.
    ///
    /// Inserts the [`XWayland`] source into calloop; on [`XWaylandEvent::Ready`] it starts the
    /// [`X11Wm`], sets the per-client scale, records the `DISPLAY` number, and exports `DISPLAY`
    /// into the compositor's own environment so any X11 children CDE spawns find the server.
    pub fn start_xwayland(&mut self) {
        let (xwayland, client) = match XWayland::spawn(
            &self.display_handle,
            None,
            std::iter::empty::<(String, String)>(),
            true,
            Stdio::null(),
            Stdio::null(),
            |_| (),
        ) {
            Ok(ret) => ret,
            Err(err) => {
                error!(%err, "Failed to start XWayland");
                return;
            }
        };

        let display_handle = self.display_handle.clone();
        let ret = self
            .handle
            .insert_source(xwayland, move |event, _, data| match event {
                XWaylandEvent::Ready {
                    x11_socket,
                    display_number,
                } => {
                    // X11 clients can't interpret fractional wl_output scale; apply it per-client.
                    let scale = xwayland_scale(std::env::var(XWAYLAND_SCALE_ENV).ok().as_deref());
                    data.client_compositor_state(&client)
                        .set_client_scale(scale);

                    match X11Wm::start_wm(
                        data.handle.clone(),
                        &display_handle,
                        x11_socket,
                        client.clone(),
                    ) {
                        Ok(wm) => {
                            data.xwm = Some(wm);
                            data.xdisplay = Some(display_number);
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
                        }
                        Err(err) => error!(%err, "Failed to attach X11 window manager"),
                    }
                }
                XWaylandEvent::Error => {
                    warn!("XWayland crashed on startup");
                }
            });
        if let Err(err) = ret {
            error!(%err, "Failed to insert the XWayland source into the event loop");
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
        self.map_managed_window(WindowElement::new_x11_window(window));
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
        // Honor size requests but not self-positioning (placement is the compositor's job).
        let mut geo = window.last_configure();
        if let Some(w) = w {
            geo.size.w = w as i32;
        }
        if let Some(h) = h {
            geo.size.h = h as i32;
        }
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
        // Interactive resize grabs are Phase 2 (CDE tiling owns geometry); no-op for WS-5.
        trace!("Ignoring X11 resize_request (interactive grabs are Phase 2)");
    }

    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {
        // Interactive move grabs are Phase 2 (CDE tiling owns geometry); no-op for WS-5.
        trace!("Ignoring X11 move_request (interactive grabs are Phase 2)");
    }

    fn send_selection(
        &mut self,
        _xwm: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
    ) {
        // X11 client asked for the current Wayland selection: pump it to the provided fd.
        match selection {
            SelectionTarget::Clipboard => {
                if let Err(err) = request_data_device_client_selection(&self.seat, mime_type, fd) {
                    error!(?err, "Failed to send Wayland clipboard to XWayland");
                }
            }
            SelectionTarget::Primary => {
                if let Err(err) = request_primary_client_selection(&self.seat, mime_type, fd) {
                    error!(?err, "Failed to send Wayland primary selection to XWayland");
                }
            }
        }
    }

    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        // An X11 client took ownership of a selection: advertise it to Wayland clients.
        match selection {
            SelectionTarget::Clipboard => {
                set_data_device_selection(&self.display_handle, &self.seat, mime_types, ());
            }
            SelectionTarget::Primary => {
                set_primary_selection(&self.display_handle, &self.seat, mime_types, ());
            }
        }
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        match selection {
            SelectionTarget::Clipboard => {
                if current_data_device_selection_userdata(&self.seat).is_some() {
                    clear_data_device_selection(&self.display_handle, &self.seat);
                }
            }
            SelectionTarget::Primary => {
                if current_primary_selection_userdata(&self.seat).is_some() {
                    clear_primary_selection(&self.display_handle, &self.seat);
                }
            }
        }
    }

    fn disconnected(&mut self, _xwm: XwmId) {
        self.xwm = None;
    }
}

#[cfg(test)]
mod tests {
    use super::xwayland_scale;

    #[test]
    fn scale_defaults_to_one_when_unset() {
        assert_eq!(xwayland_scale(None), 1.0);
    }

    #[test]
    fn scale_parses_a_positive_value() {
        assert_eq!(xwayland_scale(Some("2")), 2.0);
        assert_eq!(xwayland_scale(Some(" 1.5 ")), 1.5);
    }

    #[test]
    fn scale_rejects_garbage_and_non_positive() {
        assert_eq!(xwayland_scale(Some("not-a-number")), 1.0);
        assert_eq!(xwayland_scale(Some("0")), 1.0);
        assert_eq!(xwayland_scale(Some("-3")), 1.0);
        assert_eq!(xwayland_scale(Some("")), 1.0);
    }
}
