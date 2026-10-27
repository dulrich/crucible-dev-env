//! Window placement: pending placements keyed by spawned PID ([`Target`]), PID-ancestor matching,
//! restore-order insertion, and the new-toplevel map paths.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crucible_layout::{PaneId, WindowId, WorkspaceId};
use tracing::{info, warn};

use super::proc_root;
use crate::element::WindowElement;
use crate::layout::{WindowClass, WindowMeta};
use crate::state::{Backend, CrucibleState};

/// Pending placements and restore order (a field of [`super::PersistState`]).
#[derive(Debug, Default)]
pub(super) struct PlacementState {
    pub(super) pending: HashMap<u32, Pending>,
    pub(super) placed: HashMap<PaneId, Vec<(WindowId, usize)>>,
}

/// How long a pending placement waits for its window to map.
pub const PENDING_TTL: Duration = Duration::from_secs(30);

/// How far up `/proc` a client PID is walked looking for a spawned/pending ancestor.
pub const ANCESTOR_DEPTH: usize = 8;

/// Where a window expected from a spawned PID should land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Into `pane` of a restored workspace, at its saved stack position.
    Pane {
        /// The restored pane.
        pane: PaneId,
        /// The window's index in the saved stack.
        saved_index: usize,
        /// Whether it was the pane's focused window.
        focused: bool,
    },
    /// Into the live slot a relaunch placeholder held: `pane` at `index` (its position in the
    /// pane's stack when Enter was pressed, never the saved order).
    Slot {
        /// The placeholder's pane.
        pane: PaneId,
        /// The placeholder's current index in that pane's stack.
        index: usize,
        /// Whether the placeholder was the pane's focused window.
        focused: bool,
        /// The placeholder's restore placement entry (`placed`), inherited by the app.
        saved_index: Option<usize>,
    },
    /// An overlay (picker / launcher; unmanaged, centred, keyboard-focused).
    Overlay,
    /// A pre-warmed overlay host: stored hidden in the warm slot, never mapped until requested.
    WarmOverlay,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Pending {
    pub(super) target: Target,
    pub(super) since: Instant,
}

/// The key `has` accepts among `pid` itself and then its `ancestors` (parent first).
#[must_use]
pub fn match_pending(pid: u32, ancestors: &[u32], has: impl Fn(u32) -> bool) -> Option<u32> {
    std::iter::once(pid)
        .chain(ancestors.iter().copied())
        .find(|p| has(*p))
}

/// The stack index a restored window takes: the number of already-placed windows in its pane
/// whose saved index is smaller, so saved order survives any map order.
#[must_use]
pub fn insert_index(placed_saved: impl IntoIterator<Item = usize>, saved_index: usize) -> usize {
    placed_saved
        .into_iter()
        .filter(|i| *i < saved_index)
        .count()
}

/// The live slot of `target` in a pane stack `windows` whose focused index is `focused`:
/// `(index, was_focused)`, or `None` when it is not in the stack.
#[must_use]
pub fn slot_for(windows: &[WindowId], focused: usize, target: WindowId) -> Option<(usize, bool)> {
    let index = windows.iter().position(|w| *w == target)?;
    Some((index, index == focused))
}

/// The index a relaunched app maps at: the recorded slot, clamped to the live stack length.
#[must_use]
pub fn slot_index(index: usize, live_len: usize) -> usize {
    index.min(live_len)
}

