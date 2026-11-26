//! The IPC focus requests: `focus-pid`, `workspace-of-pid` and `focus-live` (plan 54).

use super::*;
use crucible_layout::{Rect, World};
use tracing::debug;

/// The window a process `pid` stands for, over `windows` as `(id, meta.pid, anchor)`: the
/// `relaunch` matcher ([`relaunch_window_pid`], unchanged) over `pid` and its `ancestors` gives a
/// `meta.pid`, and the lowest [`WindowId`] with that pid wins.
fn window_for_pid(
    pid: u32,
    ancestors: &[u32],
    windows: &[(WindowId, Option<u32>, Option<ProcAnchor>)],
) -> Option<WindowId> {
    let pairs: Vec<(Option<u32>, Option<ProcAnchor>)> =
        windows.iter().map(|(_, m, a)| (*m, *a)).collect();
    let meta_pid = relaunch_window_pid(pid, ancestors, &pairs)?;
    windows
        .iter()
        .filter(|(_, m, _)| *m == Some(meta_pid))
        .map(|(id, _, _)| *id)
        .min()
}

/// The workspace `focus-live <name>` switches to: `name` must be a workspace of `world` that is
/// live (in `live`). A saved-only (or unknown) name is refused; nothing is restored.
fn live_focus_target(
    world: &World,
    live: &[WorkspaceId],
    name: &str,
) -> Result<WorkspaceId, String> {
    world
        .workspace_by_name(name)
        .filter(|ws| live.contains(ws))
        .ok_or_else(|| format!("no live workspace {name}"))
}

/// The centre of `rect` in global logical coordinates (half-pixel for odd extents).
fn pane_centre(rect: Rect) -> (f64, f64) {
    (
        f64::from(rect.x) + f64::from(rect.width) / 2.0,
        f64::from(rect.y) + f64::from(rect.height) / 2.0,
    )
}

