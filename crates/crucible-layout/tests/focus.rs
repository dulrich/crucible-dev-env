//! End-to-end `World` tests of directional focus, move and resize, plus `focus_window` and the focus path, through the public API only.

use crucible_layout::{Direction::*, LayoutError, WindowId, World};

mod common;
use common::*;

#[test]
fn focus_moves_between_panes_directionally() {
    let mut w = World::new("1");
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
        let mut w = World::new("1");
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
    let mut w = World::new("1");
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
    let mut w = World::new("1");
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
    let mut w = World::new("1");
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
    let mut w = World::new("1");
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
fn deep_nested_focus_ascent_never_surfaces_parent() {
    // Layout V( H(1,3), 2 ): top row is [1 | 3], the bottom row is the full-width pane 2.
    // From pane 3 (top-right), focusing Down must ascend across the inner H split to the outer V
    // split to reach pane 2 — a single directional move, never a surfaced "focus parent" climb.
    let mut w = World::new("1");
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
    let mut w = World::new("1");
    let first = w.focused_output();
    w.set_output_rect(first, R1).unwrap();
    let a = w.open_window();
    let second = w.add_output(R2, "o2").unwrap();
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
    let mut w = World::new("1");
    let a = w.open_window();
    let ws1 = w.active_workspace_id();
    w.new_workspace("two").unwrap();
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
    let mut w = World::new("1");
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

    w.new_workspace("code").unwrap();
    let ws2 = w.focus_path();
    assert_eq!(ws2.workspace_name, "code");
    assert_eq!(ws2.window, None);
}

#[test]
fn focus_path_display_is_one_compact_line() {
    let mut w = World::new("1");
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
