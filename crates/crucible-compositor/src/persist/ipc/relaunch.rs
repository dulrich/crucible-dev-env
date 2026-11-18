//! The relaunch path: swapping a placeholder window for its app and finding its siblings.

use super::*;

impl<B: Backend + 'static> CrucibleState<B> {
    /// Swaps the placeholder window run by `peer` for its app: spawn the saved cmdline, register
    /// the placeholder's live slot as pending, close the placeholder.
    pub(in crate::persist) fn ipc_relaunch(&mut self, peer: Option<u32>) -> Result<(), String> {
        let peer = peer.ok_or_else(|| "not a relaunch placeholder".to_string())?;
        let args = crucible_persist::capture::read_cmdline(proc_root(), peer);
        let capture =
            crucible_persist::RelaunchWrapper::from_argv(&args, &crucible_persist::own_exe_name())
                .ok_or_else(|| "not a relaunch placeholder".to_string())?;
        let ancestors = crucible_persist::ancestors(proc_root(), peer, ANCESTOR_DEPTH);
        let layout = &self.layout;
        let windows: Vec<(Option<u32>, Option<ProcAnchor>)> =
            layout.metas().map(|(_, m)| (m.pid, m.anchor())).collect();
        let pid = relaunch_window_pid(peer, &ancestors, &windows)
            .ok_or_else(|| "no placeholder window".to_string())?;
        let mut ids: Vec<WindowId> = layout
            .metas()
            .filter(|(_, m)| m.pid == Some(pid))
            .map(|(id, _)| id)
            .collect();
        ids.sort();
        // A sibling still queued by a live spawn joins that spawn: no second process. Expired
        // spawns took their queues with their pending entries.
        if let Some(id) = ids
            .iter()
            .copied()
            .find(|id| self.persist.placement.is_queued(*id))
        {
            info!(
                ?id,
                "placeholder is queued on a live relaunch; not respawning"
            );
            return Ok(());
        }
        let world = &self.layout.world;
        let (id, own) = ids
            .into_iter()
            .find_map(|id| Some((id, self.placeholder_slot(id)?)))
            .ok_or_else(|| "no placeholder window".to_string())?;
        let pane = own.pane;
        let ws = self
            .live_workspaces()
            .into_iter()
            .map(|(_, ws)| ws)
            .find(|ws| {
                world
                    .workspace(*ws)
                    .is_some_and(|w| w.panes().contains(&pane))
            })
            .unwrap_or_else(|| world.active_workspace_id());
        let element = self.layout.element(id).cloned();
        // The app stands for the placeholder's saved record: carried as the spawn's origin.
        let origin = crucible_persist::WindowRecord::App {
            app_id: capture.app_id.clone(),
            cmdline: Some(capture.cmdline.clone()),
        };
        let dir = self.spawn_dir(DirPolicy::Inherit, ws);
        let pid = self
            .spawn(
                SpawnRequest::shell_app(&capture.cmdline)
                    .in_dir(dir.run_in)
                    .with_origin(Some(origin)),
            )
            .ok_or_else(|| "spawn failed".to_string())?;
        let siblings = self.sibling_placeholders(id, &capture.cmdline);
        let mut queue = VecDeque::from([SlotEntry::Resolved(own)]);
        queue.extend(siblings.iter().map(|s| SlotEntry::Placeholder(*s)));
        self.persist.placement.pending.insert(
            pid,
            Pending {
                target: Target::Slot(own),
                since: Instant::now(),
                queue: Some(queue),
            },
        );
        // The relaunched placeholder's own window stays a plain close (deliberately not
        // `end_window` / `end_placeholder`): its `crucible relaunch` wrapper is this IPC peer.
        if let Some(element) = element {
            close_element(&element);
        }
        info!(
            pid,
            cmdline = capture.cmdline,
            siblings = siblings.len(),
            "relaunching app into its placeholder slot"
        );
        Ok(())
    }

    /// The other live relaunch placeholders of `own` that stand for `cmdline`, in workspace, pane
    /// and stack order, not already queued by another spawn. A placeholder is recognised from its
    /// spawn-time facts ([`placeholder_cmdline`]), with no `/proc` read.
    fn sibling_placeholders(&self, own: WindowId, cmdline: &str) -> Vec<WindowId> {
        let world = &self.layout.world;
        let mut candidates = Vec::new();
        for (_, ws) in self.live_workspaces() {
            let Some(w) = world.workspace(ws) else {
                continue;
            };
            for pane in w.panes() {
                let Some(pane) = world.pane(pane) else {
                    continue;
                };
                for id in pane.windows() {
                    let placeholder = self
                        .layout
                        .meta(*id)
                        .and_then(placeholder_cmdline)
                        .map(str::to_string);
                    candidates.push((*id, placeholder));
                }
            }
        }
        let placement = &self.persist.placement;
        select_siblings(candidates, own, cmdline, |id| placement.is_queued(id))
    }
}
