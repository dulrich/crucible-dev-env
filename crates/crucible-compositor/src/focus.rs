//! Keyboard/pointer focus targets spanning Wayland and XWayland surfaces.
//!
//! Two enums let the same seat focus every surface kind CDE maps:
//!
//! - [`KeyboardFocusTarget`] — a window ([`WindowElement`], Wayland *or* XWayland), a
//!   [`LayerSurface`], or a [`PopupKind`]. This is the seat's `KeyboardFocus`.
//! - [`PointerFocusTarget`] — a bare [`WlSurface`] or an XWayland
//!   [`X11Surface`](smithay::xwayland::X11Surface). This is the seat's `PointerFocus`/`TouchFocus`.
//!
//! Both enums forward the relevant Smithay focus trait
//! ([`KeyboardTarget`]/[`PointerTarget`]/[`TouchTarget`]/[`WaylandFocus`]) to their inner kind, so
//! the seat drives Wayland and X11 focus uniformly. Driving keyboard focus to a
//! [`KeyboardFocusTarget::Window`] backed by an X11 surface activates the X11 window (the
//! [`KeyboardTarget`] impl on `X11Surface` issues the X11 `set_input_focus`/take-focus); CDE
//! additionally raises it via [`X11Wm::raise_window`](smithay::xwayland::X11Wm) on the input path.
//!
//! CDE has no server-side decorations, so anvil's `SSD` pointer arm is intentionally omitted.
//! Wayland-client pointer drag-and-drop is driven (see [`CrucibleOfferData`] and the grab in
//! [`crate::state`]); touch and XWayland-initiated DnD are not.

use std::{borrow::Cow, sync::Arc};

use smithay::{
    backend::input::KeyState,
    desktop::{LayerSurface, PopupKind, WindowSurface},
    input::{
        Seat,
        dnd::{DndFocus, OfferData, Source},
        keyboard::{KeyboardTarget, KeysymHandle, ModifiersState},
        pointer::{
            AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent,
            GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
            GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent, MotionEvent,
            PointerTarget, RelativeMotionEvent,
        },
        touch::TouchTarget,
    },
    reexports::wayland_server::{
        DisplayHandle, backend::ObjectId, protocol::wl_surface::WlSurface,
    },
    utils::{IsAlive, Logical, Point, Serial},
    wayland::{seat::WaylandFocus, selection::data_device::WlOfferData},
    xwayland::{X11Surface, xwm::XwmOfferData},
};

use crate::{
    element::WindowElement,
    state::{Backend, CrucibleState},
};

/// What the seat's keyboard is focused on.
///
/// Spans a mapped window ([`WindowElement`], Wayland or XWayland), a layer-shell surface, or a
/// popup. Forwards [`KeyboardTarget`]/[`WaylandFocus`] to the inner kind.
#[derive(Debug, Clone, PartialEq)]
// The `Window` variant is larger than the surface-handle variants; boxing it would complicate the
// trait forwarding for no real benefit (focus targets are short-lived and rarely allocated).
#[allow(clippy::large_enum_variant)]
pub enum KeyboardFocusTarget {
    /// A mapped toplevel window (Wayland toplevel or XWayland window).
    Window(WindowElement),
    /// A wlr-layer-shell surface (e.g. a future status bar).
    LayerSurface(LayerSurface),
    /// An xdg or input-method popup.
    Popup(PopupKind),
    /// An `ext-session-lock-v1` lock surface (the locker; see [`crate::lock`]).
    LockSurface(WlSurface),
}

/// What the seat's pointer (and touch) is focused on.
///
/// Either a bare Wayland [`WlSurface`] or an XWayland [`X11Surface`]; both implement Smithay's
/// pointer/touch target traits, which this enum forwards to.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum PointerFocusTarget {
    /// A native Wayland surface (toplevel, subsurface, popup, layer, or cursor surface).
    WlSurface(WlSurface),
    /// An XWayland X11 surface.
    X11Surface(X11Surface),
}

impl IsAlive for KeyboardFocusTarget {
    #[inline]
    fn alive(&self) -> bool {
        match self {
            KeyboardFocusTarget::Window(w) => w.alive(),
            KeyboardFocusTarget::LayerSurface(l) => l.alive(),
            KeyboardFocusTarget::Popup(p) => p.alive(),
            KeyboardFocusTarget::LockSurface(s) => s.alive(),
        }
    }
}

