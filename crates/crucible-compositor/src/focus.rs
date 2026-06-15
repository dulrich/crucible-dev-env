//! Keyboard/pointer focus targets and focus-following policy.
//!
//! Future home of the focus model: the `KeyboardFocusTarget`/`PointerFocusTarget` enums that
//! unify Wayland surfaces and XWayland windows under Smithay's seat, plus the policy that keeps
//! compositor focus in sync with the focused pane/window in the `crucible_layout` model and drives
//! the visual focus indication.
//!
//! ## WS-3 state — focus targets are plain `WlSurface`
//! WS-3 creates the [`wl_seat`](smithay::input::Seat) global with keyboard + pointer capabilities
//! but does **not** route input events (that is WS-4). At this stage CDE follows Smithay's
//! `smallvil` reference and uses [`WlSurface`](smithay::reexports::wayland_server::protocol::wl_surface::WlSurface)
//! itself as the seat's keyboard/pointer/touch focus type (see
//! [`SeatHandler`](smithay::input::SeatHandler) in [`crate::state`]). `WlSurface` already implements
//! every focus-target trait Smithay requires, so no wrapper enum is needed yet and no input-event
//! plumbing is pulled in early.
//!
//! ## WS-4 / WS-5 seam
//! When input dispatch arrives (WS-4) and XWayland (WS-5), this module gains the
//! `KeyboardFocusTarget`/`PointerFocusTarget` enums (Wayland surface · layer · popup · X11) and the
//! seat's associated focus types switch from `WlSurface` to those enums. That is a localized change
//! to the [`SeatHandler`](smithay::input::SeatHandler) impl plus the focus-following policy that
//! lives here.
