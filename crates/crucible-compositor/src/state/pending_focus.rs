//! Deferred keyboard focus for Wayland toplevels that have not yet committed a buffer (X2).

/// Whether a keyboard-focus grant must wait for the target's first buffer (X2).
///
/// Only a Wayland toplevel that has not yet committed a buffer is deferred: clients such as
/// foot ≤1.20.1 mishandle `wl_keyboard.enter` before their initial configure/buffer. X11
/// windows, layer surfaces, popups and lock surfaces are never deferred.
#[must_use]
pub fn defer_focus(is_wayland_window: bool, has_buffer: bool) -> bool {
    is_wayland_window && !has_buffer
}

/// The single parked keyboard-focus intent awaiting a first buffer (X2).
///
/// The latest focus intent always wins: a new park overwrites, and any non-deferred focus change
/// clears the slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingFocus<W>(Option<W>);

impl<W> Default for PendingFocus<W> {
    fn default() -> Self {
        Self(None)
    }
}

impl<W> PendingFocus<W> {
    /// Parks `window` as the pending focus, replacing any earlier one.
    pub fn park(&mut self, window: W) {
        self.0 = Some(window);
    }

    /// Drops any pending focus (a later, non-deferred focus intent superseded it).
    pub fn clear(&mut self) {
        self.0 = None;
    }

    /// The parked window, if any.
    #[must_use]
    pub fn get(&self) -> Option<&W> {
        self.0.as_ref()
    }

    /// Takes the parked window when a commit of a matching surface left it `ready`.
    ///
    /// A non-matching commit, or a matching one still without a buffer, leaves the slot as is.
    pub fn take_if_ready(&mut self, matches: impl FnOnce(&W) -> bool, ready: bool) -> Option<W> {
        if ready && self.0.as_ref().is_some_and(matches) {
            self.0.take()
        } else {
            None
        }
    }
}

#[cfg(test)]
mod pending_focus_tests {
    use super::{PendingFocus, defer_focus};

    #[test]
    fn only_an_unbuffered_wayland_window_is_deferred() {
        assert!(defer_focus(true, false));
        assert!(!defer_focus(true, true));
        assert!(!defer_focus(false, false));
        assert!(!defer_focus(false, true));
    }

    #[test]
    fn park_then_later_park_overwrites() {
        let mut slot = PendingFocus::default();
        slot.park(1);
        assert_eq!(slot.get(), Some(&1));
        slot.park(2);
        assert_eq!(slot.get(), Some(&2));
    }

    #[test]
    fn clear_drops_the_parked_window() {
        // Both a `None` focus and `force_keyboard_focus` route through `clear`.
        let mut slot = PendingFocus::default();
        slot.park(1);
        slot.clear();
        assert_eq!(slot.get(), None);
    }

    #[test]
    fn matching_ready_commit_grants_once() {
        let mut slot = PendingFocus::default();
        slot.park(7);
        assert_eq!(slot.take_if_ready(|w| *w == 7, true), Some(7));
        assert_eq!(slot.get(), None);
        assert_eq!(slot.take_if_ready(|w| *w == 7, true), None);
    }

    #[test]
    fn non_matching_or_unready_commit_keeps_the_slot() {
        let mut slot = PendingFocus::default();
        slot.park(7);
        assert_eq!(slot.take_if_ready(|w| *w == 8, true), None);
        assert_eq!(slot.take_if_ready(|w| *w == 7, false), None);
        assert_eq!(slot.get(), Some(&7));
    }
}
