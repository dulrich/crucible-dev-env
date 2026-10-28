//! End-to-end `World` tests of the window, split, stack, close-pane, workspace and clone/churn verbs, asserting the model invariants after every interesting mutation via `check_invariants`.

use crucible_layout::{Direction::*, FocusChange, LayoutError, WindowId, WorkspaceId, World};

mod common;
use common::*;

#[test]
fn new_world_has_one_output_one_workspace_one_empty_pane() {
    let w = World::new("1");
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
    let mut w = World::new("1");
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
    let mut w = World::new("1");
    let a = w.open_window();
    w.close_window(a).unwrap();
    let err = w.close_window(a).unwrap_err();
    assert!(matches!(err, LayoutError::UnknownWindow(_)));
}

#[test]
fn closing_last_window_of_only_pane_keeps_the_empty_pane() {
    let mut w = World::new("1");
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
    let mut w = World::new("1");
    w.open_window();
    let new_pane = w.split(Right);
    w.check_invariants();
    assert_eq!(w.focused_pane_id(), new_pane);
    let ws = w.workspace(w.active_workspace_id()).unwrap();
    assert_eq!(ws.pane_count(), 2);
    // New pane is empty until a window opens into it.
    assert_eq!(w.pane(new_pane).unwrap().windows().len(), 0);
}

#[test]
fn split_then_open_lands_window_in_new_pane() {
    let mut w = World::new("1");
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
        let mut w = World::new("1");
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
    let mut w = World::new("1");
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
    assert_eq!(w.pane(w.focused_pane_id()).unwrap().windows(), vec![a, b]);
}

#[test]
fn cycle_stack_wraps_around() {
    let mut w = World::new("1");
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
fn close_window_keeps_emptied_pane() {
    let mut w = World::new("1");
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
    let mut w = World::new("1");
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
    let mut w = World::new("1");
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
    let mut w = World::new("1");
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
    let mut w = World::new("1");
    let before = w.clone();
    assert_eq!(w.close_pane(), Err(LayoutError::LastPane));
    assert_eq!(w, before);
    w.check_invariants();
}

#[test]
fn open_window_after_close_pane_lands_in_newly_focused_pane() {
    let mut w = World::new("1");
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
    let mut w = World::new("1");
    w.open_window();
    let original = w.active_workspace_id();
    let ws2 = w.new_workspace("code").unwrap();
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
    let mut w = World::new("1");
    let a = w.open_window();
    let original = w.active_workspace_id();
    let ws2 = w.new_workspace("two").unwrap();
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
    let mut w = World::new("1");
    let other = w.new_workspace("x").unwrap();
    // A raw id far beyond any allocated workspace is guaranteed absent.
    let foreign = WorkspaceId::from_raw(u64::MAX);
    let err = w.switch_workspace(foreign).unwrap_err();
    assert!(matches!(err, LayoutError::UnknownWorkspace(_)));
    // sanity: switching to a real one works.
    w.switch_workspace(other).unwrap();
}

#[test]
fn cycle_workspace_wraps() {
    let mut w = World::new("1");
    let first = w.active_workspace_id();
    let second = w.new_workspace("b").unwrap();
    let third = w.new_workspace("c").unwrap();
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
    let mut w = World::new("1");
    let id = w.active_workspace_id();
    w.rename_workspace(id, "main").unwrap();
    assert_eq!(w.workspace(id).unwrap().name, "main");

    let foreign = WorkspaceId::from_raw(u64::MAX);
    let err = w.rename_workspace(foreign, "nope").unwrap_err();
    assert!(matches!(err, LayoutError::UnknownWorkspace(_)));
}

#[test]
fn move_window_to_workspace_transfers_and_keeps_source_pane() {
    let mut w = World::new("1");
    let a = w.open_window(); // workspace 1, pane A
    w.split(Right);
    let b = w.open_window(); // workspace 1, pane B (focused)
    let ws1 = w.active_workspace_id();
    let ws2 = w.new_workspace("two").unwrap();
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
    let mut w = World::new("1");
    let a = w.open_window();
    let here = w.active_workspace_id();
    let err = w.move_window_to_workspace(a, here).unwrap_err();
    assert!(matches!(err, LayoutError::SameWorkspace));
}

#[test]
fn move_unknown_window_to_workspace_errors() {
    let mut w = World::new("1");
    let ws2 = w.new_workspace("two").unwrap();
    let fw = WindowId::from_raw(u64::MAX);
    let err = w.move_window_to_workspace(fw, ws2).unwrap_err();
    assert!(matches!(err, LayoutError::UnknownWindow(_)));
}

#[test]
fn model_is_clone_and_independent() {
    let mut w = World::new("1");
    w.open_window();
    w.split(Right);
    w.open_window();
    let snapshot = w.clone();
    // Mutate the original; the clone is unaffected.
    w.open_window();
    w.new_workspace("scratch").unwrap();
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
    let mut w = World::new("1");
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
    let mut w = World::new("1");
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
