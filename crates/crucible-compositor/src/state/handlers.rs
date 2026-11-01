//! Wayland protocol handler impls for [`CrucibleState`]: compositor, buffer, shm, output, seat,
//! XWayland shell and xdg-decoration, plus the single `delegate_dispatch2!` wiring every global.
//!
//! Clipboard, primary selection and drag-and-drop handlers live in [`crate::selection`].

use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::delegate_dispatch2;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::utils::IsAlive;
use smithay::wayland::shell::xdg::ToplevelSurface;
use smithay::{
    input::{
        Seat, SeatHandler, SeatState, keyboard::LedState, pointer::CursorImageStatus,
    },
    reexports::wayland_server::{
        Client, Resource,
        protocol::{wl_buffer::WlBuffer, wl_surface::WlSurface},
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes,
            get_parent, is_sync_subsurface, with_states,
        },
        output::OutputHandler,
        seat::WaylandFocus,
        selection::{data_device::set_data_device_focus, primary_selection::set_primary_focus},
        shm::{ShmHandler, ShmState},
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::XWaylandClientData,
};

use super::{Backend, ClientState, CrucibleState};
use crate::{
    focus::{KeyboardFocusTarget, PointerFocusTarget},
    shell::ensure_initial_configure,
};

impl<B: Backend + 'static> CompositorHandler for CrucibleState<B> {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        // XWayland clients carry Smithay's own `XWaylandClientData` rather than CDE's `ClientState`,
        // so check for it first; both expose a `CompositorClientState`.
        if let Some(state) = client.get_data::<XWaylandClientData>() {
            return &state.compositor_state;
        }
        if let Some(state) = client.get_data::<ClientState>() {
            return &state.compositor_state;
        }
        panic!("Unknown client data type")
    }

    fn commit(&mut self, surface: &WlSurface) {
        // Register the committed buffer with Smithay's bookkeeping, then let the backend import it.
        on_commit_buffer_handler::<Self>(surface);
        self.backend_data.early_import(surface);

        // Notify the mapped window (if any) that its root surface committed, so its cached state
        // (size/geometry) refreshes. Walk up to the root surface first for subsurfaces.
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            // The hidden warm overlay host is not in the Space (audit E7); it still needs on_commit.
            if let Some(window) = self
                .window_for_surface(&root)
                .or_else(|| self.warm_element_for(&root))
            {
                window.on_commit();
            }

            // X2: grant a deferred keyboard focus once its root surface has its first buffer.
            let ready = crate::element::surface_has_buffer(&root);
            if let Some(element) = self
                .pending_keyboard_focus
                .take_if_ready(|w| w.wl_surface().is_some_and(|s| *s == root), ready)
                && element.alive()
            {
                self.set_keyboard_focus(Some(crate::focus::KeyboardFocusTarget::Window(element)));
            }
        }

        // Drive popup commit bookkeeping.
        self.popups.commit(surface);

        // A drag icon's `attach` offset moves its hotspot: accumulate it (as anvil does).
        if let Some(icon) = self.dnd_icon.as_mut()
            && &icon.surface == surface
        {
            with_states(&icon.surface, |states| {
                let delta = states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .current()
                    .buffer_delta
                    .take()
                    .unwrap_or_default();
                icon.offset += delta;
            });
        }

        // Send the first configure for whichever role this surface holds, if not yet sent.
        let warm = self.warm_element_for(surface);
        ensure_initial_configure(surface, &self.space, warm, &mut self.popups);
    }
}

impl<B: Backend + 'static> BufferHandler for CrucibleState<B> {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl<B: Backend + 'static> ShmHandler for CrucibleState<B> {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl<B: Backend + 'static> OutputHandler for CrucibleState<B> {}

impl<B: Backend + 'static> SeatHandler for CrucibleState<B> {
    // The seat's focus types are the surface-kind-spanning enums in `crate::focus`, so the same
    // seat focuses Wayland *and* XWayland surfaces (layer/popup/X11 included).
    type KeyboardFocus = KeyboardFocusTarget;
    type PointerFocus = PointerFocusTarget;
    type TouchFocus = PointerFocusTarget;

    fn seat_state(&mut self) -> &mut SeatState<CrucibleState<B>> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, target: Option<&KeyboardFocusTarget>) {
        // Keep the data-device and primary-selection focus in sync with keyboard focus so the
        // focused client owns the clipboard.
        let dh = &self.display_handle;
        let client = target
            .and_then(WaylandFocus::wl_surface)
            .and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client.clone());
        set_primary_focus(dh, seat, client);
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor.status = image;
    }

    fn led_state_changed(&mut self, _seat: &Seat<Self>, led_state: LedState) {
        self.backend_data.update_led_state(led_state);
    }
}

// XWayland associates the `wl_surface` a client commits with the X11 window it belongs to via this
// shell protocol. `X11Wm::start_wm` requires the handler; the default `surface_associated` is
// sufficient for CDE (the WM tracks the association internally).
impl<B: Backend + 'static> XWaylandShellHandler for CrucibleState<B> {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }
}

/// The decoration mode CDE answers for every toplevel, whatever the client asked for: server-side.
/// CDE tiles every window and draws its own focus ring, and draws no title bar, so a client must
/// never draw one either.
fn decoration_mode(_requested: Option<DecorationMode>) -> DecorationMode {
    DecorationMode::ServerSide
}

/// Sets [`decoration_mode`] as `toplevel`'s pending mode, and re-configures it when its initial
/// configure already went out (before that, the initial configure carries it).
fn apply_decoration_mode(toplevel: &ToplevelSurface, requested: Option<DecorationMode>) {
    toplevel.with_pending_state(|state| state.decoration_mode = Some(decoration_mode(requested)));
    if toplevel.is_initial_configure_sent() {
        toplevel.send_pending_configure();
    }
}

impl<B: Backend + 'static> smithay::wayland::shell::xdg::decoration::XdgDecorationHandler
    for CrucibleState<B>
{
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        apply_decoration_mode(&toplevel, None);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: DecorationMode) {
        apply_decoration_mode(&toplevel, Some(mode));
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        apply_decoration_mode(&toplevel, None);
    }
}

// This Smithay rev replaces the per-protocol `delegate_*!` macros with a single
// `delegate_dispatch2!` that provides `Dispatch`/`GlobalDispatch` for *every* protocol whose
// per-resource user data implements `Dispatch2`/`GlobalDispatch2` (i.e. every global initialized
// above). One invocation therefore wires compositor + shm + output + xdg-shell + layer-shell +
// seat + data-device + primary-selection (and any future globals) at once.
delegate_dispatch2!(@<B: Backend + 'static> CrucibleState<B>);

#[cfg(test)]
mod decoration_tests {
    use super::{DecorationMode, decoration_mode};

    #[test]
    fn every_request_is_answered_server_side() {
        for requested in [
            None,
            Some(DecorationMode::ClientSide),
            Some(DecorationMode::ServerSide),
        ] {
            assert_eq!(decoration_mode(requested), DecorationMode::ServerSide);
        }
    }
}
