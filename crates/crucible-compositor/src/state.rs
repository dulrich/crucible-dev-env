//! Compositor state and the calloop event-loop wiring.
//!
//! Home of [`CrucibleState`]: the backend-agnostic compositor state generic over a [`Backend`].
//! It owns the Smithay [`DisplayHandle`], the calloop [`LoopHandle`], the desktop [`Space`], the
//! [`PopupManager`], the [`Seat`], and the Wayland globals.
//!
//! ## Wayland globals
//! Alongside `wl_compositor` / `wl_shm` / `wl_output`, the core globals a real client needs to map
//! and render a window:
//! - **xdg-shell** ([`XdgShellState`]) — toplevels and popups (handlers in [`crate::shell`]).
//! - **wlr-layer-shell** ([`WlrLayerShellState`]) — layer surfaces for future status bars.
//! - **wl_seat** ([`SeatState`] / [`Seat`]) — created with keyboard + pointer **capabilities** so
//!   clients see a seat with the right caps and a cursor renders. Input events are routed into the
//!   seat by [`crate::input`]; this module only advertises the seat and its capabilities.
//! - **wl_data_device** ([`DataDeviceState`]) + **primary selection** ([`PrimarySelectionState`]) —
//!   clipboard/DnD plumbing with minimal handlers.
//!
//! The structure mirrors anvil's `AnvilState`/`Backend` split (a state struct generic over a
//! backend trait, with backend specifics isolated in `winit.rs`/`udev.rs`) so anvil-derived code
//! ports cleanly as the compositor grows.

use std::sync::{Arc, atomic::AtomicBool};

use crucible_config::Config;
use crucible_keys::Bindings;

use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::delegate_dispatch2;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::wayland::shell::xdg::ToplevelSurface;
use smithay::{
    desktop::{PopupManager, Space},
    input::{
        Seat, SeatHandler, SeatState,
        dnd::DndGrabHandler,
        keyboard::{LedState, XkbConfig},
        pointer::{CursorImageStatus, PointerHandle},
    },
    output::Output,
    reexports::{
        calloop::{Interest, LoopHandle, Mode, PostAction, generic::Generic},
        wayland_server::{
            Client, Display, DisplayHandle, Resource,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_buffer::WlBuffer, wl_surface::WlSurface},
        },
    },
    utils::{Clock, Monotonic},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, get_parent,
            is_sync_subsurface,
        },
        output::{OutputHandler, OutputManagerState},
        seat::WaylandFocus,
        selection::{
            SelectionHandler, SelectionSource, SelectionTarget,
            data_device::{
                DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
            },
            primary_selection::{
                PrimarySelectionHandler, PrimarySelectionState, set_primary_focus,
            },
        },
        shell::{wlr_layer::WlrLayerShellState, xdg::XdgShellState},
        shm::{ShmHandler, ShmState},
        socket::ListeningSocketSource,
        fractional_scale::{FractionalScaleHandler, FractionalScaleManagerState},
        viewporter::ViewporterState,
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{X11Wm, XWaylandClientData},
};
use tracing::{info, warn};

use crucible_layout::Projection;

use crate::{
    element::WindowElement,
    focus::{KeyboardFocusTarget, PointerFocusTarget},
    input::SuppressedKeys,
    layout::LayoutState,
    shell::ensure_initial_configure,
};

