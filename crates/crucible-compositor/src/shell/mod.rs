//! Wayland shell protocol handlers (xdg-shell and wlr-layer-shell).
//!
//! Implements the surface-role lifecycle that turns a committed Wayland surface into a window CDE
//! can render:
//!
//! - **xdg-shell** ([`XdgShellHandler`]): new toplevels are wrapped in a [`WindowElement`] and
//!   mapped into the desktop [`Space`] with placeholder placement (centered on the output — real CDE
//!   tiling is Phase 2). Popups are tracked in the [`PopupManager`]. The first `configure` is sent
//!   lazily from the commit handler ([`ensure_initial_configure`]), as the protocol requires.
//! - **wlr-layer-shell** ([`WlrLayerShellHandler`]): layer surfaces are accepted and arranged via
//!   [`layer_map_for_output`]. No status bar is rendered yet (that is a later phase); this exists so
//!   the global is advertised and the exclusive-zone bookkeeping is in place.
//!
//! Protocol I/O is kept at this edge so the layout core stays compositor-free. Input handling,
//! interactive move/resize grabs, and the bridge into the `crucible_layout` model are **not** part
//! of WS-3 (they arrive in WS-4 / Phase 2); the handlers here only do the minimum to map and
//! configure surfaces.

use smithay::{
    desktop::{
        PopupKind, PopupManager, Space, WindowSurfaceType, find_popup_root_surface,
        get_popup_toplevel_coords, layer_map_for_output,
    },
    output::Output,
    reexports::wayland_server::protocol::{wl_output::WlOutput, wl_seat, wl_surface::WlSurface},
    utils::{IsAlive, Logical, Rectangle, Serial},
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
    focus::KeyboardFocusTarget,
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
        let window = WindowElement::new_wayland_window(surface);
        place_new_window(&mut self.space, &window, true);

        // Give the newly mapped toplevel keyboard focus so the client actually receives keys.
        self.set_keyboard_focus(Some(KeyboardFocusTarget::Window(window)));
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
        // Popup grabs (keyboard/pointer) are wired with the input path in WS-4.
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.window_for_surface(surface.wl_surface()) {
            self.space.unmap_elem(&window);
        }

        // If the destroyed window held keyboard focus, move focus to the next mapped window (or
        // clear it) so keys keep flowing. Placeholder policy — Phase 2's tiling owns real
        // focus-follows-layout behavior.
        let still_focused = matches!(
            self.keyboard_focus(),
            Some(KeyboardFocusTarget::Window(ref w)) if w.alive()
        );
        if !still_focused {
            let next = self
                .space
                .elements()
                .next_back()
                .cloned()
                .map(KeyboardFocusTarget::Window);
            self.set_keyboard_focus(next);
        }
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

/// Logical-coordinate area a new window is placed within when no output is mapped yet.
const FALLBACK_PLACEMENT_AREA: (i32, i32) = (1280, 800);

/// Computes the top-left location that centers a window of `win_size` within `area`.
///
/// Pure placement math (no compositor state) so it can be unit-tested without a GPU/display. If the
/// window is larger than the area in a dimension, that coordinate is clamped to the area's origin
/// rather than going negative.
fn centered_location(
    area: Rectangle<i32, Logical>,
    win_size: smithay::utils::Size<i32, Logical>,
) -> smithay::utils::Point<i32, Logical> {
    let x = area.loc.x + (area.size.w - win_size.w).max(0) / 2;
    let y = area.loc.y + (area.size.h - win_size.h).max(0) / 2;
    (x, y).into()
}

/// Maps a freshly created toplevel into `space` with **placeholder placement**.
///
/// Phase-1 placement is intentionally a placeholder: the window is centered within the first
/// output's logical area (falling back to a fixed area if no output is mapped yet). Real CDE
/// tiling — directional splits, stacks, workspaces — is Phase 2; this just gets the surface visible.
pub fn place_new_window(space: &mut Space<WindowElement>, window: &WindowElement, activate: bool) {
    let area = space
        .outputs()
        .next()
        .and_then(|o| space.output_geometry(o))
        .unwrap_or_else(|| Rectangle::from_size(FALLBACK_PLACEMENT_AREA.into()));

    // The window's size may still be zero (the client has not committed a buffer yet); placement is
    // recomputed only here and the client picks its own size, so it re-centers naturally on commit.
    let location = centered_location(area, window.geometry().size);
    space.map_element(window.clone(), location, activate);
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
    popups: &mut PopupManager,
) {
    // Toplevel: send the first configure if relevant.
    if let Some(window) = space
        .elements()
        .find(|window| window.wl_surface().map(|s| &*s == surface).unwrap_or(false))
        .cloned()
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

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::utils::{Point, Size};

    #[test]
    fn centers_window_smaller_than_area() {
        let area = Rectangle::from_size(Size::from((1000, 800)));
        let loc = centered_location(area, Size::from((200, 100)));
        // (1000 - 200) / 2 = 400 ; (800 - 100) / 2 = 350
        assert_eq!(loc, Point::from((400, 350)));
    }

    #[test]
    fn honors_area_origin_offset() {
        let area = Rectangle::new(Point::from((100, 50)), Size::from((1000, 800)));
        let loc = centered_location(area, Size::from((200, 100)));
        assert_eq!(loc, Point::from((100 + 400, 50 + 350)));
    }

    #[test]
    fn clamps_window_larger_than_area_to_origin() {
        let area = Rectangle::new(Point::from((10, 20)), Size::from((100, 100)));
        // A window wider/taller than the area must not be placed at negative coordinates.
        let loc = centered_location(area, Size::from((400, 400)));
        assert_eq!(loc, Point::from((10, 20)));
    }

    #[test]
    fn zero_size_window_centers_on_area_center() {
        let area = Rectangle::from_size(Size::from((1280, 800)));
        let loc = centered_location(area, Size::from((0, 0)));
        assert_eq!(loc, Point::from((640, 400)));
    }
}
