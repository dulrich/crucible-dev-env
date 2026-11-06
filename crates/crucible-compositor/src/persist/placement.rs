//! Window placement: pending placements keyed by spawned PID ([`Target`]), PID-ancestor matching,
//! restore-order insertion, and the new-toplevel map paths.
//!
//! A relaunch spawn's slot queue lives inside its own [`Pending`] entry (`queue`): the
//! relaunched placeholder's [`Slot`] first, then each sibling placeholder standing for the same
//! cmdline. The queue hands out one slot per new toplevel (a dialog takes none) and the entry is
//! dropped exactly when the queue is exhausted; expiry and workspace close drop the queue with the
//! entry, so "is this placeholder already queued" is answered from `pending` alone. Placeholders
//! are recognised from spawn-time facts ([`placeholder_cmdline`]), never from `/proc`.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crucible_layout::{PaneId, WindowId, WorkspaceId, World};
use tracing::{info, warn};

use super::{WindowIdentity, proc_root};
use crate::element::WindowElement;
use crate::layout::{WindowClass, WindowMeta};
use crate::state::{Backend, CrucibleState};

/// Pending placements and restore order (a field of [`super::PersistState`]).
#[derive(Debug, Default)]
pub(super) struct PlacementState {
    pub(super) pending: HashMap<u32, Pending>,
    pub(super) placed: HashMap<PaneId, Vec<(WindowId, usize)>>,
    /// Loss slot reservations (plan 03): per pane, the live stack order at loss time with each
    /// lost window kept in place until its placeholder maps (then its id replaces the lost one).
    pub(super) reserved: HashMap<PaneId, Vec<WindowId>>,
}

impl PlacementState {
    /// Whether `id` is a sibling placeholder still queued by some live relaunch spawn.
    pub(super) fn is_queued(&self, id: WindowId) -> bool {
        is_queued(self.pending.values(), id)
    }

    /// Whether a pending `run-on` placement targets workspace `ws`.
    pub(super) fn targets_workspace(&self, ws: WorkspaceId) -> bool {
        self.pending
            .values()
            .any(|p| p.target == Target::Workspace(ws))
    }
}

/// Where a `run-on` window into workspace `ws` lands, read at map time: `ws`'s focused pane, at
/// the end of its stack, and whether it becomes that stack's focused window. It does except in
/// the globally focused pane, where stack focus is keyboard focus: there it joins behind the
/// current window (an empty pane still focuses it, as an empty stack focuses its first item).
#[must_use]
pub fn run_on_slot(world: &World, ws: WorkspaceId) -> Option<(PaneId, usize, bool)> {
    let pane = world.workspace(ws)?.focused_pane;
    let len = world.pane(pane)?.windows().len();
    Some((pane, len, pane != world.focused_pane_id()))
}

/// How long a pending placement waits for its window to map.
pub const PENDING_TTL: Duration = Duration::from_secs(30);

/// How far up `/proc` a client PID is walked looking for a spawned/pending ancestor.
pub const ANCESTOR_DEPTH: usize = 8;

/// The live slot a relaunch placeholder held: `pane` at `index` (its position in the pane's
/// stack when Enter was pressed, never the saved order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    /// The placeholder's pane.
    pub pane: PaneId,
    /// The placeholder's current index in that pane's stack.
    pub index: usize,
    /// Whether the placeholder was the pane's focused window.
    pub focused: bool,
    /// The placeholder's restore placement entry (`placed`), inherited by the app.
    pub saved_index: Option<usize>,
}

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
    /// Into the live slot a relaunch placeholder held. A pending relaunch entry carries this
    /// (the relaunched placeholder's own slot, for workspace bookkeeping) plus its slot queue.
    Slot(Slot),
    /// An overlay (picker / launcher; unmanaged, centred, keyboard-focused).
    Overlay,
    /// A pre-warmed overlay host: stored hidden in the warm slot, never mapped until requested.
    WarmOverlay,
    /// A loss placeholder (plan 03): into the live slot its lost window held, reserved in
    /// `PlacementState::reserved`.
    Lost(LostSlot),
    /// A `run-on` spawn: into workspace `ws`'s focused pane as of map time ([`run_on_slot`]).
    Workspace(WorkspaceId),
}

