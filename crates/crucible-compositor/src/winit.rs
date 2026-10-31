//! Nested `winit` backend for development.
//!
//! Runs CDE as a window inside an existing Wayland or X11 session, for fast iteration without a
//! spare TTY. [`run_winit`] brings up a [`Display`] and a calloop [`EventLoop`] parameterized over
//! [`CrucibleState<WinitData>`], creates a [`winit`](smithay::backend::winit) window backed by a
//! [`GlesRenderer`], treats that window as a single Smithay [`Output`], and drives a render loop
//! that composites the mapped client surfaces plus the cursor each frame. Selected at runtime via
//! `--winit`.
//!
//! ## Render path
//! Each frame collects the mapped windows from the desktop
//! [`Space`](smithay::desktop::Space), the chrome, header and cursor (or, while locked, only the
//! lock surface) via the shared [`crate::render::frame_elements`], presents them, then sends frame
//! callbacks so clients keep drawing. `space.refresh()` / `popups.cleanup()` run each iteration.
//! Input events are handed to [`CrucibleState::process_input_event`].

use std::time::{Duration, Instant};

use smithay::{
    backend::{
        allocator::dmabuf::Dmabuf,
        egl::EGLDevice,
        renderer::{
            ImportDma, ImportMemWl, damage::Error as OutputDamageTrackerError,
            damage::OutputDamageTracker, gles::GlesRenderer,
        },
        winit::{self, WinitEvent},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::EventLoop, wayland_server::Display, winit::event_loop::pump_events::PumpStatus,
    },
    utils::{Scale, Transform},
    wayland::dmabuf::{
        DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier,
    },
};
use tracing::{error, info, warn};

use crate::{
    drawing::PointerElement,
    render::{FrameView, frame_elements},
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
    /// The `zwp_linux_dmabuf_v1` state and global (created before the first client can connect).
    dmabuf_state: (DmabufState, DmabufGlobal),
}

