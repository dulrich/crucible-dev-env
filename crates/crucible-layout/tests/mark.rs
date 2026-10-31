//! `World::mark_window` / `World::move_window_to_pane` through the public API only.

use crucible_layout::{Direction::*, LayoutError, PaneId, Rect, WindowId, World};

#[test]
fn mark_marks_focused_window() {
    let mut w = World::new("1");
    let a = w.open_window();
    w.mark_window();
    w.check_invariants();
    assert_eq!(w.marked(), Some(a));
}

#[test]
fn mark_with_no_focused_window_is_noop() {
    let mut w = World::new("1");
    w.mark_window();
    w.check_invariants();
    assert_eq!(w.marked(), None);
}

#[test]
fn second_press_places_within_workspace() {
    let mut w = World::new("1");
    let a = w.open_window();
    let a2 = w.open_window(); // same pane stack as a; a2 focused
    w.split(Right);
    let b = w.open_window();
    w.focus(Left); // pane 1, focused a2
    w.mark_window();
    assert_eq!(w.marked(), Some(a2));
    w.focus(Right);
    w.mark_window();
    w.check_invariants();
    assert_eq!(w.marked(), None);
    assert_eq!(w.focused_window(), Some(a2));
    assert_eq!(w.pane_of_window(a2), Some(w.focused_pane_id()));
    assert_eq!(w.pane_of_window(b), Some(w.focused_pane_id()));
    // The source pane keeps its other window.
    assert_ne!(w.pane_of_window(a), w.pane_of_window(a2));
}

#[test]
fn second_press_places_across_workspaces() {
    let mut w = World::new("1");
    let a = w.open_window();
    let ws1 = w.active_workspace_id();
    w.mark_window();
    let ws2 = w.new_workspace("two").unwrap();
    assert_eq!(w.active_workspace_id(), ws2);
    w.mark_window();
    w.check_invariants();
    assert_eq!(w.marked(), None);
    assert_eq!(w.focused_window(), Some(a));
    assert_eq!(w.active_workspace_id(), ws2);
    w.switch_workspace(ws1).unwrap();
    assert_eq!(w.focused_window(), None);
}

#[test]
fn second_press_places_across_outputs() {
    let mut w = World::new("1");
    let first = w.focused_output();
    w.set_output_rect(first, Rect::new(0, 0, 1920, 1080))
        .unwrap();
    let a = w.open_window();
    let source_pane = w.focused_pane_id();
    w.mark_window();
    let second = w.add_output(Rect::new(1920, 0, 1280, 1024), "o2").unwrap();
    w.set_focused_output(second).unwrap();
    let target_pane = w.focused_pane_id();
    w.mark_window();
    w.check_invariants();
    assert_eq!(w.focused_output(), second);
    assert_eq!(w.pane_of_window(a), Some(target_pane));
    assert_ne!(source_pane, target_pane);
    assert_eq!(w.focused_window(), Some(a));
}

#[test]
fn same_pane_press_clears_and_keeps_window() {
    let mut w = World::new("1");
    let a = w.open_window();
    let pane = w.focused_pane_id();
    w.mark_window();
    w.mark_window();
    w.check_invariants();
    assert_eq!(w.marked(), None);
    assert_eq!(w.pane_of_window(a), Some(pane));
}

#[test]
fn closing_marked_window_clears_mark() {
    let mut w = World::new("1");
    let a = w.open_window();
    w.mark_window();
    w.close_window(a).unwrap();
    w.check_invariants();
    assert_eq!(w.marked(), None);
}

#[test]
fn removing_workspace_with_marked_window_clears_mark() {
    let mut w = World::new("1");
    let a = w.open_window();
    let ws1 = w.active_workspace_id();
    w.mark_window();
    w.new_workspace("two").unwrap();
    w.remove_workspace(ws1).unwrap();
    w.check_invariants();
    assert_eq!(w.marked(), None);
    assert_eq!(w.window(a), None);
}

#[test]
fn closing_other_window_keeps_mark() {
    let mut w = World::new("1");
    let a = w.open_window();
    let b = w.open_window();
    w.mark_window(); // marks b
    w.close_window(a).unwrap();
    w.check_invariants();
    assert_eq!(w.marked(), Some(b));
}

#[test]
fn emptied_source_pane_persists() {
    let mut w = World::new("1");
    w.open_window();
    w.split(Right);
    let src = w.focused_pane_id();
    let b = w.open_window();
    w.focus(Left);
    let dst = w.focused_pane_id();
    w.focus(Right);
    w.move_window_to_pane(b, dst).unwrap();
    w.check_invariants();
    assert_eq!(
        w.workspace(w.active_workspace_id()).unwrap().pane_count(),
        2
    );
    assert_eq!(w.pane_of_window(b), Some(dst));
    assert_eq!(w.focused_pane_id(), dst);
    assert!(w.pane_of_window(b) != Some(src));
}

#[test]
fn move_window_to_pane_errors_mutate_nothing() {
    let mut w = World::new("1");
    let a = w.open_window();
    w.split(Right);
    w.open_window();
    let pane_a = w.pane_of_window(a).unwrap();
    let before = w.clone();

    assert_eq!(w.move_window_to_pane(a, pane_a), Err(LayoutError::SamePane));
    assert_eq!(w, before);
    let ghost_window = WindowId::from_raw(u64::MAX);
    assert_eq!(
        w.move_window_to_pane(ghost_window, pane_a),
        Err(LayoutError::UnknownWindow(ghost_window))
    );
    assert_eq!(w, before);
    let ghost_pane = PaneId::from_raw(u64::MAX);
    assert_eq!(
        w.move_window_to_pane(a, ghost_pane),
        Err(LayoutError::UnknownPane(ghost_pane))
    );
    assert_eq!(w, before);
}
