//! Input handling: pointer, keyboard, and Super-leader dispatch.
//!
//! Future home of input processing: routes `libinput`/`winit` events through the seat, maintains
//! pointer focus and keyboard focus, and applies the global Super (Mod4) leader grab — every
//! non-Super key passes through to the focused client, while Super-prefixed chords are resolved by
//! `crucible_keys` into layout verbs. Keymap state comes from `xkbcommon`.
//!
//! Implemented in a later workstream (WS-4); this is a WS-1 stub.
//!
//! WS-1 references [`xkbcommon`] below only to *link* the dependency under `-D warnings`; real
//! keymap handling lands in WS-4.

use xkbcommon as _;
