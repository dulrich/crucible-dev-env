//! Keyboard I/O: the seat filter closure, `Super`-leader resolution at the edge, and stable
//! consumed-key identity.
//!
//! [`CrucibleState::process_keyboard_event`] is the only place Smithay's `ModifiersState`/`Keysym`
//! meet [`crucible_keys`]: it converts them to [`crucible_keys::Modifiers`]/[`crucible_keys::Keysym`]
//! (the latter is a type-identical re-export, so no conversion code exists for it) and calls
//! [`crucible_keys::resolve`]. The binding table itself lives entirely in `crucible-keys`.

use std::collections::BTreeSet;

use smithay::backend::input::{Event, InputBackend, KeyState, KeyboardKeyEvent};
use smithay::input::keyboard::{FilterResult, Keysym};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::SERIAL_COUNTER;
use smithay::wayland::seat::WaylandFocus;
use tracing::debug;

use super::modal::InputMode;
use crate::focus::KeyboardFocusTarget;
use crate::snip::Event as SnipEvent;
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

/// What an intercepted key press does.
#[derive(Debug, Clone, PartialEq, Eq)]
enum KeyHit {
    /// Run this (admitted) chord action.
    Action(crucible_keys::Action),
    /// Snip mode: feed this event to the selection (`None`: the key is just swallowed).
    Snip(Option<SnipEvent>),
}

/// The selection event a key press makes in snip mode: `Escape` cancels, `Return`/`KP_Enter`
/// accept; every other key is swallowed.
#[must_use]
fn snip_key(keysym: Keysym) -> Option<SnipEvent> {
    match keysym {
        Keysym::Escape => Some(SnipEvent::Esc),
        Keysym::Return | Keysym::KP_Enter => Some(SnipEvent::Enter),
        _ => None,
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
    ///
    /// `mode` is the event's input mode, resolved once by the router.
    pub(super) fn process_keyboard_event<I: InputBackend>(
        &mut self,
        mode: &InputMode,
        evt: I::KeyboardKeyEvent,
    ) {
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
                    // The one admission gate (`admit`, shared with the action gate): while locked
                    // only VT switching runs (every other key goes to the locker); while snipping
                    // only `snip` (cancel) and VT switches run as chords.
                    let action = crucible_keys::resolve(&state.bindings, mods, keysym)
                        .filter(|a| super::execute::admit(mode, a).is_ok());
                    // The single snip branch: every press is intercepted; a non-admitted one
                    // feeds the selection (Esc / Enter) or is swallowed.
                    if mode.is_snipping() {
                        state.suppressed_keys.consume_press(raw_keycode);
                        return FilterResult::Intercept(Some(match action {
                            Some(action) => KeyHit::Action(action),
                            None => KeyHit::Snip(snip_key(keysym)),
                        }));
                    }
                    if action.is_some() {
                        state.suppressed_keys.consume_press(raw_keycode);
                    }
                    // The filter's `Intercept` payload type is `Option<KeyHit>` (a release
                    // intercepts with `None`); wrap the matched action accordingly.
                    match action {
                        Some(action) => FilterResult::Intercept(Some(KeyHit::Action(action))),
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
        match action {
            Some(Some(KeyHit::Action(action))) => {
                // Failures are already logged by the gate at their usual levels.
                let _ = self.handle_key_action(action, super::Origin::Chord);
            }
            Some(Some(KeyHit::Snip(Some(event)))) => self.snip_event(event),
            _ => {}
        }
    }

    /// Returns the seat's current [`KeyboardFocusTarget`], if any.
    pub fn keyboard_focus(&self) -> Option<KeyboardFocusTarget> {
        self.seat.get_keyboard().and_then(|kbd| kbd.current_focus())
    }

    /// The `WlSurface` of the seat's current keyboard focus, if any (the pointer-constraint
    /// policy's "keyboard on surface" input).
    pub(crate) fn keyboard_focus_surface(&self) -> Option<WlSurface> {
        self.keyboard_focus()
            .and_then(|f| f.wl_surface().map(|s| s.into_owned()))
    }

    /// Sets (or clears) the seat's keyboard focus to `target`.
    ///
    /// Called from the shell map path (so a newly mapped toplevel/X11 window receives keys) and
    /// from the input path (click-to-focus). A fresh serial is allocated
    /// for the focus change. When `target` is an XWayland window the seat's
    /// [`KeyboardTarget`](smithay::input::keyboard::KeyboardTarget) impl drives the X11
    /// `set_input_focus`/take-focus that activates it.
    pub fn set_keyboard_focus(&mut self, target: Option<KeyboardFocusTarget>) {
        // While the session is locked, focus never leaves the lock surface (`crate::lock`).
        if self.is_session_locked() && !matches!(target, Some(KeyboardFocusTarget::LockSurface(_)))
        {
            return;
        }
        // X2: a Wayland toplevel without its first buffer is parked and granted on that commit
        // (`CompositorHandler::commit`); the old focus stays. Any other target supersedes it.
        if let Some(KeyboardFocusTarget::Window(w)) = &target
            && let Some(root) = w.wl_surface()
            && crate::state::defer_focus(!w.is_x11(), crate::element::surface_has_buffer(&root))
        {
            self.pending_keyboard_focus.park(w.clone());
            return;
        }
        self.force_keyboard_focus(target);
    }

    /// Sets keyboard focus unconditionally (the lock glue's own path; see [`Self::set_keyboard_focus`]).
    pub(crate) fn force_keyboard_focus(&mut self, target: Option<KeyboardFocusTarget>) {
        self.pending_keyboard_focus.clear();
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

    #[test]
    fn snip_keys_map_escape_and_enter_only() {
        assert_eq!(snip_key(Keysym::Escape), Some(SnipEvent::Esc));
        assert_eq!(snip_key(Keysym::Return), Some(SnipEvent::Enter));
        assert_eq!(snip_key(Keysym::KP_Enter), Some(SnipEvent::Enter));
        assert_eq!(snip_key(Keysym::a), None);
        assert_eq!(snip_key(Keysym::space), None);
    }

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

    /// Plan 23 audit E3: chords match the keysym the active layout produces, so the physical
    /// key that is `-` (`Super+minus`, split-down) under `us` yields `apostrophe` under `es`.
    #[test]
    fn punctuation_keysyms_follow_the_active_layout() {
        use xkbcommon::xkb;
        let sym = |layout: &str, evdev_key: u32| {
            let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
            let keymap = xkb::Keymap::new_from_names(
                &ctx,
                "",
                "",
                layout,
                "",
                None,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
            .expect("keymap compiles");
            let state = xkb::State::new(&keymap);
            state.key_get_one_sym(xkb::Keycode::new(evdev_key + 8))
        };
        // evdev KEY_MINUS = 12.
        assert_eq!(sym("us", 12), xkb::Keysym::minus);
        assert_eq!(sym("es", 12), xkb::Keysym::apostrophe);
        // Letters are unaffected: evdev KEY_E = 18.
        assert_eq!(sym("es", 18), xkb::Keysym::e);
    }
}
