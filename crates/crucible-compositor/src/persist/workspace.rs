//! Workspace verbs: close, close-all, move to another output, name seeding and slot pins, with the
//! pure naming/output decisions they use.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crucible_layout::{OutputId, PaneId, WorkspaceId, World};
use crucible_persist::slots::slots_path;
use tracing::{debug, info, warn};

use super::overlay::PickerMode;
use super::skip_record;
use crate::state::{Backend, CrucibleState};

/// One workspace's persistence-side facts: the single record behind what used to be two parallel maps.
#[derive(Debug, Default)]
pub(super) struct WorkspaceMeta {
    /// Folder root of the workspace (new terminals start there).
    pub(super) root: Option<PathBuf>,
    /// Connector name of the output the workspace was last viewed on (see [`note_last_viewed`]).
    pub(super) last_output: Option<String>,
}

/// Per-workspace meta and the pending slot pin (a field of [`super::PersistState`]).
#[derive(Debug, Default)]
pub(super) struct WorkspaceState {
    /// Per-workspace root and last-viewed output; dropped together by `forget_workspace`.
    pub(super) meta: HashMap<WorkspaceId, WorkspaceMeta>,
    /// Slot `n` awaiting a workspace: set when an unpinned `Super+<n>` opens the picker, applied by the
    /// next successful open/new/open-folder, cleared by any overlay close or other overlay.
    pub(super) pending_pin: Option<u8>,
}

impl WorkspaceState {
    /// The root of `ws`, if it has one.
    pub(super) fn root(&self, ws: WorkspaceId) -> Option<&PathBuf> {
        self.meta.get(&ws).and_then(|m| m.root.as_ref())
    }

    /// Sets (`Some`) or clears (`None`) the root of `ws`.
    pub(super) fn set_root(&mut self, ws: WorkspaceId, root: Option<PathBuf>) {
        self.meta.entry(ws).or_default().root = root;
    }

    /// The output `ws` was last viewed on, if known.
    pub(super) fn last_output(&self, ws: WorkspaceId) -> Option<&String> {
        self.meta.get(&ws).and_then(|m| m.last_output.as_ref())
    }
}

/// Something that changes the pending slot pin (see [`pin_transition`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PinEvent {
    /// An unpinned `Super+<n>` opens the picker in pin mode.
    SlotOpen(u8),
    /// An overlay open was requested (before its decision: also when ignored or refocused);
    /// `keeps_pin` only for the plain picker.
    OpenOverlay { keeps_pin: bool },
    /// The overlay closed or was given up.
    OverlayClosed,
    /// An open/new/open-folder request completed.
    Completed { succeeded: bool },
}

/// The one pending-pin transition: `(slot to pin now, pending pin afterwards)`.
#[must_use]
pub(super) fn pin_transition(pending: Option<u8>, event: PinEvent) -> (Option<u8>, Option<u8>) {
    match event {
        PinEvent::SlotOpen(n) => (None, Some(n)),
        PinEvent::OpenOverlay { keeps_pin: true } => (None, pending),
        PinEvent::OpenOverlay { keeps_pin: false } | PinEvent::OverlayClosed => (None, None),
        PinEvent::Completed { succeeded } => (pending.filter(|_| succeeded), None),
    }
}

// ---- pure decisions ---------------------------------------------------------------------------

/// The smallest positive number (as a string) for which `taken` is false.
#[must_use]
pub fn first_free_number(taken: impl Fn(&str) -> bool) -> String {
    (1u64..)
        .map(|n| n.to_string())
        .find(|n| !taken(n))
        .expect("unbounded")
}

/// The auto-name for a new workspace: the first number neither live in `world` nor in `saved`.
#[must_use]
pub fn new_workspace_name(world: &World, saved: &HashSet<String>) -> String {
    first_free_number(|n| world.workspace_by_name(n).is_some() || saved.contains(n))
}

/// The startup seed name: [`new_workspace_name`] over a world with no live workspaces yet, i.e.
/// the first number not in `saved`.
#[must_use]
pub fn startup_seed_name(saved: &HashSet<String>) -> String {
    first_free_number(|n| saved.contains(n))
}

