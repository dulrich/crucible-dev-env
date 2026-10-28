//! Tests of the persistence core: topology snapshot round-trips, stack replay, and unique workspace names.

use crucible_layout::{
    Direction::*, LayoutError, LayoutNode, PaneId, WindowId, WorkspaceId, WorkspaceLayout, World,
};

mod common;
use common::*;

// ---- Phase 3 persistence core: topology snapshot, stack replay, unique names ----

/// The pane ids of a returned tree, in tree order.
fn leaf_ids(root: &LayoutNode<PaneId>) -> Vec<PaneId> {
    root.leaves().into_iter().copied().collect()
}

/// The shape of a layout with its leaf payloads erased.
fn shape<P>(layout: &WorkspaceLayout<P>) -> WorkspaceLayout<()> {
    WorkspaceLayout {
        root: layout.root.map_ref(|_| ()),
        focused_pane: layout.focused_pane,
    }
}

/// Builds a nested 4-pane workspace with non-0.5 ratios and returns its id.
fn nested_workspace(w: &mut World) -> WorkspaceId {
    w.split(Right);
    w.resize(Right, 0.1);
    w.split(Down);
    w.resize(Down, 0.2);
    w.split(Right);
    w.resize(Right, -0.15);
    w.check_invariants();
    w.active_workspace_id()
}

#[test]
fn topology_round_trips_through_insert_workspace() {
    let mut w = world();
    let ws = nested_workspace(&mut w);
    let snap = w.workspace_layout(ws).unwrap();
    assert_eq!(snap.root.pane_count(), 4);
    assert_eq!(snap.focused_pane, 3);
    let LayoutNode::Split { ratio, .. } = &snap.root else {
        panic!("expected a split root");
    };
    assert!((*ratio - 0.6).abs() < 1e-9);

    let out = w.focused_output();
    let (new_ws, root) = w.insert_workspace(out, "copy", &snap).unwrap();
    let panes = leaf_ids(&root);
    w.check_invariants();
    assert_eq!(panes.len(), 4);
    assert_eq!(w.workspace(new_ws).unwrap().panes(), panes);
    let copied = w.workspace_layout(new_ws).unwrap();
    // The returned tree is exactly the new workspace's snapshot (same pane ids, tree order).
    assert_eq!(copied.root, root);
    assert_eq!(shape(&copied), shape(&snap));
    // Not switched to, and appended last.
    assert_eq!(w.active_workspace_id(), ws);
    assert_eq!(w.output(out).unwrap().workspaces().last(), Some(&new_ws));
    assert_eq!(w.workspace(new_ws).unwrap().focused_pane, panes[3]);
}

#[test]
fn single_pane_snapshot_round_trips() {
    let mut w = world();
    let snap = w.workspace_layout(w.active_workspace_id()).unwrap();
    assert_eq!(snap.root, LayoutNode::Pane(w.focused_pane_id()));
    let out = w.focused_output();
    let (id, panes) = w.insert_workspace(out, "solo", &snap).unwrap();
    let panes = leaf_ids(&panes);
    w.check_invariants();
    assert_eq!(panes.len(), 1);
    assert_eq!(shape(&w.workspace_layout(id).unwrap()), shape(&snap));
}

#[test]
fn insert_workspace_clamps_focus_and_rejects_bad_input() {
    let mut w = world();
    let out = w.focused_output();
    let mut snap = WorkspaceLayout {
        root: LayoutNode::Pane(()),
        focused_pane: 9,
    };
    let (id, panes) = w.insert_workspace(out, "a", &snap).unwrap();
    let panes = leaf_ids(&panes);
    assert_eq!(w.workspace(id).unwrap().focused_pane, panes[0]);
    assert_eq!(w.workspace_layout(id).unwrap().focused_pane, 0);

    // Duplicate name, unknown output, bad ratio: nothing is mutated.
    let before = w.clone();
    assert_eq!(
        w.insert_workspace(out, "a", &snap),
        Err(LayoutError::DuplicateName("a".into()))
    );
    snap.root = LayoutNode::Split {
        axis: crucible_layout::Axis::Horizontal,
        ratio: 1.5,
        first: Box::new(LayoutNode::Pane(())),
        second: Box::new(LayoutNode::Pane(())),
    };
    assert_eq!(
        w.insert_workspace(out, "b", &snap),
        Err(LayoutError::InvalidRatio)
    );
    for r in [0.0, 1.0, f64::NAN, -0.2] {
        if let LayoutNode::Split { ratio, .. } = &mut snap.root {
            *ratio = r;
        }
        assert_eq!(
            w.insert_workspace(out, "b", &snap),
            Err(LayoutError::InvalidRatio)
        );
    }
    assert_eq!(w, before);
    w.check_invariants();
    assert_eq!(w.workspace_layout(WorkspaceId::from_raw(999)), None);
}

