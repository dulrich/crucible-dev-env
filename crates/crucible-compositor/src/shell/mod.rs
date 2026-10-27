//! Wayland shell protocol handlers (xdg-shell and wlr-layer-shell).
//!
//! Implements the surface-role lifecycle that turns a committed Wayland surface into a window CDE
//! can render:
//!
//! - **xdg-shell** ([`XdgShellHandler`]): new toplevels are wrapped in a [`WindowElement`] and
//!   mapped through the lifecycle controller in [`crate::layout`], which places them from the
//!   projection. Popups are tracked in the [`PopupManager`]. The first `configure` is sent
//!   lazily from the commit handler ([`ensure_initial_configure`]), as the protocol requires.
//! - **wlr-layer-shell** ([`WlrLayerShellHandler`]): layer surfaces are accepted and arranged via
//!   [`layer_map_for_output`]. No status bar is rendered yet; this exists so
//!   the global is advertised and the exclusive-zone bookkeeping is in place.
//!
//! Protocol I/O is kept at this edge so the layout core stays compositor-free. Input handling lives
//! in [`crate::input`] and the bridge into the `crucible_layout` model in [`crate::layout`]; the
//! handlers here only do the minimum to map and configure surfaces.

use smithay::{
    desktop::{
        PopupKind, PopupManager, Space, WindowSurfaceType, find_popup_root_surface,
        get_popup_toplevel_coords, layer_map_for_output,
    },
    output::Output,
    reexports::wayland_server::protocol::{wl_output::WlOutput, wl_seat, wl_surface::WlSurface},
    utils::Serial,
    wayland::{
        compositor::with_states,
        shell::{
            wlr_layer::{
                Layer, LayerSurface, LayerSurfaceData, WlrLayerShellHandler, WlrLayerShellState,
            },
            xdg::{
                PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
                XdgToplevelSurfaceData,
            },
        },
    },
};
use tracing::warn;

use crate::{
    element::WindowElement,
    state::{Backend, CrucibleState},
};

impl<B: Backend + 'static> CrucibleState<B> {
    /// Finds the mapped [`WindowElement`] whose root surface is `surface`, if any.
    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<WindowElement> {
        self.space
            .elements()
            .find(|window| window.wl_surface().map(|s| &*s == surface).unwrap_or(false))
            .cloned()
    }
}

impl<B: Backend + 'static> XdgShellHandler for CrucibleState<B> {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Do NOT configure here: the initial configure of an xdg_surface must be sent during the
        // first commit (see `ensure_initial_configure`), once the surface is otherwise ready.
        let pid = self.wayland_client_pid(surface.wl_surface());
        let window = WindowElement::new_wayland_window(surface);

        // Route the map through the lifecycle controller, which owns the model/binding/Space commit
        // order. The projection — not a centered placeholder — decides where and at what size this
        // window lands. A window from a pending restore/picker PID is placed accordingly.
        self.map_new_window(window, pid);
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        // Likewise, no configure here — it is sent on commit.
        self.unconstrain_popup(&surface);
        if let Err(err) = self.popups.track_popup(PopupKind::from(surface)) {
            warn!("Failed to track popup: {}", err);
        }
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // Popup grabs (keyboard/pointer) are not yet driven; popups render without a grab.
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        // The controller closes the model first and only then releases the binding + Space mapping,
        // so a rejected close cannot leave a half-torn-down window behind. Matching on the binding
        // (not on the `Space`) also covers a toplevel destroyed while hidden on an inactive
        // workspace or behind a pane stack. A repeated destroy resolves to nothing and is a no-op.
        let target = surface.wl_surface().clone();
        if self.warm_toplevel_destroyed(&target) {
            return;
        }
        self.unmap_matching(|element| element.wl_surface().map(|s| *s == target).unwrap_or(false));
    }

    fn popup_destroyed(&mut self, _surface: PopupSurface) {
        // `PopupManager::cleanup` (run each loop iteration) prunes dead popups.
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Constrains a popup's geometry to the union of its parent window's outputs.
    ///
    /// Mirrors anvil: compute the positioner's anchor rectangle relative to the parent window and
    /// let the positioner pick an unconstrained geometry within the visible outputs.
    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(window) = self.window_for_surface(&root) else {
            return;
        };

        let mut outputs_for_window = self.space.outputs_for_element(&window);
        if outputs_for_window.is_empty() {
            return;
        }

        // Union of all the outputs the parent window is on.
        let mut outputs_geo = match self
            .space
            .output_geometry(&outputs_for_window.pop().unwrap())
        {
            Some(geo) => geo,
            None => return,
        };
        for output in outputs_for_window {
            if let Some(geo) = self.space.output_geometry(&output) {
                outputs_geo = outputs_geo.merge(geo);
            }
        }

        let Some(window_geo) = self.space.element_geometry(&window) else {
            return;
        };

        // The positioner's target must be relative to the parent's geometry.
        let mut target = outputs_geo;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= window_geo.loc;

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}

