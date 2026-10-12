//! End-to-end tests of the `World` public API: every verb, all directions, and the invariants.
//!
//! These exercise only the public surface (`crucible_layout::*`) — the compositor's view of the
//! model — and assert the model invariants after every interesting mutation via the test-only
//! `check_invariants` helper.
//!
//! Mutating ops return no change hints: what the compositor must show is the derived
//! [`crucible_layout::Projection`] (`World::project`/`project_stored`), which these tests assert on
//! directly.

use crucible_layout::{
    Direction::*, FocusChange, GapConfig, LayoutError, OutputId, PaneId, Rect, WindowId,
    WorkspaceId, World,
};

/// Zero gaps keep expected rects exact; a single 800x600 output keeps them readable.
const GAPS: GapConfig = GapConfig::new(0, 0);
const SCREEN: Rect = Rect::new(0, 0, 800, 600);
const R1: Rect = Rect::new(0, 0, 1920, 1080);
const R2: Rect = Rect::new(1920, 0, 1280, 1024);

/// A `World` with one output sized [`SCREEN`], so `project_stored` yields real rects.
fn world() -> World {
    let mut w = World::new();
    let output = w.focused_output();
    w.set_output_rect(output, SCREEN).unwrap();
    w
}

#[test]
fn new_world_has_one_output_one_workspace_one_empty_pane() {
    let w = World::new();
    w.check_invariants();
    assert_eq!(w.window_count(), 0);
    assert_eq!(w.focused_window(), None);
    let ws = w.workspace(w.active_workspace_id()).unwrap();
    assert_eq!(ws.pane_count(), 1);
    assert_eq!(ws.name, "1");
    // Focused pane exists.
    assert_eq!(w.focused_pane_id(), ws.focused_pane);
}

#[test]
fn open_window_focuses_it_and_the_projection_shows_it_full_screen() {
    let mut w = world();
    let before = w.project_stored(GAPS);
    let id = w.open_window();
    w.check_invariants();
    assert_eq!(w.focused_window(), Some(id));
    assert_eq!(w.visible_windows(), vec![id]);

    let after = w.project_stored(GAPS);
    assert_eq!(after.focus(), Some(id));
    assert_eq!(after.rect_of(id), Some(SCREEN));
    // The diff is exactly "place this one window, move focus to it".
    let diff = before.diff(&after);
    assert_eq!(diff.changes.len(), 1);
    assert_eq!(diff.focus, FocusChange::To(Some(id)));
}

#[test]
fn ids_are_unique_across_opens() {
    let mut w = World::new();
    let a = w.open_window();
    let b = w.open_window();
    let c = w.open_window();
    assert_ne!(a, b);
    assert_ne!(b, c);
    assert_ne!(a, c);
    // They all stack in the same pane (no split), so only the last is visible.
    assert_eq!(w.visible_windows(), vec![c]);
    assert_eq!(w.focused_window(), Some(c));
}

#[test]
fn close_window_drops_it_from_the_projection_and_updates_focus() {
    let mut w = world();
    let a = w.open_window();
    let b = w.open_window();
    let before = w.project_stored(GAPS);
    w.close_window(b).unwrap();
    w.check_invariants();

    let after = w.project_stored(GAPS);
    assert!(!after.is_visible(b));
    assert_eq!(after.focus(), Some(a));
    // b was visible, a was hidden behind it in the stack: b hides, a is placed.
    let diff = before.diff(&after);
    assert_eq!(diff.changes.len(), 2);
    assert_eq!(diff.focus, FocusChange::To(Some(a)));
    // Focus falls back to the remaining stacked window.
    assert_eq!(w.focused_window(), Some(a));
    assert_eq!(w.window_count(), 1);
}

#[test]
fn close_unknown_window_errors() {
    let mut w = World::new();
    let a = w.open_window();
    w.close_window(a).unwrap();
    let err = w.close_window(a).unwrap_err();
    assert!(matches!(err, LayoutError::UnknownWindow(_)));
}

#[test]
fn closing_last_window_of_only_pane_keeps_the_empty_pane() {
    let mut w = World::new();
    let a = w.open_window();
    let pane_before = w.focused_pane_id();
    w.close_window(a).unwrap();
    w.check_invariants();
    // The pane survives (empty) so a focused pane always exists.
    assert_eq!(w.focused_pane_id(), pane_before);
    assert_eq!(w.focused_window(), None);
    assert_eq!(
        w.workspace(w.active_workspace_id()).unwrap().pane_count(),
        1
    );
}

#[test]
fn split_creates_new_focused_empty_pane() {
    let mut w = World::new();
    w.open_window();
    let new_pane = w.split(Right);
    w.check_invariants();
    assert_eq!(w.focused_pane_id(), new_pane);
    let ws = w.workspace(w.active_workspace_id()).unwrap();
    assert_eq!(ws.pane_count(), 2);
    // New pane is empty until a window opens into it.
    assert_eq!(w.pane(new_pane).unwrap().windows.len(), 0);
}

