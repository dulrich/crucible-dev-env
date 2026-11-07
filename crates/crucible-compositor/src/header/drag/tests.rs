//! Tests for the header press/drag/release decisions and the cached drag preview.

use super::*;
use crucible_layout::Rect;

fn ws(n: u64) -> WorkspaceId {
    WorkspaceId::from_raw(n)
}

fn hit(output: OutputId, workspace: Option<WorkspaceId>) -> Option<StripHit> {
    Some(StripHit { output, workspace })
}

fn press_on(output: OutputId, w: u64) -> HeaderPress {
    HeaderPress {
        output,
        workspace: Some(ws(w)),
    }
}

fn two_outputs() -> StackOrders {
    vec![
        (OutputId::from_raw(1), vec![ws(1), ws(2), ws(3)]),
        (OutputId::from_raw(2), vec![ws(7), ws(8)]),
    ]
}

#[test]
fn consumed_press_swallows_exactly_its_release() {
    const LEFT: u32 = 0x110;
    const RIGHT: u32 = 0x111;
    let press = HeaderPress {
        output: OutputId::from_raw(1),
        workspace: None,
    };
    let mut c = ConsumedButtons::default();
    // A release whose press was forwarded is forwarded.
    assert!(c.consume_release(RIGHT).is_none());
    c.consume_press(LEFT, press);
    assert!(c.consume_release(RIGHT).is_none()); // unrelated button forwarded
    // Swallowed even if the strip was hidden in between (the record is independent of it).
    assert_eq!(c.consume_release(LEFT), Some(press));
    assert!(c.consume_release(LEFT).is_none()); // cleared
}

#[test]
fn release_decision_table() {
    let (out, other) = (OutputId::from_raw(1), OutputId::from_raw(2));
    let orders = two_outputs(); // out: [1, 2, 3], other: [7, 8]
    let press = press_on(out, 1);
    let decide = |release, normal| drag_action(press, release, normal, false, &orders);
    let reorder = |to| ReleaseAction::Reorder {
        workspace: ws(1),
        to,
    };
    let move_to = |to| ReleaseAction::MoveToOutput {
        workspace: ws(1),
        output: other,
        to,
    };
    // Same segment: the existing click.
    assert_eq!(
        decide(hit(out, Some(ws(1))), true),
        ReleaseAction::Click(press)
    );
    // A different segment of the same output: reorder to that segment's index.
    assert_eq!(decide(hit(out, Some(ws(3))), true), reorder(2));
    // The same output's strip outside every label: move to the end.
    assert_eq!(decide(hit(out, None), true), reorder(2));
    // Another output's label: move there at that label's index.
    assert_eq!(decide(hit(other, Some(ws(8))), true), move_to(1));
    // Another output's strip outside every label: append there.
    assert_eq!(decide(hit(other, None), true), move_to(2));
    // A label the target stack does not hold, off every strip, or a cancelled mode: nothing.
    assert_eq!(decide(hit(other, Some(ws(2))), true), ReleaseAction::None);
    assert_eq!(decide(None, true), ReleaseAction::None);
    assert_eq!(decide(hit(other, None), false), ReleaseAction::None);
    // Releasing the last workspace on its own strip's empty area is already at the end.
    let last = press_on(out, 3);
    assert_eq!(
        drag_action(last, hit(out, None), true, false, &orders),
        ReleaseAction::None
    );
    // A target missing from the stack order: nothing.
    assert_eq!(decide(hit(out, Some(ws(9))), true), ReleaseAction::None);
}

#[test]
fn release_decision_label_less_press_clicks_in_place_only() {
    let out = OutputId::from_raw(1);
    let orders = two_outputs();
    let press = HeaderPress {
        output: out,
        workspace: None,
    };
    let decide = |release| drag_action(press, release, true, false, &orders);
    assert_eq!(decide(hit(out, None)), ReleaseAction::Click(press));
    assert_eq!(decide(hit(out, Some(ws(2)))), ReleaseAction::None);
}

