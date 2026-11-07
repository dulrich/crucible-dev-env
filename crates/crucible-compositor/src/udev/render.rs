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
        renderer::{damage::Error as OutputDamageTrackerError, gles::GlesRenderbuffer},
    },
    output::Output,
    reexports::{
        calloop::timer::{TimeoutAction, Timer},
        drm::control::crtc,
        wayland_protocols::wp::presentation_time::server::wp_presentation_feedback,
    },
    wayland::presentation::Refresh,
};
use tracing::{debug, error, warn};

use crate::{
    render::{frame_elements, frame_view},
    state::{
        CrucibleState, send_dmabuf_feedback, take_presentation_feedback,
        update_primary_scanout_output,
    },
};

use super::UdevData;
use super::output::{FlipAction, UdevOutputId, output_for};

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

        // Powered off: the flip that was pending at `clear()` time. Nothing is shown, so a locked
        // frame it carried is not counted (the off output is exempt from the lock handshake).
        if surface.power.is_off() {
            self.session_lock.take_queued(&output);
            return;
        }

        let submit_result = surface
            .drm_output
            .frame_submitted()
            .map_err(Into::<SwapBuffersError>::into);

        let schedule_render = match submit_result {
            Ok(user_data) => {
                let had_frame = user_data.is_some();
                if surface.power.on_flip(had_frame) == FlipAction::Drop {
                    // A stale pre-blanking flip after the wake: the wake render owns the chain.
                    return;
                }
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

        // E2: the page-flip of a locked frame completed, so that output now shows it.
        if self.session_lock.take_queued(&output) {
            self.lock_output_presented(&output);
        }

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
    /// Reuses the backend-agnostic [`frame_elements`] composition (so Wayland *and* XWayland windows
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
        let Some(surface) = device.surfaces.get(&crtc) else {
            return;
        };
        // Powered off by idle blanking: render, queue and reschedule nothing (no frame callbacks
        // either, so clients pause). The wake path schedules the next render.
        if surface.power.is_off() {
            return;
        }
        let render_node = device.render_node.unwrap_or(primary_gpu);

        // Dead-cursor reset plus this output's owned frame inputs (the cursor only when the
        // pointer is over this output, output-local).
        let inputs = self.begin_frame(&output);

        // Select a renderer: same-GPU is a single renderer, cross-GPU needs the scanout format.
        let format = self.backend_data.backends[&node].surfaces[&crtc]
            .drm_output
            .format();
        let mut renderer = UdevData::renderer_for(
            &mut self.backend_data.gpus,
            primary_gpu,
            render_node,
            format,
        )
        .unwrap();

        // Compose from disjoint field borrows: `renderer` holds `self.backend_data.gpus`; the
        // cursor is the separate `self.cursor` field.
        let locked = self.session_lock.is_locked();
        let view = frame_view!(self, &output, &inputs, self.dnd_icon.as_ref());
        let (elements, clear_color) =
            frame_elements(view, &mut self.header_text, &mut self.cursor, &mut renderer);

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
            update_primary_scanout_output(&self.space, &output, self.dnd_icon.as_ref(), &states);
            if rendered {
                let feedback = take_presentation_feedback(&output, &self.space, &states);
                // A failed queue must reach the recovery match below: swallowing it (as this
                // port once did) leaves the CRTC with no page-flip pending and no timer, so the
                // output never renders again — the black screen after a VT round-trip.
                self.backend_data
                    .backends
                    .get_mut(&node)
                    .and_then(|device| device.surfaces.get_mut(&crtc))
                    .expect("the surface rendered above still exists")
                    .drm_output
                    .queue_frame(Some(feedback))
                    .map_err(Into::<SwapBuffersError>::into)?;
                // E2: `frame_finish` counts this output as presented iff this frame was locked.
                self.session_lock.note_queued(&output, locked);
            }
            // Send frame callbacks so clients draw their next frame.
            self.finish_frame(
                &output,
                self.clock.now().into(),
                Some(Duration::from_secs(1)),
            );
            let surface = self
                .backend_data
                .backends
                .get(&node)
                .and_then(|device| device.surfaces.get(&crtc));
            if let Some(fb) = surface.and_then(|surface| surface.dmabuf_feedback.as_ref()) {
                send_dmabuf_feedback(
                    &self.space,
                    &output,
                    &states,
                    &fb.render_feedback,
                    &fb.scanout_feedback,
                );
            }
            Ok(rendered)
        });
        // Preferred fractional scale per surface, tracking its primary scan-out output (recorded
        // above by `update_primary_scanout_output`).
        if result.is_ok() {
            self.update_fractional_scales(&output);
        }

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

impl CrucibleState<UdevData> {
    /// Captures `output` offscreen with no cursor (plan 34): the same renderer selection as
    /// [`CrucibleState::render_surface`] (the output's render node, cross-GPU through the primary
    /// GPU with the scanout format), rendering into a [`GlesRenderbuffer`] and reading it back.
    pub(super) fn capture_surface(
        &mut self,
        output: &Output,
    ) -> Result<crate::capture::Capture, crate::capture::CaptureError> {
        use crate::capture::CaptureError;
        let id = *output
            .user_data()
            .get::<UdevOutputId>()
            .ok_or(CaptureError::NoOutput)?;
        let primary_gpu = self.backend_data.primary_gpu;
        let device = self
            .backend_data
            .backends
            .get(&id.device_id)
            .ok_or(CaptureError::NoOutput)?;
        let surface = device
            .surfaces
            .get(&id.crtc)
            .ok_or(CaptureError::NoOutput)?;
        let render_node = device.render_node.unwrap_or(primary_gpu);
        let format = surface.drm_output.format();
        let inputs = self.capture_inputs(output);
        let mut renderer = UdevData::renderer_for(
            &mut self.backend_data.gpus,
            primary_gpu,
            render_node,
            format,
        )
        .map_err(|e| CaptureError::Render(e.to_string()))?;
        let view = frame_view!(self, output, &inputs, None);
        crate::capture::render_capture::<_, GlesRenderbuffer>(
            &mut renderer,
            view,
            &mut self.header_text,
            &mut self.cursor,
        )
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
