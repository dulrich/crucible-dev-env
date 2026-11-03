//! Per-workspace fullscreen enter/exit rules and its projection, through the public API only.

use crucible_layout::{Direction::*, GapConfig, Rect, WindowId, World};

const OUT: Rect = Rect::new(0, 0, 800, 600);

/// One output (800x600) holding two side-by-side panes: `a` (left) and `b` (right, focused).
fn two_panes() -> (World, WindowId, WindowId) {
    let mut w = World::new("1");
    let out = w.focused_output();
    w.set_output_rect(out, OUT).unwrap();
    let a = w.open_window();
    w.split(Right);
    let b = w.open_window();
    (w, a, b)
}

fn gaps() -> GapConfig {
    GapConfig::new(10, 10)
}

#[test]
fn request_enters_projects_raw_output_rect_and_hides_others() {
    let (mut w, a, b) = two_panes();
    assert!(w.set_fullscreen(a, true).unwrap());
    w.check_invariants();
    assert!(w.is_fullscreen(a));
    assert_eq!(w.output_fullscreen(w.focused_output()), Some(a));
    // The fullscreen window is focused and alone, over the full raw rect (no gaps).
    let p = w.project_stored(gaps());
    assert_eq!(p.windows(), &[(a, OUT)]);
    assert_eq!(p.focus(), Some(a));
    assert!(!p.is_visible(b));
    assert_eq!(w.pane_rects(&[(w.focused_output(), OUT)], gaps()).len(), 1);
    // A repeat request changes nothing; an unset of another window changes nothing.
    assert!(!w.set_fullscreen(a, true).unwrap());
    assert!(!w.set_fullscreen(b, false).unwrap());
}

#[test]
fn unset_and_toggle_exit_and_restore_tiling() {
    let (mut w, a, b) = two_panes();
    let tiled = w.project_stored(gaps());
    assert!(w.toggle_fullscreen());
    assert!(w.is_fullscreen(b));
    assert!(w.toggle_fullscreen());
    assert!(!w.is_fullscreen(b));
    assert_eq!(w.project_stored(gaps()), tiled);
    w.set_fullscreen(a, true).unwrap();
    assert!(w.set_fullscreen(a, false).unwrap());
    assert_eq!(w.fullscreen_windows(), Vec::<WindowId>::new());
    w.check_invariants();
}

#[test]
fn at_most_one_per_workspace() {
    let (mut w, a, b) = two_panes();
    w.set_fullscreen(a, true).unwrap();
    w.set_fullscreen(b, true).unwrap();
    assert_eq!(w.fullscreen_windows(), vec![b]);
    assert!(!w.is_fullscreen(a));
}

#[test]
fn closing_the_window_exits() {
    let (mut w, a, _b) = two_panes();
    w.set_fullscreen(a, true).unwrap();
    w.close_window(a).unwrap();
    w.check_invariants();
    assert!(w.fullscreen_windows().is_empty());
}

#[test]
fn leaving_the_workspace_exits() {
    let (mut w, a, _b) = two_panes();
    let other = w.new_workspace("2").unwrap();
    let first = w.workspace_by_name("1").unwrap();
    w.switch_workspace(first).unwrap();
    w.set_fullscreen(a, true).unwrap();
    w.move_window_to_workspace(a, other).unwrap();
    w.check_invariants();
    assert!(!w.is_fullscreen(a));
}

#[test]
fn layout_verbs_on_the_workspace_exit_first() {
    type Verb = fn(&mut World);
    let verbs: [(&str, Verb); 6] = [
        ("focus", |w| w.focus(Right)),
        ("split", |w| {
            w.split(Down);
        }),
        ("move", |w| w.move_(Right)),
        ("cycle-stack", |w| w.cycle_stack(1)),
        ("open", |w| {
            w.open_window();
        }),
        ("mark-move", |w| {
            w.mark_window();
            w.focus(Right);
            w.mark_window();
        }),
    ];
    for (name, verb) in verbs {
        let (mut w, a, _b) = two_panes();
        w.set_fullscreen(a, true).unwrap();
        verb(&mut w);
        w.check_invariants();
        assert!(!w.is_fullscreen(a), "{name} exits fullscreen");
    }
}

#[test]
fn focusing_another_window_exits_but_focusing_itself_keeps() {
    let (mut w, a, b) = two_panes();
    w.set_fullscreen(a, true).unwrap();
    w.focus_window(a).unwrap();
    assert!(w.is_fullscreen(a));
    w.focus_window(b).unwrap();
    assert!(!w.is_fullscreen(a));
}

#[test]
fn switching_workspaces_away_and_back_keeps_it() {
    let (mut w, a, _b) = two_panes();
    w.set_fullscreen(a, true).unwrap();
    let first = w.active_workspace_id();
    let other = w.new_workspace("2").unwrap();
    w.switch_workspace(other).unwrap();
    assert!(w.is_fullscreen(a));
    assert_eq!(w.output_fullscreen(w.focused_output()), None);
    assert!(!w.project_stored(gaps()).is_visible(a));
    w.switch_workspace(first).unwrap();
    assert_eq!(w.project_stored(gaps()).windows(), &[(a, OUT)]);
}

#[test]
fn entering_from_a_hidden_stack_position_surfaces_the_window() {
    let mut w = World::new("1");
    let out = w.focused_output();
    w.set_output_rect(out, OUT).unwrap();
    let a = w.open_window();
    let _b = w.open_window(); // same pane; b on top
    w.set_fullscreen(a, true).unwrap();
    assert_eq!(w.project_stored(gaps()).windows(), &[(a, OUT)]);
    assert_eq!(w.focused_window(), Some(a));
}

#[test]
fn not_serialized() {
    let (mut w, _a, b) = two_panes();
    let ws = w.active_workspace_id();
    let before = w.workspace_layout(ws);
    w.set_fullscreen(b, true).unwrap();
    assert_eq!(w.workspace_layout(ws), before);
}