#[test]
fn split_then_open_lands_window_in_new_pane() {
    let mut w = World::new();
    let first = w.open_window();
    w.split(Right);
    let second = w.open_window();
    w.check_invariants();
    // Both panes show their focused window.
    let mut vis = w.visible_windows();
    vis.sort();
    let mut expected = vec![first, second];
    expected.sort();
    assert_eq!(vis, expected);
}

#[test]
fn split_all_directions_keep_invariants() {
    for dir in [Left, Right, Up, Down] {
        let mut w = World::new();
        w.open_window();
        w.split(dir);
        w.open_window();
        w.check_invariants();
        assert_eq!(
            w.workspace(w.active_workspace_id()).unwrap().pane_count(),
            2
        );
    }
}

#[test]
fn stack_window_pushes_onto_focused_pane() {
    let mut w = World::new();
    let a = w.open_window();
    let b = w.stack_window();
    w.check_invariants();
    // Single pane still — stacking does not create a pane.
    assert_eq!(
        w.workspace(w.active_workspace_id()).unwrap().pane_count(),
        1
    );
    // Only the newly-focused window is visible.
    assert_eq!(w.visible_windows(), vec![b]);
    assert_eq!(w.pane(w.focused_pane_id()).unwrap().windows, vec![a, b]);
}

#[test]
fn cycle_stack_wraps_around() {
    let mut w = World::new();
    let a = w.open_window();
    let b = w.open_window();
    let c = w.open_window();
    // Focus is on c (last opened).
    assert_eq!(w.focused_window(), Some(c));
    w.cycle_stack(1); // c -> a (wrap)
    assert_eq!(w.focused_window(), Some(a));
    w.cycle_stack(1); // a -> b
    assert_eq!(w.focused_window(), Some(b));
    w.cycle_stack(-1); // b -> a
    assert_eq!(w.focused_window(), Some(a));
    w.cycle_stack(-1); // a -> c (wrap back)
    assert_eq!(w.focused_window(), Some(c));
    w.check_invariants();
}

#[test]
fn focus_moves_between_panes_directionally() {
    let mut w = World::new();
    let left = w.open_window(); // pane 1
    w.split(Right); // new pane to the right, focused
    let right = w.open_window(); // pane 2
    assert_eq!(w.focused_window(), Some(right));
    w.focus(Left);
    w.check_invariants();
    assert_eq!(w.focused_window(), Some(left));
    w.focus(Right);
    assert_eq!(w.focused_window(), Some(right));
}

#[test]
fn focus_then_opposite_is_identity_for_sibling_pair() {
    // For a clean sibling pair the property holds exactly in both axes: focus(dir) then
    // focus(opposite) returns to the origin. (In deeper trees descent bias can pick a different
    // sibling, so the strict inverse property is asserted on the unambiguous two-pane case.)
    for (split_dir, fwd, back) in [(Right, Right, Left), (Down, Down, Up)] {
        let mut w = World::new();
        w.open_window();
        w.split(split_dir);
        w.open_window();
        // Focus the first pane.
        w.focus(back);
        let origin = w.focused_pane_id();
        w.focus(fwd);
        assert_ne!(w.focused_pane_id(), origin);
        w.focus(back);
        assert_eq!(
            w.focused_pane_id(),
            origin,
            "inverse failed for {split_dir:?}"
        );
    }
}

#[test]
fn focus_at_edge_is_noop() {
    let mut w = World::new();
    w.open_window();
    let origin = w.focused_pane_id();
    // Single pane: every direction is an edge.
    for dir in [Left, Right, Up, Down] {
        w.focus(dir);
        assert_eq!(w.focused_pane_id(), origin);
    }
}

#[test]
fn move_swaps_panes_and_focus_follows() {
    let mut w = World::new();
    let a = w.open_window(); // pane A on the left
    w.split(Right);
    let b = w.open_window(); // pane B on the right, focused
    // Move B left: it swaps with A. Focus follows the moved window.
    w.move_(Left);
    w.check_invariants();
    assert_eq!(w.focused_window(), Some(b));
    // Now focusing Right should land on A (which moved to the right slot).
    w.focus(Right);
    assert_eq!(w.focused_window(), Some(a));
}

#[test]
fn move_at_edge_is_noop() {
    let mut w = World::new();
    w.open_window();
    w.split(Right);
    w.open_window();
    w.focus(Left);
    let before = w.visible_windows();
    w.move_(Left); // already leftmost
    assert_eq!(w.visible_windows(), before);
    w.check_invariants();
}

#[test]
fn resize_adjusts_ratio_without_changing_focus_or_panes() {
    let mut w = World::new();
    let a = w.open_window();
    w.split(Right);
    let _b = w.open_window();
    let focus_before = w.focused_window();
    w.resize(Left, 0.1);
    w.check_invariants();
    assert_eq!(w.focused_window(), focus_before);
    // Pane count unchanged.
    assert_eq!(
        w.workspace(w.active_workspace_id()).unwrap().pane_count(),
        2
    );
    // a still present.
    assert!(w.window(a).is_some());
}

