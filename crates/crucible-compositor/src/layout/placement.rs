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
        to_logical(window_rect(
            tile,
            self.window_class(id),
            &self.config.tiling,
        ))
    }
}

/// Sends the `configure` a projected placement needs to whichever surface kind backs `element`.
///
/// Wayland toplevels only learn their size (position is purely compositor-side), so they are
/// configured only when `resized`. X11 windows also carry a position in the X server, which
/// XWayland uses for input and for placing the client's own override-redirect menus, so they are
/// configured with the full `rect` whenever it differs from their X geometry — a move alone counts.
/// Kept free of `self` so it does not borrow the whole state.
pub(super) fn configure_placement(
    element: &WindowElement,
    rect: Rectangle<i32, Logical>,
    resized: bool,
) {
    if let Some(toplevel) = element.toplevel() {
        // Every managed toplevel is tiled on all four edges so the client fills its tile instead
        // of drawing floating-window chrome (shadow margins, own sizing). Never `Maximized`.
        let had_tiled = toplevel.with_pending_state(|state| {
            let had = has_tiled_states(&state.states);
            for tiled in TILED_STATES {
                state.states.set(tiled);
            }
            if resized {
                state.size = Some(rect.size);
            }
            had
        });
        if toplevel_needs_configure(resized, had_tiled) {
            toplevel.send_configure();
        }
    } else if let Some(surface) = element.x11_surface()
        && x11_needs_configure(surface.geometry(), rect)
    {
        let _ = surface.configure(Some(rect));
    }
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
fn toplevel_needs_configure(resized: bool, had_tiled: bool) -> bool {
    resized || !had_tiled
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
}
