//! Pointer I/O: absolute/relative motion, buttons, scroll axes, and click-to-focus.
//!
//! Mirrors [`super::keyboard`]'s per-event seat handlers but for the pointer: winit delivers
//! **absolute** motion ([`Self::process_pointer_motion_absolute`]) while real input devices (via
//! libinput) deliver **relative** motion ([`Self::process_pointer_motion_relative`]); both paths
//! share button/axis handling and click-to-focus.

use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, Event, InputBackend, PointerAxisEvent,
    PointerButtonEvent, PointerMotionEvent,
};
use smithay::desktop::WindowSurfaceType;
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent};
use smithay::reexports::wayland_server::protocol::wl_pointer;
use smithay::utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Serial};

use crate::element::WindowElement;
use crate::focus::{KeyboardFocusTarget, PointerFocusTarget};
use crate::state::{Backend, CrucibleState};

impl<B: Backend + 'static> CrucibleState<B> {
    /// Maps absolute winit pointer motion into the output and delivers it to the seat.
    ///
    /// Transforms the device position into the output's logical geometry, updates the pointer
    /// location and pointer focus (the surface under the cursor, from the
    /// [`Space`](smithay::desktop::Space)), so the focused client receives enter/leave/motion.
    /// No-op if no output is mapped yet.
    pub(super) fn process_pointer_motion_absolute<I: InputBackend>(
        &mut self,
        evt: I::PointerMotionAbsoluteEvent,
    ) {
        let Some(output) = self.space.outputs().next().cloned() else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };

        let pos = evt.position_transformed(output_geo.size) + output_geo.loc.to_f64();
        let serial = SERIAL_COUNTER.next_serial();
        let time = evt.time_msec();

        let under = self.surface_under(pos);
        let pointer = self.pointer.clone();
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: pos,
                serial,
                time,
            },
        );
        pointer.frame(self);
    }

    /// Maps **relative** pointer motion (real input devices, via libinput) into the seat.
    ///
    /// Unlike winit's absolute motion, libinput delivers a motion *delta*. The new pointer location
    /// is the previous location plus the delta, clamped to the union of all mapped outputs so the
    /// cursor cannot leave the visible area (it can cross freely between outputs laid left-to-right).
    /// The seat receives both the relative-motion event (for clients that requested it via the
    /// relative-pointer protocol, gated by [`Backend::HAS_RELATIVE_MOTION`]) and the resulting
    /// absolute motion. No-op until at least one output is mapped.
    pub(super) fn process_pointer_motion_relative<I: InputBackend>(
        &mut self,
        evt: I::PointerMotionEvent,
    ) {
        if self.space.outputs().next().is_none() {
            return;
        }

        let serial = SERIAL_COUNTER.next_serial();
        let pointer = self.pointer.clone();

        let mut location = pointer.current_location() + evt.delta();
        location = self.clamp_coords(location);

        let under = self.surface_under(location);
        pointer.relative_motion(
            self,
            under.clone(),
            &RelativeMotionEvent {
                delta: evt.delta(),
                delta_unaccel: evt.delta_unaccel(),
                utime: evt.time(),
            },
        );
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location,
                serial,
                time: evt.time_msec(),
            },
        );
        pointer.frame(self);
    }

    /// Clamps a pointer location to the bounding box of all mapped outputs.
    ///
    /// With outputs laid out left-to-right (see the udev backend), the union is a single horizontal
    /// strip, so this keeps the cursor on-screen while letting it move between adjacent outputs.
    /// Returns `pos` unchanged when no output is mapped.
    fn clamp_coords(&self, pos: Point<f64, Logical>) -> Point<f64, Logical> {
        let mut bbox: Option<Rectangle<i32, Logical>> = None;
        for output in self.space.outputs() {
            if let Some(geo) = self.space.output_geometry(output) {
                bbox = Some(match bbox {
                    Some(acc) => acc.merge(geo),
                    None => geo,
                });
            }
        }
        let Some(bbox) = bbox else {
            return pos;
        };
        let bbox = bbox.to_f64();
        let x = pos.x.clamp(bbox.loc.x, bbox.loc.x + bbox.size.w - 1.0);
        let y = pos.y.clamp(bbox.loc.y, bbox.loc.y + bbox.size.h - 1.0);
        (x, y).into()
    }

    /// Delivers a pointer button to the seat, moving keyboard focus on press.
    pub(super) fn process_pointer_button<I: InputBackend>(&mut self, evt: I::PointerButtonEvent) {
        let serial = SERIAL_COUNTER.next_serial();
        let button = evt.button_code();
        let state = wl_pointer::ButtonState::from(evt.state());

        if wl_pointer::ButtonState::Pressed == state {
            self.update_keyboard_focus_under_pointer(serial);
        }

        let pointer = self.pointer.clone();
        pointer.button(
            self,
            &ButtonEvent {
                button,
                state: state.try_into().unwrap(),
                serial,
                time: evt.time_msec(),
            },
        );
        pointer.frame(self);
    }

    /// Delivers a scroll axis frame to the seat.
    pub(super) fn process_pointer_axis<I: InputBackend>(&mut self, evt: I::PointerAxisEvent) {
        let horizontal = evt
            .amount(Axis::Horizontal)
            .unwrap_or_else(|| evt.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.0);
        let vertical = evt
            .amount(Axis::Vertical)
            .unwrap_or_else(|| evt.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.0);
        let horizontal_v120 = evt.amount_v120(Axis::Horizontal);
        let vertical_v120 = evt.amount_v120(Axis::Vertical);

        let mut frame = AxisFrame::new(evt.time_msec()).source(evt.source());
        if horizontal != 0.0 {
            frame = frame
                .relative_direction(Axis::Horizontal, evt.relative_direction(Axis::Horizontal));
            frame = frame.value(Axis::Horizontal, horizontal);
            if let Some(v120) = horizontal_v120 {
                frame = frame.v120(Axis::Horizontal, v120 as i32);
            }
        }
        if vertical != 0.0 {
            frame =
                frame.relative_direction(Axis::Vertical, evt.relative_direction(Axis::Vertical));
            frame = frame.value(Axis::Vertical, vertical);
            if let Some(v120) = vertical_v120 {
                frame = frame.v120(Axis::Vertical, v120 as i32);
            }
        }
        if evt.source() == AxisSource::Finger {
            if evt.amount(Axis::Horizontal) == Some(0.0) {
                frame = frame.stop(Axis::Horizontal);
            }
            if evt.amount(Axis::Vertical) == Some(0.0) {
                frame = frame.stop(Axis::Vertical);
            }
        }

        let pointer = self.pointer.clone();
        pointer.axis(self, frame);
        pointer.frame(self);
    }

    /// Returns the [`PointerFocusTarget`] (and its top-left in global logical coords) under `pos`,
    /// if any.
    ///
    /// Resolution walks the [`Space`](smithay::desktop::Space)'s mapped windows and dispatches over
    /// the window kind so X11 windows resolve to a [`PointerFocusTarget::X11Surface`] and Wayland
    /// windows to a [`PointerFocusTarget::WlSurface`] (see [`WindowElement::surface_under`]).
    fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(PointerFocusTarget, Point<f64, Logical>)> {
        self.space
            .element_under(pos)
            .and_then(|(window, location)| {
                window
                    .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(target, surface_loc)| (target, (location + surface_loc).to_f64()))
            })
    }

    /// Moves keyboard focus to the window under the current pointer location, if any.
    ///
    /// Used on pointer press (click-to-focus). Raises the window in the
    /// [`Space`](smithay::desktop::Space) and assigns the seat's keyboard focus to it (activating
    /// X11 windows through the focus path).
    fn update_keyboard_focus_under_pointer(&mut self, _serial: Serial) {
        let location = self.pointer.current_location();
        // Route the model's focused output to the monitor under the pointer so new windows and the
        // (WS-4) workspace verbs act on the monitor the user is interacting with. Click is a clean,
        // unambiguous focus-change signal; pure pointer-enter routing can follow in WS-4 if desired.
        let output_under = self.space.output_under(location).next().cloned();
        if let Some(output) = output_under {
            self.focus_output(&output);
        }
        if let Some((window, _)) = self
            .space
            .element_under(location)
            .map(|(w, p)| (w.clone(), p))
        {
            self.focus_window(window);
        }
    }

    /// Raises `window` in the [`Space`](smithay::desktop::Space) and gives it keyboard focus.
    ///
    /// For an XWayland window this both activates it (the seat drives the X11 `set_input_focus`
    /// through the [`KeyboardFocusTarget::Window`] target) and raises it in the X11 stacking order
    /// via the running [`X11Wm`](smithay::xwayland::X11Wm).
    fn focus_window(&mut self, window: WindowElement) {
        self.space.raise_element(&window, true);
        if let Some(surface) = window.x11_surface() {
            if let Some(xwm) = self.xwm.as_mut() {
                let _ = xwm.raise_window(surface);
            }
        }
        self.set_keyboard_focus(Some(KeyboardFocusTarget::Window(window)));
    }
}