#[test]
fn close_window_keeps_emptied_pane() {
    let mut w = World::new();
    let _a = w.open_window(); // pane A
    w.split(Right);
    let b = w.open_window(); // pane B, focused
    // Close the only window of pane B -> pane B persists (empty).
    w.close_window(b).unwrap();
    w.check_invariants();
    assert_eq!(
        w.workspace(w.active_workspace_id()).unwrap().pane_count(),
        2
    );
}

#[test]
fn close_window_in_focused_pane_keeps_focus_on_empty_pane() {
    let mut w = World::new();
    w.open_window();
    w.split(Right);
    let pane_b = w.focused_pane_id();
    let b = w.open_window();
    w.close_window(b).unwrap();
    w.check_invariants();
    assert_eq!(w.focused_pane_id(), pane_b);
    assert_eq!(w.focused_window(), None);
}

#[test]
fn close_pane_removes_empty_pane_and_focuses_promoted_sibling() {
    let mut w = World::new();
    let a = w.open_window(); // pane A
    w.split(Right); // [A | B]
    let pane_b = w.focused_pane_id();
    w.split(Down); // [A | (B / C)], C focused and empty
    let pane_c = w.focused_pane_id();
    w.close_pane().unwrap();
    w.check_invariants();
    let ws = w.workspace(w.active_workspace_id()).unwrap();
    assert_eq!(ws.pane_count(), 2);
    assert!(!ws.panes().contains(&pane_c));
    // B took C's place, so B (not first-leaf A) is focused.
    assert_eq!(w.focused_pane_id(), pane_b);
    assert_eq!(w.visible_windows(), vec![a]);
}

#[test]
fn close_pane_refuses_non_empty_pane() {
    let mut w = World::new();
    w.open_window();
    w.split(Right);
    let pane_b = w.focused_pane_id();
    let b = w.open_window();
    let before = w.clone();
    assert_eq!(w.close_pane(), Err(LayoutError::PaneNotEmpty));
    assert_eq!(w, before);
    assert_eq!(w.focused_pane_id(), pane_b);
    assert_eq!(w.focused_window(), Some(b));
}

#[test]
fn close_pane_refuses_last_pane() {
    let mut w = World::new();
    let before = w.clone();
    assert_eq!(w.close_pane(), Err(LayoutError::LastPane));
    assert_eq!(w, before);
    w.check_invariants();
}

#[test]
fn open_window_after_close_pane_lands_in_newly_focused_pane() {
    let mut w = World::new();
    w.split(Right); // [A | B], B focused, both empty
    w.focus(Left);
    let pane_a = w.focused_pane_id();
    w.focus(Right);
    w.close_pane().unwrap(); // remove B; A promoted and focused
    assert_eq!(w.focused_pane_id(), pane_a);
    let x = w.open_window();
    w.check_invariants();
    assert_eq!(w.focused_window(), Some(x));
    assert_eq!(
        w.workspace(w.active_workspace_id()).unwrap().pane_count(),
        1
    );
}

#[test]
fn new_workspace_switches_to_a_fresh_empty_workspace() {
    let mut w = World::new();
    w.open_window();
    let original = w.active_workspace_id();
    let ws2 = w.new_workspace("code");
    w.check_invariants();
    assert_ne!(ws2, original);
    assert_eq!(w.active_workspace_id(), ws2);
    assert_eq!(w.workspace(ws2).unwrap().name, "code");
    // Fresh workspace is empty.
    assert_eq!(w.visible_windows(), Vec::new());
    assert_eq!(w.project_stored(GAPS).focus(), None);
}

#[test]
fn switch_workspace_changes_visible_set() {
    let mut w = World::new();
    let a = w.open_window();
    let original = w.active_workspace_id();
    let ws2 = w.new_workspace("two");
    let b = w.open_window();
    assert_eq!(w.visible_windows(), vec![b]);
    w.switch_workspace(original).unwrap();
    w.check_invariants();
    assert_eq!(w.visible_windows(), vec![a]);
    assert_eq!(w.focused_window(), Some(a));
    w.switch_workspace(ws2).unwrap();
    assert_eq!(w.visible_windows(), vec![b]);
}

#[test]
fn switch_to_unknown_workspace_errors() {
    let mut w = World::new();
    let other = w.new_workspace("x");
    // A raw id far beyond any allocated workspace is guaranteed absent.
    let foreign = WorkspaceId::from_raw(u64::MAX);
    let err = w.switch_workspace(foreign).unwrap_err();
    assert!(matches!(err, LayoutError::UnknownWorkspace(_)));
    // sanity: switching to a real one works.
    w.switch_workspace(other).unwrap();
}

#[test]
fn cycle_workspace_wraps() {
    let mut w = World::new();
    let first = w.active_workspace_id();
    let second = w.new_workspace("b");
    let third = w.new_workspace("c");
    // active is third; cycle forward wraps to first.
    w.cycle_workspace(1);
    assert_eq!(w.active_workspace_id(), first);
    w.cycle_workspace(-1);
    assert_eq!(w.active_workspace_id(), third);
    w.cycle_workspace(-1);
    assert_eq!(w.active_workspace_id(), second);
    w.check_invariants();
}

