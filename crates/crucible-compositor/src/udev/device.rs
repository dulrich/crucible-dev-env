//! DRM device ownership for the `udev` backend: opening GPUs, keeping their renderers, and
//! rescanning their connectors.
//!
//! One [`BackendData`] exists per DRM node. This module owns its whole lifetime — the `libseat`
//! open, the GBM allocator and framebuffer exporter, the [`DrmOutputManager`], the VBlank event
//! source, and the entry in the multi-GPU [`GpuManager`] — plus the GPU selection helpers used to
//! bring the backend up. It also owns the *device-level* transitions: a rescan
//! ([`CrucibleState::device_changed`]) hands each connector event to [`super::output`], and a
//! device removal plans the loss of all of its connectors as one transition.
//!
//! Both of the backend's `unsafe` EGL/GLES constructions live here, each next to the GBM/EGL
//! object whose lifetime makes it sound.

use std::collections::HashMap;
use std::path::Path;

use smithay::{
    backend::{
        allocator::{
            Fourcc, Modifier,
            format::FormatSet,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            CreateDrmNodeError, DrmDevice, DrmDeviceFd, DrmError, DrmEvent, DrmNode, NodeType,
            exporter::gbm::GbmFramebufferExporter, output::DrmOutputManager,
        },
        egl::{EGLContext, EGLDevice, EGLDisplay, context::ContextPriority},
        renderer::{
            gles::GlesRenderer,
            multigpu::{Error as MultiError, GpuManager, gbm::GbmGlesBackend},
        },
        session::{
            Session,
            libseat::{self, LibSeatSession},
        },
        udev::{all_gpus, primary_gpu},
    },
    desktop::utils::OutputPresentationFeedback,
    reexports::{calloop::RegistrationToken, drm::control::crtc, rustix::fs::OFlags},
    utils::DeviceFd,
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};
use tracing::{debug, error, info, warn};

use crate::state::CrucibleState;

use super::output::{self, PendingAdoption, SurfaceData, UdevOutputId};
use super::{UdevData, plan};

/// Color formats CDE asks the DRM compositor to use, in preference order.
///
/// 10-bit first (better gradients) with 8-bit fallback — both are widely supported by display
/// controllers. Mirrors anvil's choice.
const SUPPORTED_FORMATS: &[Fourcc] = &[
    Fourcc::Abgr2101010,
    Fourcc::Argb2101010,
    Fourcc::Abgr8888,
    Fourcc::Argb8888,
];

/// Per-DRM-device state: its output manager, connector scanner, and live outputs.
pub(super) struct BackendData {
    /// The mapped outputs on this device, keyed by CRTC.
    pub surfaces: HashMap<crtc::Handle, SurfaceData>,
    /// Start-up outputs that are mapped but wait for their wallpaper before their DRM adoption
    /// (plan 44); an output is in `surfaces` or here, never both.
    pub pending: HashMap<crtc::Handle, PendingAdoption>,
    /// Manages the GBM-backed [`DrmOutput`]s for this device's CRTCs.
    pub drm_output_manager: DrmOutputManager<
        GbmAllocator<DrmDeviceFd>,
        GbmFramebufferExporter<DrmDeviceFd>,
        Option<OutputPresentationFeedback>,
        DrmDeviceFd,
    >,
    /// Tracks connector connect/disconnect across `scan_connectors` calls.
    pub drm_scanner: DrmScanner,
    /// The render node for this device, if it has one (else rendering falls back to the primary).
    pub render_node: Option<DrmNode>,
    /// calloop token for this device's DRM event (VBlank) source, removed on device removal.
    pub registration_token: RegistrationToken,
}

