//! Action execution: dispatches a resolved [`crucible_keys::Action`] to the compositor.
//!
//! [`CrucibleState::handle_key_action`] is the **one** execution arm per verb the review asked for
//! (`reviews/2026-07-30-tn-code-review.md` F3): compositor-level verbs (`Quit`, `RunTerminal`,
//! `Run`, `VtSwitch`) are handled directly; every model verb is routed through
//! [`apply_model_action`] and the single mutation entry point ([`CrucibleState::with_model`]).
//! [`apply_model_action`] itself only touches a bare `crucible_layout::World`, so the verb→call mapping is unit-tested
//! below without any compositor state — the same spirit as `crate::layout`'s `LayoutState<TestElement>`
//! tests, minus the payload bindings this logic never needs.

use std::sync::atomic::Ordering;

use crucible_keys::Action;
use crucible_layout::{LayoutError, World};
use tracing::{debug, info, warn};

use crate::persist::new_workspace_name;
use crate::state::{Backend, CrucibleState};

/// Amount by which a single resize keystroke adjusts the split ratio.
///
/// A compositor-edge constant, not chord policy: the leader table only names *which* edge a
/// `Resize` verb moves (see `crucible_keys::Action::Resize`'s docs).
const RESIZE_DELTA: f64 = 0.05;