/// Records each output's **active** workspace as last viewed on that output's connector name.
/// `viewed` holds one `(model output, active workspace, connector name)` triple per connected,
/// bound output. Workspaces not active anywhere (e.g. migrated inactive by an unplug) keep their
/// entry untouched.
pub fn note_last_viewed(
    map: &mut HashMap<WorkspaceId, WorkspaceMeta>,
    viewed: &[(OutputId, WorkspaceId, String)],
) {
    for (_, ws, name) in viewed {
        let meta = map.entry(*ws).or_default();
        if meta.last_output.as_ref() != Some(name) {
            meta.last_output = Some(name.clone());
        }
    }
}

/// The output a restored workspace lands on: the connected output named `wanted`, else `focused`.
#[must_use]
pub fn restore_target(
    wanted: Option<&str>,
    connected: &[(String, OutputId)],
    focused: OutputId,
) -> OutputId {
    wanted
        .and_then(|w| connected.iter().find(|(name, _)| name == w))
        .map_or(focused, |(_, id)| *id)
}

/// The output `delta` steps from `current` in `order` (wraparound). `None` when there is only one
/// output, `current` is not in `order`, or the step lands back on `current`.
#[must_use]
pub fn shifted_output(order: &[OutputId], current: OutputId, delta: i64) -> Option<OutputId> {
    let len = i64::try_from(order.len()).ok()?;
    if len < 2 {
        return None;
    }
    let at = i64::try_from(order.iter().position(|o| *o == current)?).ok()?;
    let target = order[usize::try_from((at + delta).rem_euclid(len)).ok()?];
    (target != current).then_some(target)
}