/// Errors that can occur while adding a DRM device.
#[derive(Debug, thiserror::Error)]
pub(super) enum DeviceAddError {
    /// `libseat` failed to open the device node.
    #[error("Failed to open device using libseat: {0}")]
    DeviceOpen(libseat::Error),
    /// The DRM device could not be initialized.
    #[error("Failed to initialize drm device: {0}")]
    DrmDevice(DrmError),
    /// The GBM device could not be initialized.
    #[error("Failed to initialize gbm device: {0}")]
    GbmDevice(std::io::Error),
    /// The device path did not resolve to a DRM node.
    #[error("Failed to access drm node: {0}")]
    DrmNode(CreateDrmNodeError),
    /// The render node could not be added to the [`GpuManager`].
    #[error("Failed to add device to GpuManager: {0}")]
    AddNode(smithay::backend::egl::Error),
    /// The device exposes no usable render node and no primary GPU is available to borrow from.
    #[error("The device has no render node and no primary GPU is available")]
    NoAllocator,
}

/// Picks any available GPU as a last resort when the seat has no obvious primary GPU.
fn fallback_gpu(session: &LibSeatSession) -> DrmNode {
    all_gpus(session.seat())
        .unwrap()
        .into_iter()
        .find_map(|x| DrmNode::from_path(x).ok())
        .expect("No GPU!")
}

/// Picks the GPU CDE renders on: `CDE_DRM_DEVICE` if set, else the seat's primary (preferring its
/// render node), else any GPU at all.
///
/// Returns `None` only when `CDE_DRM_DEVICE` names something that is not a DRM node, which is a
/// user error the caller reports by refusing to start.
pub(super) fn pick_primary_gpu(session: &LibSeatSession) -> Option<DrmNode> {
    if let Ok(var) = std::env::var("CDE_DRM_DEVICE") {
        return match DrmNode::from_path(&var) {
            Ok(node) => Some(node),
            Err(err) => {
                error!(path = var, %err, "Invalid CDE_DRM_DEVICE path");
                None
            }
        };
    }
    Some(match primary_gpu(session.seat()) {
        Ok(Some(path)) => DrmNode::from_path(&path)
            .ok()
            .and_then(|node| node.node_with_type(NodeType::Render).and_then(|n| n.ok()))
            .or_else(|| DrmNode::from_path(&path).ok())
            .unwrap_or_else(|| fallback_gpu(session)),
        _ => fallback_gpu(session),
    })
}

/// The GBM/GLES backend the multi-GPU manager is built over, on both the render and the target
/// side.
type GbmGles = GbmGlesBackend<GlesRenderer, DrmDeviceFd>;

/// Builds the multi-GPU renderer manager (high-priority EGL contexts for the compositor).
pub(super) fn init_gpu_manager() -> Result<GpuManager<GbmGles>, MultiError<GbmGles, GbmGles>> {
    GpuManager::new(GbmGlesBackend::with_factory(|display| {
        let context = EGLContext::new_with_priority(display, ContextPriority::High)?;
        // SAFETY: the GLES renderer is created from a freshly built EGL context that Smithay owns
        // for the lifetime of this GPU node; no other renderer shares it.
        #[allow(unsafe_code)]
        let capabilities = unsafe { GlesRenderer::supported_capabilities(&context)? };
        #[allow(unsafe_code)]
        let renderer = unsafe { GlesRenderer::with_capabilities(context, capabilities)? };
        Ok(renderer)
    }))
    .inspect(|_| {
        info!(
            elapsed_ms = crate::startup::elapsed_ms(),
            "startup: GPU manager created"
        );
    })
}

impl CrucibleState<UdevData> {
    /// Opens a DRM device, registers its VBlank source, and adds it to the GPU manager.
    ///
    /// During a background start (plan 45) a device that cannot be added yet (e.g. a seat backend
    /// that refuses opens while the session is inactive) is retried at session activation.
    pub(super) fn device_added(
        &mut self,
        node: DrmNode,
        path: &Path,
    ) -> Result<(), DeviceAddError> {
        let result = self.try_device_added(node, path);
        if result.is_err() && self.backend_data.phase.background() {
            info!(
                ?node,
                "startup: device deferred until the session activates"
            );
            self.backend_data
                .deferred_devices
                .push((node.dev_id(), path.to_path_buf()));
        }
        result
    }

