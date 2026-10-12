//! Per-output rendering and VBlank/frame scheduling for the `udev` backend.
//!
//! Each mapped output drives itself: a page-flip completion
//! ([`CrucibleState::frame_finish`]) delivers presentation feedback and schedules the next
//! repaint one frame out, and [`CrucibleState::render_surface`] composites and submits that
//! frame through the CRTC's [`DrmOutput`](smithay::backend::drm::output::DrmOutput). The
//! element collection itself is the backend-agnostic one in [`crate::render`], so Wayland and
//! XWayland windows appear identically on the winit and DRM paths.
//!
//! Nothing here changes the output topology; it only ever renders outputs that
//! [`super::output`] has already mapped.

use std::io;
use std::time::Duration;

use smithay::{
    backend::{
        SwapBuffersError,
        drm::{DrmAccessError, DrmError, DrmEventMetadata, DrmNode, compositor::FrameFlags},
        renderer::{damage::Error as OutputDamageTrackerError, element::AsRenderElements},
    },
    input::pointer::CursorImageStatus,
    reexports::{
        calloop::timer::{TimeoutAction, Timer},
        drm::control::crtc,
        wayland_protocols::wp::presentation_time::server::wp_presentation_feedback,
    },
    utils::{IsAlive, Scale},
    wayland::presentation::Refresh,
};
use tracing::{debug, error, warn};

use crate::{
    render::{CustomRenderElements, output_elements},
    state::{CrucibleState, take_presentation_feedback, update_primary_scanout_output},
};

use super::output::{UdevOutputId, output_for};
use super::{UdevData, UdevRenderer};