#[test]
fn rename_workspace_updates_name_and_errors_on_unknown() {
    let mut w = World::new();
    let id = w.active_workspace_id();
    w.rename_workspace(id, "main").unwrap();
    assert_eq!(w.workspace(id).unwrap().name, "main");

    let foreign = WorkspaceId::from_raw(u64::MAX);
    let err = w.rename_workspace(foreign, "nope").unwrap_err();
    assert!(matches!(err, LayoutError::UnknownWorkspace(_)));
}

#[test]
fn move_window_to_workspace_transfers_and_keeps_source_pane() {
    let mut w = World::new();
    let a = w.open_window(); // workspace 1, pane A
    w.split(Right);
    let b = w.open_window(); // workspace 1, pane B (focused)
    let ws1 = w.active_workspace_id();
    let ws2 = w.new_workspace("two");
    w.switch_workspace(ws1).unwrap();

    // Move b (in pane B) to ws2. Pane B empties but persists.
    w.move_window_to_workspace(b, ws2).unwrap();
    w.check_invariants();
    assert_eq!(w.workspace(ws1).unwrap().pane_count(), 2);
    // On ws1 only a is visible now.
    w.switch_workspace(ws1).unwrap();
    assert_eq!(w.visible_windows(), vec![a]);
    // On ws2, b is present and focused.
    w.switch_workspace(ws2).unwrap();
    assert_eq!(w.focused_window(), Some(b));
    assert_eq!(w.visible_windows(), vec![b]);
}

#[test]
fn move_window_to_same_workspace_errors() {
    let mut w = World::new();
    let a = w.open_window();
    let here = w.active_workspace_id();
    let err = w.move_window_to_workspace(a, here).unwrap_err();
    assert!(matches!(err, LayoutError::SameWorkspace));
}

#[test]
fn move_unknown_window_to_workspace_errors() {
    let mut w = World::new();
    let ws2 = w.new_workspace("two");
    let fw = WindowId::from_raw(u64::MAX);
    let err = w.move_window_to_workspace(fw, ws2).unwrap_err();
    assert!(matches!(err, LayoutError::UnknownWindow(_)));
}

#[test]
fn model_is_clone_and_independent() {
    let mut w = World::new();
    w.open_window();
    w.split(Right);
    w.open_window();
    let snapshot = w.clone();
    // Mutate the original; the clone is unaffected.
    w.open_window();
    w.new_workspace("scratch");
    assert_ne!(snapshot, w);
    snapshot.check_invariants();
    // The snapshot still has its two windows and two panes.
    assert_eq!(snapshot.window_count(), 2);
    assert_eq!(
        snapshot
            .workspace(snapshot.active_workspace_id())
            .unwrap()
            .pane_count(),
        2
    );
}

#[test]
fn total_pane_and_window_counts_stay_consistent_under_churn() {
    let mut w = World::new();
    let mut windows = Vec::new();
    // Build a busy layout.
    for i in 0..6 {
        let id = w.open_window();
        windows.push(id);
        if i % 2 == 0 {
            w.split(if i % 4 == 0 { Right } else { Down });
        }
    }
    w.check_invariants();
    let count_before = w.window_count();
    assert_eq!(count_before, windows.len());

    // Close half the windows; invariants and counts must stay consistent.
    for id in windows.iter().take(3) {
        w.close_window(*id).unwrap();
        w.check_invariants();
    }
    assert_eq!(w.window_count(), count_before - 3);
}

#[test]
fn close_then_reopen_keeps_invariants() {
    let mut w = World::new();
    let a = w.open_window();
    w.split(Down);
    let b = w.open_window();
    w.close_window(a).unwrap();
    w.check_invariants();
    // Reopen into the surviving pane.
    let c = w.open_window();
    w.check_invariants();
    assert!(w.window(b).is_some());
    assert!(w.window(c).is_some());
    assert!(w.window(a).is_none());
}

#[test]
fn deep_nested_focus_ascent_never_surfaces_parent() {
    // Layout V( H(1,3), 2 ): top row is [1 | 3], the bottom row is the full-width pane 2.
    // From pane 3 (top-right), focusing Down must ascend across the inner H split to the outer V
    // split to reach pane 2 — a single directional move, never a surfaced "focus parent" climb.
    let mut w = World::new();
    let one = w.open_window(); // pane 1 (top, before the H split)
    w.split(Down);
    let two = w.open_window(); // pane 2 (full-width bottom)
    w.focus(Up); // back to pane 1
    assert_eq!(w.focused_window(), Some(one));
    w.split(Right);
    let three = w.open_window(); // pane 3 (top-right)
    assert_eq!(w.focused_window(), Some(three));
    // Down from the top-right pane ascends two levels and lands on the bottom pane.
    w.focus(Down);
    w.check_invariants();
    assert_eq!(w.focused_window(), Some(two));
    // And the bottom full-width pane has no horizontal neighbour (edge); Right is a no-op.
    w.focus(Right);
    assert_eq!(w.focused_window(), Some(two));
}