    /// Activates a background start (plan 45) at its first `ActivateSession`: re-activates the
    /// devices (the seat makes CDE DRM master now), adds the devices deferred at start (their
    /// connectors are scanned and their outputs registered as they are added), then adopts every
    /// pending output — its first commit is whatever frame is ready, the login ready rule having
    /// held already when a login daemon activated the session — and disables the CRTCs CDE did not
    /// adopt.
    pub(super) fn activate_background_start(&mut self) {
        let nodes: Vec<DrmNode> = self.backend_data.backends.keys().copied().collect();
        for node in &nodes {
            if let Some(backend) = self.backend_data.backends.get_mut(node) {
                if let Err(err) = backend.drm_output_manager.lock().activate(false) {
                    error!(%err, "Failed to activate drm device at background-start activation");
                }
            }
        }
        for (device_id, path) in std::mem::take(&mut self.backend_data.deferred_devices) {
            if let Err(err) = DrmNode::from_dev_id(device_id)
                .map_err(DeviceAddError::DrmNode)
                .and_then(|node| self.device_added(node, &path))
            {
                error!("Skipping deferred device {device_id}: {err}");
            }
        }
        info!(
            elapsed_ms = crate::startup::elapsed_ms(),
            ready = self.login_ready.done(),
            "startup: adopting at session activation"
        );
        self.finish_startup_adoption();
        self.idle_activity();
        self.evaluate_night_light();
    }

    fn try_device_added(&mut self, node: DrmNode, path: &Path) -> Result<(), DeviceAddError> {
        let fd = self
            .backend_data
            .session
            .open(
                path,
                OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
            )
            .map_err(DeviceAddError::DeviceOpen)?;
        let fd = DrmDeviceFd::new(DeviceFd::from(fd));

        // Keep-lit hand-off (plan 44): record what the previous DRM client left on screen, then keep
        // it (`false`) so the panel stays on until CDE's first commit replaces it. The CRTCs CDE
        // does not adopt are disabled once start-up adoption is done (`disable_unadopted_crtcs`).
        log_crtc_state(&fd, "cde start", &[]);
        let (drm, notifier) =
            DrmDevice::new(fd.clone(), false).map_err(DeviceAddError::DrmDevice)?;
        info!(
            elapsed_ms = crate::startup::elapsed_ms(),
            ?node,
            "startup: DrmDevice::new done"
        );
        let gbm = GbmDevice::new(fd).map_err(DeviceAddError::GbmDevice)?;

        // Drive page-flip completion (VBlank) for every CRTC on this device.
        let registration_token = self
            .handle
            .insert_source(notifier, move |event, metadata, data| match event {
                DrmEvent::VBlank(crtc) => {
                    data.frame_finish(node, crtc, metadata);
                }
                DrmEvent::Error(error) => {
                    error!("{:?}", error);
                }
            })
            .unwrap();

        // Add this device's render node to the GPU manager (falls back to the primary on failure).
        let render_node = (|| {
            // SAFETY: `gbm` is a live GBM device owned for this DRM node's lifetime; the EGL display
            // borrows it and is dropped before the device is.
            #[allow(unsafe_code)]
            let display = unsafe { EGLDisplay::new(gbm.clone()).map_err(DeviceAddError::AddNode)? };
            let egl_device =
                EGLDevice::device_for_display(&display).map_err(DeviceAddError::AddNode)?;
            let render_node = egl_device
                .try_get_render_node()
                .ok()
                .flatten()
                .unwrap_or(node);
            self.backend_data
                .gpus
                .as_mut()
                .add_node(render_node, gbm.clone())
                .map_err(DeviceAddError::AddNode)?;
            Ok::<DrmNode, DeviceAddError>(render_node)
        })()
        .inspect_err(|err| warn!(?err, "failed to initialize gpu render node"))
        .ok();
        info!(
            elapsed_ms = crate::startup::elapsed_ms(),
            ?node,
            "startup: render node added"
        );

        // Use this device's own allocator if it can render, else borrow the primary GPU's.
        let allocator = render_node
            .is_some()
            .then(|| {
                GbmAllocator::new(
                    gbm.clone(),
                    GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
                )
            })
            .or_else(|| {
                self.backend_data
                    .backends
                    .values()
                    .find(|backend| backend.render_node == Some(self.backend_data.primary_gpu))
                    .map(|backend| backend.drm_output_manager.allocator().clone())
            })
            .ok_or(DeviceAddError::NoAllocator)?;

        let framebuffer_exporter = GbmFramebufferExporter::new(gbm.clone(), render_node.into());

        let mut renderer = self
            .backend_data
            .gpus
            .single_renderer(&render_node.unwrap_or(self.backend_data.primary_gpu))
            .unwrap();
        // Only allow non-linear modifiers when this device has its own render node.
        let render_formats = renderer
            .as_mut()
            .egl_context()
            .dmabuf_render_formats()
            .iter()
            .filter(|format| render_node.is_some() || format.modifier == Modifier::Linear)
            .copied()
            .collect::<FormatSet>();
        drop(renderer);

        let mut drm_output_manager = DrmOutputManager::new(
            drm,
            allocator,
            framebuffer_exporter,
            Some(gbm),
            SUPPORTED_FORMATS.iter().copied(),
            render_formats,
        );

        info!(
            elapsed_ms = crate::startup::elapsed_ms(),
            ?node,
            "startup: DrmOutputManager::new done"
        );
        // Background start (plan 45): smithay marks a new device active; it is not, so mark it
        // paused until `ActivateSession` (no commit can reach the kernel meanwhile).
        if self.backend_data.phase.background() {
            drm_output_manager.pause();
        }

        self.backend_data.backends.insert(
            node,
            BackendData {
                registration_token,
                drm_output_manager,
                drm_scanner: DrmScanner::new(),
                render_node,
                surfaces: HashMap::new(),
                pending: HashMap::new(),
            },
        );

        self.device_changed(node);
        // A device added after start-up adopts its outputs at once: clean it up now. At start-up
        // the cleanup waits for every output's adoption (`finish_startup_adoption`).
        if !self.backend_data.phase.defers_adoption() {
            self.disable_unadopted_crtcs(node);
        }
        Ok(())
    }