impl IsAlive for PointerFocusTarget {
    #[inline]
    fn alive(&self) -> bool {
        match self {
            PointerFocusTarget::WlSurface(w) => w.alive(),
            PointerFocusTarget::X11Surface(w) => w.alive(),
        }
    }
}

impl KeyboardFocusTarget {
    /// Returns the inner [`KeyboardTarget`] the seat should drive for this focus.
    fn inner_keyboard_target<B: Backend + 'static>(&self) -> &dyn KeyboardTarget<CrucibleState<B>> {
        match self {
            Self::Window(w) => match w.0.underlying_surface() {
                WindowSurface::Wayland(w) => w.wl_surface(),
                WindowSurface::X11(s) => s,
            },
            Self::LayerSurface(l) => l.wl_surface(),
            Self::Popup(p) => p.wl_surface(),
            Self::LockSurface(s) => s,
        }
    }
}

impl PointerFocusTarget {
    /// Returns the inner [`PointerTarget`] the seat should drive for this focus.
    fn inner_pointer_target<B: Backend + 'static>(&self) -> &dyn PointerTarget<CrucibleState<B>> {
        match self {
            Self::WlSurface(w) => w,
            Self::X11Surface(w) => w,
        }
    }

    /// Returns the inner [`TouchTarget`] the seat should drive for this focus.
    fn inner_touch_target<B: Backend + 'static>(&self) -> &dyn TouchTarget<CrucibleState<B>> {
        match self {
            Self::WlSurface(w) => w,
            Self::X11Surface(w) => w,
        }
    }
}

impl WaylandFocus for KeyboardFocusTarget {
    #[inline]
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            KeyboardFocusTarget::Window(w) => w.wl_surface(),
            KeyboardFocusTarget::LayerSurface(l) => Some(Cow::Borrowed(l.wl_surface())),
            KeyboardFocusTarget::Popup(p) => Some(Cow::Borrowed(p.wl_surface())),
            KeyboardFocusTarget::LockSurface(s) => Some(Cow::Borrowed(s)),
        }
    }
}

impl WaylandFocus for PointerFocusTarget {
    #[inline]
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            PointerFocusTarget::WlSurface(w) => w.wl_surface(),
            PointerFocusTarget::X11Surface(w) => w.wl_surface().map(Cow::Owned),
        }
    }
    #[inline]
    fn same_client_as(&self, object_id: &ObjectId) -> bool {
        match self {
            PointerFocusTarget::WlSurface(w) => w.same_client_as(object_id),
            PointerFocusTarget::X11Surface(w) => w.same_client_as(object_id),
        }
    }
}

impl<B: Backend + 'static> KeyboardTarget<CrucibleState<B>> for KeyboardFocusTarget {
    fn enter(
        &self,
        seat: &Seat<CrucibleState<B>>,
        data: &mut CrucibleState<B>,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        self.inner_keyboard_target().enter(seat, data, keys, serial);
    }
    fn leave(&self, seat: &Seat<CrucibleState<B>>, data: &mut CrucibleState<B>, serial: Serial) {
        self.inner_keyboard_target().leave(seat, data, serial);
    }
    fn key(
        &self,
        seat: &Seat<CrucibleState<B>>,
        data: &mut CrucibleState<B>,
        key: KeysymHandle<'_>,
        state: KeyState,
        serial: Serial,
        time: u32,
    ) {
        self.inner_keyboard_target()
            .key(seat, data, key, state, serial, time);
    }
    fn modifiers(
        &self,
        seat: &Seat<CrucibleState<B>>,
        data: &mut CrucibleState<B>,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        self.inner_keyboard_target()
            .modifiers(seat, data, modifiers, serial);
    }
}