#[test]
fn a_mode_change_between_press_and_release_cancels_but_the_release_is_consumed() {
    let out = OutputId::from_raw(1);
    let orders = two_outputs();
    let mut c = ConsumedButtons::default();
    c.consume_press(0x110, press_on(out, 1));
    // The mode is no longer Normal at release (a lock or overlay opened): no action...
    let recorded = c.consume_release(0x110).expect("release is consumed");
    for h in [hit(out, Some(ws(1))), hit(out, Some(ws(2)))] {
        let act = drag_action(recorded, h, false, false, &orders);
        assert_eq!(act, ReleaseAction::None);
    }
    // ...and the record is cleared, so it is not consumed twice.
    assert!(c.consume_release(0x110).is_none());
}

#[test]
fn drag_goes_live_only_past_the_threshold_and_a_label_less_press_never_arms() {
    let out = OutputId::from_raw(1);
    let hit = hit(out, Some(ws(2)));
    let mut d = HeaderDrag::arm(1, press_on(out, 1), (100.0, 10.0)).expect("label press arms");
    assert!(!d.motion((104.9, 10.0), hit, true) && !d.is_live());
    assert_eq!(d.target(), None);
    assert!(d.motion((105.1, 10.0), hit, true) && d.is_live());
    assert_eq!(d.target(), hit);
    // Same target again: no change (no redraw); travelling back keeps it live.
    assert!(!d.motion((101.0, 10.0), hit, true) && d.is_live());
    let none = HeaderPress {
        output: out,
        workspace: None,
    };
    assert!(HeaderDrag::arm(1, none, (0.0, 0.0)).is_none());
}

/// A real model: output `a` holds `[s, x, y]`, output `b` holds `[t, u]` (or just `[t]`).
fn world(b_holds_two: bool) -> (World, OutputId, OutputId, [WorkspaceId; 5]) {
    let mut w = World::new("s");
    let a = w.focused_output();
    let s = w.active_workspace_id();
    let b = w.add_output(Rect::new(1920, 0, 1920, 1080), "t").unwrap();
    let t = w.output(b).unwrap().workspaces()[0];
    let x = w.new_workspace("x").unwrap();
    let y = w.new_workspace("y").unwrap();
    let u = w.new_workspace("u").unwrap();
    if b_holds_two {
        w.move_workspace_to_output(u, b).unwrap();
    }
    w.switch_workspace(s).unwrap();
    (w, a, b, [s, x, y, t, u])
}

fn orders_of(w: &World) -> StackOrders {
    w.outputs()
        .map(|(id, o)| (id, o.workspaces().to_vec()))
        .collect()
}

fn press(output: OutputId, workspace: WorkspaceId) -> HeaderPress {
    StripHit {
        output,
        workspace: Some(workspace),
    }
}

#[test]
fn preview_reorders_within_and_across_outputs() {
    let (w, a, b, [s, x, y, t, u]) = world(true);
    let base = orders_of(&w);
    let after = |p, target| preview_of(&w, &base, p, target).map(|p| p.orders);
    let get = |o: &StackOrders, id| o.iter().find(|(i, _)| *i == id).unwrap().1.clone();
    // Forward and backward within one output; the empty area appends.
    let fwd = after(press(a, s), hit(a, Some(y))).unwrap();
    assert_eq!(get(&fwd, a), [x, y, s]);
    let back = after(press(a, y), hit(a, Some(s))).unwrap();
    assert_eq!(get(&back, a), [y, s, x]);
    let end = after(press(a, s), hit(a, None)).unwrap();
    assert_eq!(get(&end, a), [x, y, s]);
    // Across outputs: at a label's index, and at the end.
    let at = after(press(a, x), hit(b, Some(u))).unwrap();
    assert_eq!(get(&at, a), [s, y]);
    assert_eq!(get(&at, b), [t, x, u]);
    let app = preview_of(&w, &base, press(a, x), hit(b, None)).unwrap();
    assert_eq!(get(&app.orders, b), [t, u, x]);
    assert_eq!(app.dragged, x);
}

