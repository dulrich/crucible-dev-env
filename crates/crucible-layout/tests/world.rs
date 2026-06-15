//! End-to-end tests of the `World` public API: every verb, all directions, and the invariants.
//!
//! These exercise only the public surface (`crucible_layout::*`) — the compositor's view of the
//! model — and assert the model invariants after every interesting mutation via the test-only
//! `check_invariants` helper.
//!
//! `Reconcile` is `#[must_use]` so production callers (WS-3) cannot silently drop it; these tests
//! deliberately ignore reconciles they do not assert on, so the lint is relaxed file-wide here.
#![allow(unused_must_use)]

use crucible_layout::{Direction::*, LayoutError, WindowId, WorkspaceId, World};

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
fn open_window_focuses_it_and_reconcile_reports_focus() {
    let mut w = World::new();
    let (id, rec) = w.open_window();
    w.check_invariants();
    assert_eq!(w.focused_window(), Some(id));
    assert_eq!(rec.focused_window, Some(id));
    assert!(rec.visibility_changed);
    assert!(rec.removed.is_empty());
    assert_eq!(w.visible_windows(), vec![id]);
}

#[test]
fn ids_are_unique_across_opens() {
    let mut w = World::new();
    let (a, _) = w.open_window();
    let (b, _) = w.open_window();
    let (c, _) = w.open_window();
    assert_ne!(a, b);
    assert_ne!(b, c);
    assert_ne!(a, c);
    // They all stack in the same pane (no split), so only the last is visible.
    assert_eq!(w.visible_windows(), vec![c]);
    assert_eq!(w.focused_window(), Some(c));
}

#[test]
fn close_window_reports_removal_and_updates_focus() {
    let mut w = World::new();
    let (a, _) = w.open_window();
    let (b, _) = w.open_window();
    let rec = w.close_window(b).unwrap();
    w.check_invariants();
    assert_eq!(rec.removed, vec![b]);
    assert!(rec.visibility_changed);
    // Focus falls back to the remaining stacked window.
    assert_eq!(w.focused_window(), Some(a));
    assert_eq!(w.window_count(), 1);
}

#[test]
fn close_unknown_window_errors() {
    let mut w = World::new();
    let (a, _) = w.open_window();
    w.close_window(a).unwrap();
    let err = w.close_window(a).unwrap_err();
    assert!(matches!(err, LayoutError::UnknownWindow(_)));
}

#[test]
fn closing_last_window_of_only_pane_keeps_the_empty_pane() {
    let mut w = World::new();
    let (a, _) = w.open_window();
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
    let (new_pane, rec) = w.split(Right);
    w.check_invariants();
    assert_eq!(w.focused_pane_id(), new_pane);
    assert!(rec.visibility_changed);
    let ws = w.workspace(w.active_workspace_id()).unwrap();
    assert_eq!(ws.pane_count(), 2);
    // New pane is empty until a window opens into it.
    assert_eq!(w.pane(new_pane).unwrap().windows.len(), 0);
}

#[test]
fn split_then_open_lands_window_in_new_pane() {
    let mut w = World::new();
    let (first, _) = w.open_window();
    w.split(Right);
    let (second, _) = w.open_window();
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
    let (a, _) = w.open_window();
    let (b, rec) = w.stack_window();
    w.check_invariants();
    // Single pane still — stacking does not create a pane.
    assert_eq!(
        w.workspace(w.active_workspace_id()).unwrap().pane_count(),
        1
    );
    assert_eq!(rec.focused_window, Some(b));
    // Only the newly-focused window is visible.
    assert_eq!(w.visible_windows(), vec![b]);
    assert_eq!(w.pane(w.focused_pane_id()).unwrap().windows, vec![a, b]);
}

#[test]
fn cycle_stack_wraps_around() {
    let mut w = World::new();
    let (a, _) = w.open_window();
    let (b, _) = w.open_window();
    let (c, _) = w.open_window();
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
    let (left, _) = w.open_window(); // pane 1
    w.split(Right); // new pane to the right, focused
    let (right, _) = w.open_window(); // pane 2
    assert_eq!(w.focused_window(), Some(right));
    let rec = w.focus(Left);
    w.check_invariants();
    assert_eq!(w.focused_window(), Some(left));
    assert_eq!(rec.focused_window, Some(left));
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
    let (a, _) = w.open_window(); // pane A on the left
    w.split(Right);
    let (b, _) = w.open_window(); // pane B on the right, focused
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
    let (a, _) = w.open_window();
    w.split(Right);
    let (_b, _) = w.open_window();
    let focus_before = w.focused_window();
    let rec = w.resize(Left, 0.1);
    w.check_invariants();
    assert_eq!(w.focused_window(), focus_before);
    assert!(rec.removed.is_empty());
    assert!(rec.visibility_changed);
    // Pane count unchanged.
    assert_eq!(
        w.workspace(w.active_workspace_id()).unwrap().pane_count(),
        2
    );
    // a still present.
    assert!(w.window(a).is_some());
}