impl<B: Backend + 'static> WlrLayerShellHandler for CrucibleState<B> {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: LayerSurface,
        wl_output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        let output = wl_output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| self.space.outputs().next().cloned());
        let Some(output) = output else {
            warn!("Rejecting layer surface: no output available to anchor it");
            return;
        };
        let mut map = layer_map_for_output(&output);
        if let Err(err) = map.map_layer(&smithay::desktop::LayerSurface::new(surface, namespace)) {
            warn!("Failed to map layer surface: {}", err);
        }
    }

    fn layer_destroyed(&mut self, surface: LayerSurface) {
        if let Some((mut map, layer)) = self.space.outputs().find_map(|o| {
            let map = layer_map_for_output(o);
            let layer = map
                .layers()
                .find(|&layer| layer.layer_surface() == &surface)
                .cloned();
            layer.map(|layer| (map, layer))
        }) {
            map.unmap_layer(&layer);
        }
    }
}

/// Sends the initial `configure` for a newly committed surface, if it has not been sent yet.
///
/// The xdg-shell protocol forbids configuring a surface before its first commit, so the compositor
/// defers the first configure to here (called from [`CrucibleState`]'s commit handler). Handles all
/// three surface roles: xdg toplevel, xdg popup, and wlr layer surface (the latter is also arranged
/// in its output's layer map first, to respect any size the client requested).
pub fn ensure_initial_configure(
    surface: &WlSurface,
    space: &Space<WindowElement>,
    warm: Option<WindowElement>,
    popups: &mut PopupManager,
) {
    // Toplevel: send the first configure if relevant. `warm` is the hidden pre-warmed overlay
    // host's toplevel for `surface` (audit E7): it is never in the Space, yet needs its configure.
    if let Some(window) = space
        .elements()
        .find(|window| window.wl_surface().map(|s| &*s == surface).unwrap_or(false))
        .cloned()
        .or(warm)
    {
        if let Some(toplevel) = window.toplevel() {
            let initial_configure_sent = with_states(surface, |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .initial_configure_sent
            });
            if !initial_configure_sent {
                toplevel.send_configure();
            }
        }
        return;
    }

    // Popup: send the first configure if relevant.
    if let Some(popup) = popups.find_popup(surface) {
        match popup {
            PopupKind::Xdg(ref xdg) => {
                if !xdg.is_initial_configure_sent() {
                    // The initial configure is always allowed, so this cannot fail.
                    xdg.send_configure().expect("initial configure failed");
                }
            }
            // Input-method popups don't require a configure.
            PopupKind::InputMethod(_) => {}
        }
        return;
    }

    // Layer surface: arrange the output's layer map, then send the first configure if relevant.
    if let Some(output) = space.outputs().find(|o| {
        let map = layer_map_for_output(o);
        map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
            .is_some()
    }) {
        let initial_configure_sent = with_states(surface, |states| {
            states
                .data_map
                .get::<LayerSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .initial_configure_sent
        });

        let mut map = layer_map_for_output(output);
        // Arrange before configuring so the client's requested size is respected.
        map.arrange();
        if !initial_configure_sent {
            if let Some(layer) = map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL) {
                layer.layer_surface().send_configure();
            }
        }
    }
}
