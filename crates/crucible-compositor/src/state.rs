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
//!   clipboard plumbing with minimal handlers, plus Wayland-client pointer drag-and-drop grabs
//!   (touch and XWayland-initiated DnD are not driven).
//!
//! The structure mirrors anvil's `AnvilState`/`Backend` split (a state struct generic over a
//! backend trait, with backend specifics isolated in `winit.rs`/`udev.rs`) so anvil-derived code
//! ports cleanly as the compositor grows.

mod handlers;
mod pending_focus;
mod surface_output;

pub use pending_focus::{PendingFocus, defer_focus};
pub use surface_output::{
    send_dmabuf_feedback, send_dnd_icon_frames, take_presentation_feedback,
    update_primary_scanout_output,
};

use std::sync::{Arc, atomic::AtomicBool};

use crucible_config::Config;
use crucible_keys::Bindings;

use smithay::{
    desktop::{PopupManager, Space},
    input::{
        Seat, SeatState,
        keyboard::{LedState, XkbConfig},
        pointer::{CursorImageStatus, PointerHandle},
    },
    output::Output,
    reexports::{
        calloop::{Interest, LoopHandle, Mode, PostAction, generic::Generic},
        wayland_server::{
            Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{Clock, Logical, Monotonic, Point},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        fractional_scale::FractionalScaleManagerState,
        output::OutputManagerState,
        selection::{data_device::DataDeviceState, primary_selection::PrimarySelectionState},
        shell::{wlr_layer::WlrLayerShellState, xdg::XdgShellState},
        shm::ShmState,
        socket::ListeningSocketSource,
        viewporter::ViewporterState,
        xwayland_shell::XWaylandShellState,
    },
    xwayland::X11Wm,
};
use tracing::{info, warn};

use crucible_layout::Projection;

use crate::{
    element::WindowElement,
    input::{HoverTarget, SuppressedKeys},
    layout::LayoutState,
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
            state.relayout_output(output);
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
    /// The icon of the active pointer drag-and-drop grab, if any; composed at the pointer by
    /// [`crate::render::frame_elements`] and cleared when the drag ends.
    pub dnd_icon: Option<DndIcon>,
    /// A Wayland toplevel whose keyboard focus is deferred until its first buffer commits (X2);
    /// see [`CrucibleState::set_keyboard_focus`].
    pub pending_keyboard_focus: PendingFocus<WindowElement>,
}

/// The icon surface a client attached to a pointer drag, with its accumulated hotspot offset.
#[derive(Debug, Clone)]
pub struct DndIcon {
    /// The icon's `wl_surface`.
    pub surface: WlSurface,
    /// Offset of the icon from the pointer, accumulated from the surface's `attach` offsets.
    pub offset: Point<i32, Logical>,
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
            dnd_icon: None,
            pending_keyboard_focus: PendingFocus::default(),
        }
    }
}