/// The workspace name for folder `path`: its basename, else `basename (parent)`, else
/// `basename (parent) N`. `None` when the path has no UTF-8 basename.
#[must_use]
pub fn folder_workspace_name(path: &Path, taken: impl Fn(&str) -> bool) -> Option<String> {
    let base = path.file_name()?.to_str()?;
    if !taken(base) {
        return Some(base.to_string());
    }
    let parent = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|p| p.to_str());
    let stem = match parent {
        Some(p) => format!("{base} ({p})"),
        None => base.to_string(),
    };
    if !taken(&stem) {
        return Some(stem);
    }
    (2..10_000)
        .map(|n| format!("{stem} {n}"))
        .find(|c| !taken(c))
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Saves and closes every open workspace (`close-all-workspaces`), leaving one fresh seed per
    /// output. Each output gets its seed first (so none is ever its last workspace), then each other
    /// workspace is closed exactly as `close-workspace` does; one with a pending restore is skipped.
    pub(crate) fn close_all_workspaces(&mut self) {
        self.flush_workspaces();
        let focused = self.layout.world.focused_output();
        let outputs: Vec<OutputId> = self.layout.world.output_ids().collect();
        for output in outputs {
            if let Err(err) = self.with_model(|w| w.set_focused_output(output)) {
                warn!(%err, "close-all-workspaces: cannot focus an output");
                continue;
            }
            let saved = self.persist.saved_names();
            let name = new_workspace_name(&self.layout.world, &saved);
            let mut created = None;
            if let Err(err) = self.with_model(|w| {
                created = Some(w.new_workspace(name)?);
                Ok(())
            }) {
                warn!(%err, "close-all-workspaces: cannot create a fresh seed");
                continue;
            }
            let Some(seed) = created else { continue };
            let others: Vec<WorkspaceId> = self
                .layout
                .world
                .output(output)
                .map(|o| {
                    o.workspaces()
                        .iter()
                        .copied()
                        .filter(|w| *w != seed)
                        .collect()
                })
                .unwrap_or_default();
            for ws in others {
                if self.workspace_has_pending(ws) {
                    info!(
                        ?ws,
                        "close-all-workspaces: a restore is still pending; skipping"
                    );
                    continue;
                }
                if self.with_model(|w| w.switch_workspace(ws)).is_ok() {
                    self.close_workspace();
                }
            }
            if let Err(err) = self.with_model(|w| w.switch_workspace(seed)) {
                warn!(%err, "close-all-workspaces: cannot show the fresh seed");
            }
        }
        if let Err(err) = self.with_model(|w| w.set_focused_output(focused)) {
            warn!(%err, "close-all-workspaces: cannot restore the focused output");
        }
        self.write_session();
    }

    /// Notes every connected, bound output's active workspace as last viewed there. Called by
    /// `apply_layout`, the only model-to-Space path.
    pub(crate) fn note_last_viewed_outputs(&mut self) {
        let world = &self.layout.world;
        let viewed: Vec<(OutputId, WorkspaceId, String)> = self
            .space
            .outputs()
            .filter_map(|o| {
                let id = crate::layout::model_output_id(o)?;
                let ws = world.output(id)?.active_workspace();
                Some((id, ws, o.name()))
            })
            .collect();
        note_last_viewed(&mut self.persist.workspace.meta, &viewed);
    }

    /// Closes the focused output's active workspace (`close-workspace`): refuse while a restore is
    /// pending or on the output's last workspace; flush its record (refuse if the write fails);
    /// drop it from the model; then end each of its windows (kill the held child, else a polite
    /// close) after releasing its binding so no late teardown lands elsewhere.
    pub(crate) fn close_workspace(&mut self) {
        let world = &self.layout.world;
        let ws = world.active_workspace_id();
        // (a) A restore in flight would race the teardown; refuse before any flush.
        if self.workspace_has_pending(ws) {
            info!(?ws, "close-workspace: a restore is still pending; refusing");
            return;
        }
        // (b) Never the output's last workspace (checked before flushing anything).
        let output = world.focused_output();
        if world
            .output(output)
            .is_none_or(|o| o.workspaces().len() < 2)
        {
            info!(
                ?ws,
                "close-workspace: last workspace on this output; refusing"
            );
            return;
        }
        // (c) Flush first; a failed write refuses the close (never tear down unsaved state).
        let Some(record) = self.capture_record(ws) else {
            warn!(?ws, "close-workspace: workspace vanished; refusing");
            return;
        };
        if !skip_record(&record) && !self.write_record(record, Instant::now()) {
            warn!(?ws, "close-workspace: save failed; refusing to close");
            return;
        }
        // (d) Snapshot the panes before the model forgets them (for step (g)).
        let panes = self
            .layout
            .world
            .workspace(ws)
            .map(|w| w.panes())
            .unwrap_or_default();
        // (e) The lifecycle controller drops the workspace, its bindings and its `Space` mappings.
        let ended = match self.end_workspace(ws) {
            Ok(ended) => ended,
            Err(err) => {
                info!(%err, ?ws, "close-workspace refused by the model");
                return;
            }
        };
        // (f) Policy: end each window — kill a CDE-spawned child, else ask the client to close.
        for (element, meta) in ended {
            self.end_window(meta.pid, Some(&element));
        }
        // (g) Drop per-workspace bookkeeping.
        self.forget_workspace(ws, &panes);
        info!(?ws, "workspace closed");
    }

    /// Drops everything keyed by the removed workspace `ws`: its meta (root, last output), its
    /// pending placements and its placed entries, by the `panes` it had.
    pub(super) fn forget_workspace(&mut self, ws: WorkspaceId, panes: &[PaneId]) {
        self.persist.workspace.meta.remove(&ws);
        self.persist
            .placement
            .pending
            .retain(|_, p| p.target.pane().is_none_or(|pane| !panes.contains(&pane)));
        for pane in panes {
            self.persist.placement.placed.remove(pane);
        }
    }

    /// Moves the focused output's active workspace `delta` outputs along the model output order
    /// (`move-workspace-to-output`). No-op with one output; refused on the source's last workspace.
    pub(crate) fn move_active_workspace_to_output(&mut self, delta: i64) {
        let world = &self.layout.world;
        let order: Vec<OutputId> = world.output_ids().collect();
        let source = world.focused_output();
        let Some(target) = shifted_output(&order, source, delta) else {
            debug!(delta, "move-workspace-to-output: no other output; ignoring");
            return;
        };
        let ws = world.active_workspace_id();
        if let Err(err) = self.with_model(|w| w.move_workspace_to_output(ws, target)) {
            info!(%err, ?ws, "move-workspace-to-output refused");
        }
    }

    pub(super) fn save_slots(&self) {
        let path = slots_path(&self.persist.store_dir);
        if let Err(err) = self.persist.slots.save(&path) {
            warn!(%err, path = %path.display(), "cannot save the slot pins");
        }
    }

    /// `Action::PinSlot(n)`: pins the active workspace's name to slot `n` and saves the pins.
    pub(crate) fn pin_slot(&mut self, n: u8) {
        let ws = self.layout.world.active_workspace_id();
        let Some(name) = self.layout.world.workspace(ws).map(|w| w.name.clone()) else {
            return;
        };
        if !self.persist.slots.pin(n, &name) {
            return;
        }
        info!(slot = n, workspace = name, "slot pinned");
        self.save_slots();
    }

    /// Applies `event` to the pending slot pin (see [`pin_transition`]), pinning when it says so.
    pub(super) fn pin_event(&mut self, event: PinEvent) {
        let (pin, rest) = pin_transition(self.persist.workspace.pending_pin, event);
        self.persist.workspace.pending_pin = rest;
        if let Some(n) = pin {
            self.pin_slot(n);
        }
    }

    /// After an open/new/open-folder request: pins the now-active workspace to the pending slot on
    /// success and clears the pending pin either way ([`PinEvent::Completed`]).
    pub(super) fn complete_pending_pin(
        &mut self,
        result: Result<(), String>,
    ) -> Result<(), String> {
        self.pin_event(PinEvent::Completed {
            succeeded: result.is_ok(),
        });
        result
    }

    /// `Action::Slot(n)`: opens the workspace pinned to slot `n` (live, else restored from disk
    /// on its last-viewed output). An unpinned slot, or a stale pin (the workspace is neither live
    /// nor saved), opens the picker in pin mode; any other failed open is an `info!` no-op and the
    /// pin is kept.
    pub(crate) fn slot_open(&mut self, n: u8) {
        let pinned = self.persist.slots.get(n).map(str::to_string);
        let target = match pinned {
            None => SlotTarget::OpenPicker { stale: None },
            Some(name) => {
                let live = self.layout.world.workspace_by_name(&name).is_some();
                // An invalid name has no saved file, so it counts as not saved (stale).
                let saved = self.persist.file_of(&name).is_ok_and(|p| p.exists());
                slot_target(Some(name), live, saved)
            }
        };
        match target {
            SlotTarget::OpenPicker { stale } => {
                match stale {
                    None => info!(
                        slot = n,
                        "slot is not pinned; opening the picker in pin mode"
                    ),
                    Some(name) => info!(
                        slot = n,
                        workspace = name,
                        "pinned workspace no longer exists; opening the picker in pin mode"
                    ),
                }
                self.pin_event(PinEvent::SlotOpen(n));
                self.open_picker(PickerMode::Pick);
            }
            SlotTarget::Pinned(name) => {
                if let Err(reason) = self.ipc_open(&name) {
                    info!(
                        slot = n,
                        workspace = name,
                        reason,
                        "slot workspace not opened"
                    );
                }
            }
        }
    }
}

