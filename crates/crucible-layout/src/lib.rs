//! Pure layout model for Crucible Dev Env (CDE).
//!
//! This crate owns the `Output → Workspace → Pane → Window` hierarchy and the operations over it
//! (directional split, stack, focus, move, serialize). It deliberately has **no compositor
//! dependency** so the layout logic can be exhaustively unit-tested without a running compositor —
//! this is the near-TDD core of the project. See `AGENTS.md` for the canonical vocabulary.
//!
//! The model is `Clone` and free of `Rc`/`RefCell` so it can be cheaply snapshotted and (Phase 3)
//! serialized. The public surface is:
//!
//! - typed ids: [`WindowId`], [`PaneId`], [`WorkspaceId`], [`OutputId`];
//! - model types: [`Window`], [`Pane`], [`Workspace`], [`Output`], [`World`];
//! - the [`Reconcile`] result every mutating op returns (what the compositor must apply);
//! - [`LayoutError`] for the fallible ops.

mod ids;
mod tree;
mod world;

pub use ids::{OutputId, PaneId, WindowId, WorkspaceId};
pub use world::{LayoutError, Output, Pane, Reconcile, Window, Workspace, World};

/// A cardinal direction, used for directional split, focus, and move operations.
///
/// CDE uses explicit directional verbs (e.g. `split-right`, `split-down`) rather than i3's
/// orientation-then-spawn model, so every layout mutation names the direction it acts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Toward the left.
    Left,
    /// Toward the right.
    Right,
    /// Upward.
    Up,
    /// Downward.
    Down,
}

impl Direction {
    /// Returns the direction pointing the opposite way.
    #[must_use]
    pub fn opposite(self) -> Self {
        match self {
            Direction::Left => Direction::Right,
            Direction::Right => Direction::Left,
            Direction::Up => Direction::Down,
            Direction::Down => Direction::Up,
        }
    }

    /// Returns `true` if this direction lies on the horizontal axis (left/right).
    #[must_use]
    pub fn is_horizontal(self) -> bool {
        matches!(self, Direction::Left | Direction::Right)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opposite_is_an_involution() {
        for d in [
            Direction::Left,
            Direction::Right,
            Direction::Up,
            Direction::Down,
        ] {
            assert_eq!(d.opposite().opposite(), d);
            assert_ne!(d.opposite(), d);
        }
    }

    #[test]
    fn axis_classification() {
        assert!(Direction::Left.is_horizontal());
        assert!(Direction::Right.is_horizontal());
        assert!(!Direction::Up.is_horizontal());
        assert!(!Direction::Down.is_horizontal());
    }
}