impl<B: Backend + 'static> PointerTarget<CrucibleState<B>> for PointerFocusTarget {
    fn enter(
        &self,
        seat: &Seat<CrucibleState<B>>,
        data: &mut CrucibleState<B>,
        event: &MotionEvent,
    ) {
        self.inner_pointer_target().enter(seat, data, event);
    }
    fn motion(
        &self,
        seat: &Seat<CrucibleState<B>>,
        data: &mut CrucibleState<B>,
        event: &MotionEvent,
    ) {
        self.inner_pointer_target().motion(seat, data, event);
    }
    fn relative_motion(
        &self,
        seat: &Seat<CrucibleState<B>>,
        data: &mut CrucibleState<B>,
        event: &RelativeMotionEvent,
    ) {
        self.inner_pointer_target()
            .relative_motion(seat, data, event);
    }
    fn button(
        &self,
        seat: &Seat<CrucibleState<B>>,
        data: &mut CrucibleState<B>,
        event: &ButtonEvent,
    ) {
        self.inner_pointer_target().button(seat, data, event);
    }
    fn axis(&self, seat: &Seat<CrucibleState<B>>, data: &mut CrucibleState<B>, frame: AxisFrame) {
        self.inner_pointer_target().axis(seat, data, frame);
    }
    fn frame(&self, seat: &Seat<CrucibleState<B>>, data: &mut CrucibleState<B>) {
        self.inner_pointer_target().frame(seat, data);
    }
    fn leave(
        &self,
        seat: &Seat<CrucibleState<B>>,
        data: &mut CrucibleState<B>,
        serial: Serial,
        time: u32,
    ) {
        self.inner_pointer_target().leave(seat, data, serial, time);
    }
    fn gesture_swipe_begin(
        &self,
        seat: &Seat<CrucibleState<B>>,
        data: &mut CrucibleState<B>,
        event: &GestureSwipeBeginEvent,
    ) {
        self.inner_pointer_target()
            .gesture_swipe_begin(seat, data, event);
    }
    fn gesture_swipe_update(
        &self,
        seat: &Seat<CrucibleState<B>>,
        data: &mut CrucibleState<B>,
        event: &GestureSwipeUpdateEvent,
    ) {
        self.inner_pointer_target()
            .gesture_swipe_update(seat, data, event);
    }
    fn gesture_swipe_end(
        &self,
        seat: &Seat<CrucibleState<B>>,
        data: &mut CrucibleState<B>,
        event: &GestureSwipeEndEvent,
    ) {
        self.inner_pointer_target()
            .gesture_swipe_end(seat, data, event);
    }
    fn gesture_pinch_begin(
        &self,
        seat: &Seat<CrucibleState<B>>,
        data: &mut CrucibleState<B>,
        event: &GesturePinchBeginEvent,
    ) {
        self.inner_pointer_target()
            .gesture_pinch_begin(seat, data, event);
    }
    fn gesture_pinch_update(
        &self,
        seat: &Seat<CrucibleState<B>>,
        data: &mut CrucibleState<B>,
        event: &GesturePinchUpdateEvent,
    ) {
        self.inner_pointer_target()
            .gesture_pinch_update(seat, data, event);
    }
    fn gesture_pinch_end(
        &self,
        seat: &Seat<CrucibleState<B>>,
        data: &mut CrucibleState<B>,
        event: &GesturePinchEndEvent,
    ) {
        self.inner_pointer_target()
            .gesture_pinch_end(seat, data, event);
    }
    fn gesture_hold_begin(
        &self,
        seat: &Seat<CrucibleState<B>>,
        data: &mut CrucibleState<B>,
        event: &GestureHoldBeginEvent,
    ) {
        self.inner_pointer_target()
            .gesture_hold_begin(seat, data, event);
    }
    fn gesture_hold_end(
        &self,
        seat: &Seat<CrucibleState<B>>,
        data: &mut CrucibleState<B>,
        event: &GestureHoldEndEvent,
    ) {
        self.inner_pointer_target()
            .gesture_hold_end(seat, data, event);
    }
}

