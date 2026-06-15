//! Nested `winit` backend for development.
//!
//! Runs CDE as a window inside an existing Wayland or X11 session, for fast iteration without a
//! spare TTY. [`run_winit`] brings up a [`Display`] and a calloop [`EventLoop`] parameterized over
//! [`CrucibleState<WinitData>`], creates a [`winit`](smithay::backend::winit) window backed by a
//! [`GlesRenderer`], treats that window as a single Smithay [`Output`], and drives a render loop
//! that clears the output to a solid color each frame while dispatching Wayland clients. Selected
//! at runtime via `--winit`.
//!
//! This is the WS-2 bring-up: a client can connect and enumerate globals, but nothing maps yet
//! (window mapping/placement is WS-3, input is WS-4). The structure mirrors anvil's `winit.rs` so
//! the per-frame compositor logic can be filled in incrementally.

use std::time::Duration;

use smithay::{
    backend::{
        renderer::{
            ImportMemWl,
            damage::{Error as OutputDamageTrackerError, OutputDamageTracker},
            element::solid::SolidColorRenderElement,
            gles::GlesRenderer,
        },
        winit::{self, WinitEvent},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::EventLoop, wayland_server::Display, winit::event_loop::pump_events::PumpStatus,
    },
    utils::Transform,
};
use tracing::{error, info, warn};

use crate::state::{Backend, CrucibleState};

/// Name advertised for the single output backed by the winit window.
pub const OUTPUT_NAME: &str = "winit";

/// Solid color the output is cleared to each frame (a dark slate), as `[r, g, b, a]`.
const CLEAR_COLOR: [f32; 4] = [0.05, 0.05, 0.08, 1.0];

/// Backend data for the nested `winit` development backend.
///
/// Holds the `winit`-hosted [`GlesRenderer`] and the per-output damage tracker driving rendering.
#[derive(Debug)]
pub struct WinitData {
    /// The `winit` graphics backend wrapping the GL ES renderer and its window/EGL surface.
    backend: winit::WinitGraphicsBackend<GlesRenderer>,
    /// Damage tracker for the single winit output.
    damage_tracker: OutputDamageTracker,
}

impl Backend for WinitData {
    fn seat_name(&self) -> String {
        String::from("winit")
    }

    fn reset_buffers(&mut self, _output: &Output) {}

    fn early_import(
        &mut self,
        _surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
    }

    fn update_led_state(&mut self, _led_state: smithay::input::keyboard::LedState) {}
}

/// Brings up and runs the nested `winit` development backend.
///
/// Creates the Wayland [`Display`] and calloop [`EventLoop`], opens a `winit` window with a
/// [`GlesRenderer`], registers it as a single [`Output`] whose [`Mode`] matches the window, builds
/// the [`CrucibleState`] (which exports the Wayland socket / `WAYLAND_DISPLAY` and the WS-2
/// globals), then loops: pumping [`WinitEvent`]s, clearing the output to [`CLEAR_COLOR`] each
/// frame, dispatching the event loop, and flushing clients.
///
/// Returns once the window is closed or a fatal error occurs.
pub fn run_winit() {
    let mut event_loop: EventLoop<CrucibleState<WinitData>> = match EventLoop::try_new() {
        Ok(loop_) => loop_,
        Err(err) => {
            error!("Failed to create the event loop: {}", err);
            return;
        }
    };
    let display: Display<CrucibleState<WinitData>> = Display::new().unwrap();
    let mut display_handle = display.handle();

    let (backend, mut winit) = match winit::init::<GlesRenderer>() {
        Ok(ret) => ret,
        Err(err) => {
            error!("Failed to initialize winit backend: {}", err);
            return;
        }
    };
    let size = backend.window_size();

    let mode = Mode {
        size,
        refresh: 60_000,
    };
    let output = Output::new(
        OUTPUT_NAME.to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Crucible".into(),
            model: "Winit".into(),
            serial_number: "Unknown".into(),
        },
    );
    let _global = output.create_global::<CrucibleState<WinitData>>(&display_handle);
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);

    let damage_tracker = OutputDamageTracker::from_output(&output);
    let data = WinitData {
        backend,
        damage_tracker,
    };

    let mut state = CrucibleState::init(display, event_loop.handle(), data, true);
    state
        .shm_state
        .update_formats(state.backend_data.backend.renderer().shm_formats());
    state.space.map_output(&output, (0, 0));

    info!("Initialization completed, starting the main loop.");

    while state.running.load(std::sync::atomic::Ordering::SeqCst) {
        let status = winit.dispatch_new_events(|event| match event {
            WinitEvent::Resized { size, .. } => {
                let mode = Mode {
                    size,
                    refresh: 60_000,
                };
                output.change_current_state(Some(mode), None, None, None);
                output.set_preferred(mode);
                state.space.map_output(&output, (0, 0));
            }
            WinitEvent::CloseRequested => {
                state
                    .running
                    .store(false, std::sync::atomic::Ordering::SeqCst);
            }
            // Input is wired in WS-4; other events are ignored for WS-2.
            _ => (),
        });

        if let PumpStatus::Exit(_) = status {
            state
                .running
                .store(false, std::sync::atomic::Ordering::SeqCst);
            break;
        }

        // Drawing logic: clear the single output to a solid color.
        {
            let backend = &mut state.backend_data.backend;
            let damage_tracker = &mut state.backend_data.damage_tracker;
            let age = backend.buffer_age().unwrap_or(0);

            let render_res = backend.bind().and_then(|(renderer, mut fb)| {
                // No mapped windows yet (WS-3); render an empty element list so the tracker
                // simply clears the output to the background color.
                let elements: [SolidColorRenderElement; 0] = [];
                damage_tracker
                    .render_output(renderer, &mut fb, age, &elements, CLEAR_COLOR)
                    .map_err(|err| match err {
                        OutputDamageTrackerError::Rendering(err) => err.into(),
                        OutputDamageTrackerError::OutputNoMode(_) => unreachable!(),
                    })
            });

            match render_res {
                Ok(render_output_result) => {
                    if let Some(damage) = render_output_result.damage {
                        if let Err(err) = backend.submit(Some(damage)) {
                            warn!("Failed to submit buffer: {}", err);
                        }
                    }
                }
                Err(smithay::backend::SwapBuffersError::ContextLost(err)) => {
                    error!("Critical rendering error: {}", err);
                    state
                        .running
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                }
                Err(err) => warn!("Rendering error: {}", err),
            }
        }

        let result = event_loop.dispatch(Some(Duration::from_millis(16)), &mut state);
        if result.is_err() {
            state
                .running
                .store(false, std::sync::atomic::Ordering::SeqCst);
        } else {
            state.space.refresh();
            display_handle.flush_clients().unwrap();
        }
    }
}