/// Per-client state stored in each Wayland client's [`ClientData`].
///
/// Smithay's compositor implementation needs a small amount of per-client state (for surface
/// bookkeeping); storing it here ensures it is cleaned up automatically when the client
/// disconnects.
#[derive(Debug, Default)]
pub struct ClientState {
    /// Compositor-specific per-client state required by [`CompositorHandler`].
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// Backend abstraction implemented by each rendering backend (`winit` now, `udev` later).
///
/// Mirrors anvil's `Backend` trait so backend-specific code stays isolated behind a stable
/// interface.
pub trait Backend {
    /// Whether this backend emits relative pointer motion (used to gate the relevant global).
    const HAS_RELATIVE_MOTION: bool = false;
    /// Whether this backend emits pointer gesture events.
    const HAS_GESTURES: bool = false;
    /// Human-readable seat name advertised to clients.
    fn seat_name(&self) -> String;
    /// Forces a full redraw of `output` on the next frame (e.g. after a buffer-pool reset).
    fn reset_buffers(&mut self, output: &Output);
    /// Pre-imports a client buffer ahead of rendering, when the backend supports it.
    fn early_import(&mut self, surface: &WlSurface);
    /// Propagates keyboard LED state changes to the backend's input devices.
    fn update_led_state(&mut self, led_state: LedState);
    /// Requests a switch to virtual terminal `vt`, returning whether the backend handled it.
    ///
    /// Only the udev/DRM backend owns a `libseat` session and can switch VTs; the default
    /// implementation returns `false` so the nested `winit` backend (which has no session) ignores
    /// `Ctrl+Alt+F<n>` chords.
    fn change_vt(&mut self, _vt: i32) -> bool {
        false
    }
    /// Re-lays the outputs after their geometry changed (an output scale was applied or changed).
    ///
    /// The default re-arranges each output's layer map in place, re-syncs its model rect and
    /// re-applies the layout (the single-output `winit` backend). The udev backend overrides it to
    /// reflow the whole output strip, since a narrower or wider output moves its right neighbours.
    fn outputs_changed(state: &mut CrucibleState<Self>)
    where
        Self: Sized + 'static,
    {
        let outputs: Vec<Output> = state.space.outputs().cloned().collect();
        for output in &outputs {
            smithay::desktop::layer_map_for_output(output).arrange();
            state.sync_output_rect(output);
        }
        state.apply_layout();
    }
}

/// Central, backend-agnostic compositor state driven by the calloop event loop.
///
/// Generic over the active [`Backend`]. Holds the Wayland [`DisplayHandle`], the calloop
/// [`LoopHandle`], the desktop [`Space`], the [`PopupManager`], the [`Seat`], and the Wayland
/// globals. The active backend's data is stored in [`backend_data`](CrucibleState::backend_data).
#[derive(Debug)]
pub struct CrucibleState<B: Backend + 'static> {
    /// Backend-specific data (renderer, damage tracker, …).
    pub backend_data: B,
    /// Name of the Wayland socket this compositor is listening on, if any.
    pub socket_name: Option<String>,
    /// Handle to the Wayland display, used to create globals and dispatch clients.
    pub display_handle: DisplayHandle,
    /// Set to `false` to request the main loop to exit.
    pub running: Arc<AtomicBool>,
    /// Handle to the calloop event loop driving this state.
    pub handle: LoopHandle<'static, CrucibleState<B>>,
    /// Monotonic clock used for frame timing and presentation.
    pub clock: Clock<Monotonic>,

    /// Desktop space holding mapped windows and outputs.
    pub space: Space<WindowElement>,
    /// Tracks xdg/layer popups and their grab/dismiss lifecycle.
    pub popups: PopupManager,

    /// The pure layout model + the compositor payload bindings (decision #1/#5).
    ///
    /// The single source of truth for placement, visibility, stacking, and focus; the `Space` above
    /// is the *mechanism* driven from this model by [`CrucibleState::apply_layout`].
    pub layout: LayoutState,
    /// The [`Projection`] last applied to the `Space` (global Space coords).
    ///
    /// Diffed against each freshly derived projection so only windows whose rect/visibility actually
    /// changed are reconfigured, and focus is re-pointed only when its identity moved (idempotent
    /// apply — no configure storms). See [`crucible_layout::Projection::diff`].
    pub projection: Projection,