impl Target {
    /// The pane a placement lands in (`None` for overlays).
    #[must_use]
    pub fn pane(&self) -> Option<PaneId> {
        match self {
            Target::Pane { pane, .. }
            | Target::Slot(Slot { pane, .. })
            | Target::Lost(LostSlot { pane, .. }) => Some(*pane),
            // A `run-on` target resolves its pane at map time, so it holds no pane now (and so is
            // not part of the restore save gate).
            Target::Overlay | Target::WarmOverlay | Target::Workspace(_) => None,
        }
    }
}

/// The slot a loss placeholder fills: the lost window `lost` in `pane` (kept in the pane's
/// reservation), with its stack focus and restore placement entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LostSlot {
    /// The lost window's pane.
    pub pane: PaneId,
    /// The lost window (its id stands in the pane's reservation until the placeholder maps).
    pub lost: WindowId,
    /// Whether the lost window was the pane's focused window.
    pub focused: bool,
    /// The lost window's restore placement entry (`placed`), inherited by the placeholder.
    pub saved_index: Option<usize>,
}

/// One slot a relaunch spawn will fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotEntry {
    /// A slot already resolved (the relaunched placeholder's, whose window is closed at relaunch).
    Resolved(Slot),
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

/// The relaunch cmdline a window stands for if it is a relaunch placeholder: a CDE-spawned
/// terminal (`class == Terminal`) whose spawn origin is a saved App record. The class guard is
/// load-bearing (audit E1): a relaunched app carries the very same App origin but is an `App`.
#[must_use]
pub fn placeholder_cmdline(meta: &WindowMeta) -> Option<&str> {
    match (&meta.class, &meta.origin) {
        (
            WindowClass::Terminal,
            Some(crucible_persist::WindowRecord::App {
                cmdline: Some(cmdline),
                ..
            }),
        ) => Some(cmdline),
        _ => None,
    }
}

/// Whether `id` is a sibling placeholder still queued by one of the `pending` relaunch spawns.
#[must_use]
pub fn is_queued<'a>(pending: impl IntoIterator<Item = &'a Pending>, id: WindowId) -> bool {
    pending.into_iter().any(|p| {
        p.queue
            .as_ref()
            .is_some_and(|q| q.contains(&SlotEntry::Placeholder(id)))
    })
}

/// One pending placement: its target, when it was registered and, for a relaunch spawn, the slots
/// it still has to fill (dropped with the entry on expiry or workspace close).
#[derive(Debug, Clone)]
pub(super) struct Pending {
    pub(super) target: Target,
    pub(super) since: Instant,
    /// A relaunch spawn's slot queue (`Some` exactly for relaunch spawns).
    pub(super) queue: Option<VecDeque<SlotEntry>>,
}

impl Pending {
    /// A whole-target placement registered `since`.
    pub(super) fn new(target: Target, since: Instant) -> Self {
        Self {
            target,
            since,
            queue: None,
        }
    }
}

/// The key `has` accepts among `pid` itself and then its `ancestors` (parent first).
#[must_use]
pub fn match_pending(pid: u32, ancestors: &[u32], has: impl Fn(u32) -> bool) -> Option<u32> {
    std::iter::once(pid)
        .chain(ancestors.iter().copied())
        .find(|p| has(*p))
}

/// The pid a new toplevel is placed, classified and recorded by: a bound surface's `spawn_pid`
/// (its thin client, never the shared daemon client), else the client pid.
#[must_use]
pub fn placement_pid(client_pid: Option<u32>, identity: Option<&WindowIdentity>) -> Option<u32> {
    identity.map_or(client_pid, |i| Some(i.spawn_pid))
}

