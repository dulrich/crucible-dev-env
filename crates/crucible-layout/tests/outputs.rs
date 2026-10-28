//! End-to-end `World` tests of multi-output behavior: independent stacks, isolation, and workspace/window migration on output removal.

use crucible_layout::{Direction::*, LayoutError, OutputId, Rect, World};

mod common;
use common::*;

// ---- multi-output (P2-WS-3.5) -------------------------------------------------------------------

#[test]
fn add_then_remove_output_preserves_all_windows_end_to_end() {
    let mut w = World::new("1");
    let first = w.focused_output();
    w.set_output_rect(first, Rect::new(0, 0, 1920, 1080))
        .unwrap();

    // A window on the first output.
    let win_a = w.open_window();

    // Add a second output, focus it, and open a window there (on its own workspace stack).
    let second = w.add_output(Rect::new(1920, 0, 1280, 1024), "o2").unwrap();
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
    let mut w = World::new("1");
    let only = w.focused_output();
    assert_eq!(w.remove_output(only), Err(LayoutError::LastOutput));
    w.check_invariants();
}

// ---- multi-output behavior: independent stacks, isolation, migration on removal ----

#[test]
fn new_world_has_one_output_with_placeholder_rect() {
    let w = World::new("1");
    assert_eq!(w.output_count(), 1);
    let id = w.focused_output();
    assert_eq!(w.output(id).unwrap().rect, Rect::new(0, 0, 0, 0));
    w.check_invariants();
}

#[test]
fn add_output_creates_an_independent_stack() {
    let mut w = World::new("1");
    let first = w.focused_output();
    let before = w.project_stored(GAPS);
    let second = w.add_output(R2, "o2").unwrap();
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
    let mut w = World::new("1");
    let first = w.focused_output();
    let second = w.add_output(R2, "o2").unwrap();

    let second_before = w.output(second).unwrap().clone();
    // All of these act on `first` (the focused output).
    w.open_window();
    w.split(Right);
    w.new_workspace("scratch").unwrap();

    // The second output is byte-for-byte unchanged.
    assert_eq!(w.output(second).unwrap(), &second_before);
    // ...but the first output gained a workspace.
    assert_eq!(w.output(first).unwrap().workspaces().len(), 2);
    w.check_invariants();
}

#[test]
fn switching_focused_output_then_ops_affect_only_that_output() {
    let mut w = World::new("1");
    let first = w.focused_output();
    let second = w.add_output(R2, "o2").unwrap();

    let first_before = w.output(first).unwrap().clone();
    w.set_focused_output(second).unwrap();
    assert_eq!(w.focused_output(), second);

    let win = w.open_window();
    w.new_workspace("2").unwrap();

    // First output untouched.
    assert_eq!(w.output(first).unwrap(), &first_before);
    // Second output now has two workspaces, and the window landed on it.
    assert_eq!(w.output(second).unwrap().workspaces().len(), 2);
    assert!(w.window(win).is_some());
    w.check_invariants();
}

#[test]
fn per_output_geometry_is_independent() {
    let mut w = World::new("1");
    let first = w.focused_output();
    w.set_output_rect(first, R1).unwrap();
    let second = w.add_output(R2, "o2").unwrap();

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
    let mut w = World::new("1");
    let first = w.focused_output();
    let second = w.add_output(R2, "o2").unwrap();

    // Put a window on the second output (on its own workspace).
    w.set_focused_output(second).unwrap();
    let win = w.open_window();
    w.new_workspace("extra").unwrap(); // second now has 2 workspaces
    let win2 = w.open_window();
    assert_eq!(w.window_count(), 2);

    // Refocus first, then remove the second output.
    w.set_focused_output(first).unwrap();
    let first_ws_before = w.output(first).unwrap().workspaces().len();
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
        w.output(first).unwrap().workspaces().len(),
        first_ws_before + 2
    );
    assert_eq!(w.output_count(), 1);
    w.check_invariants();
}

#[test]
fn remove_focused_output_repoints_focus_to_survivor() {
    let mut w = World::new("1");
    let first = w.focused_output();
    let second = w.add_output(R2, "o2").unwrap();

    w.set_focused_output(second).unwrap();
    assert_eq!(w.focused_output(), second);

    w.remove_output(second).unwrap();
    assert_eq!(w.focused_output(), first);
    assert_eq!(w.project_stored(GAPS).focus(), w.focused_window());
    w.check_invariants();
}

#[test]
fn remove_last_output_errors() {
    let mut w = World::new("1");
    let only = w.focused_output();
    assert_eq!(w.remove_output(only), Err(LayoutError::LastOutput));
    w.check_invariants();
}

#[test]
fn remove_unknown_output_errors() {
    let mut w = World::new("1");
    let bogus = OutputId::from_raw(999);
    assert_eq!(
        w.remove_output(bogus),
        Err(LayoutError::UnknownOutput(bogus))
    );
}

#[test]
fn set_output_rect_and_set_focused_errors_on_unknown() {
    let mut w = World::new("1");
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
    let mut w = World::new("1");
    let first = w.focused_output();
    let second = w.add_output(R2, "o2").unwrap();
    let third = w.add_output(R2, "o3").unwrap();

    w.set_focused_output(third).unwrap();
    w.remove_output(third).unwrap();
    // Lowest remaining id is `first`.
    assert_eq!(w.focused_output(), first);
    assert!(w.output(first).is_some());
    assert!(w.output(second).is_some());
    w.check_invariants();
}
