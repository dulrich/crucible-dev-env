//! Tests of the derived `Projection` (`World::project`/`project_stored`): the single source of visible geometry and focus.

use crucible_layout::{Direction::*, FocusChange, Rect, World};

mod common;
use common::*;

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
    let ws2 = w.new_workspace("two").unwrap();
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
    let mut w = World::new("1");
    let first = w.focused_output();
    w.set_output_rect(first, Rect::new(0, 0, 800, 600)).unwrap();
    let a = w.open_window();

    let second = w.add_output(Rect::new(800, 0, 1024, 768), "o2").unwrap();
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
    let stale = w.add_output(Rect::new(800, 0, 640, 480), "o2").unwrap();
    let bindings = [(real, SCREEN), (stale, Rect::new(800, 0, 640, 480))];
    w.remove_output(stale).unwrap();

    let projection = w.project(&bindings, GAPS);
    assert_eq!(projection.windows().len(), 1);
    assert_eq!(projection.rect_of(id), Some(SCREEN));
    assert_eq!(projection.focus(), Some(id));
}
