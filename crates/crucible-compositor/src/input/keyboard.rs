//! Keyboard I/O: the seat filter closure, `Super`-leader resolution at the edge, and stable
//! consumed-key identity (`reviews/2026-09-07-tn-code-review.md` F10).
//!
//! [`CrucibleState::process_keyboard_event`] is the only place Smithay's `ModifiersState`/`Keysym`
//! meet [`crucible_keys`]: it converts them to [`crucible_keys::Modifiers`]/[`crucible_keys::Keysym`]
//! (the latter is a type-identical re-export, so no conversion code exists for it) and calls
//! [`crucible_keys::resolve`]. The binding table itself lives entirely in `crucible-keys`.

use std::collections::BTreeSet;

use smithay::backend::input::{Event, InputBackend, KeyState, KeyboardKeyEvent};
use smithay::input::keyboard::FilterResult;
use smithay::utils::SERIAL_COUNTER;
use tracing::debug;

use crate::focus::KeyboardFocusTarget;
use crate::state::{Backend, CrucibleState};

/// Physical-keycode identity for consumed Super-leader key presses (F10).
///
/// Keyed by the **physical keycode**, never the resolved keysym: a modifier change between a
/// press and its release (e.g. releasing `Shift` before `H`) must not desync which release gets
/// swallowed, because the *symbol* a physical key resolves to can change between the two events.
/// Concretely: `Super+Shift+H` intercepts `H`'s press; if `Shift` is released before `H`, `H`'s
/// release now resolves to lowercase `h` — a symbol-keyed lookup misses and forwards a release the
/// client never got a press for. The physical keycode never changes for a given key, so it is the
/// stable identity a press/release pair shares. The same coupling affects `Shift+Tab` /
/// `ISO_Left_Tab`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SuppressedKeys(BTreeSet<u32>);

impl SuppressedKeys {
    /// An empty set: nothing consumed yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `keycode`'s press as consumed (its chord matched and was intercepted).
    ///
    /// Idempotent: a repeated press (key-repeat auto-fires before any release) leaves the set
    /// unchanged rather than requiring one release per press.
    pub fn consume_press(&mut self, keycode: u32) {
        self.0.insert(keycode);
    }

