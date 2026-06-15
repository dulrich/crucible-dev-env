//! Typed identifiers for the layout model.
//!
//! Every entity in the model (`Window`, `Pane`, `Workspace`, `Output`) is referred to by a `Copy`
//! newtype id over `u64` rather than by reference (decision #5 in `plans/02-tiling-model.md`). This
//! keeps the model `Clone` and serde-friendly (Phase 3) and lets the compositor hold a stable
//! `WindowId ↔ WindowElement` map.
//!
//! Ids are handed out by a monotonically increasing [`IdGen`] held inside the `World`, so an id is
//! never reused in a way that aliases a live-or-once-live entity for the lifetime of a `World`.

/// Generates a sequence of `u64` ids that never repeat for the life of the generator.
///
/// The model embeds one of these per id space (windows, panes, workspaces, outputs) so removing an
/// entity never frees its id for reuse — preventing the classic ABA aliasing bug where a stale
/// handle silently resolves to a different entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IdGen {
    next: u64,
}

impl IdGen {
    /// Creates a fresh generator that will hand out `0, 1, 2, …`.
    pub(crate) fn new() -> Self {
        Self { next: 0 }
    }

    /// Returns the next raw id and advances the counter.
    pub(crate) fn alloc(&mut self) -> u64 {
        let id = self.next;
        self.next += 1;
        id
    }
}

impl Default for IdGen {
    fn default() -> Self {
        Self::new()
    }
}

/// Declares a `Copy` newtype id over `u64` with the standard derives and a `raw`/`from_raw` pair.
macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(u64);

        impl $name {
            /// Wraps a raw `u64` as this id. Primarily for tests and (Phase 3) deserialization.
            #[must_use]
            pub const fn from_raw(raw: u64) -> Self {
                Self(raw)
            }

            /// Returns the underlying `u64`.
            #[must_use]
            pub const fn raw(self) -> u64 {
                self.0
            }
        }
    };
}

define_id!(
    /// Identifies a [`Window`](crate::Window) — a Wayland/X11 surface living inside a pane.
    WindowId
);
define_id!(
    /// Identifies a [`Pane`](crate::Pane) — a screen-geometry slot holding a stack of windows.
    PaneId
);
define_id!(
    /// Identifies a [`Workspace`](crate::Workspace) — a named, stackable arrangement of panes.
    WorkspaceId
);
define_id!(
    /// Identifies an [`Output`](crate::Output) — a physical monitor.
    OutputId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_gen_is_monotonic_and_never_repeats() {
        let mut alloc = IdGen::new();
        let ids: Vec<u64> = (0..5).map(|_| alloc.alloc()).collect();
        assert_eq!(ids, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn raw_round_trips() {
        assert_eq!(WindowId::from_raw(7).raw(), 7);
        assert_eq!(PaneId::from_raw(0).raw(), 0);
    }

    #[test]
    fn distinct_ids_are_unequal() {
        assert_ne!(WindowId::from_raw(1), WindowId::from_raw(2));
        assert_eq!(WindowId::from_raw(3), WindowId::from_raw(3));
    }
}