#[test]
fn preview_refuses_what_the_release_refuses() {
    let (w, a, b, [_, x, y, t, _]) = world(false); // b holds only [t]
    let base = orders_of(&w);
    let act = |p, target, normal, dragged| drag_action(p, target, normal, dragged, &base);
    // The last workspace never leaves its output: no preview, and the model refuses the move.
    assert_eq!(preview_of(&w, &base, press(b, t), hit(a, None)), None);
    let mut committed = w.clone();
    let refused = apply_release(&mut committed, act(press(b, t), hit(a, None), true, true));
    assert_eq!(refused, Err(LayoutError::LastWorkspace(b)));
    assert_eq!(committed, w);
    // Own slot, off every strip, a non-Normal mode: no preview and a no-op release.
    assert_eq!(preview_of(&w, &base, press(a, x), hit(a, Some(x))), None);
    let own = hit(a, Some(x));
    assert_eq!(act(press(a, x), own, true, true), ReleaseAction::None);
    assert_eq!(preview_of(&w, &base, press(a, x), None), None);
    let other = hit(a, Some(y));
    assert_eq!(act(press(a, x), other, false, true), ReleaseAction::None);
    // A non-Normal drag motion clears the target, so the cached preview is empty too.
    let mut d = HeaderDrag::arm(1, press(a, x), (0.0, 0.0)).unwrap();
    d.motion((50.0, 0.0), other, false);
    d.refresh_preview(&w, base.clone());
    assert_eq!(d.preview(&base), None);
    // Before the threshold the existing click path is unchanged.
    assert_eq!(
        act(press(a, x), own, true, false),
        ReleaseAction::Click(press(a, x))
    );
}

#[test]
fn apply_release_commits_the_model_verbs_and_leaves_clicks_alone() {
    let (w, a, b, [s, x, y, t, u]) = world(true);
    let mut m = w.clone();
    let click = ReleaseAction::Click(press(a, s));
    assert_eq!(apply_release(&mut m, click), Ok(()));
    assert_eq!(apply_release(&mut m, ReleaseAction::None), Ok(()));
    assert_eq!(m, w);
    let reorder = ReleaseAction::Reorder {
        workspace: s,
        to: 9,
    };
    apply_release(&mut m, reorder).unwrap(); // clamped to the end
    assert_eq!(m.output(a).unwrap().workspaces(), [x, y, s]);
    let mv = ReleaseAction::MoveToOutput {
        workspace: y,
        output: b,
        to: 1,
    };
    apply_release(&mut m, mv).unwrap();
    assert_eq!(m.output(b).unwrap().workspaces(), [t, y, u]);
}

#[test]
fn the_cached_preview_recomputes_only_on_a_target_or_committed_order_change() {
    let (w, a, b, [s, x, y, ..]) = world(true);
    let base = orders_of(&w);
    let mut d = HeaderDrag::arm(1, press(a, s), (0.0, 0.0)).unwrap();
    // Not live: nothing cached.
    assert!(!d.refresh_preview(&w, base.clone()) && d.preview(&base).is_none());
    d.motion((50.0, 0.0), hit(a, Some(y)), true);
    assert!(d.refresh_preview(&w, base.clone()));
    let first = d.preview(&base).cloned().expect("a reorder previews");
    // Neither changed: no recompute, same value.
    assert!(!d.refresh_preview(&w, base.clone()));
    assert!(!d.motion((60.0, 0.0), hit(a, Some(y)), true));
    assert!(!d.refresh_preview(&w, base.clone()));
    assert_eq!(d.preview(&base), Some(&first));
    // The target changed: recompute.
    assert!(d.motion((70.0, 0.0), hit(b, None), true));
    assert!(d.refresh_preview(&w, base.clone()));
    assert_eq!(d.preview(&base).unwrap().orders[0].1, [x, y]);
    // The committed orders changed under the drag (an IPC or key move): the stale cache is
    // never read, and the next refresh recomputes over the new base.
    let mut moved = w.clone();
    moved.reorder_workspace(x, 0).unwrap();
    let next = orders_of(&moved);
    assert_eq!(d.preview(&next), None);
    assert!(d.refresh_preview(&moved, next.clone()));
    assert_eq!(d.preview(&next).unwrap().orders[0].1, [x, y]);
}