    /// `wl_compositor` / `wl_subcompositor` global state.
    pub compositor_state: CompositorState,
    /// `wl_shm` global state.
    pub shm_state: ShmState,
    /// `wl_output` + `xdg-output` manager state.
    pub output_manager_state: OutputManagerState,
    /// `xdg_wm_base` (xdg-shell) global state.
    pub xdg_shell_state: XdgShellState,
    /// `zwlr_layer_shell_v1` (wlr-layer-shell) global state.
    pub layer_shell_state: WlrLayerShellState,
    /// `wl_data_device_manager` global state (clipboard + drag-and-drop).
    pub data_device_state: DataDeviceState,
    /// `zwp_primary_selection_device_manager_v1` global state.
    pub primary_selection_state: PrimarySelectionState,
    /// `xwayland_shell_v1` global state (lets XWayland associate `wl_surface`s with X11 windows).
    pub xwayland_shell_state: XWaylandShellState,
    /// `zxdg_decoration_manager_v1` global state: CDE answers server-side for every toplevel (and
    /// draws no frame), so toolkits such as winit never draw their own title bar.
    pub xdg_decoration_state: smithay::wayland::shell::xdg::decoration::XdgDecorationState,
    /// `wp_fractional_scale_manager_v1` global state: tells each surface its output's fractional
    /// scale (see [`Self::update_fractional_scales`]).
    pub fractional_scale_state: FractionalScaleManagerState,
    /// `wp_viewporter` global state: lets fractional-scale clients map a native-resolution buffer
    /// onto their logical size.
    pub viewporter_state: ViewporterState,
    /// `wl_seat` state managing seats and their capabilities.
    pub seat_state: SeatState<CrucibleState<B>>,

    /// The single seat advertised to clients (keyboard + pointer capabilities).
    pub seat: Seat<CrucibleState<B>>,
    /// Pointer handle for the seat. Held so the cursor position is available to the render path.
    pub pointer: PointerHandle<CrucibleState<B>>,
    /// Current cursor image requested by the focused client (or the default), drawn by the render
    /// path.
    pub cursor_status: CursorImageStatus,
    /// Human-readable seat name advertised to clients.
    pub seat_name: String,
    /// Physical keycodes whose *press* was intercepted as a Super-leader chord.
    ///
    /// Each entry's matching *release* is swallowed so the focused client never sees a dangling
    /// key-up for a key the compositor consumed — tracked by keycode, not the resolved keysym, so
    /// a modifier change between press and release cannot desync the match
    /// (`reviews/2026-09-07-tn-code-review.md` F10). See [`crate::input`].
    pub suppressed_keys: SuppressedKeys,

    /// The running XWayland X11 window manager, once XWayland has reached `Ready`.
    ///
    /// `None` until [`Self::start_xwayland`] has spawned the server and `X11Wm::start_wm` attached
    /// the WM; used to drive X11 focus/raise and the clipboard bridge. See [`crate::xwayland`].
    pub xwm: Option<X11Wm>,
    /// The X11 `DISPLAY` number XWayland is serving (e.g. `1` for `:1`), once known.
    pub xdisplay: Option<u32>,
    /// Once-only latch for the `autostart` commands (see [`crate::autostart`]).
    pub autostart_gate: crate::autostart::AutostartGate,
    /// Whether pointer motion moves focus onto the managed window (or output) under the cursor.
    ///
    /// On by default; set from the config (`focus-follows-mouse`) by [`Self::apply_config`].
    pub focus_follows_mouse: bool,
    /// The active configuration (defaults until a config file is applied); see
    /// [`Self::apply_config`].
    pub config: Config,
    /// The active chord table, rebuilt from `config.binds` by [`Self::apply_config`].
    pub bindings: Bindings,
    /// The last hover target seen by focus-follows-mouse; focus moves only when this changes
    /// (pointer *enter*), so keyboard-driven focus is not undone by a pointer twitch.
    pub hover_target: Option<HoverTarget>,
    /// A focus-follows-mouse enter that a mapped overlay held back (overlays are modal). The next
    /// overlay open applies it first, so the overlay follows the pointer to its output.
    pub hover_deferred: bool,
    /// Workspace persistence, IPC and picker state; see [`crate::persist`].
    pub persist: crate::persist::PersistState,
    /// Workspace header strip toggle and consumed-button record.
    pub header: crate::header::HeaderState,
    /// Font + glyph cache for the header labels; disabled until the config selects a font.
    pub header_text: crate::text::TextRenderer,
    /// Screen-lock state (`ext-session-lock-v1`); see [`crate::lock`].
    pub session_lock: crate::lock::SessionLock,
}

