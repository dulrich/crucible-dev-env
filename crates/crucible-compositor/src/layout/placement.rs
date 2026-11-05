//! Window placement geometry: tile to window rect, class insets, and per-surface configure rules.

use crucible_config::TilingConfig;
use crucible_layout::{GapConfig, Rect, WindowId};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgState;
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::element::WindowElement;
use crate::state::{Backend, CrucibleState};

/// Converts a model [`Rect`] (global Space coordinates) into a Smithay logical rectangle.
#[must_use]
fn to_logical(rect: Rect) -> Rectangle<i32, Logical> {
    Rectangle::new(
        Point::from((rect.x, rect.y)),
        Size::from((rect.width.max(0), rect.height.max(0))),
    )
}

/// Layout geometry is edge to edge: tiles abut and cover the output. Per-class gaps are applied
/// to the *window* inside its tile ([`window_rect`]), never to the tile.
pub const NO_GAPS: GapConfig = GapConfig::new(0, 0);

/// Whether a managed window is a CDE-spawned terminal or an app (everything else).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowClass {
    /// A CDE-spawned terminal: fills its tile and draws the gap as its own padding.
    Terminal,
    /// Any other managed window: inset inside its tile by the app gap.
    App,
}

/// `gap` clamped so that insetting `tile` by it on every side never yields a negative size.
#[must_use]
pub fn clamped_gap(tile: Rect, gap: u32) -> i32 {
    i32::try_from(gap)
        .unwrap_or(i32::MAX)
        .min(tile.width / 2)
        .min(tile.height / 2)
        .max(0)
}