impl CrucibleState<UdevData> {
    /// Handles a completed page-flip (VBlank): delivers presentation feedback and schedules the next
    /// frame.
    pub(super) fn frame_finish(
        &mut self,
        dev_id: DrmNode,
        crtc: crtc::Handle,
        metadata: &mut Option<DrmEventMetadata>,
    ) {
        let Some(device_backend) = self.backend_data.backends.get_mut(&dev_id) else {
            error!("Trying to finish frame on non-existent backend {}", dev_id);
            return;
        };
        let Some(surface) = device_backend.surfaces.get_mut(&crtc) else {
            error!("Trying to finish frame on non-existent crtc {:?}", crtc);
            return;
        };

        let Some(output) = output_for(
            &self.space,
            UdevOutputId {
                device_id: dev_id,
                crtc,
            },
        ) else {
            return;
        };

        let Some(frame_duration) = output
            .current_mode()
            .map(|mode| Duration::from_secs_f64(1_000f64 / mode.refresh as f64))
        else {
            return;
        };

        // Derive the presentation clock + flags from the VBlank metadata.
        let tp = metadata.as_ref().and_then(|metadata| match metadata.time {
            smithay::backend::drm::DrmEventTime::Monotonic(tp) => (!tp.is_zero()).then_some(tp),
            smithay::backend::drm::DrmEventTime::Realtime(_) => None,
        });
        let seq = metadata.as_ref().map(|m| m.sequence).unwrap_or(0);
        let (clock, flags) = if let Some(tp) = tp {
            (
                tp.into(),
                wp_presentation_feedback::Kind::Vsync
                    | wp_presentation_feedback::Kind::HwClock
                    | wp_presentation_feedback::Kind::HwCompletion,
            )
        } else {
            (self.clock.now(), wp_presentation_feedback::Kind::Vsync)
        };

        let submit_result = surface
            .drm_output
            .frame_submitted()
            .map_err(Into::<SwapBuffersError>::into);

        let schedule_render = match submit_result {
            Ok(user_data) => {
                if let Some(mut feedback) = user_data.flatten() {
                    feedback.presented(clock, Refresh::fixed(frame_duration), seq as u64, flags);
                }
                true
            }
            Err(err) => {
                warn!("Error during frame submission: {:?}", err);
                match err {
                    SwapBuffersError::AlreadySwapped => true,
                    // A deactivated device will be re-kicked on session resume; don't reschedule.
                    SwapBuffersError::TemporaryFailure(err) => !matches!(
                        err.downcast_ref::<DrmError>(),
                        Some(&DrmError::DeviceInactive)
                    ),
                    SwapBuffersError::ContextLost(err) => panic!("Rendering loop lost: {err}"),
                }
            }
        };

        if schedule_render {
            // Schedule the next repaint roughly one frame out.
            let timer = Timer::from_duration(frame_duration);
            self.handle
                .insert_source(timer, move |_, _, data| {
                    data.render(dev_id, Some(crtc));
                    TimeoutAction::Drop
                })
                .expect("failed to schedule frame timer");
        }
    }
    /// Renders one CRTC (when `crtc` is `Some`) or all CRTCs on `node`.
    pub(super) fn render(&mut self, node: DrmNode, crtc: Option<crtc::Handle>) {
        let Some(device_backend) = self.backend_data.backends.get(&node) else {
            error!("Trying to render on non-existent backend {}", node);
            return;
        };
        match crtc {
            Some(crtc) => self.render_surface(node, crtc),
            None => {
                let crtcs: Vec<_> = device_backend.surfaces.keys().copied().collect();
                for crtc in crtcs {
                    self.render_surface(node, crtc);
                }
            }
        }
    }
    /// Composites and submits one frame for the output on `(node, crtc)`.
    ///
    /// Reuses the backend-agnostic [`output_elements`] collection (so Wayland *and* XWayland windows
    /// render), submits the frame through the [`DrmOutput`], records the primary-scanout output for
    /// every surface, queues presentation feedback, and sends frame callbacks. Re-schedules a repaint
    /// when the frame produced no damage.
    pub(super) fn render_surface(&mut self, node: DrmNode, crtc: crtc::Handle) {
        let Some(output) = output_for(
            &self.space,
            UdevOutputId {
                device_id: node,
                crtc,
            },
        ) else {
            return;
        };

        let primary_gpu = self.backend_data.primary_gpu;
        let Some(device) = self.backend_data.backends.get(&node) else {
            return;
        };
        if !device.surfaces.contains_key(&crtc) {
            return;
        }
        let render_node = device.render_node.unwrap_or(primary_gpu);

        // Reset a dead client cursor surface to the default before drawing.
        if let CursorImageStatus::Surface(ref s) = self.cursor_status {
            if !s.alive() {
                self.cursor_status = CursorImageStatus::default_named();
            }
        }
        self.backend_data
            .pointer_element
            .set_status(self.cursor_status.clone());

        let scale = Scale::from(output.current_scale().fractional_scale());
        let output_geo = self.space.output_geometry(&output).unwrap();
        let pointer_location = self.pointer.current_location();
        let empty_focus_rect = self.focused_empty_pane_rect();

        // Select a renderer: same-GPU is a single renderer, cross-GPU needs the scanout format.
        let format = self.backend_data.backends[&node].surfaces[&crtc]
            .drm_output
            .format();
        let mut renderer = if primary_gpu == render_node {
            self.backend_data.gpus.single_renderer(&render_node)
        } else {
            self.backend_data
                .gpus
                .renderer(&primary_gpu, &render_node, format)
        }
        .unwrap();

        // Chrome (focus border + stack badges) — built here using the free function directly to
        // avoid a whole-`self` borrow conflict with `renderer` (which holds `&mut self.backend_data`).
        // SolidColorRenderElement is renderer-agnostic; no renderer reference is needed.
        let mut chrome: Vec<CustomRenderElements<UdevRenderer<'_>>> =
            crate::chrome::build_chrome_elements(
                &self.layout.world,
                &self.projection,
                empty_focus_rect,
                (output_geo.loc.x, output_geo.loc.y),
            )
            .into_iter()
            .map(CustomRenderElements::SolidColor)
            .collect();

        // Cursor element, drawn only on the output the pointer is over (on top of chrome).
        if output_geo.to_f64().contains(pointer_location) {
            let cursor_pos = pointer_location - output_geo.loc.to_f64();
            let cursor_elements: Vec<CustomRenderElements<UdevRenderer<'_>>> =
                self.backend_data.pointer_element.render_elements(
                    &mut renderer,
                    cursor_pos.to_physical(scale).to_i32_round(),
                    scale,
                    1.0,
                );
            // Front-to-back: cursor → chrome → windows.
            let mut all = cursor_elements;
            all.append(&mut chrome);
            chrome = all;
        }

        let (elements, clear_color) = output_elements(&output, &self.space, chrome, &mut renderer);

        let surface = self
            .backend_data
            .backends
            .get_mut(&node)
            .unwrap()
            .surfaces
            .get_mut(&crtc)
            .unwrap();
        let result = surface
            .drm_output
            .render_frame(&mut renderer, &elements, clear_color, FrameFlags::DEFAULT)
            .map(|res| (!res.is_empty, res.states))
            .map_err(|err| match err {
                smithay::backend::drm::compositor::RenderFrameError::PrepareFrame(err) => {
                    SwapBuffersError::from(err)
                }
                smithay::backend::drm::compositor::RenderFrameError::RenderFrame(
                    OutputDamageTrackerError::Rendering(err),
                ) => SwapBuffersError::from(err),
                _ => unreachable!(),
            });

        let result = result.and_then(|(rendered, states)| {
            update_primary_scanout_output(&self.space, &output, &states);
            if rendered {
                let feedback = take_presentation_feedback(&output, &self.space, &states);
                // A failed queue must reach the recovery match below: swallowing it (as this
                // port once did) leaves the CRTC with no page-flip pending and no timer, so the
                // output never renders again — the black screen after a VT round-trip.
                surface
                    .drm_output
                    .queue_frame(Some(feedback))
                    .map_err(Into::<SwapBuffersError>::into)?;
            }
            // Send frame callbacks so clients draw their next frame.
            let now = self.clock.now();
            let throttle = Some(Duration::from_secs(1));
            self.space.elements().for_each(|window| {
                window.send_frame(&output, now, throttle, |_, _| Some(output.clone()));
            });
            Ok(rendered)
        });

        let reschedule = match result {
            Ok(rendered) => !rendered,
            Err(err) => match recovery_for(&err) {
                Recovery::Retry => {
                    debug!("Rendering deferred: {:?}", err);
                    true
                }
                Recovery::ResetAndRetry => {
                    // A foreign DRM master (X on another VT) changed CRTC/connector/plane state
                    // while we were paused, so our atomic test no longer validates. Reset the whole
                    // device to a known state and repaint, as smithay's anvil does.
                    warn!("Atomic test failed, resetting DRM state: {:?}", err);
                    if let Some(device) = self.backend_data.backends.get_mut(&node) {
                        if let Err(err) = device.drm_output_manager.device_mut().reset_state() {
                            error!(%err, "Failed to reset DRM device state");
                        }
                    }
                    true
                }
                Recovery::Drop => {
                    warn!("Error during rendering: {:?}", err);
                    false
                }
                Recovery::Fatal => panic!("Rendering loop lost: {err}"),
            },
        };

        if reschedule {
            if let Some(mode) = output.current_mode() {
                let refresh = Duration::from_millis(1_000_000 / mode.refresh as u64);
                let timer = Timer::from_duration(refresh);
                self.handle
                    .insert_source(timer, move |_, _, data| {
                        data.render(node, Some(crtc));
                        TimeoutAction::Drop
                    })
                    .expect("failed to schedule frame timer");
            }
        }
    }
}

