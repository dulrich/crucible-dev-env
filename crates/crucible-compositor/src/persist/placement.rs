//! Window placement: pending placements keyed by spawned PID ([`Target`]), PID-ancestor matching,
//! restore-order insertion, and the new-toplevel map paths.

use std::collections::{HashMap, VecDeque};
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
    /// Per relaunch spawn pid: the slots still to fill, in order (P's slot first, then each
    /// sibling placeholder). The spawn's `pending` entry stays until its queue is empty.
    pub(super) slot_queues: HashMap<u32, VecDeque<SlotEntry>>,
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

/// One slot a relaunch spawn will fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotEntry {
    /// A slot already resolved (the relaunched placeholder's, whose window is closed at relaunch).
    Resolved(Target),
    /// A sibling placeholder, still on screen: its slot is read from its live position when
    /// filled, and the placeholder is closed then.
    Placeholder(WindowId),
}

/// The outcome of offering a new toplevel to a spawn's slot queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Claim {
    /// The slot taken, or `None` when the toplevel maps normally.
    pub entry: Option<SlotEntry>,
    /// Whether the queue is now exhausted (the spawn's pending entry should be dropped).
    pub done: bool,
}

/// Offers a new toplevel to `queue`: a dialog (`has_parent`) takes nothing; otherwise the next
/// entry whose placeholder is still `alive` is taken (dead sibling placeholders are skipped).
pub fn claim_slot(
    queue: &mut VecDeque<SlotEntry>,
    has_parent: bool,
    alive: impl Fn(WindowId) -> bool,
) -> Claim {
    if has_parent {
        return Claim {
            entry: None,
            done: queue.is_empty(),
        };
    }
    while let Some(entry) = queue.pop_front() {
        let live = match entry {
            SlotEntry::Resolved(_) => true,
            SlotEntry::Placeholder(id) => alive(id),
        };
        if live {
            return Claim {
                entry: Some(entry),
                done: queue.is_empty(),
            };
        }
    }
    Claim {
        entry: None,
        done: true,
    }
}

/// The sibling placeholders of `own`: `candidates` (in live workspace/pane/stack order, each with
/// its relaunch cmdline if it is a placeholder) other than `own`, whose cmdline equals `cmdline`
/// and which no other spawn has `claimed`.
#[must_use]
pub fn select_siblings(
    candidates: impl IntoIterator<Item = (WindowId, Option<String>)>,
    own: WindowId,
    cmdline: &str,
    claimed: impl Fn(WindowId) -> bool,
) -> Vec<WindowId> {
    candidates
        .into_iter()
        .filter(|(id, c)| *id != own && c.as_deref() == Some(cmdline) && !claimed(*id))
        .map(|(id, _)| id)
        .collect()
}

/// Drops the queues of spawns whose pending entry is gone (expired or closed with its workspace).
pub fn prune_queues(
    queues: &mut HashMap<u32, VecDeque<SlotEntry>>,
    has_pending: impl Fn(u32) -> bool,
) {
    queues.retain(|pid, _| has_pending(*pid));
}

