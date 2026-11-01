//! `zwp_linux_dmabuf_v1` plumbing shared by the winit and udev backends.
//!
//! [`create_global`] advertises the global (v5 with default feedback when the backend has it,
//! otherwise a plain v3 format list); [`notify_import`] answers a client's import request from the
//! backend's import result. Each backend keeps its own renderer choice and fallback policy.

use smithay::{
    backend::allocator::{Format, dmabuf::Dmabuf},
    reexports::{
        wayland_protocols::wp::linux_dmabuf::zv1::server::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        wayland_protocols::wp::linux_dmabuf::zv1::server::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        wayland_server::{Dispatch, DisplayHandle, GlobalDispatch, protocol::wl_buffer::WlBuffer},
    },
    wayland::{
        buffer::BufferHandler,
        dmabuf::{
            DmabufFeedback, DmabufGlobal, DmabufGlobalData, DmabufHandler, DmabufParamsData,
            DmabufState, ImportNotifier,
        },
    },
};

/// Creates the dmabuf state and global for state type `D`.
///
/// With `default_feedback` the global is v5 (`create_global_with_default_feedback`, which takes
/// its formats from the feedback); without it, v3 over `formats`.
pub fn create_global<D>(
    display: &DisplayHandle,
    formats: impl IntoIterator<Item = Format>,
    default_feedback: Option<&DmabufFeedback>,
) -> (DmabufState, DmabufGlobal)
where
    D: GlobalDispatch<ZwpLinuxDmabufV1, DmabufGlobalData> + BufferHandler + DmabufHandler + 'static,
{
    let mut state = DmabufState::new();
    let global = match default_feedback {
        Some(feedback) => state.create_global_with_default_feedback::<D>(display, feedback),
        None => state.create_global::<D>(display, formats),
    };
    (state, global)
}

/// Answers a dmabuf import: on `imported`, runs `on_success` (e.g. recording the importing GPU's
/// node on `dmabuf`) **before** `notifier.successful`; otherwise `notifier.failed()`.
pub fn notify_import<D>(
    imported: bool,
    dmabuf: &Dmabuf,
    notifier: ImportNotifier,
    on_success: impl FnOnce(&Dmabuf),
) where
    D: Dispatch<ZwpLinuxBufferParamsV1, DmabufParamsData>
        + Dispatch<WlBuffer, Dmabuf>
        + BufferHandler
        + DmabufHandler
        + 'static,
{
    if imported {
        on_success(dmabuf);
        let _ = notifier.successful::<D>();
    } else {
        notifier.failed();
    }
}