/// What the pointer hovers, for focus-follows-mouse enter detection.
///
/// Keyed at **pane** level: a managed window and the area of its pane it does not cover share
/// one identity, so moving between them is not an enter. The window under the pointer is kept
/// separately by the caller for routing.
#[derive(Debug, Clone, PartialEq)]
pub enum HoverTarget {
    /// A pane (via its managed window, or its uncovered area).
    Pane(crucible_layout::PaneId),
    /// Gap / empty space on this output, outside every pane.
    Output(smithay::output::Output),
}

/// The pane-level hover key for a pointer location, before the output fallback.
///
/// `managed_window` is the managed window under the pointer (if any); `pane_under` is the pane
/// whose rect contains the pointer. A managed window keys by the pane holding it; otherwise the
/// pane under the pointer. `None` means the caller falls back to the output.
#[must_use]
pub fn hover_pane_key(
    world: &crucible_layout::World,
    managed_window: Option<crucible_layout::WindowId>,
    pane_under: Option<crucible_layout::PaneId>,
) -> Option<crucible_layout::PaneId> {
    match managed_window {
        Some(id) => world.pane_of_window(id),
        None => pane_under,
    }
}

/// Records `current` as the hover target and reports whether it is a new one (an *enter*).
///
/// `None` (an unmanaged element, or nothing resolvable) leaves `stored` unchanged and is never an
/// enter.
pub fn hover_entered<T: PartialEq>(stored: &mut Option<T>, current: Option<T>) -> bool {
    let Some(current) = current else {
        return false;
    };
    if stored.as_ref() == Some(&current) {
        return false;
    }
    *stored = Some(current);
    true
}

#[cfg(test)]
mod hover_tests {
    use super::hover_entered;

    #[test]
    fn first_target_is_an_enter_and_repeats_are_not() {
        let mut stored = None;
        assert!(hover_entered(&mut stored, Some(1)));
        assert!(!hover_entered(&mut stored, Some(1)));
        assert!(hover_entered(&mut stored, Some(2)));
        assert_eq!(stored, Some(2));
    }

    #[test]
    fn none_leaves_the_tracker_unchanged() {
        let mut stored = Some(1);
        assert!(!hover_entered(&mut stored, None));
        assert_eq!(stored, Some(1));
        // Returning to the same target after an unmanaged detour is not a new enter.
        assert!(!hover_entered(&mut stored, Some(1)));
    }

