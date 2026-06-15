//! Input handling: pointer, keyboard, and the global Super (Mod4) leader.
//!
//! WS-4 funnels the nested `winit` backend's [`InputEvent`]s into the seat so the compositor becomes
//! interactive:
//!
//! - **Keyboard** ([`CrucibleState::process_keyboard_event`]): every key is run through the seat's
//!   [`keyboard.input`](smithay::input::keyboard::KeyboardHandle::input) filter closure. The closure
//!   computes [`process_keyboard_shortcut`]; a matching `Super`-chord is **intercepted**
//!   ([`FilterResult::Intercept`]) and never reaches the client, while every other key is
//!   **forwarded** ([`FilterResult::Forward`]) to the focused client. The **suppressed-keys**
//!   mechanism records an intercepted *press* keysym and swallows its matching *release* so clients
//!   never see a dangling key-up.
//! - **Pointer** ([`CrucibleState::process_pointer_motion_absolute`] / `..._button` / `..._axis`):
//!   absolute winit motion is mapped into the output, the pointer location and pointer-focus
//!   (surface-under from the [`Space`](smithay::desktop::Space)) are updated so clients get
//!   enter/leave/motion, and buttons/axis frames are delivered. A pointer press also moves keyboard
//!   focus to the window under the cursor.
//!
//! ## Pure, testable shortcut mapping
//! [`process_keyboard_shortcut`] is a pure function of (`modifiers`, `keysym`) → `Option<KeyAction>`
//! with no compositor state, so the binding logic is unit-tested without a running compositor (see
//! the tests at the bottom of this module).
//!
//! ## Scope (WS-4)
//! This is the **winit** event path only — `udev`/libinput is WS-6. Focus targets stay bare
//! [`WlSurface`] (Wayland-only); the `KeyboardFocusTarget`/`PointerFocusTarget` enums that unify
//! layer/popup/X11 focus are WS-5 work (see [`crate::focus`]). The directional focus actions
//! (`Super+H/J/K/L`) are an explicit **stub**: real directional/tiling movement is Phase 2; for now
//! they perform a simple focus cycle among mapped windows.

use std::process::Command;
use std::sync::atomic::Ordering;

use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, Event, InputBackend, InputEvent, KeyState,
        KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    desktop::WindowSurfaceType,
    input::{
        keyboard::{FilterResult, Keysym, ModifiersState},
        pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent},
    },
    reexports::wayland_server::protocol::wl_pointer,
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Serial},
};
use tracing::{debug, error, info, warn};

use crate::{
    element::WindowElement,
    focus::{KeyboardFocusTarget, PointerFocusTarget},
    state::{Backend, CrucibleState},
    winit::WinitData,
};

/// Default terminal command used when `$TERMINAL` is unset, tried in order.
///
/// CDE targets Wayland-native terminals; the first one found on `$PATH` at spawn time is used. The
/// child is launched with `WAYLAND_DISPLAY` pointed at CDE's own socket so it connects back to us.
const DEFAULT_TERMINALS: &[&str] = &["foot", "alacritty", "kitty", "weston-terminal"];

/// A compositor action triggered by an intercepted `Super`-leader chord.
///
/// Produced by [`process_keyboard_shortcut`] and consumed by
/// [`CrucibleState::handle_key_action`]. The directional variants are **stubs** for Phase 1 (the
/// real directional/tiling movement is Phase 2 — see [`CrucibleState::cycle_focus`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    /// Quit the compositor cleanly (stop the main loop).
    Quit,
    /// Spawn a program (currently only the terminal, via `Super+Return`).
    Run(String),
    /// Move focus left — **stubbed** to a focus cycle (Phase 2 owns directional/tiling logic).
    FocusLeft,
    /// Move focus down — **stubbed** to a focus cycle (Phase 2 owns directional/tiling logic).
    FocusDown,
    /// Move focus up — **stubbed** to a focus cycle (Phase 2 owns directional/tiling logic).
    FocusUp,
    /// Move focus right — **stubbed** to a focus cycle (Phase 2 owns directional/tiling logic).
    FocusRight,
    /// Switch to virtual terminal `n` (only meaningful on the udev/DRM backend).
    ///
    /// Produced when libinput delivers a `Ctrl+Alt+F<n>` chord (the `XF86Switch_VT_<n>` keysyms).
    /// The nested `winit` backend never sees these, so this arm is a no-op there.
    VtSwitch(i32),
}

