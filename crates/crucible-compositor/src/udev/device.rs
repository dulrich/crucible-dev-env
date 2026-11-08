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
use tracing::{debug, error, warn};

use crate::state::CrucibleState;

use super::output::{self, SurfaceData, UdevOutputId};
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
}

impl CrucibleState<UdevData> {
    /// Opens a DRM device, registers its VBlank source, and adds it to the GPU manager.
    pub(super) fn device_added(
        &mut self,
        node: DrmNode,
        path: &Path,
    ) -> Result<(), DeviceAddError> {
        let fd = self
            .backend_data
            .session
            .open(
                path,
                OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
            )
            .map_err(DeviceAddError::DeviceOpen)?;
        let fd = DrmDeviceFd::new(DeviceFd::from(fd));

        let (drm, notifier) =
            DrmDevice::new(fd.clone(), true).map_err(DeviceAddError::DrmDevice)?;
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

        let drm_output_manager = DrmOutputManager::new(
            drm,
            allocator,
            framebuffer_exporter,
            Some(gbm),
            SUPPORTED_FORMATS.iter().copied(),
            render_formats,
        );

        self.backend_data.backends.insert(
            node,
            BackendData {
                registration_token,
                drm_output_manager,
                drm_scanner: DrmScanner::new(),
                render_node,
                surfaces: HashMap::new(),
            },
        );

        self.device_changed(node);
        Ok(())
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