impl<B: Backend + 'static> CrucibleState<B> {
    /// Dispatches a resolved [`Action`] to its compositor effect or `World` op.
    ///
    /// `Quit`/`RunTerminal`/`Run`/`OpenPicker`/`RenameWorkspace`/`VtSwitch` are compositor-level and return immediately; every
    /// other verb is a model mutation applied through [`Self::with_model`], the single entry point
    /// (`SameWorkspace` is logged at debug level as an expected no-op; anything else surfaces as a
    /// warning without panicking).
    ///
    /// Overlays are modal for chords as for clicks: a chord pressed while an overlay is mapped closes
    /// it first, then runs (see [`chord_closes_overlay`]).
    pub(super) fn handle_key_action(&mut self, action: Action) {
        if chord_closes_overlay(self.mapped_overlay().is_some(), &action) {
            self.close_mapped_overlay();
        }
        let model_action = match action {
            Action::Quit => {
                info!("Super+Shift+Q: flushing workspaces and quitting");
                self.flush_workspaces();
                self.remove_ipc_socket();
                self.running.store(false, Ordering::SeqCst);
                return;
            }
            // Spawning causes the client to connect, map a toplevel, and flow through
            // `new_toplevel` -> `map_new_window`, which pushes the window onto the focused pane's
            // stack. We must not call `World::open_window`/`stack_window` directly from a keybind:
            // doing so allocates a `WindowId` with no backing surface.
            Action::RunTerminal => {
                let cmd = self.terminal_program();
                info!(cmd, "spawning terminal into focused pane");
                self.spawn_terminal_here(&cmd);
                return;
            }
            Action::Run(cmd) => {
                info!(cmd, "running command into focused pane");
                if let Err(reason) = self.run_checked(&cmd) {
                    warn!(cmd, reason, "run keybind not spawned");
                }
                return;
            }
            Action::RunInTerminal(cmd) => {
                info!(cmd, "running command in a new terminal");
                if let Err(reason) = self.run_in_terminal_checked(&cmd) {
                    warn!(cmd, reason, "run-in-terminal keybind not spawned");
                }
                return;
            }
            Action::OpenPicker => {
                self.open_picker(false);
                return;
            }
            Action::CloseWorkspace => {
                self.close_workspace();
                return;
            }
            Action::Slot(n) => {
                self.slot_open(n);
                return;
            }
            Action::PinSlot(n) => {
                self.pin_slot(n);
                return;
            }
            Action::MoveWorkspaceToOutput(delta) => {
                self.move_active_workspace_to_output(delta);
                return;
            }
            Action::RenameWorkspace => {
                self.open_picker(true);
                return;
            }
            Action::OpenLauncher => {
                self.open_launcher();
                return;
            }
            Action::OpenSettings => {
                self.open_settings();
                return;
            }
            // Auto-names skip live AND saved names, so a fresh "2" never overwrites a saved "2".
            Action::NewWorkspace => {
                let saved = crucible_persist::store::list_saved(&self.persist.store_dir)
                    .into_iter()
                    .map(|(n, _)| n)
                    .collect();
                let name = new_workspace_name(&self.layout.world, &saved);
                info!(name, "Super+N: creating new workspace");
                self.update_model(|w| {
                    w.new_workspace(name);
                });
                return;
            }
            Action::VtSwitch(vt) => {
                dispatch_vt_switch(&mut self.backend_data, vt);
                return;
            }
            other => other,
        };

        match self.with_model(|world| apply_model_action(world, &model_action)) {
            // One info line per layout verb (not per keystroke) with the resulting model focus,
            // so hardware logs show where the next split/spawn/workspace verb will land.
            Ok(()) => {
                info!(action = ?model_action, focus = %self.layout.world.focus_path(), "layout action");
            }
            Err(LayoutError::SameWorkspace) => {
                debug!("move-window-to-workspace: already on target workspace");
            }
            // close-pane refusals are expected no-ops (non-empty or last pane), not failures.
            Err(err @ (LayoutError::PaneNotEmpty | LayoutError::LastPane)) => {
                debug!(%err, "close-pane refused; no-op");
            }
            Err(err) => {
                warn!(action = ?model_action, %err, "model action failed");
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

/// Applies a resolved model verb's effect to `world`.
///
/// One arm per verb (F3): parameterized directions/deltas replace what used to be one match arm
/// per direction/sign. Operates on a bare [`World`], so the verb→call mapping is unit-tested
/// without any compositor state (see `mod tests`).
///
/// # Panics
/// Panics if given a non-model [`Action`] (`Quit`, `RunTerminal`, `Run`, `OpenPicker`, `RenameWorkspace`, `VtSwitch`);
/// [`CrucibleState::handle_key_action`] always routes those before this function is reached.
fn apply_model_action(world: &mut World, action: &Action) -> Result<(), LayoutError> {
    match action {
        Action::Focus(dir) => {
            world.focus(*dir);
            Ok(())
        }
        Action::Move(dir) => {
            world.move_(*dir);
            Ok(())
        }
        Action::Split(dir) => {
            world.split(*dir);
            Ok(())
        }
        Action::Resize(dir) => {
            world.resize(*dir, RESIZE_DELTA);
            Ok(())
        }
        Action::CycleStack(delta) => {
            world.cycle_stack(*delta);
            Ok(())
        }
        Action::CycleWorkspace(delta) => {
            world.cycle_workspace(*delta);
            Ok(())
        }
        Action::MoveWindowToWorkspace(delta) => move_window_to_adjacent_workspace(world, *delta),
        Action::ClosePane => world.close_pane(),
        Action::Quit
        | Action::RunTerminal
        | Action::Run(_)
        | Action::RunInTerminal(_)
        | Action::OpenPicker
        | Action::OpenLauncher
        | Action::OpenSettings
        | Action::RenameWorkspace
        | Action::CloseWorkspace
        | Action::MoveWorkspaceToOutput(_)
        | Action::Slot(_)
        | Action::PinSlot(_)
        | Action::NewWorkspace
        | Action::VtSwitch(_) => {
            unreachable!("compositor-level action; handle_key_action routes it before with_model")
        }
    }
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
    let ws_count = output.workspaces.len();
    if ws_count < 2 {
        debug!("move-window-to-workspace: only one workspace; ignoring");
        return Ok(());
    }
    let active_idx = output.active;
    // Wrap-around index arithmetic: same formula as `cycle_index` in `crucible-layout`.
    let target_idx = ((active_idx as i64 + delta).rem_euclid(ws_count as i64)) as usize;
    let target_ws = output.workspaces[target_idx];

    world.move_window_to_workspace(window_id, target_ws)
}

/// Whether a chord closes the mapped overlay before it runs: the keyboard twin of the click rule
/// (`click_closes_overlay`), so a workspace verb never leaves an overlay stranded over another
/// workspace. Overlay-opening chords are exempt (they replace or toggle the overlay themselves),
/// as are VT switches (the overlay is still there on return).
#[must_use]
fn chord_closes_overlay(overlay_mapped: bool, action: &Action) -> bool {
    overlay_mapped
        && !matches!(
            action,
            Action::OpenPicker
                | Action::RenameWorkspace
                | Action::OpenLauncher
                | Action::OpenSettings
                | Action::VtSwitch(_)
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_layout::Direction;

    #[test]
    fn chord_closes_a_mapped_overlay_except_overlay_openers_and_vt_switch() {
        for action in [
            Action::Slot(7),
            Action::CycleWorkspace(1),
            Action::NewWorkspace,
            Action::CloseWorkspace,
            Action::Focus(Direction::Left),
            Action::RunTerminal,
        ] {
            assert!(chord_closes_overlay(true, &action), "{action}");
            assert!(!chord_closes_overlay(false, &action), "{action}");
        }
        for action in [
            Action::OpenPicker,
            Action::RenameWorkspace,
            Action::OpenLauncher,
            Action::OpenSettings,
            Action::VtSwitch(2),
        ] {
            assert!(!chord_closes_overlay(true, &action), "{action}");
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
        apply_model_action(&mut world, &action).expect("model action succeeds");
        expected(&mut want);
        assert_eq!(
            world, want,
            "{action:?} did not dispatch the expected World call"
        );
    }

    #[test]
    fn close_pane_dispatches_to_world_close_pane() {
        let mut world = World::new();
        world.split(Direction::Right); // empty, non-last focused pane
        assert_dispatches(world, Action::ClosePane, |w| {
            w.close_pane().unwrap();
        });
    }

    #[test]
    fn close_pane_refusal_leaves_world_unchanged() {
        let mut world = World::new();
        let before = world.clone();
        assert_eq!(
            apply_model_action(&mut world, &Action::ClosePane),
            Err(LayoutError::LastPane)
        );
        assert_eq!(world, before);
    }

    #[test]
    fn focus_dispatches_to_world_focus() {
        let mut world = World::new();
        world.split(Direction::Right); // creates a neighbour so focus has somewhere to go
        assert_dispatches(world, Action::Focus(Direction::Left), |w| {
            w.focus(Direction::Left);
        });
    }

    #[test]
    fn move_dispatches_to_world_move() {
        let mut world = World::new();
        world.split(Direction::Right);
        assert_dispatches(world, Action::Move(Direction::Left), |w| {
            w.move_(Direction::Left);
        });
    }

    #[test]
    fn split_dispatches_to_world_split() {
        let world = World::new();
        assert_dispatches(world, Action::Split(Direction::Down), |w| {
            w.split(Direction::Down);
        });
    }

    #[test]
    fn resize_dispatches_to_world_resize_with_the_edge_delta() {
        let mut world = World::new();
        world.split(Direction::Right);
        assert_dispatches(world, Action::Resize(Direction::Left), |w| {
            w.resize(Direction::Left, RESIZE_DELTA);
        });
    }

    #[test]
    fn cycle_stack_dispatches_to_world_cycle_stack() {
        let mut world = World::new();
        world.open_window();
        world.open_window();
        assert_dispatches(world, Action::CycleStack(-1), |w| {
            w.cycle_stack(-1);
        });
    }

    #[test]
    fn cycle_workspace_dispatches_to_world_cycle_workspace() {
        let mut world = World::new();
        world.new_workspace("ws2");
        assert_dispatches(world, Action::CycleWorkspace(-1), |w| {
            w.cycle_workspace(-1);
        });
    }

    #[test]
    fn move_window_to_workspace_dispatches_to_world_move_window_to_workspace() {
        let mut world = World::new();
        let window = world.open_window();
        world.new_workspace("ws2"); // now active; output = [ws0, ws2]
        let ws0 = world.output(world.focused_output()).unwrap().workspaces[0];
        world.switch_workspace(ws0).unwrap(); // back on ws0, `window` is focused again

        assert_dispatches(world, Action::MoveWindowToWorkspace(1), |w| {
            let target = w.output(w.focused_output()).unwrap().workspaces[1];
            w.move_window_to_workspace(window, target).unwrap();
        });
    }

    #[test]
    fn move_window_to_workspace_is_a_noop_without_a_focused_window() {
        // A freshly-closed pane's workspace has no window at all; the model action must not
        // panic or error, just leave the world untouched (mirrors the compositor's silent no-op).
        let mut world = World::new();
        world.new_workspace("ws2");
        let before = world.clone();
        apply_model_action(&mut world, &Action::MoveWindowToWorkspace(1)).unwrap();
        assert_eq!(world, before);
    }

    #[test]
    fn move_window_to_workspace_is_a_noop_with_only_one_workspace() {
        let mut world = World::new();
        world.open_window();
        let before = world.clone();
        apply_model_action(&mut world, &Action::MoveWindowToWorkspace(1)).unwrap();
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