/// Why the post-focus pointer warp is skipped, or `None` to warp. `rect` is the focused pane's
/// global rect (`None` when its output has no mapped binding); `grabbed` a seat pointer grab;
/// `header_pressed` an outstanding header-consumed press (its release reads the current pointer
/// location); `constrained` a pointer constraint still active after the focus change.
fn warp_skip(
    rect: Option<Rect>,
    grabbed: bool,
    header_pressed: bool,
    constrained: bool,
) -> Result<Rect, &'static str> {
    let rect = rect.ok_or("pane not shown")?;
    if rect.width <= 0 || rect.height <= 0 {
        return Err("zero-extent pane");
    }
    if grabbed {
        return Err("pointer grabbed");
    }
    if header_pressed {
        return Err("header press outstanding");
    }
    if constrained {
        return Err("pointer constraint active");
    }
    Ok(rect)
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Plan 55: after a successful IPC focus, moves the pointer to the focused pane's centre
    /// when the pointer is free and the pane is shown ([`warp_skip`]). Never changes keyboard or
    /// model focus: the constraint is reconciled at the delivered location and both hover trackers
    /// settle there without refocusing.
    fn warp_to_focused_pane(&mut self) {
        let pane = self.layout.world.focused_pane_id();
        let rect =
            self.layout
                .world
                .pane_rect(&self.output_bindings(), crate::layout::NO_GAPS, pane);
        let rect = match warp_skip(
            rect,
            self.pointer.is_grabbed(),
            !self.header.consumed.is_empty(),
            self.pointer_lock.has_active(),
        ) {
            Ok(rect) => rect,
            Err(reason) => {
                debug!(reason, rect = ?rect, "ipc focus: pointer warp skipped");
                return;
            }
        };
        let (x, y) = pane_centre(rect);
        let mode = self.input_mode();
        let time = self.clock.now().as_millis();
        self.deliver_motion(
            &mode,
            (x, y).into(),
            smithay::utils::SERIAL_COUNTER.next_serial(),
            time,
            None,
        );
        let keyboard = self.keyboard_focus_surface();
        self.reconcile_pointer_constraint(keyboard);
        self.settle_hover_at_pointer();
        debug!(
            location = ?self.pointer.current_location(),
            rect = ?rect,
            "ipc focus: pointer warped to the focused pane centre"
        );
    }

    /// The layout-bound window process `pid` (or its nearest ancestor) stands for.
    fn resolve_pid_window(&self, pid: u32) -> Option<WindowId> {
        let ancestors = crucible_persist::ancestors(proc_root(), pid, ANCESTOR_DEPTH);
        let windows: Vec<(WindowId, Option<u32>, Option<ProcAnchor>)> = self
            .layout
            .metas()
            .map(|(id, m)| (id, m.pid, m.anchor()))
            .collect();
        window_for_pid(pid, &ancestors, &windows)
    }

    /// The (field-encoded) name of the workspace holding `id`, live or hidden.
    fn workspace_name_of(&self, id: WindowId) -> Option<String> {
        let world = &self.layout.world;
        let ws = world.workspace_of_window(id)?;
        Some(crate::ipc::encode_field(&world.workspace(ws)?.name))
    }

    /// `workspace-of-pid`: a read-only lookup; never changes focus.
    pub(super) fn ipc_workspace_of_pid(&self, pid: u32) -> Response {
        match self
            .resolve_pid_window(pid)
            .and_then(|id| self.workspace_name_of(id))
        {
            Some(name) => Response::Ok(name),
            None => Response::Err(format!("no window for pid {pid}")),
        }
    }

    /// `focus-pid`: focus the window `pid` stands for (switching to its workspace and output).
    pub(super) fn ipc_focus_pid(&mut self, pid: u32) -> Response {
        if let Err(reason) = self.ipc_focus_admitted() {
            return Response::Err(reason);
        }
        let Some(id) = self.resolve_pid_window(pid) else {
            return Response::Err(format!("no window for pid {pid}"));
        };
        let Some(name) = self.workspace_name_of(id) else {
            return Response::Err(format!("no window for pid {pid}"));
        };
        let focused = self.with_hover_absorbed(|s| s.with_model(|w| w.focus_window(id)));
        if let Err(e) = focused {
            return Response::Err(e.to_string());
        }
        self.reconcile_keyboard_focus();
        self.warp_to_focused_pane();
        Response::Ok(name)
    }

    /// `focus-live`: switch to the live workspace `name` only. Never restores, never applies the
    /// bring policy, never completes a slot pin. An empty workspace is a switch with no keyboard
    /// target.
    pub(super) fn ipc_focus_live(&mut self, name: &str) -> Response {
        if let Err(reason) = self.ipc_focus_admitted() {
            return Response::Err(reason);
        }
        let live: Vec<WorkspaceId> = self.live_workspaces().into_iter().map(|(_, w)| w).collect();
        let ws = match live_focus_target(&self.layout.world, &live, name) {
            Ok(ws) => ws,
            Err(reason) => return Response::Err(reason),
        };
        if let Err(reason) = self.with_hover_absorbed(|s| s.switch_to_live(ws)) {
            return Response::Err(reason);
        }
        self.reconcile_keyboard_focus();
        self.warp_to_focused_pane();
        Response::Ok(crate::ipc::encode_field(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAEMON: u32 = 9031;
    const ROOT_A: u32 = 1307353;
    const ROOT_B: u32 = 1400000;
    const CLIENT_A: u32 = 500;
    const CLIENT_B: u32 = 501;

    fn id(n: u64) -> WindowId {
        WindowId::from_raw(n)
    }

    /// Two cterm windows bound to their own shell roots under one shared daemon, plus an unbound
    /// app window.
    fn windows() -> Vec<(WindowId, Option<u32>, Option<ProcAnchor>)> {
        vec![
            (id(3), Some(CLIENT_A), Some(ProcAnchor::Shell(ROOT_A))),
            (id(4), Some(CLIENT_B), Some(ProcAnchor::Shell(ROOT_B))),
            (id(7), Some(77), Some(ProcAnchor::Terminal(77))),
        ]
    }

    #[test]
    fn an_agent_under_a_bound_root_resolves_to_its_own_window_not_the_daemon() {
        // claude (1307441) -> bash (ROOT_A) -> cterm daemon -> init.
        let found = window_for_pid(1307441, &[ROOT_A, DAEMON, 1], &windows());
        assert_eq!(found, Some(id(3)));
        assert_eq!(
            window_for_pid(1400001, &[ROOT_B, DAEMON, 1], &windows()),
            Some(id(4))
        );
        // The daemon itself (an ancestor of every root) matches nothing.
        assert_eq!(window_for_pid(DAEMON, &[1], &windows()), None);
    }

    #[test]
    fn a_direct_or_ancestor_meta_pid_matches() {
        assert_eq!(window_for_pid(77, &[1], &windows()), Some(id(7)));
        assert_eq!(window_for_pid(800, &[77, 1], &windows()), Some(id(7)));
    }

    #[test]
    fn several_windows_of_one_pid_resolve_to_the_lowest_id() {
        let mut ws = windows();
        ws.push((id(2), Some(77), None));
        ws.push((id(9), Some(77), None));
        assert_eq!(window_for_pid(77, &[1], &ws), Some(id(2)));
    }

    #[test]
    fn an_unrelated_pid_matches_nothing() {
        assert_eq!(window_for_pid(4242, &[4000, 1], &windows()), None);
        assert_eq!(window_for_pid(4242, &[], &[]), None);
    }

    #[test]
    fn the_centre_of_an_even_rect_at_the_origin() {
        assert_eq!(pane_centre(Rect::new(0, 0, 1920, 1080)), (960.0, 540.0));
    }

    #[test]
    fn the_centre_of_an_odd_rect_with_a_non_zero_origin_is_a_half_pixel() {
        assert_eq!(pane_centre(Rect::new(10, 31, 5, 7)), (12.5, 34.5));
    }

    #[test]
    fn a_pane_on_a_second_output_centres_in_global_coordinates() {
        // Right half of a 2560x1440 output placed right of a 1920-wide one.
        let rect = Rect::new(1920 + 1280, 0, 1280, 1440);
        assert_eq!(pane_centre(rect), (3840.0, 720.0));
    }

    #[test]
    fn a_free_pointer_over_a_shown_pane_warps() {
        let rect = Rect::new(0, 0, 100, 100);
        assert_eq!(warp_skip(Some(rect), false, false, false), Ok(rect));
    }

    #[test]
    fn cross_output_jump_targets_the_destination_centre_whatever_the_start() {
        // The pointer starts on output A (0..1920); the target pane is on output B. The decision
        // and destination depend only on the target rect, never on the start point.
        let target = Rect::new(1920, 0, 1920, 1080);
        let dest = warp_skip(Some(target), false, false, false).map(pane_centre);
        assert_eq!(dest, Ok((2880.0, 540.0)));
        assert!(dest.unwrap().0 >= 1920.0);
    }

    #[test]
    fn an_unmapped_pane_skips() {
        assert_eq!(warp_skip(None, false, false, false), Err("pane not shown"));
    }

    #[test]
    fn a_zero_extent_pane_skips() {
        for rect in [Rect::new(5, 5, 0, 10), Rect::new(5, 5, 10, 0)] {
            assert_eq!(
                warp_skip(Some(rect), false, false, false),
                Err("zero-extent pane")
            );
        }
    }

    #[test]
    fn a_grab_a_header_press_or_an_active_constraint_skips() {
        let rect = Some(Rect::new(0, 0, 100, 100));
        assert_eq!(warp_skip(rect, true, false, false), Err("pointer grabbed"));
        assert_eq!(
            warp_skip(rect, false, true, false),
            Err("header press outstanding")
        );
        assert_eq!(
            warp_skip(rect, false, false, true),
            Err("pointer constraint active")
        );
    }

    #[test]
    fn focus_live_refuses_a_saved_only_name_and_accepts_an_empty_live_workspace() {
        let mut w = World::new("main");
        let main = w.active_workspace_id();
        w.open_window();
        let empty = w.new_workspace("empty").unwrap();
        w.switch_workspace(main).unwrap();
        let live = [main, empty];
        let before = w.clone();
        // A name only saved on disk is not in the model: refused, nothing restored.
        assert_eq!(
            live_focus_target(&w, &live, "saved-only"),
            Err("no live workspace saved-only".to_string())
        );
        // A model workspace not on any output is not live either.
        assert_eq!(
            live_focus_target(&w, &[main], "empty"),
            Err("no live workspace empty".to_string())
        );
        assert_eq!(w, before);
        // An empty live workspace is a valid target; switching to it leaves no focused window.
        assert_eq!(live_focus_target(&w, &live, "empty"), Ok(empty));
        w.switch_workspace(empty).unwrap();
        assert_eq!(w.focused_window(), None);
    }
}
