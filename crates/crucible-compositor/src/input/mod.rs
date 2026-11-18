//! Input handling: pointer, keyboard, and the global Super (Mod4) leader.
//!
//! Funnels the nested `winit` backend's [`InputEvent`]s (and, from the udev backend, real
//! libinput devices) into the seat so the compositor becomes interactive. The module is split by
//! concern:
//!
//! - [`keyboard`] — keyboard I/O: runs every key through the seat's
//!   [`keyboard.input`](smithay::input::keyboard::KeyboardHandle::input) filter closure, which
//!   resolves `Super`-leader chords via [`crucible_keys::resolve`] and uses
//!   [`keyboard::SuppressedKeys`] (keyed by physical keycode, never the resolved keysym) so a key
//!   whose press was intercepted never leaks a dangling release to the client even if modifiers
//!   changed in between.
//! - [`pointer`] — pointer I/O: absolute/relative motion, buttons, axis frames, and
//!   click-to-focus.
//! - [`execute`] — the action gate: [`CrucibleState::handle_key_action`] (lock → overlay → execute) dispatches a resolved
//!   [`crucible_keys::Action`] to a compositor-level effect (`Quit`/`RunTerminal`/`Run`/
//!   `VtSwitch`) or, for every model verb, through the single model-mutation entry point
//!   (`update_model`/`with_model`, see [`crate::layout`]) — one execution arm per verb.
//!
//! ## Canonical key policy lives in `crucible-keys`
//! Shortcut *meaning* — the default chord table and the [`crucible_keys::Action`] verb set — is
//! owned by the Smithay-free `crucible-keys` crate. This module only converts backend event types (Smithay `ModifiersState`/`Keysym`,
//! physical keycodes) to that crate's types at the edge, and executes the resolved action.
//!
//! ## Entry points
//! [`CrucibleState::process_input_event`] is the nested **winit** path;
//! [`CrucibleState::process_input_event_libinput`] is the **udev**/libinput path. Both
//! funnel into the same per-event seat handlers in [`keyboard`]/[`pointer`]. Focus targets stay
//! bare `WlSurface` plus the [`KeyboardFocusTarget`](crate::focus::KeyboardFocusTarget)/
//! [`PointerFocusTarget`](crate::focus::PointerFocusTarget) enums that unify layer/popup/X11 focus
//! (see [`crate::focus`]).

mod execute;

pub(crate) use execute::{ActionError, Origin};
/// Test support: the one admission gate and the mode it takes (greeter-mode tests).
#[cfg(test)]
pub(crate) use {execute::admit, modal::InputMode as TestInputMode};
mod hover;
mod keyboard;
mod modal;
mod pointer;

use smithay::backend::input::{
    ButtonState, Event, InputBackend, InputEvent, KeyboardKeyEvent, PointerButtonEvent,
};
use smithay::utils::SERIAL_COUNTER;

use modal::InputMode;

use crate::state::{Backend, CrucibleState};
use crate::winit::WinitData;

/// Physical-keycode identity for consumed Super-leader key presses (F10). See
/// [`keyboard::SuppressedKeys`]'s docs; re-exported here (rather than making the `keyboard`
/// submodule itself public) since [`crate::state::CrucibleState::suppressed_keys`] is a public
/// field and therefore needs a public path to its type.
pub use hover::HoverTarget;
pub use keyboard::SuppressedKeys;

