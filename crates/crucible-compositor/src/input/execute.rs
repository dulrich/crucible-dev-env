//! Action execution: the one gate every [`crucible_keys::Action`] passes through.
//!
//! [`CrucibleState::handle_key_action`] is the single, fallible action gate shared by the
//! keyboard and IPC `exec`: lock ([`admit`]) → overlay ([`closes_overlay`]) → execute. It is also
//! the **one** execution arm per verb (`reviews/2026-07-30-tn-code-review.md` F3):
//! compositor-level verbs are handled directly; every model verb ([`LayoutVerb`]) is routed
//! through [`apply_model_action`] and the single mutation entry point
//! ([`CrucibleState::with_model`]). [`apply_model_action`] and the gate's decisions only touch
//! plain values, so they are unit-tested below without any compositor state.

use std::sync::atomic::Ordering;

use crucible_keys::{Action, LayoutVerb};
use crucible_layout::{LayoutError, World};
use tracing::{debug, info, warn};

use super::modal::{InputMode, chord_closes_overlay};
use crate::persist::{PickerMode, new_workspace_name};
use crate::state::{Backend, CrucibleState};

/// Amount by which a single resize keystroke adjusts the split ratio.
///
/// A compositor-edge constant, not chord policy: the leader table only names *which* edge a
/// `Resize` verb moves (see `crucible_keys::Action::Resize`'s docs).
const RESIZE_DELTA: f64 = 0.05;

/// Where an action came from; selects the overlay policy ([`closes_overlay`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    /// A keyboard chord.
    Chord,
    /// An IPC `exec` request (the `crucible keys` overlay, the launcher, `crucible msg`).
    Ipc,
}

/// Why an action did not run (or failed). `Display` is the reason sent to IPC clients.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ActionError {
    /// The session is locked and the action is not allowed while locked.
    #[error("session locked")]
    Locked,
    /// A spawn was refused (e.g. `unknown command foo`).
    #[error("{0}")]
    Spawn(String),
    /// The model refused the verb (anything but the expected `SameWorkspace` no-op).
    #[error("{0}")]
    Model(LayoutError),
}

/// The lock admission for an action: [`Action::allowed_while_locked`] is the one lock predicate,
/// shared with the keyboard filter (which forwards every non-admitted key to the locker).
///
/// # Errors
/// [`ActionError::Locked`] while `locked` for every action but a VT switch.
pub(crate) fn admit(locked: bool, action: &Action) -> Result<(), ActionError> {
    if locked && !action.allowed_while_locked() {
        Err(ActionError::Locked)
    } else {
        Ok(())
    }
}

/// The overlay policy: whether `action` from `origin` closes the mapped overlay before running.
///
/// Chords follow [`chord_closes_overlay`]. IPC `exec` closes first unconditionally, except `run`
/// and `run-in-terminal`: the launcher overlay sends those and shows an `err` reply in its own
/// status line, and closing the overlay would kill it before the reply arrives.
#[must_use]
pub(crate) fn closes_overlay(origin: Origin, overlay_mapped: bool, action: &Action) -> bool {
    match origin {
        Origin::Chord => chord_closes_overlay(&overlay_mode(overlay_mapped), action),
        Origin::Ipc => {
            overlay_mapped && !matches!(action, Action::Run(_) | Action::RunInTerminal(_))
        }
    }
}