/// The rect a window of `class` occupies inside `tile`: a terminal fills the tile, an app is
/// inset by `tiling.app.gap` on every side (clamped so the size is never negative). **Pure.**
#[must_use]
pub fn window_rect(tile: Rect, class: WindowClass, tiling: &TilingConfig) -> Rect {
    match class {
        WindowClass::Terminal => tile,
        WindowClass::App => tile.inset(clamped_gap(tile, tiling.app.gap)),
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// The window rect for model window `id` placed in `tile`: its class insets applied, in Smithay
    /// logical coordinates. The one place a tile becomes a window geometry.
    pub(crate) fn placed_rect(&self, id: WindowId, tile: Rect) -> Rectangle<i32, Logical> {
        // A fullscreen window takes its whole tile (the raw output rect): no class inset.
        if self.layout.world.is_fullscreen(id) {
            return to_logical(tile);
        }
        to_logical(window_rect(
            tile,
            self.layout.window_class(id),
            &self.config.tiling,
        ))
    }
}

/// Sends the `configure` a projected placement needs to whichever surface kind backs `element`.
///
/// Wayland toplevels only learn their size (position is purely compositor-side), so they are
/// configured only when `resized` or their states change. A `fullscreen` toplevel carries
/// `Fullscreen` (and no tiled state) at the placed size; every other toplevel is tiled on all four
/// edges. X11 windows also carry a position in the X server, which XWayland uses for input and for
/// placing the client's own override-redirect menus, so they are configured with the full `rect`
/// whenever it differs from their X geometry — a move alone counts — and have their
/// `_NET_WM_STATE_FULLSCREEN` set to match. Kept free of `self` so it does not borrow the whole
/// state.
pub(super) fn configure_placement(
    element: &WindowElement,
    rect: Rectangle<i32, Logical>,
    resized: bool,
    fullscreen: bool,
) {
    if let Some(toplevel) = element.toplevel() {
        // Every managed toplevel is tiled on all four edges so the client fills its tile instead
        // of drawing floating-window chrome (shadow margins, own sizing). Never `Maximized`.
        let states_changed = toplevel.with_pending_state(|state| {
            // A placed window is visible: clear any `Suspended` set while it was hidden.
            let changed = apply_shell_states(&mut state.states, fullscreen)
                | apply_suspended(&mut state.states, false);
            if resized || changed {
                state.size = Some(rect.size);
            }
            changed
        });
        if toplevel_needs_configure(resized, !states_changed) {
            toplevel.send_configure();
        }
    } else if let Some(surface) = element.x11_surface() {
        if surface.is_fullscreen() != fullscreen {
            let _ = surface.set_fullscreen(fullscreen);
        }
        if x11_needs_configure(surface.geometry(), rect) {
            let _ = surface.configure(Some(rect));
        }
    }
}

/// Marks a window being hidden (unmapped, kept alive) as `xdg_toplevel` `Suspended` and sends the
/// configure, only when the state actually changes. X11 windows are untouched.
pub(super) fn suspend_toplevel(element: &WindowElement) {
    if let Some(toplevel) = element.toplevel()
        && toplevel.with_pending_state(|state| apply_suspended(&mut state.states, true))
    {
        toplevel.send_configure();
    }
}

/// Sets or clears `Suspended` in `states`. Returns whether anything changed. **Pure**.
fn apply_suspended(
    states: &mut smithay::wayland::shell::xdg::ToplevelStateSet,
    suspended: bool,
) -> bool {
    let had = states.contains(XdgState::Suspended);
    if suspended {
        states.set(XdgState::Suspended);
    } else {
        states.unset(XdgState::Suspended);
    }
    had != suspended
}

/// Flips a (hidden) window's fullscreen shell state without placing it: the xdg states are
/// re-sent with a configure, the X11 `_NET_WM_STATE_FULLSCREEN` is updated. Its size follows on
/// the next placement.
pub(super) fn set_fullscreen_state(element: &WindowElement, fullscreen: bool) {
    if let Some(toplevel) = element.toplevel() {
        if toplevel.with_pending_state(|state| apply_shell_states(&mut state.states, fullscreen)) {
            toplevel.send_configure();
        }
    } else if let Some(surface) = element.x11_surface()
        && surface.is_fullscreen() != fullscreen
    {
        let _ = surface.set_fullscreen(fullscreen);
    }
}

/// Sets `states` to the managed-window set: `Fullscreen` alone when `fullscreen`, else all of
/// [`TILED_STATES`] without `Fullscreen`. Returns whether anything changed. **Pure**.
fn apply_shell_states(
    states: &mut smithay::wayland::shell::xdg::ToplevelStateSet,
    fullscreen: bool,
) -> bool {
    let had = (
        states.contains(XdgState::Fullscreen),
        has_tiled_states(states),
    );
    if fullscreen {
        states.set(XdgState::Fullscreen);
        for tiled in TILED_STATES {
            states.unset(tiled);
        }
    } else {
        states.unset(XdgState::Fullscreen);
        for tiled in TILED_STATES {
            states.set(tiled);
        }
    }
    had != (fullscreen, !fullscreen)
}

/// The xdg states every managed toplevel carries: tiled on all four edges.
const TILED_STATES: [XdgState; 4] = [
    XdgState::TiledLeft,
    XdgState::TiledRight,
    XdgState::TiledTop,
    XdgState::TiledBottom,
];

/// Whether `states` already carries all of [`TILED_STATES`]. **Pure** (no surface needed).
fn has_tiled_states(states: &smithay::wayland::shell::xdg::ToplevelStateSet) -> bool {
    TILED_STATES.iter().all(|s| states.contains(*s))
}

/// Whether a placement must (re)send a toplevel configure: a size change, or the tiled states
/// being newly applied (a states-only change at the same size still needs one). **Pure**.
fn toplevel_needs_configure(resized: bool, states_current: bool) -> bool {
    resized || !states_current
}

/// Whether an X11 window whose X geometry is `current` must be configured to reach `target`.
/// **Pure** so the rule is unit-testable without an X server.
fn x11_needs_configure(current: Rectangle<i32, Logical>, target: Rectangle<i32, Logical>) -> bool {
    current != target
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x11_placement_configures_on_a_move_alone() {
        let at = |x, y, w, h| Rectangle::<i32, Logical>::new((x, y).into(), (w, h).into());
        // The regression: same size, new position (e.g. a pane on the second output) must still
        // reach the X server, or XWayland echoes the stale position back.
        assert!(x11_needs_configure(
            at(0, 0, 1920, 2160),
            at(3840, 0, 1920, 2160)
        ));
        assert!(x11_needs_configure(
            at(0, 0, 1920, 2160),
            at(0, 0, 3840, 2160)
        ));
        assert!(!x11_needs_configure(
            at(3840, 0, 1920, 2160),
            at(3840, 0, 1920, 2160)
        ));
    }

    #[test]
    fn terminals_fill_the_tile_and_apps_inset_by_their_gap_clamped() {
        let tile = Rect::new(10, 20, 400, 300);
        let mut tiling = TilingConfig::default();
        assert_eq!(window_rect(tile, WindowClass::Terminal, &tiling), tile);
        // Default app gap is 0: the app fills the tile too.
        assert_eq!(window_rect(tile, WindowClass::App, &tiling), tile);
        tiling.app.gap = 8;
        assert_eq!(
            window_rect(tile, WindowClass::App, &tiling),
            Rect::new(18, 28, 384, 284)
        );
        // Terminal gap never insets the window (it is the terminal's own padding).
        tiling.terminal.gap = 50;
        assert_eq!(window_rect(tile, WindowClass::Terminal, &tiling), tile);
        // An oversized gap clamps instead of going negative.
        tiling.app.gap = 1000;
        let w = window_rect(Rect::new(0, 0, 40, 10), WindowClass::App, &tiling);
        assert_eq!(w, Rect::new(5, 5, 30, 0));
    }

    #[test]
    fn toplevel_configure_resent_when_only_states_change() {
        assert!(toplevel_needs_configure(true, true), "resize");
        assert!(toplevel_needs_configure(false, false), "states-only change");
        assert!(
            !toplevel_needs_configure(false, true),
            "pure move: nothing new"
        );
    }

    #[test]
    fn tiled_states_cover_all_edges_and_never_maximize() {
        use smithay::wayland::shell::xdg::ToplevelStateSet;
        let mut set = ToplevelStateSet::default();
        assert!(!has_tiled_states(&set));
        for s in TILED_STATES {
            set.set(s);
        }
        assert!(has_tiled_states(&set));
        assert!(!set.contains(XdgState::Maximized));
        assert!(!TILED_STATES.contains(&XdgState::Maximized));
    }

    #[test]
    fn suspended_set_on_hide_cleared_on_place_and_never_added_otherwise() {
        use smithay::wayland::shell::xdg::ToplevelStateSet;
        let mut set = ToplevelStateSet::default();
        // Never hidden: placing changes nothing (no extra configure).
        assert!(!apply_suspended(&mut set, false));
        assert!(!set.contains(XdgState::Suspended));
        // Hidden: Suspended appears, once.
        assert!(apply_suspended(&mut set, true));
        assert!(set.contains(XdgState::Suspended));
        assert!(!apply_suspended(&mut set, true), "already suspended");
        // Placed after hidden: cleared, and that counts as a change.
        assert!(apply_suspended(&mut set, false));
        assert!(!set.contains(XdgState::Suspended));
    }
}