impl CrucibleState<WinitData> {
    /// Routes a single winit [`InputEvent`] into the seat.
    ///
    /// Handles the events the nested winit backend emits: keyboard keys (through the Super-leader
    /// filter), absolute pointer motion, pointer buttons, and scroll axes. Other events (touch,
    /// gestures, relative motion) are not produced by the winit backend and are ignored here; they
    /// arrive with the udev/libinput path ([`CrucibleState::process_input_event_libinput`]).
    pub fn process_input_event(&mut self, event: InputEvent<smithay::backend::winit::WinitInput>) {
        type B = smithay::backend::winit::WinitInput;
        // The mode is resolved once for the event; snip mode branches here, once.
        let Some(mode) = self.pre_dispatch::<B>(&event) else {
            return;
        };
        if mode.is_snipping() {
            return self.process_snip_input::<B>(&mode, event);
        }
        match event {
            InputEvent::Keyboard { event } => self.process_keyboard_event::<B>(&mode, event),
            InputEvent::PointerMotionAbsolute { event } => {
                self.process_pointer_motion_absolute::<B>(&mode, event)
            }
            InputEvent::PointerButton { event } => self.process_pointer_button::<B>(&mode, event),
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
    /// per-event seat handlers the winit path uses (`process_keyboard_event`,
    /// `process_pointer_button`, `process_pointer_axis`). The difference from the winit path is the
    /// pointer-motion model: real input devices deliver **relative** motion
    /// ([`InputEvent::PointerMotion`]) rather than the absolute motion winit emits, so this routes
    /// motion through `process_pointer_motion_relative`. Device add/remove are surfaced to the
    /// caller (which configures libinput / tracks keyboards for LED sync) and otherwise ignored.
    pub fn process_input_event_libinput<I: InputBackend>(&mut self, event: InputEvent<I>) {
        // The mode is resolved once for the event; snip mode branches here, once. Absolute
        // motion is not handled on this path in any mode.
        let Some(mode) = self.pre_dispatch::<I>(&event) else {
            return;
        };
        match event {
            InputEvent::PointerMotionAbsolute { .. } => {}
            event if mode.is_snipping() => self.process_snip_input::<I>(&mode, event),
            InputEvent::Keyboard { event } => self.process_keyboard_event::<I>(&mode, event),
            InputEvent::PointerMotion { event } => {
                self.process_pointer_motion_relative::<I>(&mode, event)
            }
            InputEvent::PointerButton { event } => self.process_pointer_button::<I>(&mode, event),
            InputEvent::PointerAxis { event } => self.process_pointer_axis::<I>(event),
            // A removed device leaves no held remap key or layer for a reused source id.
            InputEvent::DeviceRemoved { device } => {
                use smithay::backend::input::Device;
                self.key_remap.remove_source(&device.id());
            }
            // Touch/gesture/device events are not handled yet; the udev source inspects
            // DeviceAdded/DeviceRemoved itself for keyboard-LED bookkeeping before calling us.
            _ => {}
        }
    }
}
impl<B: Backend + 'static> CrucibleState<B> {
    /// The routers' one shared pre-dispatch step, run once per event before any handler:
    ///
    /// 1. user activity ([`EventKind::is_user_activity`]) feeds the idle machine;
    /// 2. the input mode is resolved, once;
    /// 3. in greeter mode the key press that woke blanked outputs is dropped (its release too),
    ///    so it never lands in the login field;
    /// 4. while the logout notice is active, every pointer button and axis event is dropped,
    ///    whatever mode won (the lock or greeter included); motion is kept (with no focus under
    ///    the pointer while the mode is `LoggingOut`, see `surface_under`), keys run the filter.
    ///
    /// Returns the mode for the handlers, or `None` when the event was dropped.
    fn pre_dispatch<I: InputBackend>(&mut self, event: &InputEvent<I>) -> Option<InputMode> {
        let kind = EventKind::of(event);
        let woke = kind.is_user_activity() && self.idle_activity();
        let mode = self.input_mode();
        if self.drop_wake_key::<I>(woke, kind, &mode, event) {
            return None;
        }
        if drops_while_logging_out(kind, self.logging_out()) {
            return None;
        }
        Some(mode)
    }

    /// Logs a wake and, in greeter mode only, swallows the key press that woke blanked outputs
    /// (recording its keycode so the matching release is swallowed too) and traces the first key
    /// press after blanking. Returns whether `event` was dropped. Other modes never drop it.
    fn drop_wake_key<I: InputBackend>(
        &mut self,
        woke: bool,
        kind: EventKind,
        mode: &InputMode,
        event: &InputEvent<I>,
    ) -> bool {
        if woke {
            tracing::info!(event = kind.as_str(), "idle: outputs woken by input");
        }
        let InputEvent::Keyboard { event } = event else {
            return false;
        };
        let press = event.state() == smithay::backend::input::KeyState::Pressed;
        if !(press && mode.is_greeter()) {
            return false;
        }
        if std::mem::take(&mut self.idle.trace_next_key) {
            let outputs: Vec<_> = self.space.outputs().cloned().collect();
            let off = outputs
                .iter()
                .filter(|o| self.backend_data.output_powered_off(o))
                .count();
            tracing::info!(
                woke,
                outputs_off = off,
                "greeter: first key press after blanking"
            );
        }
        if !woke {
            return false;
        }
        self.suppressed_keys.consume_press(event.key_code().raw());
        true
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// The snip-mode router: every event of a snipping session lands here, once, from either
    /// backend router, and no client receives pointer focus, motion, buttons or axis.
    ///
    /// - keys run the keyboard path, whose filter holds the single snip branch (an admitted chord
    ///   runs, anything else feeds [`keyboard`]'s `snip_key`); xkb state and `suppressed_keys`
    ///   keep tracking;
    /// - motion (absolute or relative) moves the pointer with no focus, then feeds `snip_motion`
    ///   (no constraint, confinement, header drag or focus-follows-mouse);
    /// - buttons feed `snip_button`;
    /// - axis and everything else is dropped.
    fn process_snip_input<I: InputBackend>(&mut self, mode: &InputMode, event: InputEvent<I>) {
        let location = match event {
            InputEvent::Keyboard { event } => return self.process_keyboard_event::<I>(mode, event),
            InputEvent::PointerMotionAbsolute { event } => self
                .absolute_location::<I>(&event)
                .map(|pos| (pos, event.time_msec())),
            InputEvent::PointerMotion { event } => self
                .relative_location::<I>(&event)
                .map(|(_, pos)| (pos, event.time_msec())),
            InputEvent::PointerButton { event } => {
                let pressed = event.state() == ButtonState::Pressed;
                if let Some(event) = pointer::snip_button(event.button_code(), pressed) {
                    self.snip_event(event);
                }
                return;
            }
            _ => return,
        };
        if let Some((pos, time)) = location {
            self.deliver_motion_to(None, pos, SERIAL_COUNTER.next_serial(), time, None);
            self.snip_motion(pos);
        }
    }
}

/// The kind of an input event: what the pre-dispatch step classifies (activity, the logout drop)
/// and logs (never its key, button or position).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventKind {
    Keyboard,
    PointerMotion,
    PointerButton,
    PointerAxis,
    DeviceAdded,
    DeviceRemoved,
    Switch,
    Touch,
    Other,
}

impl EventKind {
    fn of<I: InputBackend>(event: &InputEvent<I>) -> Self {
        match event {
            InputEvent::Keyboard { .. } => Self::Keyboard,
            InputEvent::PointerMotion { .. } | InputEvent::PointerMotionAbsolute { .. } => {
                Self::PointerMotion
            }
            InputEvent::PointerButton { .. } => Self::PointerButton,
            InputEvent::PointerAxis { .. } => Self::PointerAxis,
            InputEvent::DeviceAdded { .. } => Self::DeviceAdded,
            InputEvent::DeviceRemoved { .. } => Self::DeviceRemoved,
            InputEvent::SwitchToggle { .. } => Self::Switch,
            InputEvent::TouchDown { .. }
            | InputEvent::TouchMotion { .. }
            | InputEvent::TouchUp { .. }
            | InputEvent::TouchCancel { .. }
            | InputEvent::TouchFrame { .. } => Self::Touch,
            _ => Self::Other,
        }
    }

