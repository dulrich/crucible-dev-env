//! Keyboard/pointer focus targets and focus-following policy.
//!
//! Future home of the focus model: the `KeyboardFocusTarget`/`PointerFocusTarget` enums that
//! unify Wayland surfaces and XWayland windows under Smithay's seat, plus the policy that keeps
//! compositor focus in sync with the focused pane/window in the `crucible_layout` model and drives
//! the visual focus indication.
//!
//! Implemented in a later workstream (WS-4); this is a WS-1 stub.