/// How [`CrucibleState::render_surface`] recovers from a failed render or queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recovery {
    /// Transient (session paused, or permission denied mid VT-switch): repaint next frame.
    Retry,
    /// The atomic test failed (stale KMS state after a VT switch): reset the device, repaint.
    ResetAndRetry,
    /// Nothing to do this frame (already swapped, or an unrecognised transient error).
    Drop,
    /// The rendering context is gone.
    Fatal,
}

/// Classifies a render/queue error into its [`Recovery`]; mirrors smithay's anvil.
fn recovery_for(err: &SwapBuffersError) -> Recovery {
    match err {
        SwapBuffersError::AlreadySwapped => Recovery::Drop,
        SwapBuffersError::TemporaryFailure(err) => match err.downcast_ref::<DrmError>() {
            Some(DrmError::DeviceInactive) => Recovery::Retry,
            Some(DrmError::Access(DrmAccessError { source, .. }))
                if source.kind() == io::ErrorKind::PermissionDenied =>
            {
                Recovery::Retry
            }
            _ => Recovery::Drop,
        },
        SwapBuffersError::ContextLost(err) => match err.downcast_ref::<DrmError>() {
            Some(DrmError::TestFailed(_)) => Recovery::ResetAndRetry,
            _ => Recovery::Fatal,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::reexports::drm::control::RawResourceHandle;

    fn crtc() -> crtc::Handle {
        crtc::Handle::from(RawResourceHandle::new(86).unwrap())
    }

    #[test]
    fn test_failed_after_a_vt_switch_resets_and_retries() {
        let err: SwapBuffersError = DrmError::TestFailed(crtc()).into();
        assert_eq!(recovery_for(&err), Recovery::ResetAndRetry);
    }

    #[test]
    fn paused_device_retries() {
        let err: SwapBuffersError = DrmError::DeviceInactive.into();
        assert_eq!(recovery_for(&err), Recovery::Retry);
    }

    #[test]
    fn already_swapped_drops_the_frame() {
        assert_eq!(
            recovery_for(&SwapBuffersError::AlreadySwapped),
            Recovery::Drop
        );
    }
}