// ---- multi-output (P2-WS-3.5) -------------------------------------------------------------------

#[test]
fn add_then_remove_output_preserves_all_windows_end_to_end() {
    let mut w = World::new();
    let first = w.focused_output();
    w.set_output_rect(first, Rect::new(0, 0, 1920, 1080))
        .unwrap();

    // A window on the first output.
    let win_a = w.open_window();

    // Add a second output, focus it, and open a window there (on its own workspace stack).
    let second = w.add_output(Rect::new(1920, 0, 1280, 1024));
    w.set_focused_output(second).unwrap();
    let win_b = w.open_window();
    w.check_invariants();
    assert_eq!(w.window_count(), 2);

    // Remove the second output: its workspace (with win_b) migrates onto the survivor; nothing dies.
    w.set_focused_output(first).unwrap();
    w.remove_output(second).unwrap();
    w.check_invariants();
    assert_eq!(w.window_count(), 2);
    assert!(w.window(win_a).is_some());
    assert!(w.window(win_b).is_some());
    assert_eq!(w.output_count(), 1);
}

#[test]
fn last_output_cannot_be_removed() {
    let mut w = World::new();
    let only = w.focused_output();
    assert_eq!(w.remove_output(only), Err(LayoutError::LastOutput));
    w.check_invariants();
}

// ---- projection (WP-R1): the single derived source of visible geometry + focus -------------------

#[test]
fn projection_is_idempotent_for_an_unchanged_model() {
    let mut w = world();
    w.open_window();
    w.split(Right);
    w.open_window();

    let first = w.project_stored(GAPS);
    let second = w.project_stored(GAPS);
    assert_eq!(first, second);
    assert!(first.diff(&second).is_empty());
}

#[test]
fn projection_hides_stacked_windows_and_cycling_swaps_them_in_place() {
    let mut w = world();
    let a = w.open_window();
    let b = w.stack_window(); // same pane, b now focused

    let before = w.project_stored(GAPS);
    assert_eq!(before.windows().len(), 1);
    assert!(!before.is_visible(a), "stacked-under window is not visible");
    assert_eq!(before.rect_of(b), Some(SCREEN));

    w.cycle_stack(1); // wraps back to a
    let after = w.project_stored(GAPS);
    assert_eq!(after.rect_of(a), Some(SCREEN));
    assert!(!after.is_visible(b));
    // Exactly: hide b, place a in the same rect, focus follows.
    let diff = before.diff(&after);
    assert_eq!(diff.changes.len(), 2);
    assert_eq!(diff.focus, FocusChange::To(Some(a)));
}

#[test]
fn projection_follows_workspace_switching() {
    let mut w = world();
    let a = w.open_window();
    let ws1 = w.active_workspace_id();
    let ws2 = w.new_workspace("two");
    let b = w.open_window();

    let on_two = w.project_stored(GAPS);
    assert!(on_two.is_visible(b) && !on_two.is_visible(a));

    w.switch_workspace(ws1).unwrap();
    let on_one = w.project_stored(GAPS);
    assert!(on_one.is_visible(a) && !on_one.is_visible(b));
    assert_eq!(on_one.focus(), Some(a));

    // Switching back reproduces the earlier projection exactly (no drift).
    w.switch_workspace(ws2).unwrap();
    assert_eq!(w.project_stored(GAPS), on_two);
}

#[test]
fn focus_only_change_leaves_geometry_untouched() {
    let mut w = world();
    let left = w.open_window();
    w.split(Right);
    let right = w.open_window();

    let before = w.project_stored(GAPS);
    w.focus(Left);
    let after = w.project_stored(GAPS);

    assert_eq!(before.windows(), after.windows(), "geometry unchanged");
    let diff = before.diff(&after);
    assert!(diff.changes.is_empty());
    assert_eq!(diff.focus, FocusChange::To(Some(left)));
    assert_eq!(before.focus(), Some(right));
}

#[test]
fn projection_places_each_output_at_its_own_offset() {
    let mut w = World::new();
    let first = w.focused_output();
    w.set_output_rect(first, Rect::new(0, 0, 800, 600)).unwrap();
    let a = w.open_window();

    let second = w.add_output(Rect::new(800, 0, 1024, 768));
    w.set_focused_output(second).unwrap();
    let b = w.open_window();

    let projection = w.project_stored(GAPS);
    assert_eq!(projection.rect_of(a), Some(Rect::new(0, 0, 800, 600)));
    assert_eq!(projection.rect_of(b), Some(Rect::new(800, 0, 1024, 768)));
    assert_eq!(projection.focus(), Some(b));
}

#[test]
fn projection_ignores_output_bindings_the_model_does_not_know() {
    let mut w = world();
    let real = w.focused_output();
    let id = w.open_window();

    // A stale binding (an output removed from the model, or never registered) contributes nothing
    // and does not disturb the surviving outputs.
    let stale = w.add_output(Rect::new(800, 0, 640, 480));
    let bindings = [(real, SCREEN), (stale, Rect::new(800, 0, 640, 480))];
    w.remove_output(stale).unwrap();

    let projection = w.project(&bindings, GAPS);
    assert_eq!(projection.windows().len(), 1);
    assert_eq!(projection.rect_of(id), Some(SCREEN));
    assert_eq!(projection.focus(), Some(id));
}

