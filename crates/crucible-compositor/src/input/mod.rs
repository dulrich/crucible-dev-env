//! Input handling: pointer, keyboard, and the global Super (Mod4) leader.
//!
//! Funnels the nested `winit` backend's [`InputEvent`]s (and, from the udev backend, real
//! libinput devices) into the seat so the compositor becomes interactive. The module is split by
//! concern (`reviews/2026-07-30-tn-code-review.md` F7):
//!
//! - [`keyboard`] — keyboard I/O: runs every key through the seat's
//!   [`keyboard.input`](smithay::input::keyboard::KeyboardHandle::input) filter closure, which
//!   resolves `Super`-leader chords via [`crucible_keys::resolve`] and uses
//!   [`keyboard::SuppressedKeys`] (keyed by physical keycode, never the resolved keysym) so a key
//!   whose press was intercepted never leaks a dangling release to the client even if modifiers
//!   changed in between (`reviews/2026-09-07-tn-code-review.md` F10).
//! - [`pointer`] — pointer I/O: absolute/relative motion, buttons, axis frames, and
//!   click-to-focus.
//! - [`execute`] — the action gate: [`CrucibleState::handle_key_action`] (lock → overlay → execute) dispatches a resolved
//!   [`crucible_keys::Action`] to a compositor-level effect (`Quit`/`RunTerminal`/`Run`/
//!   `VtSwitch`) or, for every model verb, through the single model-mutation entry point
//!   (`update_model`/`with_model`, see [`crate::layout`]) — one execution arm per verb.
//!
//! ## Canonical key policy lives in `crucible-keys`
//! Shortcut *meaning* — the default chord table and the [`crucible_keys::Action`] verb set — is
//! owned by the Smithay-free `crucible-keys` crate (`reviews/2026-07-30-tn-code-review.md` F1,
//! F3). This module only converts backend event types (Smithay `ModifiersState`/`Keysym`,
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
mod hover;
mod keyboard;
mod modal;
mod pointer;

use smithay::backend::input::{ButtonState, Event, InputBackend, InputEvent, PointerButtonEvent};
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
        self.idle_activity();
        // The mode is resolved once for the event; snip mode branches here, once.
        let mode = self.input_mode();
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
        self.idle_activity();
        // The mode is resolved once for the event; snip mode branches here, once. Absolute
        // motion is not handled on this path in any mode.
        let mode = self.input_mode();
        match event {
            InputEvent::PointerMotionAbsolute { .. } => {}
            event if mode.is_snipping() => self.process_snip_input::<I>(&mode, event),
            InputEvent::Keyboard { event } => self.process_keyboard_event::<I>(&mode, event),
            InputEvent::PointerMotion { event } => {
                self.process_pointer_motion_relative::<I>(&mode, event)
            }
            InputEvent::PointerButton { event } => self.process_pointer_button::<I>(&mode, event),
            InputEvent::PointerAxis { event } => self.process_pointer_axis::<I>(event),
            // Touch/gesture/device events are not handled yet; the udev source inspects
            // DeviceAdded/DeviceRemoved itself for keyboard-LED bookkeeping before calling us.
            _ => {}
        }
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