    /// Consumes `keycode`'s release iff its press was recorded, removing the entry.
    ///
    /// Returns `true` when the release should be intercepted (swallowed) because its press was;
    /// `false` when it should forward. Never touches any other keycode's entry, so two distinct
    /// physical keys that happen to resolve to the same symbol never interfere with each other.
    pub fn consume_release(&mut self, keycode: u32) -> bool {
        self.0.remove(&keycode)
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Runs a keyboard key through the seat, intercepting `Super`-leader chords.
    ///
    /// The [`keyboard.input`](smithay::input::keyboard::KeyboardHandle::input) filter closure
    /// receives `&mut CrucibleState<B>` as its `state` parameter — the *same* state this method
    /// was called on — so [`SuppressedKeys`] is updated directly through it; there is no need to
    /// clone `suppressed_keys` out and write it back to dodge a borrow-checker conflict (the
    /// pre-F10 code did, unnecessarily, and it was also symbol-keyed, which was the actual bug).
    ///
    /// - on a **press**, resolves the chord via [`crucible_keys::resolve`]; a match is recorded in
    ///   `suppressed_keys` (by physical keycode) and intercepted, otherwise the key is forwarded;
    /// - on a **release**, consumes the matching `suppressed_keys` entry if present and intercepts
    ///   (so the client never sees a dangling key-up), otherwise forwards.
    ///
    /// Any intercepted [`crucible_keys::Action`] is dispatched via [`Self::handle_key_action`].
    pub(super) fn process_keyboard_event<I: InputBackend>(&mut self, evt: I::KeyboardKeyEvent) {
        let keycode = evt.key_code();
        let key_state = evt.state();
        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time_msec(&evt);

        let keyboard = self.seat.get_keyboard().unwrap();

        let action = keyboard.input(
            self,
            keycode,
            key_state,
            serial,
            time,
            |state, modifiers, handle| {
                let keysym = handle.modified_sym();
                let raw_keycode = keycode.raw();
                debug!(?key_state, mods = ?modifiers, keysym = keysym.raw(), "key");

                if let KeyState::Pressed = key_state {
                    // A press that matches a Super-chord is intercepted; record it (by physical
                    // keycode) so we can also swallow its release and never leak a key-up to the
                    // client, regardless of any modifier change in between.
                    let mods = crucible_keys::Modifiers {
                        logo: modifiers.logo,
                        shift: modifiers.shift,
                        ctrl: modifiers.ctrl,
                    };
                    let action = crucible_keys::resolve(&state.bindings, mods, keysym);
                    if action.is_some() {
                        state.suppressed_keys.consume_press(raw_keycode);
                    }
                    // The filter's `Intercept` payload type is `Option<Action>` (a release
                    // intercepts with `None`); wrap the matched action accordingly.
                    match action {
                        Some(action) => FilterResult::Intercept(Some(action)),
                        None => FilterResult::Forward,
                    }
                } else if state.suppressed_keys.consume_release(raw_keycode) {
                    FilterResult::Intercept(None)
                } else {
                    FilterResult::Forward
                }
            },
        );

        // `input` returns `Option<FilterResult::Intercept(_)>`; the outer `Option` is `None` when
        // the key was forwarded. A release carries `Some(None)` (intercepted, no action).
        if let Some(Some(action)) = action {
            self.handle_key_action(action);
        }
    }

    /// Returns the seat's current [`KeyboardFocusTarget`], if any.
    pub fn keyboard_focus(&self) -> Option<KeyboardFocusTarget> {
        self.seat.get_keyboard().and_then(|kbd| kbd.current_focus())
    }

    /// Sets (or clears) the seat's keyboard focus to `target`.
    ///
    /// Called from the shell map path (so a newly mapped toplevel/X11 window receives keys) and
    /// from the input path (click-to-focus). A fresh serial is allocated
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

    // ---- F10: consumed-release transitions -------------------------------------------------
    //
    // These operate purely on physical keycodes (`u32`) — no `Keysym`/`ModifiersState` involved —
    // demonstrating `SuppressedKeys` cannot desync when a modifier changes between press and
    // release, because it never looks at the resolved symbol at all.

    #[test]
    fn consumed_press_is_swallowed_on_release_even_if_shift_released_first() {
        // Super+Shift+H intercepts H's press; Shift's own release is a distinct physical key
        // (a different keycode) and never touches H's entry, whatever order they release in.
        const H: u32 = 43;
        let mut suppressed = SuppressedKeys::new();
        suppressed.consume_press(H);
        assert!(suppressed.consume_release(H));
    }

    #[test]
    fn shift_tab_release_is_swallowed_despite_symbol_becoming_iso_left_tab() {
        // Super+Shift+Tab intercepts Tab's press. If Shift releases first, Tab's own release now
        // resolves to a different keysym (`ISO_Left_Tab` on layouts that deliver it) — but
        // `SuppressedKeys` is keyed by keycode, so this is a non-event for it.
        const TAB: u32 = 23;
        let mut suppressed = SuppressedKeys::new();
        suppressed.consume_press(TAB);
        assert!(suppressed.consume_release(TAB));
    }

    #[test]
    fn repeated_presses_are_idempotent() {
        const H: u32 = 43;
        let mut suppressed = SuppressedKeys::new();
        suppressed.consume_press(H); // initial press
        suppressed.consume_press(H); // auto-repeat fires before any release
        assert!(suppressed.consume_release(H));
        // Exactly one release consumes the entry, not two.
        assert!(!suppressed.consume_release(H));
    }

    #[test]
    fn unmatched_release_is_forwarded() {
        let mut suppressed = SuppressedKeys::new();
        assert!(!suppressed.consume_release(99));
    }

    #[test]
    fn two_distinct_keys_sharing_a_symbol_are_independent() {
        // Two different physical keys that happen to resolve to the same keysym (e.g. under a
        // layout with multiple levels/groups) must not share suppression state.
        const KEY_A: u32 = 43;
        const KEY_B: u32 = 111;
        let mut suppressed = SuppressedKeys::new();
        suppressed.consume_press(KEY_A);
        suppressed.consume_press(KEY_B);
        assert!(suppressed.consume_release(KEY_A));
        // KEY_B's press is still recorded independently of KEY_A's release.
        assert!(suppressed.consume_release(KEY_B));
    }

    #[test]
    fn default_is_empty() {
        let mut suppressed = SuppressedKeys::default();
        assert!(!suppressed.consume_release(1));
    }
}