// ---- multi-output behavior: independent stacks, isolation, migration on removal ----

#[test]
fn new_world_has_one_output_with_placeholder_rect() {
    let w = World::new();
    assert_eq!(w.output_count(), 1);
    let id = w.focused_output();
    assert_eq!(w.output(id).unwrap().rect, Rect::new(0, 0, 0, 0));
    w.check_invariants();
}

#[test]
fn add_output_creates_an_independent_stack() {
    let mut w = World::new();
    let first = w.focused_output();
    let before = w.project_stored(GAPS);
    let second = w.add_output(R2);
    // New output starts empty and does not become focused; no visible change.
    assert!(before.diff(&w.project_stored(GAPS)).is_empty());
    assert_ne!(first, second);
    assert_eq!(w.focused_output(), first);
    assert_eq!(w.output_count(), 2);

    // The second output owns its own single workspace, distinct from the first's.
    let ws_first = w.output(first).unwrap().active_workspace();
    let ws_second = w.output(second).unwrap().active_workspace();
    assert_ne!(ws_first, ws_second);
    assert_eq!(w.output(second).unwrap().rect, R2);
    w.check_invariants();
}

#[test]
fn ops_on_focused_output_do_not_disturb_other_outputs() {
    let mut w = World::new();
    let first = w.focused_output();
    let second = w.add_output(R2);

    let second_before = w.output(second).unwrap().clone();
    // All of these act on `first` (the focused output).
    w.open_window();
    w.split(Right);
    w.new_workspace("scratch");

    // The second output is byte-for-byte unchanged.
    assert_eq!(w.output(second).unwrap(), &second_before);
    // ...but the first output gained a workspace.
    assert_eq!(w.output(first).unwrap().workspaces.len(), 2);
    w.check_invariants();
}

#[test]
fn switching_focused_output_then_ops_affect_only_that_output() {
    let mut w = World::new();
    let first = w.focused_output();
    let second = w.add_output(R2);

    let first_before = w.output(first).unwrap().clone();
    w.set_focused_output(second).unwrap();
    assert_eq!(w.focused_output(), second);

    let win = w.open_window();
    w.new_workspace("2");

    // First output untouched.
    assert_eq!(w.output(first).unwrap(), &first_before);
    // Second output now has two workspaces, and the window landed on it.
    assert_eq!(w.output(second).unwrap().workspaces.len(), 2);
    assert!(w.window(win).is_some());
    w.check_invariants();
}

#[test]
fn per_output_geometry_is_independent() {
    let mut w = World::new();
    let first = w.focused_output();
    w.set_output_rect(first, R1).unwrap();
    let second = w.add_output(R2);

    // A window on the first output.
    let win1 = w.open_window();
    // Switch focus and add a window on the second output.
    w.set_focused_output(second).unwrap();
    let win2 = w.open_window();

    let g1 = w.geometry(first, R1, GAPS);
    assert_eq!(g1.len(), 1);
    assert_eq!(g1[0], (win1, R1));

    let g2 = w.geometry(second, R2, GAPS);
    assert_eq!(g2.len(), 1);
    // geometry uses an output-relative rect; we pass R2's own size at origin in real use, but
    // here we pass R2 directly and expect the window to fill it.
    assert_eq!(g2[0].0, win2);

    // geometry_for uses the stored rect (R2, offset at x=1920).
    let g2_stored = w.geometry_for(second, GAPS);
    assert_eq!(g2_stored.len(), 1);
    assert_eq!(g2_stored[0], (win2, R2));
    w.check_invariants();
}

#[test]
fn remove_output_migrates_workspaces_and_windows_without_loss() {
    let mut w = World::new();
    let first = w.focused_output();
    let second = w.add_output(R2);

    // Put a window on the second output (on its own workspace).
    w.set_focused_output(second).unwrap();
    let win = w.open_window();
    w.new_workspace("extra"); // second now has 2 workspaces
    let win2 = w.open_window();
    assert_eq!(w.window_count(), 2);

    // Refocus first, then remove the second output.
    w.set_focused_output(first).unwrap();
    let first_ws_before = w.output(first).unwrap().workspaces.len();
    w.remove_output(second).unwrap();

    // No windows destroyed: both entries still exist in the arena.
    assert_eq!(w.window_count(), 2);
    assert!(w.window(win).is_some());
    assert!(w.window(win2).is_some());
    // The migrated workspaces are inactive on the survivor, so their windows are hidden, not
    // closed: neither appears in the projection.
    let projection = w.project_stored(GAPS);
    assert!(!projection.is_visible(win));
    assert!(!projection.is_visible(win2));

    // Second output's 2 workspaces appended onto the first.
    assert_eq!(
        w.output(first).unwrap().workspaces.len(),
        first_ws_before + 2
    );
    assert_eq!(w.output_count(), 1);
    w.check_invariants();
}