impl<B: Backend + 'static> CrucibleState<B> {
    pub(super) fn workspace_has_pending(&self, ws: WorkspaceId) -> bool {
        let Some(w) = self.layout.world.workspace(ws) else {
            return false;
        };
        let panes = w.panes();
        self.persist
            .placement
            .pending
            .values()
            .any(|p| match p.target {
                Target::Pane { pane, .. } | Target::Slot { pane, .. } => panes.contains(&pane),
                Target::Overlay | Target::WarmOverlay => false,
            })
    }

    // ---- mapping new windows -------------------------------------------------------------------

    /// The client PID of an xdg toplevel's Wayland client.
    #[must_use]
    pub fn wayland_client_pid(
        &self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) -> Option<u32> {
        use smithay::reexports::wayland_server::Resource;
        let client = surface.client()?;
        let creds = client.get_credentials(&self.display_handle).ok()?;
        u32::try_from(creds.pid).ok()
    }

    /// Maps a new toplevel (xdg or X11 managed) from client `pid`: into its pending restore
    /// placement, as the picker overlay, or through the normal focused-pane path. Records whether
    /// it is a CDE-spawned terminal.
    pub fn map_new_window(&mut self, element: WindowElement, pid: Option<u32>) {
        let ancestors = pid
            .map(|p| crucible_persist::ancestors(proc_root(), p, ANCESTOR_DEPTH))
            .unwrap_or_default();
        let pending = pid
            .and_then(|p| {
                match_pending(p, &ancestors, |k| {
                    self.persist.placement.pending.contains_key(&k)
                })
            })
            .and_then(|k| self.persist.placement.pending.remove(&k));
        // The matched spawn (pid or an ancestor) feeds the latency line and carries the origin
        // record (what a restored window / relaunched app stands for) to capture time.
        let mut origin = None;
        if let Some((k, rec)) = pid
            .and_then(|p| match_pending(p, &ancestors, |k| self.persist.spawn.get(k).is_some()))
            .and_then(|k| self.persist.spawn.get(k).map(|rec| (k, rec)))
        {
            info!(
                kind = ?rec.kind,
                program = %rec.program,
                pid = k,
                spawn_to_map_ms = u64::try_from(rec.ms_until(Instant::now())).unwrap_or(u64::MAX),
                "latency: spawn→map"
            );
            origin.clone_from(&rec.origin);
        }
        // The class must be known before the first placement (every mapping path applies the
        // layout), so the window's first rect already honours terminal vs app.
        let class = if pid.is_some_and(|p| self.persist.spawn.is_terminal_spawn(p)) {
            WindowClass::Terminal
        } else {
            WindowClass::App
        };
        let meta = WindowMeta { pid, class, origin };
        match pending.map(|p| p.target) {
            Some(Target::Overlay) => self.map_overlay(element),
            Some(Target::WarmOverlay) => self.store_warm(element, pid),
            Some(Target::Pane {
                pane,
                saved_index,
                focused,
            }) => {
                self.map_placed(element, pane, saved_index, focused, meta);
            }
            Some(Target::Slot {
                pane,
                index,
                focused,
                saved_index,
            }) => {
                self.map_slot(element, pane, index, focused, saved_index, meta);
            }
            None => {
                self.map_managed_window(element, meta);
            }
        }
    }

    pub(super) fn map_placed(
        &mut self,
        element: WindowElement,
        pane: PaneId,
        saved_index: usize,
        focused: bool,
        meta: WindowMeta,
    ) -> WindowId {
        let live: Vec<WindowId> = self
            .layout
            .world
            .pane(pane)
            .map(|p| p.windows().to_vec())
            .unwrap_or_default();
        let placed = self.persist.placement.placed.entry(pane).or_default();
        placed.retain(|(w, _)| live.contains(w));
        let index = insert_index(placed.iter().map(|(_, i)| *i), saved_index);
        match self.map_managed_window_at(element.clone(), meta.clone(), pane, index) {
            Ok(id) => {
                self.finish_placement(pane, id, Some(saved_index), focused);
                id
            }
            Err(err) => {
                warn!(%err, "restore placement failed; using the focused pane");
                self.map_managed_window(element, meta)
            }
        }
    }

    /// Records a placed window's saved index (if any) and applies its stack focus.
    pub(super) fn finish_placement(
        &mut self,
        pane: PaneId,
        id: WindowId,
        saved_index: Option<usize>,
        focused: bool,
    ) {
        if let Some(saved_index) = saved_index {
            self.persist
                .placement
                .placed
                .entry(pane)
                .or_default()
                .push((id, saved_index));
        }
        if focused {
            let pos = self
                .layout
                .world
                .pane(pane)
                .and_then(|p| p.windows().iter().position(|w| *w == id));
            if let Some(pos) = pos {
                let _ = self.with_model(|w| w.set_stack_focus(pane, pos));
            }
        }
    }

    /// Maps a relaunched app into the slot its placeholder held (clamped to the live stack).
    pub(super) fn map_slot(
        &mut self,
        element: WindowElement,
        pane: PaneId,
        index: usize,
        focused: bool,
        saved_index: Option<usize>,
        meta: WindowMeta,
    ) -> WindowId {
        let Some(live_len) = self.layout.world.pane(pane).map(|p| p.windows().len()) else {
            warn!(?pane, "relaunch pane is gone; using the focused pane");
            return self.map_managed_window(element, meta);
        };
        match self.map_managed_window_at(
            element.clone(),
            meta.clone(),
            pane,
            slot_index(index, live_len),
        ) {
            Ok(id) => {
                self.finish_placement(pane, id, saved_index, focused);
                id
            }
            Err(err) => {
                warn!(%err, "relaunch placement failed; using the focused pane");
                self.map_managed_window(element, meta)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_layout::World;
    use std::collections::HashSet;

    #[test]
    fn match_pending_prefers_the_pid_then_the_nearest_ancestor() {
        let pending: HashSet<u32> = [10, 20].into();
        let has = |k| pending.contains(&k);
        assert_eq!(match_pending(10, &[20], has), Some(10));
        assert_eq!(match_pending(5, &[7, 20, 10], has), Some(20));
        assert_eq!(match_pending(5, &[7, 8], has), None);
    }

    #[test]
    fn insert_index_restores_saved_order_in_any_map_order() {
        // Saved stack [a0, a1, a2] maps as a2, a0, a1.
        let mut stack: Vec<usize> = Vec::new();
        let mut placed: Vec<usize> = Vec::new();
        for saved in [2, 0, 1] {
            let at = insert_index(placed.iter().copied(), saved);
            stack.insert(at, saved);
            placed.push(saved);
        }
        assert_eq!(stack, vec![0, 1, 2]);
        assert_eq!(insert_index([], 3), 0);
    }

    #[test]
    fn slot_for_reports_current_index_and_focus() {
        let mut w = World::new();
        let a = w.open_window();
        let b = w.open_window();
        let pane = w.focused_pane_id();
        let windows = w.pane(pane).unwrap().windows().to_vec();
        let focused = w.pane(pane).unwrap().focused_index();
        let ia = windows.iter().position(|x| *x == a).unwrap();
        let ib = windows.iter().position(|x| *x == b).unwrap();
        assert_eq!(slot_for(&windows, focused, a), Some((ia, ia == focused)));
        assert_eq!(slot_for(&windows, focused, b), Some((ib, ib == focused)));
        assert_eq!(slot_for(&[], 0, a), None);
        assert_eq!(slot_index(5, 2), 2);
        assert_eq!(slot_index(1, 2), 1);
    }

    /// Audit E1: the slot is the placeholder's live index, not its saved-order index.
    #[test]
    fn relaunch_slot_uses_live_index_not_saved_order() {
        let mut w = World::new();
        let pane = w.focused_pane_id();
        let t1 = w.open_window_at(pane, 0).unwrap();
        let p = w.open_window_at(pane, 1).unwrap();
        let t2 = w.open_window_at(pane, 2).unwrap();
        // L: an ordinary window opened after restore, no `placed` entry, lands before P.
        let l = w.open_window_at(pane, 1).unwrap();
        let windows = w.pane(pane).unwrap().windows().to_vec();
        assert_eq!(windows, vec![t1, l, p, t2]);
        let placed = [(t1, 0usize), (p, 1), (t2, 2)];
        // Saved order would put P at 1 (wrong: before L).
        let saved_order = insert_index(placed.iter().filter(|(w, _)| *w != p).map(|(_, i)| *i), 1);
        assert_eq!(saved_order, 1);
        w.set_stack_focus(pane, 2).unwrap();
        let (index, focused) =
            slot_for(&windows, w.pane(pane).unwrap().focused_index(), p).unwrap();
        assert_eq!((index, focused), (2, true));
        w.close_window(p).unwrap();
        let live_len = w.pane(pane).unwrap().windows().len();
        let app = w.open_window_at(pane, slot_index(index, live_len)).unwrap();
        assert_eq!(w.pane(pane).unwrap().windows(), vec![t1, l, app, t2]);
    }
}