    #[test]
    fn window_and_uncovered_area_of_one_pane_share_an_enter_identity() {
        use super::{HoverTarget, hover_pane_key};
        use crucible_layout::{Direction, World};

        let mut w = World::new("1");
        let win = w.open_window();
        let left = w.pane_of_window(win).unwrap();
        let right = w.split(Direction::Right);

        // Pointer over the left window: an enter into the left pane.
        let mut stored = None;
        let key = |m, p| hover_pane_key(&w, m, p).map(HoverTarget::Pane);
        assert!(hover_entered(&mut stored, key(Some(win), Some(left))));

        // Keyboard-driven focus moves elsewhere; the tracker is untouched by that.
        let mut w2 = w.clone();
        w2.focus(Direction::Right);
        assert_eq!(w2.focused_pane_id(), right);

        // Window -> uncovered area of the same pane and back: not enters.
        assert!(!hover_entered(&mut stored, key(None, Some(left))));
        assert!(!hover_entered(&mut stored, key(Some(win), Some(left))));
        // Into the other pane: an enter.
        assert!(hover_entered(&mut stored, key(None, Some(right))));
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Builds the compositor state on top of an existing [`Display`] and calloop [`LoopHandle`].
    ///
    /// Creates the Wayland socket (when `listen_on_socket` is set), registers the display as a
    /// calloop source, initializes every Wayland global, and creates the seat with keyboard +
    /// pointer capabilities. Input-event routing into the seat lives in [`crate::input`].
    pub fn init(
        display: Display<CrucibleState<B>>,
        handle: LoopHandle<'static, CrucibleState<B>>,
        backend_data: B,
        listen_on_socket: bool,
    ) -> CrucibleState<B> {
        let dh = display.handle();
        let clock = Clock::new();

        // Create and register the Wayland listening socket so clients can connect.
        let socket_name = if listen_on_socket {
            let source = ListeningSocketSource::new_auto().unwrap();
            let socket_name = source.socket_name().to_string_lossy().into_owned();
            handle
                .insert_source(source, |client_stream, _, data| {
                    if let Err(err) = data
                        .display_handle
                        .insert_client(client_stream, Arc::new(ClientState::default()))
                    {
                        warn!("Error adding wayland client: {}", err);
                    };
                })
                .expect("Failed to init wayland socket source");
            info!(name = socket_name, "Listening on wayland socket");
            Some(socket_name)
        } else {
            None
        };

        // Drive client request dispatch from the event loop.
        handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, data| {
                    // Safety: we never move out of or drop the display behind this pointer.
                    #[allow(unsafe_code)]
                    unsafe {
                        display.get_mut().dispatch_clients(data).unwrap();
                    }
                    Ok(PostAction::Continue)
                },
            )
            .expect("Failed to init wayland server source");

        // Initialize the Wayland globals.
        let compositor_state = CompositorState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&dh);
        let xwayland_shell_state = XWaylandShellState::new::<Self>(&dh);
        let xdg_decoration_state =
            smithay::wayland::shell::xdg::decoration::XdgDecorationState::new::<Self>(&dh);
        let fractional_scale_state = FractionalScaleManagerState::new::<Self>(&dh);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let session_lock = crate::lock::SessionLock::new(
            smithay::wayland::session_lock::SessionLockManagerState::new::<Self, _>(&dh, |_| true),
        );
        let mut seat_state = SeatState::new();

        // Create the seat with keyboard + pointer capabilities. NB: this only *advertises* the
        // capabilities; routing events into the keyboard/pointer is [`crate::input`]'s job.
        let seat_name = backend_data.seat_name();
        // The startup seed is named off the saved workspaces once, before the model exists, so it
        // never shadows a saved name (no later rename).
        let store_dir = crucible_persist::store::store_dir();
        let saved: std::collections::HashSet<String> =
            crucible_persist::store::list_saved(&store_dir)
                .into_iter()
                .map(|(n, _)| n)
                .collect();
        let seed = crate::persist::startup_seed_name(&saved);
        let mut seat = seat_state.new_wl_seat(&dh, seat_name.clone());
        let pointer = seat.add_pointer();
        let keyboard = seat
            .add_keyboard(XkbConfig::default(), 200, 25)
            .expect("Failed to initialize the keyboard");
        // Num Lock starts on (user, 2026-10-29). No client is focused yet, so there is nothing to
        // advertise; keyboards added later pick up the LED from `led_state()` (udev backend).
        let mut mods = keyboard.modifier_state();
        mods.num_lock = true;
        keyboard.set_modifier_state(mods);

        CrucibleState {
            backend_data,
            socket_name,
            display_handle: dh,
            running: Arc::new(AtomicBool::new(true)),
            handle,
            clock,
            space: Space::default(),
            popups: PopupManager::default(),
            layout: LayoutState::new(seed),
            projection: Projection::default(),
            compositor_state,
            shm_state,
            output_manager_state,
            xdg_shell_state,
            layer_shell_state,
            data_device_state,
            primary_selection_state,
            xwayland_shell_state,
            xdg_decoration_state,
            fractional_scale_state,
            viewporter_state,
            seat_state,
            seat,
            pointer,
            cursor_status: CursorImageStatus::default_named(),
            seat_name,
            suppressed_keys: SuppressedKeys::new(),
            xwm: None,
            xdisplay: None,
            autostart_gate: crate::autostart::AutostartGate::default(),
            focus_follows_mouse: true,
            config: Config::default(),
            bindings: Bindings::default(),
            hover_target: None,
            hover_deferred: false,
            persist: crate::persist::PersistState::new(store_dir),
            header: crate::header::HeaderState::default(),
            header_text: crate::text::TextRenderer::from_font(None),
            session_lock,
        }
    }
}

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
        }

        // Drive popup commit bookkeeping.
        self.popups.commit(surface);

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
        self.cursor_status = image;
    }

    fn led_state_changed(&mut self, _seat: &Seat<Self>, led_state: LedState) {
        self.backend_data.update_led_state(led_state);
    }
}