/// The window pid (`meta.pid`) a `crucible relaunch` peer stands for, over `windows` as
/// `(meta.pid, identity root_pid)` pairs. First the peer or its nearest ancestor that is a bound
/// window's root resolves to that window's `meta.pid`; only then today's match of the peer or an
/// ancestor against a `meta.pid`. A shared daemon (an ancestor of every root) is never a root nor
/// a `meta.pid`, so it never matches.
#[must_use]
pub fn relaunch_window_pid(
    peer: u32,
    ancestors: &[u32],
    windows: &[(Option<u32>, Option<u32>)],
) -> Option<u32> {
    match_pending(peer, ancestors, |p| {
        windows.iter().any(|(_, r)| *r == Some(p))
    })
    .and_then(|root| windows.iter().find(|(_, r)| *r == Some(root))?.0)
    .or_else(|| {
        match_pending(peer, ancestors, |p| {
            windows.iter().any(|(m, _)| *m == Some(p))
        })
    })
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

/// The reservation of a pane whose window is being lost (plan 03, M1): the `live` stack (the lost
/// window still in it), with every earlier reserved window that is no longer live (a lost window
/// whose placeholder has not mapped yet) kept at its reserved position, clamped.
#[must_use]
pub fn reserve(previous: Option<&[WindowId]>, live: &[WindowId]) -> Vec<WindowId> {
    let mut order = live.to_vec();
    for (i, w) in previous.unwrap_or_default().iter().enumerate() {
        if !live.contains(w) {
            order.insert(i.min(order.len()), *w);
        }
    }
    order
}

/// The stack index the placeholder for `lost` takes: the number of windows reserved before it that
/// are live now (survivors and already-mapped placeholders), so every reserved slot is kept
/// whatever the arrival order. A `lost` absent from `order` goes last.
#[must_use]
pub fn reserved_index(order: &[WindowId], lost: WindowId, live: &[WindowId]) -> usize {
    match order.iter().position(|w| *w == lost) {
        Some(pos) => order[..pos].iter().filter(|w| live.contains(w)).count(),
        None => live.len(),
    }
}

/// Whether any pending target lands in one of `panes` (the save gate: a workspace with one is not
/// saved, so a restore or a loss placeholder in flight never overwrites the saved commands).
#[must_use]
pub fn pending_in<'a>(panes: &[PaneId], mut targets: impl Iterator<Item = &'a Target>) -> bool {
    targets.any(|t| t.pane().is_some_and(|pane| panes.contains(&pane)))
}