/// What a slot press resolves to.
#[derive(Debug, PartialEq, Eq)]
enum SlotTarget {
    /// Open the pin-mode picker: no pin (`stale` is `None`), or a pin to a workspace that is
    /// neither live nor saved (`stale` is its name).
    OpenPicker { stale: Option<String> },
    /// Pinned to a live or saved workspace: open it (a failure is an `info!` no-op).
    Pinned(String),
}

/// Pure decision for `slot_open`: `pinned` is the slot's pin, `live`/`saved` whether that
/// workspace is live / has a saved file.
fn slot_target(pinned: Option<String>, live: bool, saved: bool) -> SlotTarget {
    match pinned {
        None => SlotTarget::OpenPicker { stale: None },
        Some(name) if live || saved => SlotTarget::Pinned(name),
        Some(name) => SlotTarget::OpenPicker { stale: Some(name) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_target_decisions() {
        let pin = || Some("vault".to_string());
        assert_eq!(
            slot_target(None, false, false),
            SlotTarget::OpenPicker { stale: None }
        );
        assert_eq!(
            slot_target(pin(), false, false),
            SlotTarget::OpenPicker {
                stale: Some("vault".into())
            }
        );
        assert_eq!(
            slot_target(pin(), true, false),
            SlotTarget::Pinned("vault".into())
        );
        assert_eq!(
            slot_target(pin(), false, true),
            SlotTarget::Pinned("vault".into())
        );
    }

    #[test]
    fn non_stale_pin_routes_to_open_so_failures_stay_a_no_op() {
        // A saved-but-unrestorable workspace is Pinned, not Stale: slot_open calls ipc_open and
        // only logs its error, never opening the picker.
        assert!(matches!(
            slot_target(Some("broken".into()), false, true),
            SlotTarget::Pinned(_)
        ));
    }

    #[test]
    fn pending_pin_truth_table() {
        use PinEvent::{Completed, OpenOverlay, OverlayClosed, SlotOpen};
        // (events applied in order from no pending pin, pins made, pending pin at the end)
        let rows: &[(&[PinEvent], &[u8], Option<u8>)] = &[
            // (a) unpinned slot: pending, and the plain picker open (or refocus) keeps it.
            (
                &[SlotOpen(3), OpenOverlay { keeps_pin: true }],
                &[],
                Some(3),
            ),
            (
                &[
                    SlotOpen(3),
                    OpenOverlay { keeps_pin: true },
                    OpenOverlay { keeps_pin: true },
                ],
                &[],
                Some(3),
            ),
            (&[SlotOpen(1), SlotOpen(2)], &[], Some(2)),
            // (b) launcher / settings / rename picker clear it, even when ignored or refocused.
            (&[SlotOpen(3), OpenOverlay { keeps_pin: false }], &[], None),
            (
                &[
                    SlotOpen(3),
                    OpenOverlay { keeps_pin: true },
                    OpenOverlay { keeps_pin: false },
                ],
                &[],
                None,
            ),
            // (c) an overlay close or the tick give-up clears it.
            (
                &[SlotOpen(3), OpenOverlay { keeps_pin: true }, OverlayClosed],
                &[],
                None,
            ),
            (&[OverlayClosed], &[], None),
            // (d) open/new/open-folder pin on success and clear either way.
            (&[SlotOpen(3), Completed { succeeded: true }], &[3], None),
            (
                &[
                    SlotOpen(0),
                    OpenOverlay { keeps_pin: true },
                    Completed { succeeded: true },
                ],
                &[0],
                None,
            ),
            (&[SlotOpen(3), Completed { succeeded: false }], &[], None),
            (&[Completed { succeeded: true }], &[], None),
            (&[Completed { succeeded: false }], &[], None),
            (
                &[SlotOpen(3), OverlayClosed, Completed { succeeded: true }],
                &[],
                None,
            ),
        ];
        for (events, pins, end) in rows {
            let mut pending = None;
            let mut pinned = Vec::new();
            for &event in *events {
                let (pin, rest) = pin_transition(pending, event);
                pinned.extend(pin);
                pending = rest;
            }
            assert_eq!((pinned.as_slice(), pending), (*pins, *end), "{events:?}");
        }
    }

    #[test]
    fn first_free_number_skips_taken() {
        assert_eq!(first_free_number(|_| false), "1");
        assert_eq!(first_free_number(|n| n == "1" || n == "2"), "3");
    }

    #[test]
    fn new_workspace_name_skips_live_and_saved() {
        let world = World::new("1"); // live "1"
        let saved: HashSet<String> = ["2".to_string()].into();
        assert_eq!(new_workspace_name(&world, &saved), "3");
        assert_eq!(new_workspace_name(&world, &HashSet::new()), "2");
    }

    /// The startup seed equals the old name-then-rename path: the first decimal not saved.
    #[test]
    fn startup_seed_skips_saved_names() {
        let saved = |names: &[&str]| names.iter().map(|n| (*n).to_string()).collect();
        assert_eq!(startup_seed_name(&saved(&[])), "1");
        assert_eq!(startup_seed_name(&saved(&["1"])), "2");
        assert_eq!(startup_seed_name(&saved(&["2"])), "1");
    }

    #[test]
    fn folder_names_disambiguate_with_the_parent() {
        let p = Path::new("/code/proj");
        assert_eq!(folder_workspace_name(p, |_| false).as_deref(), Some("proj"));
        assert_eq!(
            folder_workspace_name(p, |n| n == "proj").as_deref(),
            Some("proj (code)")
        );
        assert_eq!(
            folder_workspace_name(p, |n| n == "proj" || n == "proj (code)").as_deref(),
            Some("proj (code) 2")
        );
        assert_eq!(folder_workspace_name(Path::new("/"), |_| false), None);
    }

    fn oid(n: u64) -> OutputId {
        OutputId::from_raw(n)
    }

    #[test]
    fn shifted_output_wraps_both_ways_and_skips_single_output() {
        let order = [oid(1), oid(2), oid(3)];
        assert_eq!(shifted_output(&order, oid(1), 1), Some(oid(2)));
        assert_eq!(shifted_output(&order, oid(3), 1), Some(oid(1)));
        assert_eq!(shifted_output(&order, oid(1), -1), Some(oid(3)));
        assert_eq!(shifted_output(&order, oid(2), 7), Some(oid(3)));
        assert_eq!(shifted_output(&order, oid(2), -8), Some(oid(3)));
        // A full lap lands back on the source: no-op.
        assert_eq!(shifted_output(&order, oid(2), 3), None);
        assert_eq!(shifted_output(&[oid(1)], oid(1), 1), None);
        assert_eq!(shifted_output(&order, oid(9), 1), None);
    }

    #[test]
    fn restore_target_prefers_the_connected_last_viewed_output() {
        let connected = vec![("HDMI-1".to_string(), oid(1)), ("DP-1".to_string(), oid(2))];
        assert_eq!(restore_target(Some("DP-1"), &connected, oid(1)), oid(2));
        assert_eq!(restore_target(Some("DP-9"), &connected, oid(1)), oid(1));
        assert_eq!(restore_target(None, &connected, oid(2)), oid(2));
    }

    fn last(map: &HashMap<WorkspaceId, WorkspaceMeta>, ws: WorkspaceId) -> Option<&str> {
        map.get(&ws).and_then(|m| m.last_output.as_deref())
    }

    #[test]
    fn note_last_viewed_records_only_active_workspaces() {
        let mut map = HashMap::new();
        let (a, b) = (WorkspaceId::from_raw(1), WorkspaceId::from_raw(2));
        note_last_viewed(&mut map, &[(oid(1), a, "DP-1".into())]);
        note_last_viewed(&mut map, &[(oid(1), b, "HDMI-1".into())]);
        assert_eq!(last(&map, a), Some("DP-1"));
        assert_eq!(last(&map, b), Some("HDMI-1"));
    }

    /// Audit E2: a workspace last viewed on A, migrated inactive onto B by A's unplug, still says A
    /// and returns to A once it reconnects.
    #[test]
    fn unplug_migration_keeps_the_last_viewed_output() {
        let mut world = World::new("1");
        let a = world.focused_output();
        let b = world
            .add_output(crucible_layout::Rect::new(1920, 0, 1920, 1080), "2")
            .unwrap();
        let on_a = world.active_workspace_id();
        let on_b = world.output(b).unwrap().active_workspace();
        let mut map = HashMap::new();
        let viewed =
            |w: &World, outs: &[(OutputId, &str)]| -> Vec<(OutputId, WorkspaceId, String)> {
                outs.iter()
                    .map(|(o, n)| {
                        (
                            *o,
                            w.output(*o).unwrap().active_workspace(),
                            (*n).to_string(),
                        )
                    })
                    .collect()
            };
        note_last_viewed(&mut map, &viewed(&world, &[(a, "DP-1"), (b, "HDMI-1")]));
        world.remove_output(a).unwrap();
        assert!(world.output(b).unwrap().workspaces().contains(&on_a));
        assert_eq!(world.output(b).unwrap().active_workspace(), on_b);
        note_last_viewed(&mut map, &viewed(&world, &[(b, "HDMI-1")]));
        assert_eq!(last(&map, on_a), Some("DP-1"));
        // A reconnects as a fresh model output; the record (output = DP-1) targets it.
        let a2 = world
            .add_output(crucible_layout::Rect::new(0, 0, 1920, 1080), "3")
            .unwrap();
        let connected = vec![("HDMI-1".to_string(), b), ("DP-1".to_string(), a2)];
        assert_eq!(restore_target(last(&map, on_a), &connected, b), a2);
    }
}
