//! Nested `winit` backend for development.
//!
//! Runs CDE as a window inside an existing Wayland or X11 session, for fast iteration without a
//! spare TTY. [`run_winit`] brings up a [`Display`] and a calloop [`EventLoop`] parameterized over
//! [`CrucibleState<WinitData>`], creates a [`winit`](smithay::backend::winit) window backed by a
//! [`GlesRenderer`], treats that window as a single Smithay [`Output`], and drives a render loop
//! that composites the mapped client surfaces plus the cursor each frame. Selected at runtime via
//! `--winit`.
//!
//! ## WS-3 render path
//! WS-2 cleared the output to a solid color. WS-3 collects the mapped windows from the desktop
//! [`Space`](smithay::desktop::Space) as [`WaylandSurfaceRenderElement`]s (via
//! [`crate::render::render_output`]) and composites a cursor element on top, then sends frame
//! callbacks so clients keep drawing. `space.refresh()` / `popups.cleanup()` run each iteration.
//! Input is still **not** routed into the seat (WS-4).

use std::time::{Duration, Instant};

use smithay::{
    backend::{
        renderer::{
            ImportMemWl, damage::Error as OutputDamageTrackerError, damage::OutputDamageTracker,
            gles::GlesRenderer,
        },
        winit::{self, WinitEvent},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::EventLoop, wayland_server::Display, winit::event_loop::pump_events::PumpStatus,
    },
    utils::{Scale, Transform},
};
use tracing::{error, info, warn};

use crate::{
    drawing::PointerElement,
    render::{CustomRenderElements, render_output},
    state::{Backend, CrucibleState},
};

/// Name advertised for the single output backed by the winit window.
pub const OUTPUT_NAME: &str = "winit";

/// Solid color the output is cleared to each frame (a dark slate), as `[r, g, b, a]`.
pub const CLEAR_COLOR: [f32; 4] = [0.05, 0.05, 0.08, 1.0];

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
/// [`GlesRenderer`], registers it as a single [`Output`], builds the [`CrucibleState`] (which
/// exports the Wayland socket / `WAYLAND_DISPLAY` and all WS-3 globals + the seat), then loops:
/// pumping [`WinitEvent`]s, compositing the mapped surfaces and cursor, dispatching the event loop,
/// sending frame callbacks, and flushing clients.
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
    // Adopt the model's initial output for this single winit output and size it to the real pixels.
    state.register_output(&output, crucible_layout::Rect::new(0, 0, size.w, size.h));

    // Bring up XWayland so X11 apps run alongside Wayland clients (WS-5). On readiness this exports
    // `DISPLAY` and attaches the X11 window manager; see `crate::xwayland`.
    state.start_xwayland();

    let start_time = Instant::now();
    let mut pointer_element = PointerElement::default();

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
                // Keep the model's stored rect in sync, then re-tile (model-driven, idempotent).
                state.sync_output_rect(&output);
                state.apply_geometry();
            }
            WinitEvent::CloseRequested => {
                state
                    .running
                    .store(false, std::sync::atomic::Ordering::SeqCst);
            }
            // WS-4: funnel winit input (keyboard/pointer) into the seat. The Super-leader is
            // intercepted inside; everything else is forwarded to the focused client.
            WinitEvent::Input(event) => state.process_input_event(event),
            // Redraw/CreateSurface events are handled by the render loop below.
            _ => (),
        });

        if let PumpStatus::Exit(_) = status {
            state
                .running
                .store(false, std::sync::atomic::Ordering::SeqCst);
            break;
        }

        // Reset the cursor to the default if the client surface backing it went away.
        if let smithay::input::pointer::CursorImageStatus::Surface(ref surface) =
            state.cursor_status
        {
            if !smithay::utils::IsAlive::alive(surface) {
                state.cursor_status = smithay::input::pointer::CursorImageStatus::default_named();
            }
        }
        pointer_element.set_status(state.cursor_status.clone());

        // Compose and present the frame: mapped surfaces from the Space, with the cursor on top.
        {
            let scale = Scale::from(output.current_scale().fractional_scale());
            let cursor_pos = state.pointer.current_location();
            let cursor_phys = cursor_pos.to_physical(scale).to_i32_round();

            // Chrome (focus border + stack badges) — built before the backend borrow below since
            // SolidColorRenderElement needs no renderer reference.
            let mut chrome: Vec<CustomRenderElements<GlesRenderer>> = state.chrome_elements();

            let backend = &mut state.backend_data.backend;
            let damage_tracker = &mut state.backend_data.damage_tracker;
            let age = backend.buffer_age().unwrap_or(0);

            let render_res = backend.bind().and_then(|(renderer, mut fb)| {
                // Cursor on top of chrome.
                let cursor_elements: Vec<CustomRenderElements<GlesRenderer>> =
                    smithay::backend::renderer::element::AsRenderElements::render_elements(
                        &pointer_element,
                        renderer,
                        cursor_phys,
                        scale,
                        1.0,
                    );
                // Front-to-back: cursor → chrome → windows.
                let mut all_elements = cursor_elements;
                all_elements.append(&mut chrome);

                render_output(
                    &output,
                    &state.space,
                    all_elements,
                    renderer,
                    &mut fb,
                    damage_tracker,
                    age,
                )
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

        // Send frame callbacks so clients render their next frame.
        let frame_time = start_time.elapsed();
        state.space.elements().for_each(|window| {
            window.send_frame(&output, frame_time, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        });

        let result = event_loop.dispatch(Some(Duration::from_millis(16)), &mut state);
        if result.is_err() {
            state
                .running
                .store(false, std::sync::atomic::Ordering::SeqCst);
        } else {
            state.space.refresh();
            state.popups.cleanup();
            display_handle.flush_clients().unwrap();
        }
    }
}
