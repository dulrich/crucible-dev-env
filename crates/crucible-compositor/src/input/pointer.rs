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

use tracing::warn;

use crate::element::WindowElement;
use crate::focus::{KeyboardFocusTarget, PointerFocusTarget};
use crate::state::{Backend, CrucibleState, HoverTarget, hover_entered};

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
        self.focus_follows_pointer();
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
        self.focus_follows_pointer();
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
    /// Used on pointer press (click-to-focus). A **managed** window (one with a model
    /// [`WindowId`](crucible_layout::WindowId)) is focused through the model
    /// ([`crucible_layout::World::focus_window`] via `with_model`), so the model's output,
    /// workspace, pane and stack position follow the click and splits/new windows act there.
    /// Clicking empty space routes model focus to the output under the pointer. An unmanaged
    /// (override-redirect) element only gets keyboard focus; it is never enrolled in the model.
    fn update_keyboard_focus_under_pointer(&mut self, _serial: Serial) {
        let location = self.pointer.current_location();
        let element_under = self.space.element_under(location).map(|(w, _)| w.clone());
        match element_under {
            Some(window) => {
                if let Some(id) = window.window_id() {
                    if let Err(err) = self.with_model(|world| world.focus_window(id)) {
                        warn!(%err, ?id, "click on a window the model does not know");
                    }
                } else {
                    // Unmanaged: today's behaviour — route the model output, keyboard-only focus.
                    self.focus_output_under(location);
                }
                // Raise + keyboard focus. Idempotent when `apply_focus` already did it; required
                // for unmanaged elements and when the model focus did not change.
                self.focus_window(window);
            }
            None => self.focus_output_under(location),
        }
    }

    /// Routes the model's focused output to the monitor under `location` (no-op if unchanged).
    fn focus_output_under(&mut self, location: Point<f64, Logical>) {
        let output_under = self.space.output_under(location).next().cloned();
        if let Some(output) = output_under {
            self.focus_output(&output);
        }
    }

    /// Focus-follows-mouse: moves focus when the pointer *enters* a new hover target.
    ///
    /// The target is the managed window under the pointer, or the output when over empty space;
    /// unmanaged elements leave the tracker unchanged. Focus moves only when the target differs
    /// from the last one seen ([`hover_entered`]), so keyboard-driven focus (a split, `Super+H`…)
    /// is not reverted by a pointer twitch over the same window. While a pointer grab is active
    /// (button held, drag) the tracker still updates but no focus moves, so releasing a drag over
    /// a new window does not count as an enter. Does nothing when the option is off.
    fn focus_follows_pointer(&mut self) {
        if !self.focus_follows_mouse {
            return;
        }
        let location = self.pointer.current_location();
        let element_under = self.space.element_under(location).map(|(w, _)| w.clone());
        let current = match &element_under {
            Some(window) => window.window_id().map(HoverTarget::Window),
            None => self
                .space
                .output_under(location)
                .next()
                .cloned()
                .map(HoverTarget::Output),
        };
        let mut stored = self.hover_target.take();
        let entered = hover_entered(&mut stored, current);
        self.hover_target = stored;
        if !entered || self.pointer.is_grabbed() {
            return;
        }
        match (self.hover_target.clone(), element_under) {
            (Some(HoverTarget::Window(id)), Some(window)) => {
                if let Err(err) = self.with_model(|world| world.focus_window(id)) {
                    warn!(%err, ?id, "hover over a window the model does not know");
                    return;
                }
                // If the model already pointed at `id` (no projected focus change), `apply_focus`
                // did not run; sync keyboard focus to the entered window.
                let now = match self.keyboard_focus() {
                    Some(KeyboardFocusTarget::Window(w)) => w.window_id(),
                    _ => None,
                };
                if now != Some(id) {
                    self.focus_window(window);
                }
            }
            // `focus_output` is a no-op when that output is already focused.
            (Some(HoverTarget::Output(output)), _) => self.focus_output(&output),
            _ => {}
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
