//! Compositor state and the calloop event loop wiring.
//!
//! Future home of the central `CrucibleState`: the Smithay [`Display`], the Wayland globals
//! (compositor, SHM, seat, output, xdg-shell, …), the seat/keyboard/pointer, the desktop
//! [`Space`], and the per-backend data. It owns the [`calloop`] event loop that drives Wayland
//! client dispatch, input, and per-frame rendering, translating events into operations on the
//! pure `crucible_layout` model. Backend-agnostic; `winit` and `udev` plug their specifics in.
//!
//! Implemented in a later workstream (WS-2+); this is a WS-1 stub.

use smithay::reexports::calloop::EventLoop;

/// Creates the empty [`calloop`] event loop the compositor will be driven by.
///
/// WS-1 placeholder that exists only to *link* Smithay (it names a real Smithay reexport so the
/// dependency is exercised under `-D warnings`). The real `CrucibleState` and its handlers are
/// built on top of this loop in WS-2.
///
/// [`calloop`]: smithay::reexports::calloop
pub fn new_event_loop<T>() -> Result<EventLoop<'static, T>, smithay::reexports::calloop::Error> {
    EventLoop::try_new()
}