#[test]
fn remove_focused_output_repoints_focus_to_survivor() {
    let mut w = World::new();
    let first = w.focused_output();
    let second = w.add_output(R2);

    w.set_focused_output(second).unwrap();
    assert_eq!(w.focused_output(), second);

    w.remove_output(second).unwrap();
    assert_eq!(w.focused_output(), first);
    assert_eq!(w.project_stored(GAPS).focus(), w.focused_window());
    w.check_invariants();
}

#[test]
fn remove_last_output_errors() {
    let mut w = World::new();
    let only = w.focused_output();
    assert_eq!(w.remove_output(only), Err(LayoutError::LastOutput));
    w.check_invariants();
}

#[test]
fn remove_unknown_output_errors() {
    let mut w = World::new();
    let bogus = OutputId::from_raw(999);
    assert_eq!(
        w.remove_output(bogus),
        Err(LayoutError::UnknownOutput(bogus))
    );
}

#[test]
fn set_output_rect_and_set_focused_errors_on_unknown() {
    let mut w = World::new();
    let bogus = OutputId::from_raw(999);
    assert_eq!(
        w.set_output_rect(bogus, R1),
        Err(LayoutError::UnknownOutput(bogus))
    );
    assert_eq!(
        w.set_focused_output(bogus),
        Err(LayoutError::UnknownOutput(bogus))
    );
}

#[test]
fn remove_survivor_is_lowest_id_when_focused_is_removed() {
    // Three outputs; focus the highest, remove it -> survivor should be the focused-if-survives
    // rule; but when focused is removed, survivor is the lowest remaining id.
    let mut w = World::new();
    let first = w.focused_output();
    let second = w.add_output(R2);
    let third = w.add_output(R2);

    w.set_focused_output(third).unwrap();
    w.remove_output(third).unwrap();
    // Lowest remaining id is `first`.
    assert_eq!(w.focused_output(), first);
    assert!(w.output(first).is_some());
    assert!(w.output(second).is_some());
    w.check_invariants();
}

#[test]
fn focus_window_selects_its_pane_and_stack_position() {
    let mut w = world();
    let a = w.open_window(); // pane A
    w.split(Right);
    let b = w.open_window(); // pane B, stacked under c
    let c = w.stack_window();
    let pane_b = w.focused_pane_id();
    assert_eq!(w.focused_window(), Some(c));

    // Focus the buried stacked window b: same pane, stack index moves to b.
    w.focus_window(b).unwrap();
    w.check_invariants();
    assert_eq!(w.focused_pane_id(), pane_b);
    assert_eq!(w.focused_window(), Some(b));
    assert!(w.visible_windows().contains(&b));
    assert!(!w.visible_windows().contains(&c));

    // Focus a in the other pane: focused pane moves.
    w.focus_window(a).unwrap();
    w.check_invariants();
    assert_ne!(w.focused_pane_id(), pane_b);
    assert_eq!(w.focused_window(), Some(a));
    // A following open lands in a's pane.
    let pane_a = w.focused_pane_id();
    let d = w.open_window();
    assert_eq!(w.focused_pane_id(), pane_a);
    assert_ne!(w.focused_pane_id(), pane_b);
    assert_eq!(w.focused_window(), Some(d));
}

#[test]
fn focus_window_on_another_output_moves_output_focus() {
    let mut w = World::new();
    let first = w.focused_output();
    w.set_output_rect(first, R1).unwrap();
    let a = w.open_window();
    let second = w.add_output(R2);
    w.set_focused_output(second).unwrap();
    let b = w.open_window();
    let ws_b = w.active_workspace_id();
    assert_eq!(w.focused_window(), Some(b));

    w.focus_window(a).unwrap();
    w.check_invariants();
    assert_eq!(w.focused_output(), first);
    assert_eq!(w.focused_window(), Some(a));

    w.focus_window(b).unwrap();
    w.check_invariants();
    assert_eq!(w.focused_output(), second);
    assert_eq!(w.active_workspace_id(), ws_b);
    assert_eq!(w.focused_window(), Some(b));
}

#[test]
fn focus_window_activates_a_hidden_workspace() {
    let mut w = World::new();
    let a = w.open_window();
    let ws1 = w.active_workspace_id();
    w.new_workspace("two");
    let b = w.open_window();
    assert_eq!(w.visible_windows(), vec![b]);

    w.focus_window(a).unwrap();
    w.check_invariants();
    assert_eq!(w.active_workspace_id(), ws1);
    assert_eq!(w.visible_windows(), vec![a]);
}

#[test]
fn focus_unknown_window_errors_without_mutation() {
    let mut w = world();
    w.open_window();
    let before = w.clone();
    let bogus = WindowId::from_raw(999);
    assert_eq!(
        w.focus_window(bogus),
        Err(LayoutError::UnknownWindow(bogus))
    );
    assert_eq!(w, before);
}

