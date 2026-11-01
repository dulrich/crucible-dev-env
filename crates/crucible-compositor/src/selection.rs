//! The clipboard bridge, primary selection and pointer drag-and-drop.
//!
//! Both halves of the Wayland <-> X11 selection bridge live here:
//! - Wayland -> X11: the [`SelectionHandler`] impl advertises a Wayland client's selection to
//!   XWayland through [`claim_x11_selection`].
//! - X11 -> Wayland: the `XwmHandler` selection methods in [`crate::xwayland`] forward to the
//!   `x11_*` functions here (a trait impl cannot be split across modules).
//!
//! The data-device, primary-selection and pointer DnD-grab handlers sit beside them.

use std::os::fd::OwnedFd;

use smithay::{
    input::{
        Seat,
        dnd::{DnDGrab, DndGrabHandler, DndTarget, GrabType, Source},
        pointer::Focus,
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Serial},
    wayland::selection::{
        SelectionHandler, SelectionSource, SelectionTarget,
        data_device::{
            DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, clear_data_device_selection,
            current_data_device_selection_userdata, request_data_device_client_selection,
            set_data_device_selection,
        },
        primary_selection::{
            PrimarySelectionHandler, PrimarySelectionState, clear_primary_selection,
            current_primary_selection_userdata, request_primary_client_selection,
            set_primary_selection,
        },
    },
    xwayland::X11Wm,
};
use tracing::{error, warn};

use crate::state::{Backend, CrucibleState, DndIcon};

/// Flush queued X11 requests (notably `SetSelectionOwner`) to the X server.
///
/// Smithay's `X11Wm::new_selection` queues `SetSelectionOwner` on the x11rb connection without
/// flushing, so the ownership change only reaches XWayland on the next X11 event and an X11
/// client reading right after a Wayland copy sees the stale owner. `get_randr_primary_output`
/// is a public round trip whose `.reply()` forces x11rb to flush everything queued before it.
/// It blocks on the X server, which never waits on the compositor to answer a RandR query;
/// that cost is accepted for a once-per-copy event.
pub(crate) fn flush_xwm(wm: &X11Wm) {
    if let Err(err) = wm.get_randr_primary_output() {
        tracing::debug!(?err, "XWM flush round trip failed");
    }
}

/// Claims X11 selection `ty` for `mime_types` (`None` clears it) and always flushes.
///
/// The single claim path for both the early claim at XWayland `Ready` and every later Wayland
/// copy: `X11Wm::new_selection` only queues `SetSelectionOwner`, so each claim must be followed by
/// [`flush_xwm`] for the ownership change to reach the X server immediately.
pub(crate) fn claim_x11_selection(
    xwm: &mut X11Wm,
    ty: SelectionTarget,
    mime_types: Option<Vec<String>>,
) {
    if let Err(err) = xwm.new_selection(ty, mime_types) {
        warn!(?err, ?ty, "Failed to set XWayland selection");
    }
    flush_xwm(xwm);
}

/// X11 client asked for the current Wayland selection: pump it to the provided fd.
pub(crate) fn x11_send_selection<B: Backend + 'static>(
    state: &CrucibleState<B>,
    selection: SelectionTarget,
    mime_type: String,
    fd: OwnedFd,
) {
    match selection {
        SelectionTarget::Clipboard => {
            if let Err(err) = request_data_device_client_selection(&state.seat, mime_type, fd) {
                error!(?err, "Failed to send Wayland clipboard to XWayland");
            }
        }
        SelectionTarget::Primary => {
            if let Err(err) = request_primary_client_selection(&state.seat, mime_type, fd) {
                error!(?err, "Failed to send Wayland primary selection to XWayland");
            }
        }
    }
}

/// An X11 client took ownership of a selection: advertise it to Wayland clients.
pub(crate) fn x11_new_selection<B: Backend + 'static>(
    state: &CrucibleState<B>,
    selection: SelectionTarget,
    mime_types: Vec<String>,
) {
    match selection {
        SelectionTarget::Clipboard => {
            set_data_device_selection(&state.display_handle, &state.seat, mime_types, ());
        }
        SelectionTarget::Primary => {
            set_primary_selection(&state.display_handle, &state.seat, mime_types, ());
        }
    }
}