#[test]
fn workspace_names_are_unique() {
    let mut w = world();
    let first = w.active_workspace_id();
    let out = w.focused_output();
    let snap = w.workspace_layout(first).unwrap();
    let (second, _) = w.insert_workspace(out, "second", &snap).unwrap();
    assert_eq!(w.workspace_by_name("second"), Some(second));
    assert_eq!(w.workspace_by_name("1"), Some(first));
    assert_eq!(w.workspace_by_name("nope"), None);
    assert_eq!(
        w.rename_workspace(second, "1"),
        Err(LayoutError::DuplicateName("1".into()))
    );
    // Renaming to its own name is fine; a free name is fine.
    w.rename_workspace(second, "second").unwrap();
    w.rename_workspace(second, "renamed").unwrap();
    assert_eq!(w.workspace_by_name("second"), None);
    assert_eq!(w.workspace_by_name("renamed"), Some(second));
    w.check_invariants();
}

#[test]
fn open_window_at_preserves_focus_and_active_state() {
    let mut w = world();
    let pane = w.focused_pane_id();
    let ws = w.active_workspace_id();
    // Empty pane: insert anywhere, focus stays 0.
    let a = w.open_window_at(pane, 99).unwrap();
    w.check_invariants();
    assert_eq!(w.pane(pane).unwrap().windows(), vec![a]);
    assert_eq!(w.pane(pane).unwrap().focused_index(), 0);
    let b = w.open_window_at(pane, 1).unwrap();
    let c = w.open_window_at(pane, 0).unwrap();
    w.check_invariants();
    assert_eq!(w.pane(pane).unwrap().windows(), vec![c, a, b]);
    // `a` was focused and stays focused though inserted-before shifted its index.
    assert_eq!(w.pane(pane).unwrap().focused_index(), 1);
    assert_eq!(w.focused_window(), Some(a));

    // Opening into another workspace's pane changes neither focus nor the active workspace.
    let out = w.focused_output();
    let snap = w.workspace_layout(ws).unwrap();
    let (other, panes) = w.insert_workspace(out, "other", &snap).unwrap();
    let panes = leaf_ids(&panes);
    let d = w.open_window_at(panes[0], 0).unwrap();
    w.check_invariants();
    assert_eq!(w.active_workspace_id(), ws);
    assert_eq!(w.focused_pane_id(), pane);
    assert_eq!(w.workspace(other).unwrap().focused_pane, panes[0]);
    assert_eq!(w.pane(panes[0]).unwrap().windows(), vec![d]);
    assert_eq!(
        w.open_window_at(PaneId::from_raw(999), 0),
        Err(LayoutError::UnknownPane(PaneId::from_raw(999)))
    );
}

#[test]
fn set_stack_focus_validates_range() {
    let mut w = world();
    let pane = w.focused_pane_id();
    assert_eq!(w.set_stack_focus(pane, 0), Ok(()));
    assert!(matches!(
        w.set_stack_focus(pane, 1),
        Err(LayoutError::StackIndexOutOfRange {
            index: 1,
            len: 0,
            ..
        })
    ));
    for _ in 0..3 {
        w.open_window();
    }
    w.set_stack_focus(pane, 1).unwrap();
    assert_eq!(w.pane(pane).unwrap().focused_index(), 1);
    assert!(w.set_stack_focus(pane, 3).is_err());
    assert_eq!(w.pane(pane).unwrap().focused_index(), 1);
    assert_eq!(
        w.set_stack_focus(PaneId::from_raw(999), 0),
        Err(LayoutError::UnknownPane(PaneId::from_raw(999)))
    );
    w.check_invariants();
}

/// Review M4: a pane holding a,b,c with b focused; closing a keeps b focused/visible.
#[test]
fn close_window_below_focus_keeps_focused_window() {
    let mut w = world();
    let pane = w.focused_pane_id();
    let a = w.open_window();
    let b = w.open_window();
    let _c = w.open_window();
    w.set_stack_focus(pane, 1).unwrap();
    w.close_window(a).unwrap();
    assert_eq!(w.focused_window(), Some(b));
    assert_eq!(w.visible_windows(), vec![b]);
    w.check_invariants();
}