/// Maps a modifier+keysym combination to a [`KeyAction`], or `None` to forward the key.
///
/// **Pure** (no compositor state) so the `Super`-leader binding table is unit-testable without a
/// running compositor. Only `Super` (`modifiers.logo`, Mod4) chords are bound; every other key
/// returns `None` and is forwarded to the focused client. The bound set for WS-4 is:
///
/// - `Super+Return` → [`KeyAction::Run`] (spawn a terminal)
/// - `Super+Q` → [`KeyAction::Quit`]
/// - `Super+H` / `Super+J` / `Super+K` / `Super+L` → directional focus (stubbed; see
///   [`KeyAction`])
pub fn process_keyboard_shortcut(modifiers: ModifiersState, keysym: Keysym) -> Option<KeyAction> {
    // VT switching (`Ctrl+Alt+F<n>`) takes priority over the Super leader and is independent of it.
    // libinput maps `Ctrl+Alt+F<n>` to the `XF86Switch_VT_<n>` keysyms; map those to a VT switch.
    if (Keysym::XF86_Switch_VT_1.raw()..=Keysym::XF86_Switch_VT_12.raw()).contains(&keysym.raw()) {
        let vt = (keysym.raw() - Keysym::XF86_Switch_VT_1.raw() + 1) as i32;
        return Some(KeyAction::VtSwitch(vt));
    }

    // Only the Super (logo / Mod4) leader is bound; everything else forwards to the client.
    if !modifiers.logo {
        return None;
    }

    match keysym {
        Keysym::Return => Some(KeyAction::Run(terminal_command())),
        // Both lower- and upper-case `q` map to quit so the bind works regardless of Shift/Caps.
        Keysym::q | Keysym::Q => Some(KeyAction::Quit),
        Keysym::h | Keysym::H => Some(KeyAction::FocusLeft),
        Keysym::j | Keysym::J => Some(KeyAction::FocusDown),
        Keysym::k | Keysym::K => Some(KeyAction::FocusUp),
        Keysym::l | Keysym::L => Some(KeyAction::FocusRight),
        _ => None,
    }
}

/// Resolves the terminal command: `$TERMINAL` if set and non-empty, else the first default.
///
/// Pure aside from reading the environment; the actual lookup against `$PATH` happens when the child
/// is spawned in [`CrucibleState::spawn_terminal`].
fn terminal_command() -> String {
    match std::env::var("TERMINAL") {
        Ok(term) if !term.trim().is_empty() => term,
        _ => DEFAULT_TERMINALS[0].to_string(),
    }
}