impl DmabufHandler for CrucibleState<WinitData> {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.backend_data.dmabuf_state.0
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        if self
            .backend_data
            .backend
            .renderer()
            .import_dmabuf(&dmabuf, None)
            .is_ok()
        {
            let _ = notifier.successful::<CrucibleState<WinitData>>();
        } else {
            notifier.failed();
        }
    }
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
/// exports the Wayland socket / `WAYLAND_DISPLAY` and the Wayland globals + the seat), then loops:
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

    let (mut backend, mut winit) = match winit::init::<GlesRenderer>() {
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
    let startup_config = crate::config::load_startup_config();
    // The configured scale is set before mapping so the Space geometry and model rect are logical.
    let scale = crate::config::configured_scale(&startup_config, OUTPUT_NAME);
    output.change_current_state(None, None, Some(scale), None);
    output.set_preferred(mode);

    let damage_tracker = OutputDamageTracker::from_output(&output);

    // linux-dmabuf: v5 with default feedback when EGL device discovery yields a render node,
    // otherwise a plain v3 format list. Created here, before `start_session`, so it exists before
    // any client can connect; nested startup never fails because of it.
    let dmabuf_formats = backend.renderer().dmabuf_formats();
    let render_node = EGLDevice::device_for_display(backend.renderer().egl_context().display())
        .and_then(|device| device.try_get_render_node());
    let default_feedback = match render_node {
        Ok(Some(node)) => {
            match DmabufFeedbackBuilder::new(node.dev_id(), dmabuf_formats.clone()).build() {
                Ok(feedback) => Some(feedback),
                Err(err) => {
                    warn!(%err, "failed to build dmabuf feedback, dmabuf will use v3");
                    None
                }
            }
        }
        Ok(None) => {
            warn!("failed to query render node, dmabuf will use v3");
            None
        }
        Err(err) => {
            warn!(
                ?err,
                "failed to get egl device for display, dmabuf will use v3"
            );
            None
        }
    };
    let mut dmabuf_state = DmabufState::new();
    let dmabuf_global = match default_feedback {
        Some(feedback) => dmabuf_state
            .create_global_with_default_feedback::<CrucibleState<WinitData>>(
                &display_handle,
                &feedback,
            ),
        None => {
            dmabuf_state.create_global::<CrucibleState<WinitData>>(&display_handle, dmabuf_formats)
        }
    };

    let data = WinitData {
        backend,
        damage_tracker,
        dmabuf_state: (dmabuf_state, dmabuf_global),
    };

    let mut state = CrucibleState::init(display, event_loop.handle(), data, true);
    state.apply_config(startup_config);
    // Persistence init: slot pins, IPC socket, 1 s auto-save/config tick.
    state.start_persistence();
    state
        .shm_state
        .update_formats(state.backend_data.backend.renderer().shm_formats());
    state.space.map_output(&output, (0, 0));
    // Adopt the model's initial output for this single winit output and size it to its logical
    // size (the window's pixels divided by the configured scale).
    state.register_output(
        &output,
        crate::state::logical_output_rect((0, 0), size, scale.fractional_scale()),
    );
    // The output exists now: restore the last session or open the cold-start picker.
    state.start_session();

    // Bring up XWayland so X11 apps run alongside Wayland clients. On readiness this exports
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
                state.apply_layout();
            }
            WinitEvent::CloseRequested => {
                state
                    .running
                    .store(false, std::sync::atomic::Ordering::SeqCst);
            }
            // Funnel winit input (keyboard/pointer) into the seat. The Super-leader is
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

            // Inputs that need `&state` as a whole are read before the backend borrow below; the
            // frame is then composed from disjoint field borrows beside the renderer.
            let header_layouts = state.header_layouts();
            let empty_focus_rect = state.focused_empty_pane_rect();
            let overlay = state.overlay_chrome();

            let backend = &mut state.backend_data.backend;
            let damage_tracker = &mut state.backend_data.damage_tracker;
            let age = backend.buffer_age().unwrap_or(0);
            let locked = state.session_lock.is_locked();
            let view = FrameView {
                output: &output,
                scale,
                space: &state.space,
                session_lock: &state.session_lock,
                layout: &state.layout,
                projection: &state.projection,
                empty_focus_rect,
                overlay,
                tiling: &state.config.tiling,
                header_layouts: &header_layouts,
                header_output: crate::layout::model_output_id(&output),
                header_size: state.config.header.size,
                dnd_icon: state.dnd_icon.as_ref(),
            };
            let header_text = &mut state.header_text;
            let render_res = backend.bind().and_then(|(renderer, mut fb)| {
                // winit draws the cursor unconditionally at the pointer position.
                let (elements, clear_color) = frame_elements(
                    view,
                    header_text,
                    &pointer_element,
                    Some(cursor_phys),
                    renderer,
                );
                damage_tracker
                    .render_output(renderer, &mut fb, age, &elements, clear_color)
                    .map_err(|err| match err {
                        OutputDamageTrackerError::Rendering(err) => err.into(),
                        OutputDamageTrackerError::OutputNoMode(_) => unreachable!(),
                    })
            });

            // A locked frame counts as presented once submitted (or when nothing changed since the
            // last one: the first locked frame always carries damage, its clear colour differs).
            let mut presented_locked = false;
            match render_res {
                Ok(render_output_result) => {
                    // Track primary scan-out so the fractional-scale walk below has an output.
                    crate::state::update_primary_scanout_output(
                        &state.space,
                        &output,
                        state.dnd_icon.as_ref(),
                        &render_output_result.states,
                    );
                    if let Some(damage) = render_output_result.damage {
                        if let Err(err) = backend.submit(Some(damage)) {
                            warn!("Failed to submit buffer: {}", err);
                        } else {
                            presented_locked = locked;
                        }
                    } else {
                        presented_locked = locked;
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
            if presented_locked {
                state.lock_output_presented(&output);
            }
        }

        // Send frame callbacks so clients render their next frame.
        let frame_time = start_time.elapsed();
        if state.session_lock.is_locked() {
            crate::lock::send_lock_frames(
                &state.session_lock,
                &output,
                frame_time,
                Some(Duration::ZERO),
            );
        }
        state.space.elements().for_each(|window| {
            window.send_frame(&output, frame_time, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        });
        crate::state::send_dnd_icon_frames(
            state.dnd_icon.as_ref(),
            &output,
            frame_time,
            Some(Duration::ZERO),
        );
        state.update_fractional_scales(&output);

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