/// The mode a bare `overlay_mapped` flag stands for (the lock is already handled by [`admit`]).
fn overlay_mode(overlay_mapped: bool) -> InputMode<()> {
    if overlay_mapped {
        InputMode::Overlay(())
    } else {
        InputMode::Normal
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// The single action gate: lock ([`admit`]) → overlay ([`closes_overlay`]) → execute.
    ///
    /// Compositor-level verbs run directly; every [`LayoutVerb`] is a model mutation applied
    /// through [`Self::with_model`], the single entry point (`SameWorkspace` is logged at debug
    /// level as an expected no-op and is `Ok`; close-pane refusals are logged at debug level,
    /// other model failures and spawn refusals as warnings, and all of them return `Err`).
    ///
    /// # Errors
    /// [`ActionError`] when the gate refuses the action or the action fails.
    pub(crate) fn handle_key_action(
        &mut self,
        action: Action,
        origin: Origin,
    ) -> Result<(), ActionError> {
        let mode = self.input_mode();
        admit(mode.is_locked(), &action)?;
        if closes_overlay(origin, mode.has_overlay(), &action) {
            self.close_mapped_overlay();
        }
        if let Some(verb) = action.layout_verb() {
            return self.apply_layout_verb(verb);
        }
        match action {
            Action::Quit => {
                info!("Super+Shift+Q: flushing workspaces and quitting");
                self.flush_workspaces();
                self.remove_ipc_socket();
                self.running.store(false, Ordering::SeqCst);
            }
            // Spawning causes the client to connect, map a toplevel, and flow through
            // `new_toplevel` -> `map_new_window`, which pushes the window onto the focused pane's
            // stack. We must not call `World::open_window`/`stack_window` directly from a keybind:
            // doing so allocates a `WindowId` with no backing surface.
            Action::RunTerminal => {
                let cmd = self.terminal_program();
                info!(cmd, "spawning terminal into focused pane");
                self.spawn_terminal_here(&cmd);
            }
            Action::Run(cmd) => {
                info!(cmd, "running command into focused pane");
                if let Err(reason) = self.run_checked(&cmd) {
                    warn!(cmd, reason, "run keybind not spawned");
                    return Err(ActionError::Spawn(reason));
                }
            }
            Action::RunInTerminal(cmd) => {
                info!(cmd, "running command in a new terminal");
                if let Err(reason) = self.run_in_terminal_checked(&cmd) {
                    warn!(cmd, reason, "run-in-terminal keybind not spawned");
                    return Err(ActionError::Spawn(reason));
                }
            }
            Action::OpenPicker => self.open_picker(PickerMode::Pick),
            Action::CloseWorkspace => self.close_workspace(),
            Action::CloseAllWorkspaces => self.close_all_workspaces(),
            Action::Slot(n) => self.slot_open(n),
            Action::PinSlot(n) => self.pin_slot(n),
            Action::MoveWorkspaceToOutput(delta) => self.move_active_workspace_to_output(delta),
            Action::RenameWorkspace => self.open_picker(PickerMode::Rename),
            Action::SetWorkspaceRoot => self.open_picker(PickerMode::Root),
            Action::CaptureWorkspaceRoot => {
                if let Err(reason) = self.capture_workspace_root() {
                    info!(reason, "capture-workspace-root: root unchanged");
                    return Err(ActionError::Spawn(reason));
                }
            }
            Action::OpenLauncher => self.open_launcher(),
            Action::ToggleHeader => self.toggle_header(),
            Action::OpenSettings => self.open_settings(),
            Action::Lock => self.spawn_locker(),
            // Auto-names skip live AND saved names, so a fresh "2" never overwrites a saved "2".
            Action::NewWorkspace => {
                let saved = crucible_persist::store::list_saved(&self.persist.store_dir)
                    .into_iter()
                    .map(|(n, _)| n)
                    .collect();
                let name = new_workspace_name(&self.layout.world, &saved);
                info!(name, "Super+N: creating new workspace");
                if let Err(err) = self.with_model(|w| w.new_workspace(name).map(drop)) {
                    warn!(%err, "Super+N: cannot create the workspace");
                }
            }
            Action::VtSwitch(vt) => {
                dispatch_vt_switch(&mut self.backend_data, vt);
            }
            Action::Focus(_)
            | Action::Move(_)
            | Action::Split(_)
            | Action::Resize(_)
            | Action::CycleStack(_)
            | Action::CycleWorkspace(_)
            | Action::MoveWindowToWorkspace(_)
            | Action::MarkWindow
            | Action::SwapMarkedWindow
            | Action::ClosePane => unreachable!("layout verbs are routed by layout_verb above"),
        }
        Ok(())
    }

    /// Applies a model verb through [`Self::with_model`], logging its outcome.
    fn apply_layout_verb(&mut self, verb: LayoutVerb) -> Result<(), ActionError> {
        match self.with_model(|world| apply_model_action(world, verb)) {
            // One info line per layout verb (not per keystroke) with the resulting model focus,
            // so hardware logs show where the next split/spawn/workspace verb will land.
            Ok(()) => {
                info!(action = ?verb, focus = %self.layout.world.focus_path(), "layout action");
                Ok(())
            }
            Err(LayoutError::SameWorkspace) => {
                debug!("move-window-to-workspace: already on target workspace");
                Ok(())
            }
            // close-pane refusals are expected no-ops (non-empty or last pane), not failures.
            Err(err @ (LayoutError::PaneNotEmpty | LayoutError::LastPane)) => {
                debug!(%err, "close-pane refused; no-op");
                Err(ActionError::Model(err))
            }
            Err(err) => {
                warn!(action = ?verb, %err, "model action failed");
                Err(ActionError::Model(err))
            }
        }
    }
}

/// Requests a VT switch on `backend`, logging whether it was accepted.
///
/// Only the udev/DRM backend owns a `libseat` session and can switch VTs; the nested `winit`
/// backend's default [`Backend::change_vt`] returns `false`, which this logs as a no-op. Takes
/// `&mut B` rather than `&mut CrucibleState<B>` so it is unit-testable with a bare fake backend
/// (see `mod tests`) instead of a full compositor state.
fn dispatch_vt_switch<B: Backend>(backend: &mut B, vt: i32) -> bool {
    info!(to = vt, "VT switch requested");
    let accepted = backend.change_vt(vt);
    if !accepted {
        warn!(to = vt, "Backend does not support VT switching");
    }
    accepted
}

/// Applies a model verb's effect to `world`; one arm per [`LayoutVerb`] (F3). Operates on a bare
/// [`World`], so the verb→call mapping is unit-tested without any compositor state.
fn apply_model_action(world: &mut World, verb: LayoutVerb) -> Result<(), LayoutError> {
    match verb {
        LayoutVerb::Focus(dir) => {
            world.focus(dir);
        }
        LayoutVerb::Move(dir) => {
            world.move_(dir);
        }
        LayoutVerb::Split(dir) => {
            world.split(dir);
        }
        LayoutVerb::Resize(dir) => {
            world.resize(dir, RESIZE_DELTA);
        }
        LayoutVerb::CycleStack(delta) => {
            world.cycle_stack(delta);
        }
        LayoutVerb::CycleWorkspace(delta) => {
            world.cycle_workspace(delta);
        }
        LayoutVerb::MoveWindowToWorkspace(delta) => {
            return move_window_to_adjacent_workspace(world, delta);
        }
        LayoutVerb::MarkWindow => {
            world.mark_window();
        }
        LayoutVerb::SwapMarkedWindow => {
            world.swap_marked_window();
        }
        LayoutVerb::ClosePane => return world.close_pane(),
    }
    Ok(())
}

/// Moves the focused window `delta` workspaces from the active one on the focused output
/// (wraparound). No-op when there is no focused window or only one workspace exists on the
/// output.
fn move_window_to_adjacent_workspace(world: &mut World, delta: i64) -> Result<(), LayoutError> {
    let Some(window_id) = world.focused_window() else {
        debug!("move-window-to-workspace: no focused window; ignoring");
        return Ok(());
    };

    let output_id = world.focused_output();
    let Some(output) = world.output(output_id) else {
        return Ok(());
    };
    let ws_count = output.workspaces().len();
    if ws_count < 2 {
        debug!("move-window-to-workspace: only one workspace; ignoring");
        return Ok(());
    }
    let active_idx = output.active_index();
    // Wrap-around index arithmetic: same formula as `cycle_index` in `crucible-layout`.
    let target_idx = ((active_idx as i64 + delta).rem_euclid(ws_count as i64)) as usize;
    let target_ws = output.workspaces()[target_idx];

    world.move_window_to_workspace(window_id, target_ws)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_layout::Direction;

    #[test]
    fn locked_admits_only_vt_switch() {
        assert_eq!(admit(true, &Action::VtSwitch(2)), Ok(()));
        for action in [
            Action::Quit,
            Action::CloseAllWorkspaces,
            Action::Run("firefox".into()),
            Action::Focus(Direction::Left),
            Action::Lock,
        ] {
            assert_eq!(admit(true, &action), Err(ActionError::Locked), "{action}");
            assert_eq!(admit(false, &action), Ok(()), "{action}");
        }
        assert_eq!(ActionError::Locked.to_string(), "session locked");
    }

    #[test]
    fn spawn_errors_keep_the_spawn_reason_verbatim() {
        let reason = "unknown command bogus".to_string();
        assert_eq!(ActionError::Spawn(reason.clone()).to_string(), reason);
    }

    #[test]
    fn ipc_exec_closes_the_overlay_first_except_run_verbs() {
        for action in [
            Action::Slot(7),
            Action::OpenPicker,
            Action::ToggleHeader,
            Action::RunTerminal,
            Action::Quit,
        ] {
            assert!(closes_overlay(Origin::Ipc, true, &action), "{action}");
            assert!(!closes_overlay(Origin::Ipc, false, &action), "{action}");
        }
        for action in [
            Action::Run("firefox".into()),
            Action::RunInTerminal("htop".into()),
        ] {
            assert!(!closes_overlay(Origin::Ipc, true, &action), "{action}");
        }
    }

    #[test]
    fn chord_origin_follows_chord_closes_overlay() {
        for action in [
            Action::Slot(7),
            Action::OpenPicker,
            Action::VtSwitch(2),
            Action::Run("firefox".into()),
        ] {
            assert_eq!(
                closes_overlay(Origin::Chord, true, &action),
                chord_closes_overlay(&overlay_mode(true), &action),
                "{action}"
            );
        }
    }

    // ---- model action execution: each verb dispatches the right `World` call ----------------
    //
    // Differential style: clone the world, apply the `Action` through `apply_model_action`, apply
    // the *expected* direct `World` call to the clone, and assert the two states are identical.
    // This proves the dispatch table without re-testing `World`'s own op semantics (covered by
    // `crucible-layout`'s own suite).

    fn assert_dispatches<F: FnOnce(&mut World)>(mut world: World, action: Action, expected: F) {
        let mut want = world.clone();
        apply_model_action(&mut world, action.layout_verb().expect("a model verb"))
            .expect("model action succeeds");
        expected(&mut want);
        assert_eq!(
            world, want,
            "{action:?} did not dispatch the expected World call"
        );
    }

    #[test]
    fn close_pane_dispatches_to_world_close_pane() {
        let mut world = World::new("1");
        world.split(Direction::Right); // empty, non-last focused pane
        assert_dispatches(world, Action::ClosePane, |w| {
            w.close_pane().unwrap();
        });
    }

    #[test]
    fn close_pane_refusal_leaves_world_unchanged() {
        let mut world = World::new("1");
        let before = world.clone();
        assert_eq!(
            apply_model_action(&mut world, LayoutVerb::ClosePane),
            Err(LayoutError::LastPane)
        );
        assert_eq!(world, before);
    }

    #[test]
    fn focus_dispatches_to_world_focus() {
        let mut world = World::new("1");
        world.split(Direction::Right); // creates a neighbour so focus has somewhere to go
        assert_dispatches(world, Action::Focus(Direction::Left), |w| {
            w.focus(Direction::Left);
        });
    }

    #[test]
    fn move_dispatches_to_world_move() {
        let mut world = World::new("1");
        world.split(Direction::Right);
        assert_dispatches(world, Action::Move(Direction::Left), |w| {
            w.move_(Direction::Left);
        });
    }

    #[test]
    fn split_dispatches_to_world_split() {
        let world = World::new("1");
        assert_dispatches(world, Action::Split(Direction::Down), |w| {
            w.split(Direction::Down);
        });
    }

    #[test]
    fn resize_dispatches_to_world_resize_with_the_edge_delta() {
        let mut world = World::new("1");
        world.split(Direction::Right);
        assert_dispatches(world, Action::Resize(Direction::Left), |w| {
            w.resize(Direction::Left, RESIZE_DELTA);
        });
    }

    #[test]
    fn cycle_stack_dispatches_to_world_cycle_stack() {
        let mut world = World::new("1");
        world.open_window();
        world.open_window();
        assert_dispatches(world, Action::CycleStack(-1), |w| {
            w.cycle_stack(-1);
        });
    }

    #[test]
    fn cycle_workspace_dispatches_to_world_cycle_workspace() {
        let mut world = World::new("1");
        world.new_workspace("ws2").unwrap();
        assert_dispatches(world, Action::CycleWorkspace(-1), |w| {
            w.cycle_workspace(-1);
        });
    }

    #[test]
    fn move_window_to_workspace_dispatches_to_world_move_window_to_workspace() {
        let mut world = World::new("1");
        let window = world.open_window();
        world.new_workspace("ws2").unwrap(); // now active; output = [ws0, ws2]
        let ws0 = world.output(world.focused_output()).unwrap().workspaces()[0];
        world.switch_workspace(ws0).unwrap(); // back on ws0, `window` is focused again

        assert_dispatches(world, Action::MoveWindowToWorkspace(1), |w| {
            let target = w.output(w.focused_output()).unwrap().workspaces()[1];
            w.move_window_to_workspace(window, target).unwrap();
        });
    }

    #[test]
    fn mark_window_marks_then_places_in_the_focused_pane() {
        let mut world = World::new("1");
        let a = world.open_window();
        world.split(Direction::Right);
        let b = world.open_window();
        world.focus(Direction::Left);
        let source = world.focused_pane_id();
        apply_model_action(&mut world, LayoutVerb::MarkWindow).unwrap();
        assert_eq!(world.marked(), Some(a));
        world.focus(Direction::Right);
        let target = world.focused_pane_id();
        apply_model_action(&mut world, LayoutVerb::MarkWindow).unwrap();
        world.check_invariants();
        assert_eq!(world.marked(), None);
        assert_eq!(world.pane_of_window(a), Some(target));
        assert_eq!(world.pane_of_window(b), Some(target));
        assert_ne!(source, target);
        assert_eq!(world.focused_window(), Some(a));
    }

    #[test]
    fn mark_window_dispatches_to_world_mark_window() {
        let mut world = World::new("1");
        world.open_window();
        assert_dispatches(world, Action::MarkWindow, |w| w.mark_window());
    }

    #[test]
    fn swap_marked_window_dispatches_to_world_swap_marked_window() {
        let mut world = World::new("1");
        world.open_window();
        assert_dispatches(world, Action::SwapMarkedWindow, |w| w.swap_marked_window());
    }

    #[test]
    fn move_window_to_workspace_is_a_noop_without_a_focused_window() {
        // A freshly-closed pane's workspace has no window at all; the model action must not
        // panic or error, just leave the world untouched (mirrors the compositor's silent no-op).
        let mut world = World::new("1");
        world.new_workspace("ws2").unwrap();
        let before = world.clone();
        apply_model_action(&mut world, LayoutVerb::MoveWindowToWorkspace(1)).unwrap();
        assert_eq!(world, before);
    }

    #[test]
    fn move_window_to_workspace_is_a_noop_with_only_one_workspace() {
        let mut world = World::new("1");
        world.open_window();
        let before = world.clone();
        apply_model_action(&mut world, LayoutVerb::MoveWindowToWorkspace(1)).unwrap();
        assert_eq!(world, before);
    }

    // ---- backend-local VT routing ------------------------------------------------------------

    /// A minimal [`Backend`] test double: constructible without a live compositor state, so VT
    /// routing is testable without `Display`/`LoopHandle`/`Seat` plumbing.
    struct TestBackend {
        accepts_vt: bool,
        last_vt: Option<i32>,
    }

    impl Backend for TestBackend {
        fn seat_name(&self) -> String {
            "test".to_string()
        }

        fn reset_buffers(&mut self, _output: &smithay::output::Output) {}

        fn early_import(
            &mut self,
            _surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        ) {
        }

        fn update_led_state(&mut self, _led_state: smithay::input::keyboard::LedState) {}

        fn change_vt(&mut self, vt: i32) -> bool {
            self.last_vt = Some(vt);
            self.accepts_vt
        }
    }

    #[test]
    fn vt_switch_routes_to_the_backend_and_reports_acceptance() {
        let mut backend = TestBackend {
            accepts_vt: true,
            last_vt: None,
        };
        assert!(dispatch_vt_switch(&mut backend, 3));
        assert_eq!(backend.last_vt, Some(3));
    }

    #[test]
    fn vt_switch_reports_rejection_when_the_backend_has_no_session() {
        // Mirrors the nested `winit` backend's default `change_vt`, which always returns `false`.
        let mut backend = TestBackend {
            accepts_vt: false,
            last_vt: None,
        };
        assert!(!dispatch_vt_switch(&mut backend, 2));
        assert_eq!(backend.last_vt, Some(2));
    }
}
