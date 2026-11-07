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

use tracing::{info, warn};

use super::hover::{HoverTarget, SwitchHover, hover_entered, hover_pane_key, switch_hover_action};
use super::modal::{
    DeferredHover, HoverDecision, InputMode, deferred_hover_action, hover_decision, hover_tracks,
    press_route,
};
use crate::element::WindowElement;
use crate::focus::{KeyboardFocusTarget, PointerFocusTarget};
use crate::pointer_lock::{MotionOutcome, clamp_to_fullscreen, constrained_motion};
use crate::state::{Backend, CrucibleState};
use smithay::wayland::seat::WaylandFocus;

/// Linux `BTN_LEFT` / `BTN_RIGHT` (`input-event-codes.h`).
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

/// The selection event a button makes in snip mode: left press/release drive the drag, a right
/// press cancels; everything else is swallowed.
#[must_use]
pub(super) fn snip_button(button: u32, pressed: bool) -> Option<crate::snip::Event> {
    use crate::snip::Event;
    match (button, pressed) {
        (BTN_LEFT, true) => Some(Event::Press),
        (BTN_LEFT, false) => Some(Event::Release),
        (BTN_RIGHT, true) => Some(Event::Esc),
        _ => None,
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// The global logical location an absolute (winit) motion maps to on the first output, or
    /// `None` while no output is mapped.
    pub(super) fn absolute_location<I: InputBackend>(
        &self,
        evt: &I::PointerMotionAbsoluteEvent,
    ) -> Option<Point<f64, Logical>> {
        let output = self.space.outputs().next()?;
        let output_geo = self.space.output_geometry(output)?;
        Some(evt.position_transformed(output_geo.size) + output_geo.loc.to_f64())
    }

    /// The pointer location before a relative motion and the output-clamped location after it, or
    /// `None` while no output is mapped.
    pub(super) fn relative_location<I: InputBackend>(
        &self,
        evt: &I::PointerMotionEvent,
    ) -> Option<(Point<f64, Logical>, Point<f64, Logical>)> {
        self.space.outputs().next()?;
        let current = self.pointer.current_location();
        Some((current, self.clamp_coords(current + evt.delta())))
    }

    /// The one motion delivery: resolves the surface under `location` in `mode`
    /// ([`Self::surface_under`]) and moves the pointer there ([`Self::deliver_motion_to`]), with
    /// `relative` delivered to the same surface first. Returns nothing; the seat tracks focus.
    pub(super) fn deliver_motion(
        &mut self,
        mode: &InputMode,
        location: Point<f64, Logical>,
        serial: Serial,
        time: u32,
        relative: Option<RelativeMotionEvent>,
    ) {
        let under = self.surface_under(mode, location);
        self.deliver_motion_to(under, location, serial, time, relative);
    }

    /// Delivers `relative` (when given), then the absolute motion to `location` with pointer focus
    /// `under`, then the frame. `under = None` moves the pointer with no client focused (snip
    /// mode: the client that had focus gets a leave).
    pub(super) fn deliver_motion_to(
        &mut self,
        under: Option<(PointerFocusTarget, Point<f64, Logical>)>,
        location: Point<f64, Logical>,
        serial: Serial,
        time: u32,
        relative: Option<RelativeMotionEvent>,
    ) {
        let pointer = self.pointer.clone();
        if let Some(relative) = relative {
            pointer.relative_motion(self, under.clone(), &relative);
        }
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location,
                serial,
                time,
            },
        );
        pointer.frame(self);
    }

    /// Maps absolute winit pointer motion into the output and delivers it to the seat.
    ///
    /// Transforms the device position into the output's logical geometry, updates the pointer
    /// location and pointer focus (the surface under the cursor, from the
    /// [`Space`](smithay::desktop::Space)), so the focused client receives enter/leave/motion.
    /// No-op if no output is mapped yet. Snip mode never reaches here (see `process_snip_input`).
    pub(super) fn process_pointer_motion_absolute<I: InputBackend>(
        &mut self,
        mode: &InputMode,
        evt: I::PointerMotionAbsoluteEvent,
    ) {
        let Some(pos) = self.absolute_location::<I>(&evt) else {
            return;
        };
        self.deliver_motion(
            mode,
            pos,
            SERIAL_COUNTER.next_serial(),
            evt.time_msec(),
            None,
        );
        self.header_drag_motion();
        self.focus_follows_pointer(mode);
    }

    /// Maps **relative** pointer motion (real input devices, via libinput) into the seat.
    ///
    /// Unlike winit's absolute motion, libinput delivers a motion *delta*. The new pointer location
    /// is the previous location plus the delta, clamped to the union of all mapped outputs so the
    /// cursor cannot leave the visible area (it can cross freely between outputs laid left-to-right).
    /// The seat receives both the relative-motion event (for clients that requested it via the
    /// relative-pointer protocol, gated by [`Backend::HAS_RELATIVE_MOTION`]) and the resulting
    /// absolute motion. No-op until at least one output is mapped. Snip mode never reaches here.
    pub(super) fn process_pointer_motion_relative<I: InputBackend>(
        &mut self,
        mode: &InputMode,
        evt: I::PointerMotionEvent,
    ) {
        let Some((current, mut location)) = self.relative_location::<I>(&evt) else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let relative = RelativeMotionEvent {
            delta: evt.delta(),
            delta_unaccel: evt.delta_unaccel(),
            utime: evt.time(),
        };

        // An active pointer constraint (only ever activated on relative-motion backends, see
        // `crate::pointer_lock`) on the surface under the pointer locks or confines the motion.
        let under_current = self.surface_under(mode, current);
        let current_surface = under_current
            .as_ref()
            .and_then(|(target, origin)| Some((target.wl_surface()?.into_owned(), *origin)));
        if let Some((surface, origin)) = &current_surface {
            self.note_constraint_origin(surface, *origin);
        }
        let constraint = current_surface
            .as_ref()
            .and_then(|(surface, origin)| self.active_constraint(surface, *origin));
        if let Some((kind, region, rect)) = constraint {
            match constrained_motion(kind, region.as_ref(), rect, current, location) {
                MotionOutcome::Suppress => {
                    // Locked: relative motion only; no absolute motion, no hover focus change.
                    let pointer = self.pointer.clone();
                    pointer.relative_motion(self, under_current, &relative);
                    pointer.frame(self);
                    return;
                }
                MotionOutcome::Move(next) => {
                    if next != location {
                        tracing::trace!(
                            target: "crucible_compositor::pointer_lock",
                            ?kind,
                            ?rect,
                            proposed = ?location,
                            clamped = ?next,
                            "confined motion clamped"
                        );
                    }
                    location = next;
                }
            }
        }
        if let Some(rect) = self.fullscreen_confine() {
            location = clamp_to_fullscreen(rect, current, location);
        }
        self.deliver_motion(mode, location, serial, evt.time_msec(), Some(relative));
        self.header_drag_motion();
        self.focus_follows_pointer(mode);
        let keyboard = self.keyboard_focus_surface();
        self.reconcile_pointer_constraint(keyboard);
    }

    /// Confines a pointer location to the union of all mapped outputs; see [`confine_to_outputs`].
    ///
    /// Returns `pos` unchanged when no output is mapped.
    fn clamp_coords(&self, pos: Point<f64, Logical>) -> Point<f64, Logical> {
        let rects: Vec<Rectangle<i32, Logical>> = self
            .space
            .outputs()
            .filter_map(|output| self.space.output_geometry(output))
            .collect();
        confine_to_outputs(pos, &rects)
    }

    /// Delivers a pointer button to the seat, moving keyboard focus on press. Snip mode never
    /// reaches here (see `process_snip_input`).
    pub(super) fn process_pointer_button<I: InputBackend>(
        &mut self,
        mode: &InputMode,
        evt: I::PointerButtonEvent,
    ) {
        let serial = SERIAL_COUNTER.next_serial();
        let button = evt.button_code();
        let state = wl_pointer::ButtonState::from(evt.state());

        if wl_pointer::ButtonState::Pressed == state {
            let element_under = self
                .space
                .element_under(self.pointer.current_location())
                .map(|(w, _)| w.clone());
            let route = press_route(mode, element_under.as_ref());
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
        } else if let Some((press, dragged)) = self.header.release(button) {
            // The paired release of a press the header strip consumed: never forward a release
            // whose press the client did not see (audit E8). It also decides the click or reorder.
            self.finish_header_press(press, dragged);
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
        let Some(press) = self.header_hit(at.x.floor() as i32, at.y.floor() as i32) else {
            return false;
        };
        self.header.press(button, press, (at.x, at.y));
        true
    }

    /// Re-resolves pointer focus at the current location (a synthetic motion with no movement)
    /// through the one motion delivery. Used when snip mode ends so the client under the pointer
    /// gets a fresh enter.
    pub(crate) fn refresh_pointer_focus(&mut self) {
        let mode = self.input_mode();
        let location = self.pointer.current_location();
        let time = self.clock.now().as_millis();
        self.deliver_motion(&mode, location, SERIAL_COUNTER.next_serial(), time, None);
    }

    /// Takes pointer focus away from every client at the current location (a synthetic motion
    /// to no surface, through the shared delivery). Used when snip mode starts so the client under
    /// the pointer gets a leave; while snipping the snip router keeps the focus empty.
    pub(crate) fn clear_pointer_focus(&mut self) {
        let location = self.pointer.current_location();
        let time = self.clock.now().as_millis();
        self.deliver_motion_to(None, location, SERIAL_COUNTER.next_serial(), time, None);
    }

    /// Delivers a scroll axis frame to the seat. Snip mode drops axis events before here.
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

    /// Returns the [`PointerFocusTarget`] (and its top-left in global logical coords) under `pos`
    /// in `mode`: the lock surface while locked, else the space's ([`Self::space_surface_under`]).
    /// Snip mode never resolves a surface: its router delivers motion with no focus.
    fn surface_under(
        &self,
        mode: &InputMode,
        pos: Point<f64, Logical>,
    ) -> Option<(PointerFocusTarget, Point<f64, Logical>)> {
        if mode.is_locked() {
            return crate::lock::lock_surface_under(&self.session_lock, &self.space, pos);
        }
        self.space_surface_under(pos)
    }

    /// The [`PointerFocusTarget`] (and its top-left in global logical coords) under `pos` in the
    /// [`Space`](smithay::desktop::Space), ignoring modes.
    ///
    /// Resolution walks the space's mapped windows and dispatches over the window kind so X11
    /// windows resolve to a [`PointerFocusTarget::X11Surface`] and Wayland windows to a
    /// [`PointerFocusTarget::WlSurface`] (see [`WindowElement::surface_under`]).
    pub(crate) fn space_surface_under(
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
    /// still updates but no focus moves. Overlays are modal ([`hover_decision`]): while one is
    /// mapped the tracker still updates but hovering never moves focus off it. Does nothing when
    /// the option is off.
    fn focus_follows_pointer(&mut self, mode: &InputMode) {
        if !self.config.focus_follows_mouse || !hover_tracks(mode) {
            return;
        }
        let (current, managed) = self.hover_under_pointer();
        let mut stored = self.hover_target.take();
        let entered = hover_entered(&mut stored, current);
        self.hover_target = stored;
        let grabbed = self.pointer.is_grabbed();
        match hover_decision(mode, entered, grabbed) {
            HoverDecision::Ignore => {}
            HoverDecision::Defer => self.hover_deferred = true,
            HoverDecision::Move => {
                self.hover_deferred = false;
                if let Some(target) = self.hover_target.clone() {
                    self.focus_hover_target(target, managed);
                }
            }
        }
    }

    /// What the pointer hovers now: the pane-level [`HoverTarget`] (falling back to the output)
    /// and the managed window under it, if any. No side effects.
    fn hover_under_pointer(&self) -> (Option<HoverTarget>, Option<crucible_layout::WindowId>) {
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
        (current, element_under.and_then(|w| w.window_id()))
    }

    /// Routes a hover focus move: a pane plus a managed window focuses the window; a pane alone
    /// focuses the pane; an output focuses the output.
    fn focus_hover_target(
        &mut self,
        target: HoverTarget,
        managed: Option<crucible_layout::WindowId>,
    ) {
        match (target, managed) {
            (HoverTarget::Pane(_), Some(id)) => {
                if self.focus_window_in_model(id, "hover") {
                    // If the model already pointed at `id` (no projected focus change),
                    // `apply_focus` did not run; reconcile syncs keyboard focus to it.
                    self.reconcile_keyboard_focus();
                }
            }
            (HoverTarget::Pane(pane), None) => self.focus_pane_in_model(pane, "hover"),
            // `focus_output` is a no-op when that output is already focused.
            (HoverTarget::Output(output), _) => self.focus_output(&output),
        }
    }

    /// Applies a hover enter that a mapped overlay held back ([`HoverDecision::Defer`]): with
    /// focus-follows-mouse on, focus moves to what is under the pointer now, as if the pointer had
    /// just entered it. Called before an overlay opens, so it opens on the pointer's output rather
    /// than where the previous overlay was. Nothing moves when no enter was deferred, so
    /// keyboard-driven focus is kept.
    pub(crate) fn apply_deferred_hover(&mut self) {
        if !std::mem::take(&mut self.hover_deferred) || !self.config.focus_follows_mouse {
            return;
        }
        let (current, managed) = self.hover_under_pointer();
        let action = deferred_hover_action(current.is_some(), self.pointer.is_grabbed());
        // `Clear` ends with no tracker, as a fresh enter against a cleared tracker would.
        self.hover_target = current.clone();
        if let (DeferredHover::Focus, Some(target)) = (action, current) {
            self.focus_hover_target(target, managed);
        }
    }

    /// Focus-follows-mouse after a workspace switch: when the workspace shown on the output under
    /// the pointer changed since the last re-layout, focus moves to the pane under the pointer as
    /// if the pointer had just entered it, instead of waiting for the next pointer motion. Only a
    /// pane is focused (over a gap or the header strip the switched workspace keeps its own
    /// focus). An overlay or a pointer grab defers it to a later re-layout
    /// ([`switch_hover_action`]). Called at the end of every
    /// [`apply_layout`](Self::apply_layout).
    pub(crate) fn hover_after_workspace_switch(&mut self) {
        let location = self.pointer.current_location();
        let shown = self
            .space
            .output_under(location)
            .next()
            .and_then(crate::layout::model_output_id)
            .and_then(|id| self.layout.world.output(id))
            .map(|o| o.active_workspace());
        let mode = self.input_mode();
        let action = switch_hover_action(
            self.hover_workspace.as_ref(),
            shown.as_ref(),
            self.config.focus_follows_mouse,
            &mode,
            self.pointer.is_grabbed(),
        );
        match action {
            SwitchHover::Wait => {}
            SwitchHover::Settle => self.hover_workspace = shown,
            SwitchHover::Refocus => {
                self.hover_workspace = shown;
                let (current, managed) = self.hover_under_pointer();
                self.hover_target = current.clone();
                if let Some(target @ HoverTarget::Pane(_)) = current {
                    self.focus_hover_target(target, managed);
                }
            }
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

/// Clamps `pos` into one output rect (inclusive of `loc`, last pixel at `loc + size - 1`).
fn clamp_into(pos: Point<f64, Logical>, rect: Rectangle<i32, Logical>) -> Point<f64, Logical> {
    let r = rect.to_f64();
    let x = pos
        .x
        .clamp(r.loc.x, (r.loc.x + r.size.w - 1.0).max(r.loc.x));
    let y = pos
        .y
        .clamp(r.loc.y, (r.loc.y + r.size.h - 1.0).max(r.loc.y));
    (x, y).into()
}

/// Confines `pos` to the union of `outputs` (logical rects), not their bounding box.
///
/// A point inside some output is kept; otherwise it moves to the nearest point of the nearest
/// output. With mixed scales the outputs have unequal logical heights, so the bounding box has a
/// void below the shorter one; a point there lands on that output's bottom edge instead. Returns
/// `pos` unchanged when `outputs` is empty.
#[must_use]
pub fn confine_to_outputs(
    pos: Point<f64, Logical>,
    outputs: &[Rectangle<i32, Logical>],
) -> Point<f64, Logical> {
    // Inside means `loc <= p < loc + size`, so sub-pixel motion across a shared edge never sticks.
    let inside = outputs.iter().any(|r| {
        let r = r.to_f64();
        pos.x >= r.loc.x
            && pos.x < r.loc.x + r.size.w
            && pos.y >= r.loc.y
            && pos.y < r.loc.y + r.size.h
    });
    if inside {
        return pos;
    }
    let mut best: Option<(f64, Point<f64, Logical>)> = None;
    for rect in outputs {
        let clamped = clamp_into(pos, *rect);
        let (dx, dy) = (clamped.x - pos.x, clamped.y - pos.y);
        let dist = dx * dx + dy * dy;
        if best.is_none_or(|(d, _)| dist < d) {
            best = Some((dist, clamped));
        }
    }
    best.map_or(pos, |(_, p)| p)
}

#[cfg(test)]
mod confine_tests {
    use super::*;

    fn strip() -> [Rectangle<i32, Logical>; 2] {
        [
            Rectangle::new((0, 0).into(), (3840, 2160).into()),
            Rectangle::new((3840, 0).into(), (1920, 1080).into()),
        ]
    }

    #[test]
    fn a_point_inside_an_output_is_kept() {
        let p: Point<f64, Logical> = (4000.5, 900.25).into();
        assert_eq!(confine_to_outputs(p, &strip()), p);
        let p: Point<f64, Logical> = (100.0, 2000.0).into();
        assert_eq!(confine_to_outputs(p, &strip()), p);
    }

    #[test]
    fn below_the_shorter_output_lands_on_its_bottom_edge() {
        let p = confine_to_outputs((5000.0, 1500.0).into(), &strip());
        assert_eq!(p, (5000.0, 1079.0).into());
    }

    #[test]
    fn beyond_the_strip_clamps_to_the_nearest_output() {
        assert_eq!(
            confine_to_outputs((-50.0, 3000.0).into(), &strip()),
            (0.0, 2159.0).into()
        );
        assert_eq!(
            confine_to_outputs((9000.0, -5.0).into(), &strip()),
            (5759.0, 0.0).into()
        );
        assert_eq!(
            confine_to_outputs((1.0, 2.0).into(), &[]),
            (1.0, 2.0).into()
        );
    }
}

#[cfg(test)]
mod snip_tests {
    use super::*;
    use crate::snip::Event;

    #[test]
    fn snip_buttons_left_drags_right_cancels_rest_swallowed() {
        assert_eq!(snip_button(BTN_LEFT, true), Some(Event::Press));
        assert_eq!(snip_button(BTN_LEFT, false), Some(Event::Release));
        assert_eq!(snip_button(BTN_RIGHT, true), Some(Event::Esc));
        assert_eq!(snip_button(BTN_RIGHT, false), None);
        assert_eq!(snip_button(0x112, true), None);
    }
}
