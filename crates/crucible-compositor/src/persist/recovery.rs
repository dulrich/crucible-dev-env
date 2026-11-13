//! Window capture and loss recovery (crucible-term plan 03): the one per-window capture
//! ([`CrucibleState::window_record`]) both auto-save and recovery share, the per-tick capture
//! that feeds both ([`CrucibleState::tick_capture`]), and the recovery set a lost window's
//! placeholder is built from. Each window's `/proc` anchor is decided once, by
//! [`WindowMeta::anchor`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crucible_layout::{PaneId, WindowId, WorkspaceId};
use crucible_persist::{CaptureEnv, ProcAnchor, SpawnClass, WindowRecord, WindowRecordInput};
use tracing::{info, warn};

use super::placement::{self, LostSlot, Pending, Target};
use super::{SpawnKind, SpawnRequest, capture_env, element_app_id};
use crate::element::WindowElement;
use crate::layout::{WindowClass, WindowMeta};
use crate::state::{Backend, CrucibleState};

impl WindowMeta {
    /// Where this window's `/proc` reads start, decided once: a bound cterm window's identity
    /// root (its shell or CDE wrapper under the shared daemon), else its client pid (unbound:
    /// unchanged). `None` when neither is known.
    #[must_use]
    pub(crate) fn anchor(&self) -> Option<ProcAnchor> {
        ProcAnchor::of(self.pid, self.identity.as_ref().map(|i| i.root_pid))
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// The saved record for window `id`: its meta (pid, class, spawn origin, anchor, the bound
    /// identity's session key), element app id and the exec flag, classified by
    /// [`crucible_persist::window_record`].
    pub(super) fn window_record(&self, env: &CaptureEnv, id: WindowId) -> WindowRecord {
        let meta = self.layout.meta(id);
        let class = if meta.is_some_and(|m| m.class == WindowClass::Terminal) {
            SpawnClass::Terminal
        } else {
            SpawnClass::App
        };
        crucible_persist::window_record(
            env,
            WindowRecordInput {
                pid: meta.and_then(|m| m.pid),
                class,
                origin: meta.and_then(|m| m.origin.as_ref()),
                app_id: self.layout.element(id).and_then(element_app_id),
                exec_flag: &self.config.terminal.exec_flag,
                anchor: meta.and_then(WindowMeta::anchor),
                // The bound identity's key wins over whatever the origin carried (they are the
                // same key unless the window was re-bound).
                session: meta.and_then(|m| m.identity.as_ref()?.session.clone()),
            },
        )
    }

    /// This tick's one `/proc` capture, under one [`capture_env`]: the record of every window
    /// the auto-save would capture (live, not ephemeral, no pending restore) and of every bound
    /// window whose client is still connected. [`Self::refresh_recovery`] and `save_dirty` both
    /// read it; a window missing from it is captured fresh there.
    pub(super) fn tick_capture(&self) -> HashMap<WindowId, WindowRecord> {
        let world = &self.layout.world;
        let mut ids: HashSet<WindowId> = self
            .layout
            .metas()
            .filter(|(_, m)| {
                m.identity
                    .as_ref()
                    .is_some_and(|i| !i.client.is_disconnected())
            })
            .map(|(id, _)| id)
            .collect();
        for (_, ws) in self.live_workspaces() {
            if self.persist.workspace.is_ephemeral(ws) || self.workspace_has_pending(ws) {
                continue;
            }
            let Some(w) = world.workspace(ws) else {
                continue;
            };
            for pane in w.panes() {
                ids.extend(world.pane(pane).into_iter().flat_map(|p| p.windows()));
            }
        }
        let env = capture_env();
        ids.into_iter()
            .map(|id| (id, self.window_record(&env, id)))
            .collect()
    }

    // ---- loss recovery (crucible-term plan 03) -------------------------------------------------

    /// Sets the recovery record of a just-mapped window `id` when it is bound: its spawn origin
    /// (with the identity's session key) when there is one, else a capture. Unbound: nothing.
    pub(super) fn init_recovery(&mut self, id: WindowId) {
        let Some(meta) = self.layout.meta(id) else {
            return;
        };
        let Some(identity) = &meta.identity else {
            return;
        };
        let record = match &meta.origin {
            Some(origin) => origin.clone().with_session(identity.session.clone()),
            None => self.window_record(&capture_env(), id),
        };
        self.persist.recovery.insert(id, record);
    }

    /// Refreshes the recovery record of every bound window (each tick, before and independent of
    /// the save gate) and drops the records of closed windows. A window whose client has already
    /// disconnected keeps its last record (its shell may be dying). The others take this tick's
    /// `captured` record ([`Self::tick_capture`]), a window missing from it captured fresh.
    pub(super) fn refresh_recovery(&mut self, captured: &HashMap<WindowId, WindowRecord>) {
        let bound: Vec<(WindowId, bool)> = self
            .layout
            .metas()
            .filter_map(|(id, m)| Some((id, m.identity.as_ref()?.client.is_disconnected())))
            .collect();
        let mut fresh = HashMap::with_capacity(bound.len());
        for (id, gone) in bound {
            let kept = gone
                .then(|| self.persist.recovery.get(&id).cloned())
                .flatten();
            let record = kept
                .or_else(|| captured.get(&id).cloned())
                .unwrap_or_else(|| self.window_record(&capture_env(), id));
            fresh.insert(id, record);
        }
        self.persist.recovery = fresh;
    }

    /// Drops a pane's loss reservation once no loss placeholder for it is pending.
    pub(super) fn prune_reservations(&mut self) {
        let placement = &mut self.persist.placement;
        let waiting: HashSet<PaneId> = placement
            .pending
            .values()
            .filter_map(|p| match p.target {
                Target::Lost(LostSlot { pane, .. }) => Some(pane),
                _ => None,
            })
            .collect();
        placement.reserved.retain(|pane, _| waiting.contains(pane));
    }

    /// Drops window `id`'s recovery record (a normal close).
    pub(crate) fn forget_recovery(&mut self, id: WindowId) {
        self.persist.recovery.remove(&id);
    }

    /// Bookkeeping after managed window `id` closed (mirror of `unmanaged_closed`): forgets its
    /// recovery record, and an ephemeral `run-on` workspace goes with its last window.
    pub(crate) fn managed_closed(&mut self, id: WindowId) {
        self.forget_recovery(id);
        self.prune_ephemeral();
    }

    /// The workspace holding `pane`.
    fn workspace_of_pane(&self, pane: PaneId) -> Option<WorkspaceId> {
        self.live_workspaces().into_iter().find_map(|(_, ws)| {
            let w = self.layout.world.workspace(ws)?;
            w.panes().contains(&pane).then_some(ws)
        })
    }

    /// Handles a lost window `id` (its bound client disconnected; plan 03): plans its placeholder
    /// from its recovery record (never `/proc`), reserves its slot against the live stack, closes
    /// it in the model through `close` and spawns the placeholder into the slot with a pending
    /// entry, which holds the workspace's saves until it maps or expires.
    pub(crate) fn window_lost(&mut self, id: WindowId, close: impl Fn(&WindowElement) -> bool) {
        let record = self
            .persist
            .recovery
            .remove(&id)
            .or_else(|| self.layout.meta(id).and_then(|m| m.origin.clone()));
        let slot = self.placeholder_slot(id);
        let root = slot
            .and_then(|s| self.workspace_of_pane(s.pane))
            .and_then(|ws| self.persist.workspace.root(ws).cloned());
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let job = match &record {
            Some(record) => crucible_persist::prepare_lost(
                record,
                root.as_deref(),
                Self::crucible_exe("loss placeholder")
                    .ok_or_else(|| std::io::Error::other("crucible binary unavailable")),
                home.as_deref(),
                &self.config.terminal.exec_flag,
                Path::is_dir,
            ),
            None => Err("no recovery record".to_string()),
        };
        let (slot, job) = match (slot, job) {
            (Some(slot), Ok(Some(job))) => (slot, job),
            (slot, job) => {
                warn!(
                    ?id,
                    ?slot,
                    ?job,
                    "window lost; no placeholder can stand in, closing it"
                );
                self.unmap_matching(close);
                return;
            }
        };
        let live: Vec<WindowId> = self
            .layout
            .world
            .pane(slot.pane)
            .map(|p| p.windows().to_vec())
            .unwrap_or_default();
        let order = placement::reserve(
            self.persist
                .placement
                .reserved
                .get(&slot.pane)
                .map(Vec::as_slice),
            &live,
        );
        self.persist.placement.reserved.insert(slot.pane, order);
        // Hook-free: the recovery record is already gone and the placeholder is not pending yet, so
        // the managed-close prune would remove the ephemeral workspace it is about to fill.
        self.teardown_matching(close);
        let target = Target::Lost(LostSlot {
            pane: slot.pane,
            lost: id,
            focused: slot.focused,
            saved_index: slot.saved_index,
        });
        let program = self.terminal_program();
        let args = self.with_tiling_args(&program, job.args);
        let req = SpawnRequest::new(program, args, SpawnKind::Terminal)
            .in_dir(Some(job.dir))
            .with_origin(Some(job.origin));
        match self.spawn(req) {
            Some(pid) => {
                info!(?id, pid, pane = ?slot.pane, "window lost; placeholder spawned into its slot");
                self.persist
                    .placement
                    .pending
                    .insert(pid, Pending::new(target, Instant::now()));
            }
            None => warn!(?id, "window lost; its placeholder could not be spawned"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::{ClientLink, WindowIdentity};

    /// Moved from placement's relaunch test (was `proc_root_pid`): placeholder termination and
    /// capture root at the identity. Thin clients C1, C2 of one daemon; their roots R1, R2.
    #[test]
    fn anchor_is_the_bound_root_else_the_client_pid() {
        const C1: u32 = 500;
        const C2: u32 = 501;
        const R1: u32 = 601;
        const R2: u32 = 602;
        let bound = |spawn_pid: u32, root_pid| WindowIdentity {
            id: spawn_pid,
            spawn_pid,
            root_pid,
            session: None,
            client: ClientLink::default(),
        };
        let meta = |pid, identity| WindowMeta {
            pid,
            class: WindowClass::Terminal,
            origin: None,
            identity,
        };
        let shell = |r| Some(ProcAnchor::Shell(r));
        assert_eq!(meta(Some(C1), Some(bound(C1, R1))).anchor(), shell(R1));
        assert_eq!(meta(Some(C2), Some(bound(C2, R2))).anchor(), shell(R2));
        assert_eq!(
            meta(Some(99), None).anchor(),
            Some(ProcAnchor::Terminal(99))
        );
        assert_eq!(meta(None, None).anchor(), None);
    }
}

/// Loss recovery (crucible-term plan 03), moved from placement's tests: the loss decision, the
/// placeholder job and the slot reservation, through the pure seams.
#[cfg(test)]
mod loss_tests {
    use super::super::WindowIdentity;
    use super::super::placement::tests::{C1, R1, bound, ids};
    use super::super::placement::{fill_reservation, pending_in, reserve, reserved_index};
    use super::*;

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
            fill_reservation(&mut order, lost[k], new[k]);
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
        let record = origin.with_session(Some("k2".into()));
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
