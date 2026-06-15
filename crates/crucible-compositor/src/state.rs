//! Compositor state and the calloop event-loop wiring.
//!
//! Home of [`CrucibleState`]: the backend-agnostic compositor state generic over a [`Backend`].
//! It owns the Smithay [`DisplayHandle`], the calloop [`LoopHandle`], the desktop [`Space`], and
//! the minimal set of Wayland globals brought up in WS-2 — [`CompositorState`], [`ShmState`], and
//! [`OutputManagerState`] — enough for a client to connect and enumerate globals. Later
//! workstreams (WS-3+) extend this with xdg-shell, seat/input, layer-shell, and XWayland.
//!
//! The structure mirrors anvil's `AnvilState`/`Backend` split (a state struct generic over a
//! backend trait, with backend specifics isolated in `winit.rs`/`udev.rs`) so anvil-derived code
//! ports cleanly as the compositor grows.

use std::sync::{Arc, atomic::AtomicBool};

use smithay::delegate_dispatch2;
use smithay::{
    desktop::Space,
    output::Output,
    reexports::{
        calloop::{Interest, LoopHandle, Mode, PostAction, generic::Generic},
        wayland_server::{
            Client, Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_buffer::WlBuffer, wl_surface::WlSurface},
        },
    },
    utils::{Clock, Monotonic},
    wayland::{
        buffer::BufferHandler,
        compositor::{CompositorClientState, CompositorHandler, CompositorState, with_states},
        output::{OutputHandler, OutputManagerState},
        shm::{ShmHandler, ShmState},
        socket::ListeningSocketSource,
    },
};
use tracing::{info, warn};

use crate::element::WindowElement;

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
/// interface. WS-2 only needs [`seat_name`](Backend::seat_name); the buffer/import/LED hooks are
/// part of the contract that input and rendering workstreams (WS-4+) rely on, and are defined now
/// so handler impls can call them uniformly.
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
    fn update_led_state(&mut self, led_state: smithay::input::keyboard::LedState);
}

/// Central, backend-agnostic compositor state driven by the calloop event loop.
///
/// Generic over the active [`Backend`]. Holds the Wayland [`DisplayHandle`], the calloop
/// [`LoopHandle`], the desktop [`Space`], and the Wayland globals required for WS-2. The active
/// backend's data is stored in [`backend_data`](CrucibleState::backend_data).
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

    /// `wl_compositor` / `wl_subcompositor` global state.
    pub compositor_state: CompositorState,
    /// `wl_shm` global state.
    pub shm_state: ShmState,
    /// `wl_output` + `xdg-output` manager state.
    pub output_manager_state: OutputManagerState,
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Builds the compositor state on top of an existing [`Display`] and calloop [`LoopHandle`].
    ///
    /// Creates the Wayland socket (when `listen_on_socket` is set) and inserts it into the loop so
    /// new clients are accepted and `WAYLAND_DISPLAY` can be exported, registers the display itself
    /// as a calloop source so client requests are dispatched, and initializes the WS-2 globals
    /// (`wl_compositor`, `wl_shm`, `wl_output`).
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

        // Initialize the minimal globals needed for WS-2.
        let compositor_state = CompositorState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);

        CrucibleState {
            backend_data,
            socket_name,
            display_handle: dh,
            running: Arc::new(AtomicBool::new(true)),
            handle,
            clock,
            space: Space::default(),
            compositor_state,
            shm_state,
            output_manager_state,
        }
    }
}

impl<B: Backend + 'static> CompositorHandler for CrucibleState<B> {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        // WS-2: drive the buffer-import bookkeeping so a committed buffer is registered. Full
        // surface-role/mapping handling arrives with xdg-shell in WS-3.
        on_commit_buffer_handler::<Self>(surface);
        self.backend_data.early_import(surface);
        with_states(surface, |_states| {});
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

use smithay::backend::renderer::utils::on_commit_buffer_handler;

// This Smithay rev replaces the per-protocol `delegate_*!` macros with a single
// `delegate_dispatch2!` that provides `Dispatch`/`GlobalDispatch` for *every* protocol whose
// per-resource user data implements `Dispatch2`/`GlobalDispatch2` (i.e. every global initialized
// above). One invocation therefore wires compositor + shm + output (and any future globals) at once.
delegate_dispatch2!(@<B: Backend + 'static> CrucibleState<B>);
