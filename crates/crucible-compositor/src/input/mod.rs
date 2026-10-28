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

pub(crate) use execute::{ActionError, Origin, admit};
mod keyboard;
mod modal;
mod pointer;

use smithay::backend::input::{InputBackend, InputEvent};

use crate::state::{Backend, CrucibleState};
use crate::winit::WinitData;

/// Physical-keycode identity for consumed Super-leader key presses (F10). See
/// [`keyboard::SuppressedKeys`]'s docs; re-exported here (rather than making the `keyboard`
/// submodule itself public) since [`crate::state::CrucibleState::suppressed_keys`] is a public
/// field and therefore needs a public path to its type.
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
    /// per-event seat handlers the winit path uses (`process_keyboard_event`,
    /// `process_pointer_button`, `process_pointer_axis`). The difference from the winit path is the
    /// pointer-motion model: real input devices deliver **relative** motion
    /// ([`InputEvent::PointerMotion`]) rather than the absolute motion winit emits, so this routes
    /// motion through `process_pointer_motion_relative`. Device add/remove are surfaced to the
    /// caller (which configures libinput / tracks keyboards for LED sync) and otherwise ignored.
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
}
