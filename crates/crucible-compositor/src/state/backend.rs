//! The per-client state and the [`Backend`] trait each rendering backend implements.

use super::*;

/// Per-client state stored in each Wayland client's [`ClientData`].
///
/// Smithay's compositor implementation needs a small amount of per-client state (for surface
/// bookkeeping); storing it here ensures it is cleaned up automatically when the client
/// disconnects.
#[derive(Debug, Default)]
pub struct ClientState {
    /// Compositor-specific per-client state required by [`CompositorHandler`].
    pub compositor_state: CompositorClientState,
    /// Set when the client disconnects; shared with every window identity the client binds, so
    /// the window's destruction can tell a lost client from a close (crucible-term plan 03). The
    /// rs backend runs `disconnected` before the client's object destructors.
    pub link: crate::persist::ClientLink,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {
        self.link.mark_disconnected();
    }
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
    /// Whether `output` is currently powered off by idle blanking (its DPMS-off succeeded). An off
    /// output shows nothing, so the session-lock handshake counts it as having presented a locked
    /// frame. The default (nested `winit`) never powers outputs off.
    fn output_powered_off(&self, _output: &Output) -> bool {
        false
    }
    /// Renders `output` offscreen with no cursor and reads it back (plan 34). Called by
    /// [`CrucibleState::capture_output`] after the lock and power admission. The default (test
    /// backends) cannot capture.
    ///
    /// # Errors
    /// [`crate::capture::CaptureError`] when the render or readback fails.
    fn capture_output(
        _state: &mut CrucibleState<Self>,
        _output: &Output,
    ) -> Result<crate::capture::Capture, crate::capture::CaptureError>
    where
        Self: Sized + 'static,
    {
        Err(crate::capture::CaptureError::Unsupported)
    }
    /// Enqueues a real render of `output` after a reveal hold released it (plan 38). The default
    /// (nested `winit`, test backends) does nothing: the winit loop renders every iteration. The
    /// udev backend schedules `render_surface` the way its idle-wake path does, skipping a
    /// powered-off output, so a held output with no pending flip is restarted.
    fn schedule_render(_state: &mut CrucibleState<Self>, _output: &Output)
    where
        Self: Sized + 'static,
    {
    }
    /// A `background` layer surface on `output` committed a buffer (plan 44). The default (nested
    /// `winit`, test backends) does nothing; the udev backend adopts `output` if its start-up DRM
    /// adoption is still waiting for its wallpaper.
    fn background_buffer_committed(_state: &mut CrucibleState<Self>, _output: &Output)
    where
        Self: Sized + 'static,
    {
    }
    /// Powers every output `on` (wake) or off (idle blanking). Called by the idle machine only on a
    /// change, never redundantly. The default (nested `winit`) logs and does nothing; the udev
    /// backend powers the DRM outputs (WP-3). An associated fn taking the whole state, like
    /// [`Backend::outputs_changed`], so a backend can reach both its DRM state and the space.
    fn set_outputs_powered(state: &mut CrucibleState<Self>, on: bool)
    where
        Self: Sized + 'static,
    {
        let _ = state;
        tracing::info!(on, "idle: output power change ignored by this backend");
    }
    /// Applies the night-light temperature `kelvin` to every output, writing only what changed
    /// (see [`crate::night_light::ApplyCache`]). The default (nested `winit`, test backends) has no
    /// gamma control: it logs once, and only when a tint is wanted.
    fn apply_night_light(state: &mut CrucibleState<Self>, kelvin: u32)
    where
        Self: Sized + 'static,
    {
        static LOGGED: std::sync::Once = std::sync::Once::new();
        let _ = state;
        if kelvin < crate::night_light::model::IDENTITY_KELVIN {
            LOGGED.call_once(|| tracing::info!("night light needs the TTY backend; ignored here"));
        }
    }
}