    /// Disables the CRTCs left active on `node` that CDE did not adopt (plan 44 invariant 1).
    ///
    /// `DrmDevice::new(fd, false)` keeps whatever the previous DRM client left on screen, so a
    /// connector CDE skipped (no free CRTC, a failed initialisation) or another client's CRTC
    /// would keep showing stale content. Disabling the CRTC (a legacy `SETCRTC` with no fb) also
    /// detaches its connectors; a connector without a CRTC is already off. Adopted and pending
    /// outputs are left untouched.
    pub(super) fn disable_unadopted_crtcs(&mut self, node: DrmNode) {
        use smithay::reexports::drm::control::Device as _;
        let Some(device) = self.backend_data.backends.get(&node) else {
            return;
        };
        let drm = device.drm_output_manager.device();
        let Ok(resources) = drm.resource_handles() else {
            return;
        };
        let active: Vec<crtc::Handle> = resources
            .crtcs()
            .iter()
            .copied()
            .filter(|&c| {
                drm.get_crtc(c)
                    .is_ok_and(|info| info.mode().is_some() || info.framebuffer().is_some())
            })
            .collect();
        let adopted: Vec<crtc::Handle> = device
            .surfaces
            .keys()
            .chain(device.pending.keys())
            .copied()
            .collect();
        for c in crtcs_to_disable(&active, &adopted) {
            match drm.set_crtc(c, None, (0, 0), &[], None) {
                Ok(()) => info!(?node, crtc = ?c, "startup: disabled an unadopted crtc"),
                Err(err) => warn!(?node, crtc = ?c, %err, "failed to disable an unadopted crtc"),
            }
        }
    }