// Wayland → X11 half of the clipboard bridge (the X11 → Wayland half is `XwmHandler::new_selection`
// / `send_selection` in `crate::xwayland`). Without it a Wayland client's clipboard (e.g. a
// Firefox screenshot) is invisible to XWayland clients: `xclip -t TARGETS` finds no owner.
impl<B: Backend + 'static> SelectionHandler for CrucibleState<B> {
    type SelectionUserData = ();

    /// A Wayland client set (or cleared) a selection: advertise its mime types to XWayland.
    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        let mime_types = source.map(|source| source.mime_types());
        tracing::debug!(
            ?ty,
            ?mime_types,
            xwm = self.xwm.is_some(),
            "Wayland selection changed"
        );
        if let Some(xwm) = self.xwm.as_mut()
            && let Err(err) = xwm.new_selection(ty, mime_types)
        {
            warn!(?err, ?ty, "Failed to set XWayland selection");
        }
    }

    /// A Wayland client is reading a selection an X11 client owns: pump it from XWayland.
    fn send_selection(
        &mut self,
        ty: SelectionTarget,
        mime_type: String,
        fd: std::os::fd::OwnedFd,
        _seat: Seat<Self>,
        _user_data: &(),
    ) {
        if let Some(xwm) = self.xwm.as_mut()
            && let Err(err) = xwm.send_selection(ty, mime_type, fd)
        {
            warn!(?err, ?ty, "Failed to send XWayland selection to Wayland");
        }
    }
}

impl<B: Backend + 'static> DataDeviceHandler for CrucibleState<B> {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

// DnD grab handlers: minimal. `WaylandDndGrabHandler`'s default `dnd_requested` cancels
// the drag; `DndGrabHandler`'s defaults clear the icon on drop/cancel. CDE does not yet drive
// interactive DnD grabs.
impl<B: Backend + 'static> WaylandDndGrabHandler for CrucibleState<B> {}
impl<B: Backend + 'static> DndGrabHandler for CrucibleState<B> {}

impl<B: Backend + 'static> PrimarySelectionHandler for CrucibleState<B> {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
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

/// Sets the preferred fractional scale of every surface in the tree rooted at `root` to `output`'s.
///
/// `set_preferred_scale` only sends an event when the value changes, so this is cheap per frame.
fn set_tree_preferred_scale(root: &WlSurface, output: &Output) {
    let scale = output.current_scale().fractional_scale();
    with_surfaces_surface_tree(root, |_, states| {
        with_fractional_scale(states, |fs| fs.set_preferred_scale(scale));
    });
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// The Smithay output backing the model's focused output, if bound.
    pub(crate) fn focused_smithay_output(&self) -> Option<Output> {
        let focused = self.layout.world.focused_output();
        self.space
            .outputs()
            .find(|o| crate::layout::model_output_id(o) == Some(focused))
            .cloned()
    }

    /// The output a root surface lives on by role: a mapped window's first output, a layer
    /// surface's output, or a lock surface's output.
    fn output_for_root(&self, root: &WlSurface) -> Option<Output> {
        if let Some(window) = self.window_for_surface(root) {
            if let Some(output) = self.space.outputs_for_element(&window).first() {
                return Some(output.clone());
            }
        }
        self.space
            .outputs()
            .find(|output| {
                smithay::desktop::layer_map_for_output(output)
                    .layer_for_surface(root, smithay::desktop::WindowSurfaceType::ALL)
                    .is_some()
                    || self
                        .session_lock
                        .surface_for(output)
                        .is_some_and(|lock| lock.wl_surface() == root)
            })
            .cloned()
    }

    /// Per-frame fractional-scale update for everything on `output` (E1): each window and layer
    /// surface tracks its primary scan-out output, the lock surface takes the output's scale, and a
    /// client cursor surface takes the scale of the output under the pointer.
    ///
    /// Shared by both backends; call after a frame for `output` was rendered. A scale change
    /// reaches clients on the next frame through this walk.
    pub fn update_fractional_scales(&self, output: &Output) {
        let set_primary =
            |surface: &WlSurface, states: &smithay::wayland::compositor::SurfaceData| {
                if let Some(primary) = surface_primary_scanout_output(surface, states) {
                    let scale = primary.current_scale().fractional_scale();
                    with_fractional_scale(states, |fs| fs.set_preferred_scale(scale));
                }
            };
        self.space.elements().for_each(|window| {
            if self.space.outputs_for_element(window).contains(output) {
                window.0.with_surfaces(set_primary);
            }
        });
        for layer_surface in smithay::desktop::layer_map_for_output(output).layers() {
            layer_surface.with_surfaces(set_primary);
        }
        if let Some(lock) = self.session_lock.surface_for(output) {
            set_tree_preferred_scale(lock.wl_surface(), output);
        }
        if let CursorImageStatus::Surface(ref surface) = self.cursor_status {
            let location = self.pointer.current_location();
            if let Some(under) = self.space.output_under(location).next() {
                set_tree_preferred_scale(surface, under);
            }
        }
    }
}