impl<B: Backend + 'static> TouchTarget<CrucibleState<B>> for PointerFocusTarget {
    fn down(
        &self,
        seat: &Seat<CrucibleState<B>>,
        data: &mut CrucibleState<B>,
        event: &smithay::input::touch::DownEvent,
        seq: Serial,
    ) {
        self.inner_touch_target().down(seat, data, event, seq);
    }
    fn up(
        &self,
        seat: &Seat<CrucibleState<B>>,
        data: &mut CrucibleState<B>,
        event: &smithay::input::touch::UpEvent,
        seq: Serial,
    ) {
        self.inner_touch_target().up(seat, data, event, seq);
    }
    fn motion(
        &self,
        seat: &Seat<CrucibleState<B>>,
        data: &mut CrucibleState<B>,
        event: &smithay::input::touch::MotionEvent,
        seq: Serial,
    ) {
        self.inner_touch_target().motion(seat, data, event, seq);
    }
    fn frame(&self, seat: &Seat<CrucibleState<B>>, data: &mut CrucibleState<B>, seq: Serial) {
        self.inner_touch_target().frame(seat, data, seq);
    }
    fn cancel(&self, seat: &Seat<CrucibleState<B>>, data: &mut CrucibleState<B>, seq: Serial) {
        self.inner_touch_target().cancel(seat, data, seq);
    }
    fn shape(
        &self,
        seat: &Seat<CrucibleState<B>>,
        data: &mut CrucibleState<B>,
        event: &smithay::input::touch::ShapeEvent,
        seq: Serial,
    ) {
        self.inner_touch_target().shape(seat, data, event, seq);
    }
    fn orientation(
        &self,
        seat: &Seat<CrucibleState<B>>,
        data: &mut CrucibleState<B>,
        event: &smithay::input::touch::OrientationEvent,
        seq: Serial,
    ) {
        self.inner_touch_target()
            .orientation(seat, data, event, seq);
    }
}

/// Per-target drag-and-drop offer data, spanning the Wayland and XWayland DnD implementations.
///
/// [`X11Wm::start_wm`](smithay::xwayland::X11Wm) requires the seat's `PointerFocus` to implement
/// [`DndFocus`]; this enum lets a single [`PointerFocusTarget`] satisfy that by delegating to either
/// Smithay's Wayland or X11 offer data. Native Wayland pointer drags run through the Wayland arm
/// (`DnDGrab` installed by `dnd_requested` in [`crate::state`]); the X11 arm only forwards.
pub enum CrucibleOfferData<S: Source> {
    /// Offer data for a native Wayland DnD target.
    Wayland(WlOfferData<S>),
    /// Offer data for an XWayland DnD target.
    X11(XwmOfferData<S>),
}

impl<S: Source> OfferData for CrucibleOfferData<S> {
    fn disable(&self) {
        match self {
            CrucibleOfferData::Wayland(data) => data.disable(),
            CrucibleOfferData::X11(data) => data.disable(),
        }
    }

    fn drop(&self) {
        match self {
            CrucibleOfferData::Wayland(data) => data.drop(),
            CrucibleOfferData::X11(data) => data.drop(),
        }
    }

    fn validated(&self) -> bool {
        match self {
            CrucibleOfferData::Wayland(data) => data.validated(),
            CrucibleOfferData::X11(data) => data.validated(),
        }
    }
}