impl CrucibleState<WinitData> {
    /// Routes a single winit [`InputEvent`] into the seat.
    ///
    /// Handles the events the nested winit backend emits: keyboard keys (through the Super-leader
    /// filter), absolute pointer motion, pointer buttons, and scroll axes. Other events (touch,
    /// gestures, relative motion) are not produced by the winit backend and are ignored here; they
    /// arrive with the udev/libinput path (see [`CrucibleState::process_input_event_libinput`]).
    pub fn process_input_event(&mut self, event: InputEvent<smithay::backend::winit::WinitInput>) {
        type B = smithay::backend::winit::WinitInput;
        match event {
            InputEvent::Keyboard { event } => self.process_keyboard_event::<B>(event),
            InputEvent::PointerMotionAbsolute { event } => {
                self.process_pointer_motion_absolute::<B>(event)
            }
            InputEvent::PointerButton { event } => self.process_pointer_button::<B>(event),
            InputEvent::PointerAxis { event } => self.process_pointer_axis::<B>(event),
            // Winit does not emit these; the udev/libinput backend handles them.
            _ => {}
        }
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Routes a single libinput [`InputEvent`] into the seat (the udev/DRM backend path).
    ///
    /// Generic over the [`InputBackend`] so it works with `LibinputInputBackend`; reuses the **same**
    /// per-event seat handlers the winit path uses ([`Self::process_keyboard_event`],
    /// [`Self::process_pointer_button`], [`Self::process_pointer_axis`]). The difference from the
    /// winit path is the pointer-motion model: real input devices deliver **relative** motion
    /// ([`InputEvent::PointerMotion`]) rather than the absolute motion winit emits, so this routes
    /// motion through [`Self::process_pointer_motion_relative`]. Device add/remove are surfaced to
    /// the caller (which configures libinput / tracks keyboards for LED sync) and otherwise ignored.
    pub fn process_input_event_libinput<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event } => self.process_keyboard_event::<I>(event),
            InputEvent::PointerMotion { event } => self.process_pointer_motion_relative::<I>(event),
            InputEvent::PointerButton { event } => self.process_pointer_button::<I>(event),
            InputEvent::PointerAxis { event } => self.process_pointer_axis::<I>(event),
            // Touch/gesture/device events are not handled yet; the udev source inspects
            // DeviceAdded/DeviceRemoved itself for keyboard-LED bookkeeping before calling us.
            _ => {}
        }
    }

    /// Runs a keyboard key through the seat, intercepting `Super`-leader chords.
    ///
    /// The [`keyboard.input`](smithay::input::keyboard::KeyboardHandle::input) filter closure:
    /// - on a **press**, computes [`process_keyboard_shortcut`]; a match is recorded in
    ///   `suppressed_keys` and intercepted, otherwise the key is forwarded;
    /// - on a **release**, if the keysym is suppressed it is removed and intercepted (so the client
    ///   never sees a dangling key-up), otherwise forwarded.
    ///
    /// Any intercepted [`KeyAction`] is dispatched via [`Self::handle_key_action`].
    fn process_keyboard_event<I: InputBackend>(&mut self, evt: I::KeyboardKeyEvent) {
        let keycode = evt.key_code();
        let state = evt.state();
        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time_msec(&evt);

        // Cloned out so the filter closure does not borrow `self`; written back afterwards.
        let mut suppressed_keys = self.suppressed_keys.clone();
        let keyboard = self.seat.get_keyboard().unwrap();

        let action = keyboard.input(
            self,
            keycode,
            state,
            serial,
            time,
            |_state, modifiers, handle| {
                let keysym = handle.modified_sym();
                debug!(?state, mods = ?modifiers, keysym = keysym.raw(), "key");

                if let KeyState::Pressed = state {
                    // A press that matches a Super-chord is intercepted; record it so we can also
                    // swallow its release and never leak a key-up to the client.
                    let action = process_keyboard_shortcut(*modifiers, keysym);
                    if action.is_some() {
                        suppressed_keys.push(keysym);
                    }
                    // The filter's `Intercept` payload type is `Option<KeyAction>` (a release
                    // intercepts with `None`); wrap the matched action accordingly.
                    match action {
                        Some(action) => FilterResult::Intercept(Some(action)),
                        None => FilterResult::Forward,
                    }
                } else {
                    // Release: swallow it iff its press was intercepted.
                    if suppressed_keys.contains(&keysym) {
                        suppressed_keys.retain(|k| *k != keysym);
                        FilterResult::Intercept(None)
                    } else {
                        FilterResult::Forward
                    }
                }
            },
        );

        self.suppressed_keys = suppressed_keys;

        // `input` returns `Option<FilterResult::Intercept(_)>`; the outer `Option` is `None` when
        // the key was forwarded. A release carries `Some(None)` (intercepted, no action).
        if let Some(Some(action)) = action {
            self.handle_key_action(action);
        }
    }

    /// Dispatches an intercepted [`KeyAction`].
    fn handle_key_action(&mut self, action: KeyAction) {
        match action {
            KeyAction::Quit => {
                info!("Super+Q: quitting");
                self.running.store(false, Ordering::SeqCst);
            }
            KeyAction::Run(cmd) => self.spawn_terminal(&cmd),
            // STUB (Phase 2 owns real directional/tiling movement): cycle focus among mapped
            // windows so the chord path is observably wired without committing to a layout model.
            KeyAction::FocusLeft | KeyAction::FocusUp => self.cycle_focus(false),
            KeyAction::FocusRight | KeyAction::FocusDown => self.cycle_focus(true),
            KeyAction::VtSwitch(vt) => {
                // Only the udev/DRM backend owns a session; on winit this is a no-op (the backend's
                // default `change_vt` returns `false`).
                info!(to = vt, "VT switch requested");
                if !self.backend_data.change_vt(vt) {
                    warn!(to = vt, "Backend does not support VT switching");
                }
            }
        }
    }

    /// Spawns `cmd` as a child connected to CDE's Wayland socket.
    ///
    /// The child inherits the environment but with `WAYLAND_DISPLAY` overridden to CDE's socket
    /// name so it connects to *this* compositor rather than the host session.
    fn spawn_terminal(&self, cmd: &str) {
        let Some(socket) = self.socket_name.clone() else {
            warn!(cmd, "No wayland socket; cannot spawn terminal");
            return;
        };
        info!(cmd, socket, "Super+Return: spawning terminal");
        match Command::new(cmd).env("WAYLAND_DISPLAY", &socket).spawn() {
            Ok(_child) => {}
            Err(err) => error!(cmd, %err, "Failed to spawn terminal"),
        }
    }

    /// **Stub** focus cycle among mapped windows (Phase 2 replaces this with directional/tiling
    /// movement).
    ///
    /// Picks the next (`forward`) or previous mapped window relative to the current keyboard focus
    /// and assigns keyboard focus to it, raising it in the [`Space`](smithay::desktop::Space). With
    /// zero or one window this is a no-op.
    fn cycle_focus(&mut self, forward: bool) {
        let windows: Vec<WindowElement> = self.space.elements().cloned().collect();
        if windows.len() < 2 {
            return;
        }

        // Locate the currently focused window (if the keyboard focus is on a window at all).
        let current = match self.keyboard_focus() {
            Some(KeyboardFocusTarget::Window(focused)) => {
                windows.iter().position(|w| *w == focused)
            }
            _ => None,
        };

        let next_idx = match current {
            Some(i) if forward => (i + 1) % windows.len(),
            Some(i) => (i + windows.len() - 1) % windows.len(),
            None => 0,
        };

        let next = windows[next_idx].clone();
        self.focus_window(next);
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

    /// Maps absolute winit pointer motion into the output and delivers it to the seat.
    ///
    /// Transforms the device position into the output's logical geometry, updates the pointer
    /// location and pointer focus (the surface under the cursor, from the [`Space`]), so the focused
    /// client receives enter/leave/motion. No-op if no output is mapped yet.
    fn process_pointer_motion_absolute<I: InputBackend>(
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
    fn process_pointer_motion_relative<I: InputBackend>(&mut self, evt: I::PointerMotionEvent) {
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
    fn process_pointer_button<I: InputBackend>(&mut self, evt: I::PointerButtonEvent) {
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
    fn process_pointer_axis<I: InputBackend>(&mut self, evt: I::PointerAxisEvent) {
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
    /// Resolution walks the [`Space`]'s mapped windows and dispatches over the window kind so X11
    /// windows resolve to a [`PointerFocusTarget::X11Surface`] and Wayland windows to a
    /// [`PointerFocusTarget::WlSurface`] (see [`WindowElement::surface_under`]).
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
    /// Used on pointer press (click-to-focus). Raises the window in the [`Space`] and assigns the
    /// seat's keyboard focus to it (activating X11 windows through the focus path).
    fn update_keyboard_focus_under_pointer(&mut self, _serial: Serial) {
        let location = self.pointer.current_location();
        if let Some((window, _)) = self
            .space
            .element_under(location)
            .map(|(w, p)| (w.clone(), p))
        {
            self.focus_window(window);
        }
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Returns the seat's current [`KeyboardFocusTarget`], if any.
    pub fn keyboard_focus(&self) -> Option<KeyboardFocusTarget> {
        self.seat.get_keyboard().and_then(|kbd| kbd.current_focus())
    }

    /// Sets (or clears) the seat's keyboard focus to `target`.
    ///
    /// Called from the shell map path (so a newly mapped toplevel/X11 window receives keys) and
    /// from the input path (click-to-focus / the stubbed focus cycle). A fresh serial is allocated
    /// for the focus change. When `target` is an XWayland window the seat's
    /// [`KeyboardTarget`](smithay::input::keyboard::KeyboardTarget) impl drives the X11
    /// `set_input_focus`/take-focus that activates it.
    pub fn set_keyboard_focus(&mut self, target: Option<KeyboardFocusTarget>) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        keyboard.set_focus(self, target, serial);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a [`ModifiersState`] with only `logo` (Super) set.
    fn super_only() -> ModifiersState {
        ModifiersState {
            logo: true,
            ..Default::default()
        }
    }

    #[test]
    fn super_return_runs_a_terminal() {
        let action = process_keyboard_shortcut(super_only(), Keysym::Return);
        assert!(matches!(action, Some(KeyAction::Run(_))));
    }

    #[test]
    fn super_q_quits() {
        assert_eq!(
            process_keyboard_shortcut(super_only(), Keysym::q),
            Some(KeyAction::Quit)
        );
        // Upper-case (Shift/Caps) variant also quits.
        assert_eq!(
            process_keyboard_shortcut(super_only(), Keysym::Q),
            Some(KeyAction::Quit)
        );
    }

    #[test]
    fn super_hjkl_maps_to_directional_focus_stubs() {
        assert_eq!(
            process_keyboard_shortcut(super_only(), Keysym::h),
            Some(KeyAction::FocusLeft)
        );
        assert_eq!(
            process_keyboard_shortcut(super_only(), Keysym::j),
            Some(KeyAction::FocusDown)
        );
        assert_eq!(
            process_keyboard_shortcut(super_only(), Keysym::k),
            Some(KeyAction::FocusUp)
        );
        assert_eq!(
            process_keyboard_shortcut(super_only(), Keysym::l),
            Some(KeyAction::FocusRight)
        );
    }

    #[test]
    fn unmatched_super_chord_is_none() {
        // A Super chord with no binding is not intercepted (forwarded to the client).
        assert_eq!(process_keyboard_shortcut(super_only(), Keysym::z), None);
    }

    #[test]
    fn non_super_keys_are_forwarded() {
        // Without the logo modifier, even bound keysyms must forward to the focused client.
        let no_mods = ModifiersState::default();
        assert_eq!(process_keyboard_shortcut(no_mods, Keysym::Return), None);
        assert_eq!(process_keyboard_shortcut(no_mods, Keysym::q), None);
        assert_eq!(process_keyboard_shortcut(no_mods, Keysym::h), None);
    }

    #[test]
    fn terminal_command_falls_back_to_default_without_env() {
        // SAFETY: single-threaded test; we restore the prior value before returning.
        let prev = std::env::var("TERMINAL").ok();
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("TERMINAL");
        }
        assert_eq!(terminal_command(), DEFAULT_TERMINALS[0]);
        #[allow(unsafe_code)]
        unsafe {
            match prev {
                Some(v) => std::env::set_var("TERMINAL", v),
                None => std::env::remove_var("TERMINAL"),
            }
        }
    }
}