impl<B: Backend + 'static> FractionalScaleHandler for CrucibleState<B> {
    /// Initial preferred scale (E1): the surface's (or its root's) primary scan-out output, else
    /// the output its root's role places it on, else the model's focused output.
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }
        let scanout =
            |s: &WlSurface| with_states(s, |states| surface_primary_scanout_output(s, states));
        let output = scanout(&surface)
            .or_else(|| (root != surface).then(|| scanout(&root)).flatten())
            .or_else(|| self.output_for_root(&root))
            .or_else(|| self.focused_smithay_output());
        if let Some(output) = output {
            let scale = output.current_scale().fractional_scale();
            with_states(&surface, |states| {
                with_fractional_scale(states, |fs| fs.set_preferred_scale(scale));
            });
        }
    }
}

use smithay::{
    backend::renderer::element::{
        RenderElementStates, default_primary_scanout_output_compare, utils::select_dmabuf_feedback,
    },
    desktop::utils::{
        OutputPresentationFeedback, surface_presentation_feedback_flags_from_states,
        surface_primary_scanout_output, update_surface_primary_scanout_output,
        with_surfaces_surface_tree,
    },
    wayland::{
        compositor::with_states, dmabuf::DmabufFeedback, fractional_scale::with_fractional_scale,
    },
};

/// Records, for every surface on `output`, which output actually scanned it out this frame.
///
/// Smithay needs this bookkeeping to deliver correct `wl_surface.frame` throttling and presentation
/// feedback when a surface spans multiple outputs. Used by the udev/DRM render path after a frame is
/// rendered (the nested `winit` path has a single output and does not need it). Mirrors anvil's
/// helper, trimmed to CDE's window set (no DnD icon / cursor-surface scanout tracking yet).
pub fn update_primary_scanout_output(
    space: &Space<WindowElement>,
    output: &Output,
    render_element_states: &RenderElementStates,
) {
    space.elements().for_each(|window| {
        window.0.with_surfaces(|surface, states| {
            update_surface_primary_scanout_output(
                surface,
                output,
                states,
                None,
                render_element_states,
                default_primary_scanout_output_compare,
            );
        });
    });
    let map = smithay::desktop::layer_map_for_output(output);
    for layer_surface in map.layers() {
        layer_surface.with_surfaces(|surface, states| {
            update_surface_primary_scanout_output(
                surface,
                output,
                states,
                None,
                render_element_states,
                default_primary_scanout_output_compare,
            );
        });
    }
}