    /// Re-scans a device's connectors and creates/destroys outputs for connect/disconnect events.
    pub(super) fn device_changed(&mut self, node: DrmNode) {
        let Some(device) = self.backend_data.backends.get_mut(&node) else {
            return;
        };

        let scan_result = match device
            .drm_scanner
            .scan_connectors(device.drm_output_manager.device())
        {
            Ok(result) => result,
            Err(err) => {
                warn!(?err, "Failed to scan connectors");
                return;
            }
        };

        info!(
            elapsed_ms = crate::startup::elapsed_ms(),
            ?node,
            "startup: scan_connectors done"
        );

        for event in scan_result {
            match event {
                DrmScanEvent::Connected {
                    connector,
                    crtc: Some(crtc),
                } => self.connector_connected(node, connector, crtc),
                DrmScanEvent::Disconnected {
                    connector,
                    crtc: Some(crtc),
                } => self.connector_disconnected(node, connector, crtc),
                _ => {}
            }
        }

        // Re-lay all outputs left-to-right and re-arrange layer maps.
        output::reflow_outputs(self);
    }

    /// Drops all state for a removed DRM device (GPU unplugged or session torn down).
    ///
    /// Every connector on the device goes away at once, so this is planned and executed as a
    /// single transition: the survivors on the *other* devices reflow once, and removing the last
    /// device simply empties the strip.
    pub(super) fn device_removed(&mut self, node: DrmNode) {
        let Some(device) = self.backend_data.backends.get(&node) else {
            return;
        };
        let departing: Vec<UdevOutputId> = device
            .drm_scanner
            .crtcs()
            .map(|(_, crtc)| UdevOutputId {
                device_id: node,
                crtc,
            })
            .collect();
        let plan = plan::plan_disconnect(&output::output_strip(&self.space), &departing);
        output::apply_plan(self, &plan);

        self.night_light.cache.remove_where(|(n, _)| *n == node);
        if let Some(backend_data) = self.backend_data.backends.remove(&node) {
            if let Some(render_node) = backend_data.render_node {
                self.backend_data.gpus.as_mut().remove_node(&render_node);
            }
            self.handle.remove(backend_data.registration_token);
            debug!("Dropped device {}", node);
        }
    }
}

/// The cleanup set: the active CRTCs CDE neither adopted nor has pending. Pure, so it is
/// unit-testable without a device.
pub(super) fn crtcs_to_disable<K: PartialEq + Copy>(active: &[K], adopted: &[K]) -> Vec<K> {
    active
        .iter()
        .copied()
        .filter(|c| !adopted.contains(c))
        .collect()
}

/// Logs one `startup:` line per CRTC (`only` empty: every CRTC) with whether it is active and the
/// framebuffer it scans out (plan 44 invariant 4: the evidence that the panel stayed lit).
pub(super) fn log_crtc_state<D: smithay::reexports::drm::control::Device>(
    dev: &D,
    at: &'static str,
    only: &[crtc::Handle],
) {
    let Ok(resources) = dev.resource_handles() else {
        return;
    };
    for &c in resources.crtcs() {
        if !only.is_empty() && !only.contains(&c) {
            continue;
        }
        if let Ok(info) = dev.get_crtc(c) {
            info!(
                elapsed_ms = crate::startup::elapsed_ms(),
                at,
                crtc = ?c,
                active = info.mode().is_some(),
                fb = ?info.framebuffer(),
                "startup: crtc state"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::crtcs_to_disable;

    #[test]
    fn cleanup_disables_only_unadopted_active_crtcs() {
        // 47 adopted, 48 pending, 49 left active by the previous client (or a skipped connector),
        // 50 inactive (not in `active`).
        assert_eq!(crtcs_to_disable(&[47u32, 48, 49], &[47, 48]), vec![49]);
    }

    #[test]
    fn cleanup_leaves_a_fully_adopted_device_alone() {
        assert!(crtcs_to_disable(&[47u32], &[47, 48]).is_empty());
        assert!(crtcs_to_disable::<u32>(&[], &[]).is_empty());
    }
}