/// Review M4, second site: moving a out of that pane keeps b focused.
#[test]
fn move_window_below_focus_keeps_focused_window() {
    let mut w = world();
    let home = w.active_workspace_id();
    let pane = w.focused_pane_id();
    let a = w.open_window();
    let b = w.open_window();
    let _c = w.open_window();
    w.set_stack_focus(pane, 1).unwrap();
    let other = w.new_workspace("other").unwrap();
    w.switch_workspace(home).unwrap();
    w.move_window_to_workspace(a, other).unwrap();
    assert_eq!(w.pane(pane).unwrap().focused_window(), Some(b));
    assert_eq!(w.focused_window(), Some(b));
    w.check_invariants();
}

/// `switch_workspace` to a workspace on a non-focused output focuses that output and activates it.
#[test]
fn switch_workspace_crosses_outputs() {
    let mut w = World::new("1");
    let first = w.focused_output();
    let second = w.add_output(R2, "o2").unwrap();
    let target = w.output(second).unwrap().active_workspace();
    w.set_focused_output(second).unwrap();
    let other = w.new_workspace("x").unwrap();
    w.set_focused_output(first).unwrap();
    assert_eq!(w.output(second).unwrap().active_workspace(), other);
    w.switch_workspace(target).unwrap();
    assert_eq!(w.focused_output(), second);
    assert_eq!(w.active_workspace_id(), target);
    assert_ne!(first, second);
    w.check_invariants();
}

#[test]
fn stack_replay_in_shuffled_order_reproduces_order_and_focus() {
    // Source: 3 panes with stacks of 3, 0 and 4 windows and assorted focus.
    let mut src = world();
    src.split(Right);
    src.split(Down);
    let ws = src.active_workspace_id();
    let src_panes = src.workspace(ws).unwrap().panes();
    assert_eq!(src_panes.len(), 3);
    let sizes = [3usize, 0, 4];
    for (p, n) in src_panes.iter().zip(sizes) {
        for _ in 0..n {
            src.open_window_at(*p, usize::MAX).unwrap();
        }
    }
    src.set_stack_focus(src_panes[0], 2).unwrap();
    src.set_stack_focus(src_panes[2], 1).unwrap();
    src.check_invariants();

    // "Record": per pane (stack length, focus).
    let record: Vec<(usize, usize)> = src_panes
        .iter()
        .map(|p| {
            let pane = src.pane(*p).unwrap();
            (pane.windows().len(), pane.focused_index())
        })
        .collect();
    let snap = src.workspace_layout(ws).unwrap();

    let mut dst = world();
    let out = dst.focused_output();
    let (_, panes) = dst.insert_workspace(out, "replay", &snap).unwrap();
    let panes = leaf_ids(&panes);

    // Windows "map" in a shuffled order: (pane index, saved stack index).
    let mut work: Vec<(usize, usize)> = record
        .iter()
        .enumerate()
        .flat_map(|(pi, (n, _))| (0..*n).map(move |si| (pi, si)))
        .collect();
    // Deterministic shuffle (xorshift) so the test is reproducible.
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    for i in (1..work.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        work.swap(i, (seed % (i as u64 + 1)) as usize);
    }
    let mut placed: Vec<Vec<usize>> = vec![Vec::new(); panes.len()];
    let mut ids: Vec<Vec<(usize, WindowId)>> = vec![Vec::new(); panes.len()];
    for (pi, si) in work {
        let at = placed[pi].iter().filter(|s| **s < si).count();
        let id = dst.open_window_at(panes[pi], at).unwrap();
        dst.check_invariants();
        placed[pi].push(si);
        ids[pi].push((si, id));
    }
    for (pi, (_, focus)) in record.iter().enumerate() {
        dst.set_stack_focus(panes[pi], *focus).unwrap();
    }
    dst.check_invariants();

    for (pi, (n, focus)) in record.iter().enumerate() {
        let pane = dst.pane(panes[pi]).unwrap();
        assert_eq!(pane.windows().len(), *n);
        assert_eq!(pane.focused_index(), *focus);
        let mut expect = ids[pi].clone();
        expect.sort_by_key(|(si, _)| *si);
        let expect: Vec<WindowId> = expect.into_iter().map(|(_, id)| id).collect();
        assert_eq!(pane.windows(), expect, "pane {pi} order");
    }
}