impl<B: Backend + 'static> CrucibleState<B> {
    pub(super) fn workspace_has_pending(&self, ws: WorkspaceId) -> bool {
        let Some(w) = self.layout.world.workspace(ws) else {
            return false;
        };
        let panes = w.panes();
        pending_in(
            &panes,
            self.persist.placement.pending.values().map(|p| &p.target),
        )
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
    /// it is a CDE-spawned terminal. A surface bound on the identity protocol (`identity`) is
    /// placed and classified by its `spawn_pid` instead of the (shared daemon) client pid.
    pub fn map_new_window(
        &mut self,
        element: WindowElement,
        pid: Option<u32>,
        identity: Option<WindowIdentity>,
    ) {
        let pid = placement_pid(pid, identity.as_ref());
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
        let meta = WindowMeta {
            pid,
            class,
            origin,
            identity,
        };
        let id = match target {
            Some(Target::Overlay) => {
                self.overlay_arrived(element, pid);
                return;
            }
            Some(Target::WarmOverlay) => {
                self.store_warm(element, pid);
                return;
            }
            Some(Target::Pane {
                pane,
                saved_index,
                focused,
            }) => self.map_placed(element, pane, saved_index, focused, meta),
            Some(Target::Slot(slot)) => self.map_slot(element, slot, meta),
            Some(Target::Lost(slot)) => self.map_lost(element, slot, meta),
            Some(Target::Workspace(ws)) => self.map_run_on(element, ws, meta),
            None => self.map_managed_window(element, meta),
        };
        self.init_recovery(id);
    }

    /// Maps a loss placeholder into the slot reserved for its lost window (plan 03, M1), then
    /// puts its own id in the reservation in the lost window's place.
    fn map_lost(&mut self, element: WindowElement, slot: LostSlot, meta: WindowMeta) -> WindowId {
        let LostSlot {
            pane,
            lost,
            focused,
            saved_index,
        } = slot;
        let Some(live) = self.layout.world.pane(pane).map(|p| p.windows().to_vec()) else {
            warn!(?pane, "lost window's pane is gone; using the focused pane");
            self.persist.placement.reserved.remove(&pane);
            return self.map_managed_window(element, meta);
        };
        let index = self
            .persist
            .placement
            .reserved
            .get(&pane)
            .map_or(live.len(), |order| reserved_index(order, lost, &live));
        match self.map_managed_window_at(element.clone(), meta.clone(), pane, index) {
            Ok(id) => {
                if let Some(order) = self.persist.placement.reserved.get_mut(&pane) {
                    for w in order.iter_mut().filter(|w| **w == lost) {
                        *w = id;
                    }
                }
                self.finish_placement(pane, id, saved_index, focused);
                id
            }
            Err(err) => {
                warn!(%err, "loss placement failed; using the focused pane");
                self.map_managed_window(element, meta)
            }
        }
    }

    /// The target the new toplevel of spawn `key` takes. A relaunch spawn (a pending entry with a
    /// slot queue) hands out its queued slots one per toplevel (a dialog takes none) and keeps its
    /// pending entry until the queue is empty; every other target is consumed whole, as before.
    fn take_pending_target(&mut self, key: u32, has_parent: bool) -> Option<Target> {
        let pending = &mut self.persist.placement.pending;
        let Some(mut queue) = pending.get_mut(&key)?.queue.take() else {
            return pending.remove(&key).map(|p| p.target);
        };
        let claim = claim_slot(&mut queue, has_parent, |id| {
            self.layout.world.pane_of_window(id).is_some()
        });
        if claim.done {
            self.persist.placement.pending.remove(&key);
        } else if let Some(p) = self.persist.placement.pending.get_mut(&key) {
            p.queue = Some(queue);
        }
        match claim.entry? {
            SlotEntry::Resolved(slot) => Some(Target::Slot(slot)),
            SlotEntry::Placeholder(id) => {
                let slot = self.placeholder_slot(id)?;
                self.end_placeholder(id);
                Some(Target::Slot(slot))
            }
        }
    }

    /// The live slot of placeholder window `id`.
    pub(super) fn placeholder_slot(&self, id: WindowId) -> Option<Slot> {
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
        Some(Slot {
            pane: pane_id,
            index,
            focused,
            saved_index,
        })
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

    /// Maps a `run-on` window into workspace `ws` ([`run_on_slot`]); no output's active workspace
    /// changes. A workspace gone by map time falls back to the focused pane.
    fn map_run_on(
        &mut self,
        element: WindowElement,
        ws: WorkspaceId,
        meta: WindowMeta,
    ) -> WindowId {
        let Some((pane, index, focused)) = run_on_slot(&self.layout.world, ws) else {
            warn!(?ws, "run-on workspace is gone; using the focused pane");
            return self.map_managed_window(element, meta);
        };
        match self.map_managed_window_at(element.clone(), meta.clone(), pane, index) {
            Ok(id) => {
                self.finish_placement(pane, id, None, focused);
                id
            }
            Err(err) => {
                warn!(%err, "run-on placement failed; using the focused pane");
                self.map_managed_window(element, meta)
            }
        }
    }

    /// Maps a relaunched app into the slot its placeholder held (clamped to the live stack).
    pub(super) fn map_slot(
        &mut self,
        element: WindowElement,
        slot: Slot,
        meta: WindowMeta,
    ) -> WindowId {
        let Slot {
            pane,
            index,
            focused,
            saved_index,
        } = slot;
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
pub(super) fn terminate(pid: u32) -> bool {
    terminate_with(pid, |pid| {
        rustix::process::kill_process(pid, rustix::process::Signal::TERM).map_err(Into::into)
    })
}

/// [`terminate`] with the signal sender injected: converts `pid` (`false` when it is not a valid
/// process id) and reports whether `send` delivered.
fn terminate_with(
    pid: u32,
    send: impl FnOnce(rustix::process::Pid) -> std::io::Result<()>,
) -> bool {
    let Some(pid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return false;
    };
    match send(pid) {
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
    fn terminate_converts_the_pid_and_reports_the_outcome() {
        let sent = std::cell::Cell::new(None);
        assert!(terminate_with(4242, |pid| {
            sent.set(Some(pid.as_raw_nonzero().get()));
            Ok(())
        }));
        assert_eq!(sent.get(), Some(4242));
        assert!(!terminate_with(4242, |_| Err(std::io::Error::other(
            "gone"
        ))));
        // A pid that is not a valid process id never reaches the sender.
        assert!(!terminate_with(0, |_| panic!("sent to pid 0")));
        assert!(!terminate_with(u32::MAX, |_| panic!(
            "sent to a negative pid"
        )));
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
        SlotEntry::Resolved(Slot {
            pane: World::new("x").focused_pane_id(),
            index: 0,
            focused: false,
            saved_index: None,
        })
    }

    fn relaunch_pending(queue: VecDeque<SlotEntry>) -> Pending {
        let SlotEntry::Resolved(own) = own_slot() else {
            unreachable!()
        };
        Pending {
            target: Target::Slot(own),
            since: Instant::now(),
            queue: Some(queue),
        }
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
        let mut state = PlacementState::default();
        state.pending.insert(7, relaunch_pending(q));
        assert!(state.is_queued(w[1]));
        // Expiry drops the spawn's entry and its queue with it: the leftover placeholder is an
        // ordinary one again.
        state.pending.retain(|_, _| false);
        assert!(!state.is_queued(w[1]));
        let mut live = PlacementState::default();
        live.pending.insert(
            7,
            relaunch_pending(VecDeque::from([SlotEntry::Placeholder(w[1])])),
        );
        live.pending.retain(|pid, _| *pid == 7);
        assert!(live.is_queued(w[1]));
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
        let mut state = PlacementState::default();
        state.pending.insert(7, relaunch_pending(q.clone()));
        // A whole-target entry queues nothing.
        state
            .pending
            .insert(8, Pending::new(Target::WarmOverlay, Instant::now()));
        // A hand relaunch checks `is_queued` and returns without spawning.
        assert!(state.is_queued(w[1]));
        assert!(!state.is_queued(w[2]));
        // A sibling closed by hand before its turn is skipped, not consumed.
        let mut q = q;
        q.push_back(SlotEntry::Placeholder(w[2]));
        let _ = claim_slot(&mut q, false, |_| true);
        let c = claim_slot(&mut q, false, |id| id == w[2]);
        assert_eq!(c.entry, Some(SlotEntry::Placeholder(w[2])));
        assert!(c.done);
    }

    /// Audit E1: only a CDE-spawned terminal with an App origin is a placeholder. A relaunched app
    /// window carries the very same App origin (its spawn stands for the same saved record), and a
    /// plain terminal has no origin: neither is selected as a sibling.
    #[test]
    fn only_placeholder_terminals_are_siblings_not_relaunched_apps_with_the_same_origin() {
        let w = ids(4);
        let (own, placeholder, relaunched, plain) = (w[0], w[1], w[2], w[3]);
        let app = || {
            Some(crucible_persist::WindowRecord::App {
                app_id: Some("firefox".to_string()),
                cmdline: Some("firefox".to_string()),
            })
        };
        let meta = |class, origin| WindowMeta {
            pid: Some(1),
            class,
            origin,
            identity: None,
        };
        let metas = [
            (own, meta(WindowClass::Terminal, app())),
            (placeholder, meta(WindowClass::Terminal, app())),
            (relaunched, meta(WindowClass::App, app())),
            (plain, meta(WindowClass::Terminal, None)),
        ];
        assert_eq!(placeholder_cmdline(&metas[1].1), Some("firefox"));
        assert_eq!(placeholder_cmdline(&metas[2].1), None);
        assert_eq!(placeholder_cmdline(&metas[3].1), None);
        let cands = metas
            .iter()
            .map(|(id, m)| (*id, placeholder_cmdline(m).map(str::to_string)));
        assert_eq!(
            select_siblings(cands, own, "firefox", |_| false),
            vec![placeholder]
        );
    }

    /// The thin clients C1, C2 of one daemon D, their roots R1, R2 (children of D).
    const D: u32 = 600;
    const C1: u32 = 500;
    const C2: u32 = 501;
    const R1: u32 = 601;
    const R2: u32 = 602;

    fn bound(spawn_pid: u32, root_pid: u32) -> WindowIdentity {
        WindowIdentity {
            id: u64::from(spawn_pid),
            spawn_pid,
            root_pid,
            session: None,
            client: super::super::ClientLink::default(),
        }
    }

    /// custom.md §4 reversed arrival, through the pure decision `map_new_window` makes (it needs a
    /// compositor itself): two bound `crucible resume` spawns with pending targets in distinct
    /// panes map in reverse spawn order, both from the daemon's client pid. Each is keyed by its
    /// `spawn_pid`, lands in its own pane and is a terminal (exact-pid terminal spawn).
    #[test]
    fn reversed_arrival_places_each_bound_window_in_its_own_slot() {
        let mut w = World::new("1");
        let p1 = w.focused_pane_id();
        let p2 = w.split(crucible_layout::Direction::Right);
        let target = |pane| Target::Pane {
            pane,
            saved_index: 0,
            focused: true,
        };
        let mut pending: HashMap<u32, Target> = [(C1, target(p1)), (C2, target(p2))].into();
        let terminal_spawns: HashSet<u32> = [C1, C2].into();
        // The daemon's own client pid matches nothing.
        assert_eq!(match_pending(D, &[1], |k| pending.contains_key(&k)), None);
        for (identity, want) in [(bound(C2, R2), p2), (bound(C1, R1), p1)] {
            let pid = placement_pid(Some(D), Some(&identity)).unwrap();
            assert_eq!(pid, identity.spawn_pid);
            let key = match_pending(pid, &[1], |k| pending.contains_key(&k)).unwrap();
            assert_eq!(pending.remove(&key), Some(target(want)));
            let class = if terminal_spawns.contains(&pid) {
                WindowClass::Terminal
            } else {
                WindowClass::App
            };
            assert_eq!(class, WindowClass::Terminal);
        }
        assert!(pending.is_empty());
        // Unbound: the client pid, as today.
        assert_eq!(placement_pid(Some(7), None), Some(7));
        assert_eq!(placement_pid(None, None), None);
    }

    /// The `crucible relaunch` pair: each root is its window's wrapper. A relaunch from R1, R2 or
    /// a child of R1 resolves to its own window's `meta.pid`, never through the shared daemon;
    /// unbound windows still match by `meta.pid`; placeholder termination roots at the identity.
    #[test]
    fn relaunch_resolves_each_bound_wrapper_to_its_own_window() {
        let windows = [(Some(C1), Some(R1)), (Some(C2), Some(R2)), (Some(99), None)];
        assert_eq!(relaunch_window_pid(R2, &[D, 1], &windows), Some(C2));
        assert_eq!(relaunch_window_pid(R1, &[D, 1], &windows), Some(C1));
        assert_eq!(relaunch_window_pid(700, &[R1, D, 1], &windows), Some(C1));
        assert_eq!(relaunch_window_pid(99, &[1], &windows), Some(99));
        assert_eq!(relaunch_window_pid(800, &[99, 1], &windows), Some(99));
        assert_eq!(relaunch_window_pid(D, &[1], &windows), None);
        assert_eq!(relaunch_window_pid(801, &[D, 1], &windows), None);

        let meta = |pid, identity| WindowMeta {
            pid,
            class: WindowClass::Terminal,
            origin: None,
            identity,
        };
        assert_eq!(
            super::super::proc_root_pid(&meta(Some(C1), Some(bound(C1, R1)))),
            Some(R1)
        );
        assert_eq!(
            super::super::proc_root_pid(&meta(Some(C2), Some(bound(C2, R2)))),
            Some(R2)
        );
        assert_eq!(super::super::proc_root_pid(&meta(Some(99), None)), Some(99));
        assert_eq!(super::super::proc_root_pid(&meta(None, None)), None);
    }
    // ---- loss recovery (crucible-term plan 03) --------------------------------------------------

    use super::super::identity::{ClientLink, Destruction, classify_destruction};
    use crucible_persist::WindowRecord;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    fn keyed_terminal(cwd: &str, command: Option<&str>, key: &str) -> WindowRecord {
        WindowRecord::Terminal {
            cwd: cwd.into(),
            command: command.map(str::to_string),
            session: Some(key.into()),
        }
    }

    fn lost_job(record: &WindowRecord) -> Option<crucible_persist::SpawnJob> {
        crucible_persist::prepare_lost(
            record,
            None,
            Ok(PathBuf::from("/bin/crucible")),
            None,
            "-e",
            |_: &Path| true,
        )
        .expect("crucible path")
    }

    fn os(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    /// Loses `lost` from `stack` (each reserved against the live stack before it closes), then maps
    /// their placeholders `new` in `arrival` order; returns the final live stack.
    fn lose_and_replace(
        stack: &[WindowId],
        lost: &[WindowId],
        new: &[WindowId],
        arrival: &[usize],
    ) -> Vec<WindowId> {
        let mut live = stack.to_vec();
        let mut order: Option<Vec<WindowId>> = None;
        for l in lost {
            order = Some(reserve(order.as_deref(), &live));
            live.retain(|w| w != l);
        }
        let mut order = order.expect("one loss");
        for &k in arrival {
            let index = reserved_index(&order, lost[k], &live);
            live.insert(index, new[k]);
            for w in order.iter_mut().filter(|w| **w == lost[k]) {
                *w = new[k];
            }
        }
        live
    }

    #[test]
    fn disconnect_then_destroy_is_a_loss_with_a_placeholder_in_the_same_slot() {
        let client = crate::state::ClientState::default();
        let identity = WindowIdentity {
            client: client.link.clone(),
            ..bound(C1, R1)
        };
        assert_eq!(classify_destruction(Some(&identity)), Destruction::Close);
        client.link.mark_disconnected();
        assert_eq!(classify_destruction(Some(&identity)), Destruction::Lost);
        let record = keyed_terminal("/p", Some("htop"), "k1");
        let job = lost_job(&record).expect("a terminal always has a placeholder");
        assert_eq!(
            job.args,
            os(&[
                "-e",
                "/bin/crucible",
                "resume",
                "--lost",
                "--cwd",
                "/p",
                "--",
                "htop"
            ])
        );
        assert_eq!(job.dir, PathBuf::from("/p"));
        // The origin carries the key, which the spawn re-exports (`session_for_spawn`).
        assert_eq!(job.origin, record);
        let w = ids(6);
        let (a, t, b) = (w[0], w[1], w[2]);
        assert_eq!(
            lose_and_replace(&[a, t, b], &[t], &[w[3]], &[0]),
            [a, w[3], b]
        );
    }

    #[test]
    fn destroy_without_disconnect_is_todays_close() {
        assert_eq!(classify_destruction(None), Destruction::Close);
        let identity = bound(C1, R1);
        assert!(!identity.client.is_disconnected());
        assert_eq!(classify_destruction(Some(&identity)), Destruction::Close);
        // A link only reports its own client: another client's disconnect does not leak.
        let other = ClientLink::default();
        other.mark_disconnected();
        assert_eq!(classify_destruction(Some(&identity)), Destruction::Close);
    }

    #[test]
    fn a_window_lost_before_its_first_save_has_its_map_time_record() {
        // At map the record is the spawn origin with the identity's key; no save has run.
        let origin = WindowRecord::Terminal {
            cwd: "/q".into(),
            command: None,
            session: None,
        };
        let record = super::super::with_session(origin, Some("k2".into()));
        assert_eq!(record, keyed_terminal("/q", None, "k2"));
        let job = lost_job(&record).expect("placeholder");
        // An idle shell still gets the lost banner (and then its shell at once).
        assert_eq!(
            job.args,
            os(&["-e", "/bin/crucible", "resume", "--lost", "--cwd", "/q"])
        );
    }

    #[test]
    fn mixed_stack_keeps_every_slot_for_one_or_two_losses_in_either_order() {
        let w = ids(7);
        let (a1, t, a2) = (w[0], w[1], w[2]);
        let (n1, n2) = (w[3], w[4]);
        let stack = [a1, t, a2];
        assert_eq!(lose_and_replace(&stack, &[t], &[n1], &[0]), [a1, n1, a2]);
        assert_eq!(lose_and_replace(&stack, &[a1], &[n1], &[0]), [n1, t, a2]);
        assert_eq!(lose_and_replace(&stack, &[a2], &[n1], &[0]), [a1, t, n1]);
        for arrival in [[0, 1], [1, 0]] {
            assert_eq!(
                lose_and_replace(&stack, &[a1, t], &[n1, n2], &arrival),
                [n1, n2, a2]
            );
            assert_eq!(
                lose_and_replace(&stack, &[a1, a2], &[n1, n2], &arrival),
                [n1, t, n2]
            );
            assert_eq!(
                lose_and_replace(&stack, &[t, a2], &[n1, n2], &arrival),
                [a1, n1, n2]
            );
        }
    }

    #[test]
    fn a_bound_relaunch_placeholder_lost_comes_back_as_a_relaunch_placeholder() {
        let app = WindowRecord::App {
            app_id: Some("org.x".into()),
            cmdline: Some("firefox".into()),
        };
        let job = lost_job(&app).expect("relaunch placeholder");
        assert_eq!(
            job.args,
            os(&[
                "-e",
                "/bin/crucible",
                "relaunch",
                "--app-id",
                "org.x",
                "--",
                "firefox"
            ])
        );
        assert_eq!(job.origin, app);
    }

    #[test]
    fn workspace_is_held_while_the_placeholder_is_pending_and_freed_after_it_maps() {
        let w = ids(4);
        let (a, t, b, n) = (w[0], w[1], w[2], w[3]);
        let pane = PaneId::from_raw(9);
        let other = PaneId::from_raw(10);
        let target = Target::Lost(LostSlot {
            pane,
            lost: t,
            focused: false,
            saved_index: None,
        });
        assert!(pending_in(&[other, pane], [target].iter()));
        assert!(!pending_in(&[other], [target].iter()));
        // Mapped: its pending entry is gone, so the save runs, with the placeholder in place.
        assert!(!pending_in(&[other, pane], std::iter::empty()));
        assert_eq!(lose_and_replace(&[a, t, b], &[t], &[n], &[0]), [a, n, b]);
    }
}

#[cfg(test)]
mod run_on_tests {
    use super::run_on_slot;
    use crucible_layout::{WindowId, WorkspaceId, WorkspaceLayout, World};

    /// Maps one `run-on` window into `ws` as `map_run_on` does, through the model only.
    fn map(world: &mut World, ws: WorkspaceId) -> WindowId {
        let (pane, index, focused) = run_on_slot(world, ws).unwrap();
        let id = world.open_window_at(pane, index).unwrap();
        if focused {
            world.set_stack_focus(pane, index).unwrap();
        }
        id
    }

    #[test]
    fn a_background_pane_focuses_the_new_window_in_its_stack_only() {
        let mut world = World::new("one");
        let one = world.active_workspace_id();
        let keyboard = world.open_window();
        let out = world.focused_output();
        let (scratch, _) = world
            .insert_workspace(out, "scratch", &WorkspaceLayout::single(()))
            .unwrap();
        let first = map(&mut world, scratch);
        let second = map(&mut world, scratch);
        let pane = world.workspace(scratch).unwrap().focused_pane;
        assert_eq!(world.pane(pane).unwrap().windows(), [first, second]);
        assert_eq!(world.pane(pane).unwrap().focused_window(), Some(second));
        assert_eq!(world.active_workspace_id(), one, "no workspace switch");
        assert_eq!(
            world.focused_window(),
            Some(keyboard),
            "no keyboard focus change"
        );
    }

    #[test]
    fn the_globally_focused_pane_takes_it_unfocused_behind_the_current_window() {
        let mut world = World::new("one");
        let keyboard = world.open_window();
        let pane = world.focused_pane_id();
        let ws = world.active_workspace_id();
        let id = map(&mut world, ws);
        assert_eq!(world.pane(pane).unwrap().windows(), [keyboard, id]);
        assert_eq!(world.focused_window(), Some(keyboard));
    }

    #[test]
    fn an_empty_globally_focused_pane_takes_it_with_keyboard_focus() {
        let mut world = World::new("one");
        let ws = world.active_workspace_id();
        let id = map(&mut world, ws);
        assert_eq!(world.focused_window(), Some(id));
    }
}