/// Whether `id` is a sibling placeholder still queued by some live relaunch spawn.
#[must_use]
pub fn is_queued(queues: &HashMap<u32, VecDeque<SlotEntry>>, id: WindowId) -> bool {
    queues
        .values()
        .any(|q| q.contains(&SlotEntry::Placeholder(id)))
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
        let key = pid.and_then(|p| {
            match_pending(p, &ancestors, |k| {
                self.persist.placement.pending.contains_key(&k)
            })
        });
        let has_parent = element.toplevel().is_some_and(|t| t.parent().is_some())
            || element
                .x11_surface()
                .is_some_and(|s| s.is_transient_for().is_some());
        let target = key.and_then(|k| self.take_pending_target(k, has_parent));
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
        match target {
            Some(Target::Overlay) => self.overlay_arrived(element, pid),
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

    /// The target the new toplevel of spawn `key` takes. A relaunch spawn hands out its queued
    /// slots one per toplevel (a dialog takes none) and keeps its pending entry until the queue is
    /// empty; every other target is consumed whole, as before.
    fn take_pending_target(&mut self, key: u32, has_parent: bool) -> Option<Target> {
        let Target::Slot {
            pane,
            index,
            focused,
            saved_index,
        } = self.persist.placement.pending.get(&key)?.target
        else {
            return self
                .persist
                .placement
                .pending
                .remove(&key)
                .map(|p| p.target);
        };
        let mut queue = self
            .persist
            .placement
            .slot_queues
            .remove(&key)
            .unwrap_or_else(|| {
                VecDeque::from([SlotEntry::Resolved(Target::Slot {
                    pane,
                    index,
                    focused,
                    saved_index,
                })])
            });
        let claim = claim_slot(&mut queue, has_parent, |id| {
            self.layout.world.pane_of_window(id).is_some()
        });
        if claim.done {
            self.persist.placement.pending.remove(&key);
        } else {
            self.persist.placement.slot_queues.insert(key, queue);
        }
        match claim.entry? {
            SlotEntry::Resolved(target) => Some(target),
            SlotEntry::Placeholder(id) => {
                let (pane, index, focused, saved_index) = self.placeholder_slot(id)?;
                // The app takes the placeholder's live slot: close the placeholder. Its
                // `crucible relaunch` is still waiting for Enter, so asking the terminal window
                // to close would make the terminal confirm killing a running process (ghostty's
                // "Close window?" dialog). End the wrapper instead; its terminal then closes on
                // its own. Only when no wrapper is found is the window asked to close.
                let wrapper = self
                    .layout
                    .meta(id)
                    .and_then(|m| m.pid)
                    .and_then(|pid| crucible_persist::relaunch_wrapper_of(proc_root(), pid));
                let ended = wrapper.is_some_and(|(pid, _)| terminate(pid));
                if !ended && let Some(element) = self.layout.element(id).cloned() {
                    super::close_element(&element);
                }
                Some(Target::Slot {
                    pane,
                    index,
                    focused,
                    saved_index,
                })
            }
        }
    }

    /// The live slot of placeholder window `id`: `(pane, index, focused, saved_index)`.
    pub(super) fn placeholder_slot(
        &self,
        id: WindowId,
    ) -> Option<(PaneId, usize, bool, Option<usize>)> {
        let world = &self.layout.world;
        let pane_id = world.pane_of_window(id)?;
        let pane = world.pane(pane_id)?;
        let (index, focused) = slot_for(pane.windows(), pane.focused_index(), id)?;
        let saved_index = self
            .persist
            .placement
            .placed
            .get(&pane_id)
            .and_then(|v| v.iter().find(|(w, _)| *w == id).map(|(_, i)| *i));
        Some((pane_id, index, focused, saved_index))
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

/// Sends SIGTERM to `pid`; whether it was delivered.
fn terminate(pid: u32) -> bool {
    let Some(pid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return false;
    };
    match rustix::process::kill_process(pid, rustix::process::Signal::TERM) {
        Ok(()) => true,
        Err(err) => {
            warn!(%err, pid = pid.as_raw_nonzero().get(), "cannot end a placeholder wrapper");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_layout::World;
    use std::collections::HashSet;

    /// A filled sibling placeholder's wrapper is ended with SIGTERM (never its window asked to
    /// close, which makes ghostty confirm); an unknown pid reports failure so the caller falls
    /// back to closing the window.
    #[test]
    fn terminate_ends_the_process_and_reports_failure() {
        use std::os::unix::process::ExitStatusExt;
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        assert!(terminate(child.id()));
        let status = child.wait().unwrap();
        const SIGTERM: i32 = 15; // Linux
        assert_eq!(status.signal(), Some(SIGTERM));
        assert!(!terminate(0));
        assert!(!terminate(u32::MAX));
    }

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
        let mut w = World::new("1");
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
        let mut w = World::new("1");
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

    // ---- relaunch slot queue -------------------------------------------------------------------

    fn own_slot() -> SlotEntry {
        SlotEntry::Resolved(Target::Slot {
            pane: World::new("x").focused_pane_id(),
            index: 0,
            focused: false,
            saved_index: None,
        })
    }

    fn ids(n: usize) -> Vec<WindowId> {
        let mut w = World::new("1");
        (0..n).map(|_| w.open_window()).collect()
    }

    #[test]
    fn siblings_are_selected_by_cmdline_in_order_and_fill_in_saved_order() {
        let w = ids(5);
        let (p, s1, other, s2, claimed) = (w[0], w[1], w[2], w[3], w[4]);
        let cands = vec![
            (s1, Some("firefox".to_string())),
            (p, Some("firefox".to_string())),
            (other, Some("gimp".to_string())),
            (s2, Some("firefox".to_string())),
            (claimed, Some("firefox".to_string())),
            (WindowId::from_raw(9999), None),
        ];
        let sibs = select_siblings(cands, p, "firefox", |id| id == claimed);
        assert_eq!(sibs, vec![s1, s2]);
        let mut q = VecDeque::from([own_slot()]);
        q.extend(sibs.iter().map(|s| SlotEntry::Placeholder(*s)));
        let order: Vec<_> = (0..3)
            .map(|_| claim_slot(&mut q, false, |_| true))
            .collect();
        assert_eq!(order[0].entry, Some(own_slot()));
        assert_eq!(order[1].entry, Some(SlotEntry::Placeholder(s1)));
        assert_eq!(order[2].entry, Some(SlotEntry::Placeholder(s2)));
        assert_eq!(
            [order[0].done, order[1].done, order[2].done],
            [false, false, true]
        );
    }

    #[test]
    fn extra_toplevels_beyond_the_slots_map_normally() {
        let mut q = VecDeque::from([own_slot()]);
        assert!(claim_slot(&mut q, false, |_| true).done);
        // The pending entry is gone after `done`; an empty queue claims nothing anyway.
        let extra = claim_slot(&mut q, false, |_| true);
        assert_eq!(extra.entry, None);
        assert!(extra.done);
    }

    #[test]
    fn fewer_toplevels_than_slots_leave_the_rest_queued_then_expired() {
        let w = ids(2);
        let mut q = VecDeque::from([own_slot(), SlotEntry::Placeholder(w[1])]);
        let first = claim_slot(&mut q, false, |_| true);
        assert!(!first.done, "the sibling is still waiting");
        let mut queues = HashMap::from([(7u32, q)]);
        assert!(is_queued(&queues, w[1]));
        // Expiry drops the spawn's queue: the leftover placeholder is an ordinary one again.
        prune_queues(&mut queues, |_| false);
        assert!(!is_queued(&queues, w[1]));
        let mut live = HashMap::from([(7u32, VecDeque::from([SlotEntry::Placeholder(w[1])]))]);
        prune_queues(&mut live, |pid| pid == 7);
        assert!(is_queued(&live, w[1]));
    }

    #[test]
    fn a_dialog_does_not_consume_a_slot() {
        let w = ids(2);
        let mut q = VecDeque::from([own_slot(), SlotEntry::Placeholder(w[1])]);
        let dialog = claim_slot(&mut q, true, |_| true);
        assert_eq!((dialog.entry, dialog.done), (None, false));
        assert_eq!(q.len(), 2);
        assert_eq!(claim_slot(&mut q, false, |_| true).entry, Some(own_slot()));
    }

    #[test]
    fn a_queued_sibling_joins_the_live_spawn_and_dead_ones_are_skipped() {
        let w = ids(3);
        let q = VecDeque::from([own_slot(), SlotEntry::Placeholder(w[1])]);
        let queues = HashMap::from([(7u32, q.clone())]);
        // A hand relaunch checks `is_queued` and returns without spawning.
        assert!(is_queued(&queues, w[1]));
        assert!(!is_queued(&queues, w[2]));
        // A sibling closed by hand before its turn is skipped, not consumed.
        let mut q = q;
        q.push_back(SlotEntry::Placeholder(w[2]));
        let _ = claim_slot(&mut q, false, |_| true);
        let c = claim_slot(&mut q, false, |id| id == w[2]);
        assert_eq!(c.entry, Some(SlotEntry::Placeholder(w[2])));
        assert!(c.done);
    }
}