/// Sends each surface on `output` (windows overlapping it, and its layer surfaces) the dmabuf
/// feedback matching how it was just presented, so clients can allocate scanout-capable buffers.
///
/// Walks the same set as [`update_primary_scanout_output`]; mirrors anvil's `post_repaint`.
pub fn send_dmabuf_feedback(
    space: &Space<WindowElement>,
    output: &Output,
    render_element_states: &RenderElementStates,
    render_feedback: &DmabufFeedback,
    scanout_feedback: &DmabufFeedback,
) {
    space.elements().for_each(|window| {
        if space.outputs_for_element(window).contains(output) {
            window
                .0
                .send_dmabuf_feedback(output, surface_primary_scanout_output, |surface, _| {
                    select_dmabuf_feedback(
                        surface,
                        render_element_states,
                        render_feedback,
                        scanout_feedback,
                    )
                });
        }
    });
    let map = smithay::desktop::layer_map_for_output(output);
    for layer_surface in map.layers() {
        layer_surface.send_dmabuf_feedback(output, surface_primary_scanout_output, |surface, _| {
            select_dmabuf_feedback(
                surface,
                render_element_states,
                render_feedback,
                scanout_feedback,
            )
        });
    }
}

/// Collects the [`OutputPresentationFeedback`] for everything visible on `output`.
///
/// Walks the mapped windows (and the output's layer surfaces) and gathers their presentation
/// feedback so the DRM backend can call `feedback.presented(..)` after the page-flip completes,
/// telling clients exactly when their frame hit the screen. Mirrors anvil's helper.
pub fn take_presentation_feedback(
    output: &Output,
    space: &Space<WindowElement>,
    render_element_states: &RenderElementStates,
) -> OutputPresentationFeedback {
    let mut output_presentation_feedback = OutputPresentationFeedback::new(output);

    space.elements().for_each(|window| {
        if space.outputs_for_element(window).contains(output) {
            window.0.take_presentation_feedback(
                &mut output_presentation_feedback,
                surface_primary_scanout_output,
                |surface, _| {
                    surface_presentation_feedback_flags_from_states(
                        surface,
                        None,
                        render_element_states,
                    )
                },
            );
        }
    });
    let map = smithay::desktop::layer_map_for_output(output);
    for layer_surface in map.layers() {
        layer_surface.take_presentation_feedback(
            &mut output_presentation_feedback,
            surface_primary_scanout_output,
            |surface, _| {
                surface_presentation_feedback_flags_from_states(
                    surface,
                    None,
                    render_element_states,
                )
            },
        );
    }

    output_presentation_feedback
}

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

/// The model rect of an output at `position` whose mode is `mode` physical pixels at `scale`.
///
/// The model is logical: the size is the mode divided by the scale, rounded **up** exactly as
/// Smithay's `Space::output_geometry` rounds, so the registered rect matches the geometry that
/// `CrucibleState::sync_output_rect` later reads back.
#[must_use]
pub fn logical_output_rect(
    position: (i32, i32),
    mode: smithay::utils::Size<i32, smithay::utils::Physical>,
    scale: f64,
) -> crucible_layout::Rect {
    let size = mode.to_f64().to_logical(scale).to_i32_ceil();
    crucible_layout::Rect::new(position.0, position.1, size.w, size.h)
}

#[cfg(test)]
mod logical_rect_tests {
    use super::logical_output_rect;
    use crucible_layout::Rect;

    #[test]
    fn logical_rect_is_the_mode_at_scale_one() {
        assert_eq!(
            logical_output_rect((10, 0), (3840, 2160).into(), 1.0),
            Rect::new(10, 0, 3840, 2160)
        );
    }

    #[test]
    fn logical_rect_divides_by_a_fractional_scale_and_rounds_up() {
        // 3840 / (170/120) = 2710.59 and 2160 / (170/120) = 1524.71, both ceil like the Space.
        assert_eq!(
            logical_output_rect((0, 0), (3840, 2160).into(), 170.0 / 120.0),
            Rect::new(0, 0, 2711, 1525)
        );
        assert_eq!(
            logical_output_rect((2711, 0), (3840, 2160).into(), 2.0),
            Rect::new(2711, 0, 1920, 1080)
        );
    }
}