impl<B: Backend + 'static> DndFocus<CrucibleState<B>> for PointerFocusTarget {
    type OfferData<S>
        = CrucibleOfferData<S>
    where
        S: Source;

    fn enter<S: Source>(
        &self,
        data: &mut CrucibleState<B>,
        dh: &DisplayHandle,
        source: Arc<S>,
        seat: &Seat<CrucibleState<B>>,
        location: Point<f64, Logical>,
        serial: &Serial,
    ) -> Option<CrucibleOfferData<S>> {
        match self {
            PointerFocusTarget::WlSurface(surface) => {
                DndFocus::enter(surface, data, dh, source, seat, location, serial)
                    .map(CrucibleOfferData::Wayland)
            }
            PointerFocusTarget::X11Surface(surface) => {
                DndFocus::enter(surface, data, dh, source, seat, location, serial)
                    .map(CrucibleOfferData::X11)
            }
        }
    }

    fn motion<S: Source>(
        &self,
        data: &mut CrucibleState<B>,
        offer: Option<&mut CrucibleOfferData<S>>,
        seat: &Seat<CrucibleState<B>>,
        location: Point<f64, Logical>,
        time: u32,
    ) {
        match self {
            PointerFocusTarget::WlSurface(surface) => {
                let offer = match offer {
                    Some(CrucibleOfferData::Wayland(offer)) => Some(offer),
                    None => None,
                    _ => return,
                };
                DndFocus::motion(surface, data, offer, seat, location, time);
            }
            PointerFocusTarget::X11Surface(surface) => {
                let offer = match offer {
                    Some(CrucibleOfferData::X11(offer)) => Some(offer),
                    None => None,
                    _ => return,
                };
                DndFocus::motion(surface, data, offer, seat, location, time);
            }
        }
    }

    fn leave<S: Source>(
        &self,
        data: &mut CrucibleState<B>,
        offer: Option<&mut CrucibleOfferData<S>>,
        seat: &Seat<CrucibleState<B>>,
    ) {
        match self {
            PointerFocusTarget::WlSurface(surface) => {
                let offer = match offer {
                    Some(CrucibleOfferData::Wayland(offer)) => Some(offer),
                    None => None,
                    _ => return,
                };
                DndFocus::leave(surface, data, offer, seat);
            }
            PointerFocusTarget::X11Surface(surface) => {
                let offer = match offer {
                    Some(CrucibleOfferData::X11(offer)) => Some(offer),
                    None => None,
                    _ => return,
                };
                DndFocus::leave(surface, data, offer, seat);
            }
        }
    }

    fn drop<S: Source>(
        &self,
        data: &mut CrucibleState<B>,
        offer: Option<&mut CrucibleOfferData<S>>,
        seat: &Seat<CrucibleState<B>>,
    ) {
        match self {
            PointerFocusTarget::WlSurface(surface) => {
                let offer = match offer {
                    Some(CrucibleOfferData::Wayland(offer)) => Some(offer),
                    None => None,
                    _ => return,
                };
                DndFocus::drop(surface, data, offer, seat);
            }
            PointerFocusTarget::X11Surface(surface) => {
                let offer = match offer {
                    Some(CrucibleOfferData::X11(offer)) => Some(offer),
                    None => None,
                    _ => return,
                };
                DndFocus::drop(surface, data, offer, seat);
            }
        }
    }
}

// ---- Conversions ---------------------------------------------------------------------------

impl From<WlSurface> for PointerFocusTarget {
    #[inline]
    fn from(value: WlSurface) -> Self {
        PointerFocusTarget::WlSurface(value)
    }
}

impl From<&WlSurface> for PointerFocusTarget {
    #[inline]
    fn from(value: &WlSurface) -> Self {
        PointerFocusTarget::WlSurface(value.clone())
    }
}

impl From<X11Surface> for PointerFocusTarget {
    #[inline]
    fn from(value: X11Surface) -> Self {
        PointerFocusTarget::X11Surface(value)
    }
}

impl From<PointerFocusTarget> for WlSurface {
    #[inline]
    fn from(target: PointerFocusTarget) -> Self {
        target.wl_surface().unwrap().into_owned()
    }
}

impl From<WindowElement> for KeyboardFocusTarget {
    #[inline]
    fn from(w: WindowElement) -> Self {
        KeyboardFocusTarget::Window(w)
    }
}

impl From<LayerSurface> for KeyboardFocusTarget {
    #[inline]
    fn from(l: LayerSurface) -> Self {
        KeyboardFocusTarget::LayerSurface(l)
    }
}

impl From<PopupKind> for KeyboardFocusTarget {
    #[inline]
    fn from(p: PopupKind) -> Self {
        KeyboardFocusTarget::Popup(p)
    }
}

impl From<KeyboardFocusTarget> for PointerFocusTarget {
    #[inline]
    fn from(value: KeyboardFocusTarget) -> Self {
        match value {
            KeyboardFocusTarget::Window(w) => match w.0.underlying_surface() {
                WindowSurface::Wayland(w) => PointerFocusTarget::WlSurface(w.wl_surface().clone()),
                WindowSurface::X11(s) => PointerFocusTarget::X11Surface(s.clone()),
            },
            KeyboardFocusTarget::LayerSurface(surface) => {
                PointerFocusTarget::WlSurface(surface.wl_surface().clone())
            }
            KeyboardFocusTarget::Popup(popup) => {
                PointerFocusTarget::WlSurface(popup.wl_surface().clone())
            }
            KeyboardFocusTarget::LockSurface(s) => PointerFocusTarget::WlSurface(s),
        }
    }
}