    /// The kind's log name.
    fn as_str(self) -> &'static str {
        match self {
            Self::Keyboard => "keyboard",
            Self::PointerMotion => "pointer-motion",
            Self::PointerButton => "pointer-button",
            Self::PointerAxis => "pointer-axis",
            Self::DeviceAdded => "device-added",
            Self::DeviceRemoved => "device-removed",
            Self::Switch => "switch",
            Self::Touch => "touch",
            Self::Other => "other",
        }
    }

    /// Whether an event of this kind is user activity for the idle machine. Device add/remove are
    /// not: libinput queues its start-up device enumeration until the first real input is
    /// dispatched, so those events arrive just ahead of the key that wakes a blanked greeter.
    /// Counting them woke the outputs first, and the key was then delivered to the login field.
    fn is_user_activity(self) -> bool {
        !matches!(self, Self::DeviceAdded | Self::DeviceRemoved)
    }
}

/// Whether an event of `kind` is dropped because the logout notice is active: buttons and axis
/// events, keyed on the notice (not the resolved mode), so a lock or greeter that wins the mode
/// during logout still drops them. Motion and keys pass.
fn drops_while_logging_out(kind: EventKind, logout_notice: bool) -> bool {
    logout_notice && matches!(kind, EventKind::PointerButton | EventKind::PointerAxis)
}

#[cfg(test)]
mod activity_tests {
    use super::{EventKind, drops_while_logging_out};

    const ALL: [EventKind; 9] = [
        EventKind::Keyboard,
        EventKind::PointerMotion,
        EventKind::PointerButton,
        EventKind::PointerAxis,
        EventKind::DeviceAdded,
        EventKind::DeviceRemoved,
        EventKind::Switch,
        EventKind::Touch,
        EventKind::Other,
    ];

    #[test]
    fn device_enumeration_is_not_user_activity() {
        for kind in ALL {
            let device = matches!(kind, EventKind::DeviceAdded | EventKind::DeviceRemoved);
            assert_eq!(kind.is_user_activity(), !device, "{}", kind.as_str());
        }
    }

    #[test]
    fn the_logout_notice_drops_buttons_and_axis_only() {
        for kind in ALL {
            assert!(!drops_while_logging_out(kind, false), "{}", kind.as_str());
            let dropped = matches!(kind, EventKind::PointerButton | EventKind::PointerAxis);
            assert_eq!(
                drops_while_logging_out(kind, true),
                dropped,
                "{}",
                kind.as_str()
            );
        }
    }

    #[test]
    fn lock_during_logout_drops_buttons_and_axis_keeps_motion() {
        // A lock (or greeter) taken while the logout notice shows wins the mode, but the drop is
        // keyed on the notice: the button no longer reaches the lock surface (plan WP-R11).
        let mode = super::modal::resolve_mode::<()>(false, true, true, false, None);
        assert!(mode.is_locked());
        assert!(drops_while_logging_out(EventKind::PointerButton, true));
        assert!(drops_while_logging_out(EventKind::PointerAxis, true));
        assert!(!drops_while_logging_out(EventKind::PointerMotion, true));
        assert!(!drops_while_logging_out(EventKind::Keyboard, true));
    }
}