/// An X11 client cleared a selection it owned: clear the matching Wayland selection.
pub(crate) fn x11_cleared_selection<B: Backend + 'static>(
    state: &CrucibleState<B>,
    selection: SelectionTarget,
) {
    match selection {
        SelectionTarget::Clipboard => {
            if current_data_device_selection_userdata(&state.seat).is_some() {
                clear_data_device_selection(&state.display_handle, &state.seat);
            }
        }
        SelectionTarget::Primary => {
            if current_primary_selection_userdata(&state.seat).is_some() {
                clear_primary_selection(&state.display_handle, &state.seat);
            }
        }
    }
}

// Wayland → X11 half of the clipboard bridge (the X11 → Wayland half is the `x11_*` functions above,
// reached from the `XwmHandler` impl in `crate::xwayland`). Without it a Wayland client's clipboard (e.g. a
// Firefox screenshot) is invisible to XWayland clients: `xclip -t TARGETS` finds no owner.
impl<B: Backend + 'static> SelectionHandler for CrucibleState<B> {
    type SelectionUserData = ();

    /// A Wayland client set (or cleared) a selection: advertise its mime types to XWayland.
    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        let mime_types = source.map(|source| source.mime_types());
        tracing::debug!(
            ?ty,
            ?mime_types,
            xwm = self.xwm.is_some(),
            "Wayland selection changed"
        );
        if let Some(xwm) = self.xwm.as_mut() {
            claim_x11_selection(xwm, ty, mime_types);
        }
    }

    /// A Wayland client is reading a selection an X11 client owns: pump it from XWayland.
    fn send_selection(
        &mut self,
        ty: SelectionTarget,
        mime_type: String,
        fd: std::os::fd::OwnedFd,
        _seat: Seat<Self>,
        _user_data: &(),
    ) {
        if let Some(xwm) = self.xwm.as_mut()
            && let Err(err) = xwm.send_selection(ty, mime_type, fd)
        {
            warn!(?err, ?ty, "Failed to send XWayland selection to Wayland");
        }
    }
}

impl<B: Backend + 'static> DataDeviceHandler for CrucibleState<B> {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

// Wayland client pointer drag-and-drop. A pointer request installs Smithay's `DnDGrab` (the grab
// drives focus and offers); the icon is stored for the scene assembler. A touch request is
// cancelled: CDE has no touch seat capability yet.
impl<B: Backend + 'static> WaylandDndGrabHandler for CrucibleState<B> {
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: Serial,
        type_: GrabType,
    ) {
        match type_ {
            GrabType::Pointer => {
                let Some(pointer) = seat.get_pointer() else {
                    source.cancel();
                    return;
                };
                let Some(start_data) = pointer.grab_start_data() else {
                    source.cancel();
                    return;
                };
                self.dnd_icon = icon.map(|surface| DndIcon {
                    surface,
                    offset: (0, 0).into(),
                });
                pointer.set_grab(
                    self,
                    DnDGrab::new_pointer(&self.display_handle, start_data, source, seat),
                    serial,
                    Focus::Keep,
                );
            }
            GrabType::Touch => source.cancel(),
        }
    }
}

impl<B: Backend + 'static> DndGrabHandler for CrucibleState<B> {
    fn dropped(
        &mut self,
        _target: Option<DndTarget<'_, Self>>,
        _validated: bool,
        _seat: Seat<Self>,
        _location: Point<f64, Logical>,
    ) {
        self.dnd_icon = None;
    }

    fn cancelled(&mut self, _seat: Seat<Self>, _location: Point<f64, Logical>) {
        self.dnd_icon = None;
    }
}

impl<B: Backend + 'static> PrimarySelectionHandler for CrucibleState<B> {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
    }
}