#[test]
fn close_empties_pane_and_collapses_tree() {
    let mut w = World::new();
    let (a, _) = w.open_window(); // pane A
    w.split(Right);
    let (b, _) = w.open_window(); // pane B, focused
    assert_eq!(
        w.workspace(w.active_workspace_id()).unwrap().pane_count(),
        2
    );
    // Close the only window of pane B -> pane B collapses, A is promoted.
    w.close_window(b).unwrap();
    w.check_invariants();
    assert_eq!(
        w.workspace(w.active_workspace_id()).unwrap().pane_count(),
        1
    );
    // Focus falls to the surviving pane holding a.
    assert_eq!(w.focused_window(), Some(a));
}

#[test]
fn close_focused_pane_repoints_focus_to_survivor() {
    let mut w = World::new();
    let (a, _) = w.open_window();
    w.split(Right);
    let (b, _) = w.open_window();
    // Focused pane is B. Close B's window — focus must move to a valid pane.
    w.close_window(b).unwrap();
    w.check_invariants();
    let fp = w.focused_pane_id();
    assert!(
        w.workspace(w.active_workspace_id())
            .unwrap()
            .panes()
            .contains(&fp)
    );
    assert_eq!(w.focused_window(), Some(a));
}

#[test]
fn new_workspace_switches_to_a_fresh_empty_workspace() {
    let mut w = World::new();
    w.open_window();
    let original = w.active_workspace_id();
    let (ws2, rec) = w.new_workspace("code");
    w.check_invariants();
    assert_ne!(ws2, original);
    assert_eq!(w.active_workspace_id(), ws2);
    assert_eq!(w.workspace(ws2).unwrap().name, "code");
    // Fresh workspace is empty.
    assert_eq!(w.visible_windows(), Vec::new());
    assert_eq!(rec.focused_window, None);
}

#[test]
fn switch_workspace_changes_visible_set() {
    let mut w = World::new();
    let (a, _) = w.open_window();
    let original = w.active_workspace_id();
    let (ws2, _) = w.new_workspace("two");
    let (b, _) = w.open_window();
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
    let (other, _) = w.new_workspace("x");
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
    let (second, _) = w.new_workspace("b");
    let (third, _) = w.new_workspace("c");
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
fn move_window_to_workspace_transfers_and_collapses_source() {
    let mut w = World::new();
    let (a, _) = w.open_window(); // workspace 1, pane A
    w.split(Right);
    let (b, _) = w.open_window(); // workspace 1, pane B (focused)
    let ws1 = w.active_workspace_id();
    let (ws2, _) = w.new_workspace("two");
    w.switch_workspace(ws1).unwrap();

    // Move b (in pane B) to ws2. Pane B empties and collapses; A is promoted.
    w.move_window_to_workspace(b, ws2).unwrap();
    w.check_invariants();
    assert_eq!(w.workspace(ws1).unwrap().pane_count(), 1);
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
    let (a, _) = w.open_window();
    let here = w.active_workspace_id();
    let err = w.move_window_to_workspace(a, here).unwrap_err();
    assert!(matches!(err, LayoutError::SameWorkspace));
}

#[test]
fn move_unknown_window_to_workspace_errors() {
    let mut w = World::new();
    let (ws2, _) = w.new_workspace("two");
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
        let (id, _) = w.open_window();
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
    let (a, _) = w.open_window();
    w.split(Down);
    let (b, _) = w.open_window();
    w.close_window(a).unwrap();
    w.check_invariants();
    // Reopen into the surviving pane.
    let (c, _) = w.open_window();
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
    let (one, _) = w.open_window(); // pane 1 (top, before the H split)
    w.split(Down);
    let (two, _) = w.open_window(); // pane 2 (full-width bottom)
    w.focus(Up); // back to pane 1
    assert_eq!(w.focused_window(), Some(one));
    w.split(Right);
    let (three, _) = w.open_window(); // pane 3 (top-right)
    assert_eq!(w.focused_window(), Some(three));
    // Down from the top-right pane ascends two levels and lands on the bottom pane.
    w.focus(Down);
    w.check_invariants();
    assert_eq!(w.focused_window(), Some(two));
    // And the bottom full-width pane has no horizontal neighbour (edge); Right is a no-op.
    w.focus(Right);
    assert_eq!(w.focused_window(), Some(two));
}
