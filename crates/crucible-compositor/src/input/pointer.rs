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
use smithay::utils::{Logical, Point, Rectangle, SERIAL_COUNTER};

use tracing::{info, warn};

use super::modal::{InputMode, hover_moves_focus, hover_tracks, press_route};
use crate::element::WindowElement;
use crate::focus::{KeyboardFocusTarget, PointerFocusTarget};
use crate::state::{Backend, CrucibleState, HoverTarget, hover_entered, hover_pane_key};

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

        let mode = self.input_mode();
        let under = self.surface_under(&mode, pos);
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
        self.focus_follows_pointer(&mode);
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

        let mode = self.input_mode();
        let under = self.surface_under(&mode, location);
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
        self.focus_follows_pointer(&mode);
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
            let mode = self.input_mode();
            let element_under = self
                .space
                .element_under(self.pointer.current_location())
                .map(|(w, _)| w.clone());
            let route = press_route(&mode, element_under.as_ref());
            if route.try_header && self.consume_header_press(button) {
                return;
            }
            if route.close_overlay {
                // The slot is cleared later by `unmanaged_closed`, which stays its single owner.
                self.close_mapped_overlay();
            }
            if route.click_focus {
                self.update_keyboard_focus_under_pointer(element_under);
            }
        } else if self.header.consumed.consume_release(button) {
            // The paired release of a press the header strip consumed: never forward a release
            // whose press the client did not see (audit E8).
            return;
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

    /// Consumes a button press that lands inside a visible header strip, recording `button` so its
    /// release is swallowed too. Returns whether the press was consumed.
    ///
    /// Only called when [`press_route`] allows a header click (`Normal` mode).
    fn consume_header_press(&mut self, button: u32) -> bool {
        let at = self.pointer.current_location();
        let Some((output, workspace)) = self.header_hit(at.x.floor() as i32, at.y.floor() as i32)
        else {
            return false;
        };
        self.header.consumed.consume_press(button);
        self.header_click(output, workspace);
        true
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
        mode: &InputMode,
        pos: Point<f64, Logical>,
    ) -> Option<(PointerFocusTarget, Point<f64, Logical>)> {
        if mode.is_locked() {
            return crate::lock::lock_surface_under(&self.session_lock, &self.space, pos);
        }
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
    /// Clicking a pane's area not covered by a window focuses that pane ([`crucible_layout::World::focus_pane`]);
    /// clicking a gap routes model focus to the output under the pointer. An unmanaged
    /// (override-redirect) element only gets keyboard focus; it is never enrolled in the model.
    fn update_keyboard_focus_under_pointer(&mut self, element_under: Option<WindowElement>) {
        let location = self.pointer.current_location();
        match element_under {
            Some(window) => {
                if let Some(id) = window.window_id() {
                    self.focus_window_in_model(id, "click");
                } else {
                    // Unmanaged: today's behaviour — route the model output, keyboard-only focus.
                    self.focus_output_under(location);
                }
                // Raise + keyboard focus. Idempotent when `apply_focus` already did it; required
                // for unmanaged elements and when the model focus did not change.
                self.focus_window(window);
            }
            None => match self.pane_under(location) {
                Some(pane) => self.focus_pane_in_model(pane, "click"),
                None => self.focus_output_under(location),
            },
        }
    }

    /// Routes model focus to `pane` because of pointer `cause`, then reconciles keyboard focus.
    ///
    /// Logs like [`Self::focus_window_in_model`]. Keyboard focus is reconciled explicitly because
    /// `apply_layout` skips `apply_focus` when the projected focus is unchanged (e.g. empty pane to
    /// empty pane, or away from an unmanaged element): an empty pane clears keyboard focus, a
    /// pane with a window focuses that window.
    fn focus_pane_in_model(&mut self, pane: crucible_layout::PaneId, cause: &str) {
        let before = self.layout.world.focus_path();
        if let Err(err) = self.with_model(|world| world.focus_pane(pane)) {
            warn!(%err, ?pane, cause, "pointer focus on a pane the model does not know");
            return;
        }
        let after = self.layout.world.focus_path();
        if after != before {
            info!(cause, focus = %after, "pointer focus");
        }
        self.reconcile_keyboard_focus();
    }

    /// Routes model focus to managed window `id` because of pointer `cause` (`"click"`/`"hover"`).
    ///
    /// Logs the new focus path at info level only when the model focus actually moved, so a
    /// click on the already-focused window stays quiet. Returns `false` if the model does not
    /// know `id` (logged as a warning).
    fn focus_window_in_model(&mut self, id: crucible_layout::WindowId, cause: &str) -> bool {
        let before = self.layout.world.focus_path();
        if let Err(err) = self.with_model(|world| world.focus_window(id)) {
            warn!(%err, ?id, cause, "pointer focus on a window the model does not know");
            return false;
        }
        let after = self.layout.world.focus_path();
        if after != before {
            info!(cause, focus = %after, "pointer focus");
        }
        true
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
    /// The enter key is the **pane** under the pointer — via the managed window over it, or its
    /// uncovered area — so moving between a window and the empty area around it in the same pane
    /// is not an enter; gaps and space outside every pane fall back to the output. Unmanaged
    /// elements leave the tracker unchanged. Focus moves only when the key differs from the last
    /// one seen ([`hover_entered`]), so keyboard-driven focus (a split, `Super+H`…) is not reverted
    /// by a pointer twitch. Routing: over a managed window the window is focused; over a pane's
    /// uncovered area the pane is. While a pointer grab is active (button held, drag) the tracker
    /// still updates but no focus moves. Overlays are modal ([`hover_moves_focus`]): while one is
    /// mapped the tracker still updates but hovering never moves focus off it. Does nothing when
    /// the option is off.
    fn focus_follows_pointer(&mut self, mode: &InputMode) {
        if !self.focus_follows_mouse || !hover_tracks(mode) {
            return;
        }
        let location = self.pointer.current_location();
        let element_under = self.space.element_under(location).map(|(w, _)| w.clone());
        let current = match &element_under {
            Some(window) => window
                .window_id()
                .and_then(|id| hover_pane_key(&self.layout.world, Some(id), None))
                .map(HoverTarget::Pane),
            None => match hover_pane_key(&self.layout.world, None, self.pane_under(location)) {
                Some(pane) => Some(HoverTarget::Pane(pane)),
                None => self
                    .space
                    .output_under(location)
                    .next()
                    .cloned()
                    .map(HoverTarget::Output),
            },
        };
        let mut stored = self.hover_target.take();
        let entered = hover_entered(&mut stored, current);
        self.hover_target = stored;
        if !hover_moves_focus(mode, entered, self.pointer.is_grabbed()) {
            return;
        }
        let managed = element_under.and_then(|w| w.window_id());
        match (self.hover_target.clone(), managed) {
            (Some(HoverTarget::Pane(_)), Some(id)) => {
                if self.focus_window_in_model(id, "hover") {
                    // If the model already pointed at `id` (no projected focus change),
                    // `apply_focus` did not run; reconcile syncs keyboard focus to it.
                    self.reconcile_keyboard_focus();
                }
            }
            (Some(HoverTarget::Pane(pane)), None) => self.focus_pane_in_model(pane, "hover"),
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
