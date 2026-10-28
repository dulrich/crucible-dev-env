//! Tests of the pane hit-test (`pane_at`) and `focus_pane` on empty panes.

use crucible_layout::{Direction::*, GapConfig, LayoutError, PaneId, Rect, World};

mod common;
use common::*;

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
    let mut w = World::new("1");
    let first = w.focused_output();
    w.set_output_rect(first, R1).unwrap();
    let second = w.add_output(R2, "o2").unwrap();
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
    let mut w = World::new("1");
    let first = w.focused_output();
    let second = w.add_output(R2, "o2").unwrap();
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