#[test]
fn focus_path_tracks_output_workspace_pane_and_window() {
    let mut w = World::new();
    let empty = w.focus_path();
    assert_eq!(empty.output, w.focused_output());
    assert_eq!(empty.workspace, w.active_workspace_id());
    assert_eq!(empty.workspace_name, "1");
    assert_eq!(empty.pane, w.focused_pane_id());
    assert_eq!(empty.window, None);

    let id = w.open_window();
    w.split(Right);
    let split = w.focus_path();
    assert_ne!(split.pane, empty.pane);
    assert_eq!(split.window, None);

    w.focus(Left);
    assert_eq!(w.focus_path().window, Some(id));

    w.new_workspace("code");
    let ws2 = w.focus_path();
    assert_eq!(ws2.workspace_name, "code");
    assert_eq!(ws2.window, None);
}

#[test]
fn focus_path_display_is_one_compact_line() {
    let mut w = World::new();
    let empty = w.focus_path();
    let expected_empty = format!(
        "output={} workspace={}:\"1\" pane={} window=-",
        empty.output.raw(),
        empty.workspace.raw(),
        empty.pane.raw()
    );
    assert_eq!(empty.to_string(), expected_empty);

    let id = w.open_window();
    assert!(
        w.focus_path()
            .to_string()
            .ends_with(&format!("window={}", id.raw()))
    );
}

// ---- empty-pane focus: pane hit-test + focus_pane -----------------------------------------------

/// Real gaps so the hit-test's gap handling is exercised (outer 8, inner 6).
const HIT_GAPS: GapConfig = GapConfig::new(8, 6);

#[test]
fn pane_at_resolves_each_half_of_a_right_split() {
    let mut w = world();
    let output = w.focused_output();
    let left_win = w.open_window();
    let left = w.pane_of_window(left_win).unwrap();
    let right = w.split(Right);
    let bindings = [(output, SCREEN)];
    assert_eq!(w.pane_at(&bindings, HIT_GAPS, 100, 300), Some(left));
    assert_eq!(w.pane_at(&bindings, HIT_GAPS, 700, 300), Some(right));
    // The hit-test agrees with the rects it is built from.
    let r = w.pane_rect(&bindings, HIT_GAPS, right).unwrap();
    assert_eq!(w.pane_at(&bindings, HIT_GAPS, r.x, r.y), Some(right));
    assert_eq!(w.pane_at(&bindings, HIT_GAPS, r.x - 1, r.y), None);
}

#[test]
fn pane_at_returns_none_in_gaps_and_off_output() {
    let mut w = world();
    let output = w.focused_output();
    w.open_window();
    w.split(Right);
    let bindings = [(output, SCREEN)];
    // Outer gap.
    assert_eq!(w.pane_at(&bindings, HIT_GAPS, 2, 2), None);
    // Inner gap between the halves: 8..792 split around 400 with a 6px gutter.
    assert_eq!(w.pane_at(&bindings, HIT_GAPS, 399, 300), None);
    // Off every bound output.
    assert_eq!(w.pane_at(&bindings, HIT_GAPS, 900, 300), None);
}

#[test]
fn pane_at_resolves_a_point_on_a_second_output() {
    let mut w = World::new();
    let first = w.focused_output();
    w.set_output_rect(first, R1).unwrap();
    let second = w.add_output(R2);
    let second_pane = w
        .workspace(w.output(second).unwrap().active_workspace())
        .unwrap()
        .focused_pane;
    let bindings = [(first, R1), (second, R2)];
    assert_eq!(w.pane_at(&bindings, HIT_GAPS, 2500, 500), Some(second_pane));
    let r = w.pane_rect(&bindings, HIT_GAPS, second_pane).unwrap();
    assert_eq!(r, Rect::new(1928, 8, 1264, 1008));
}

#[test]
fn focus_pane_on_an_empty_pane_then_open_lands_there() {
    let mut w = world();
    let left_win = w.open_window();
    let left = w.pane_of_window(left_win).unwrap();
    let right = w.split(Right);
    w.focus(Left);
    assert_eq!(w.focused_pane_id(), left);
    assert_eq!(w.focused_empty_pane(), None);

    w.focus_pane(right).unwrap();
    w.check_invariants();
    assert_eq!(w.focused_pane_id(), right);
    assert_eq!(w.focused_window(), None);
    assert_eq!(w.focused_empty_pane(), Some(right));

    let new_win = w.open_window();
    assert_eq!(w.pane_of_window(new_win), Some(right));
    // The left pane's stack is untouched.
    assert_eq!(w.pane(left).unwrap().focused_window(), Some(left_win));
}

#[test]
fn focus_pane_moves_focused_output_and_errors_on_unknown() {
    let mut w = World::new();
    let first = w.focused_output();
    let second = w.add_output(R2);
    let second_pane = w
        .workspace(w.output(second).unwrap().active_workspace())
        .unwrap()
        .focused_pane;
    w.focus_pane(second_pane).unwrap();
    w.check_invariants();
    assert_eq!(w.focused_output(), second);
    assert_ne!(first, second);

    let before = w.clone();
    let bogus = PaneId::from_raw(9999);
    assert_eq!(w.focus_pane(bogus), Err(LayoutError::UnknownPane(bogus)));
    assert_eq!(w, before);
}
