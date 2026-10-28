//! KDL workspace-file tests: golden schema example, round-trips and typed errors.

use crucible_layout::{Axis, Direction, World};
use crucible_persist::{
    PaneRecord, PersistError, RecordNode, WindowRecord, WorkspaceRecord, store,
};
use std::path::PathBuf;

const GOLDEN: &str = include_str!("golden/workspace.kdl");

fn term(cwd: &str, command: Option<&str>) -> WindowRecord {
    WindowRecord::Terminal {
        cwd: cwd.into(),
        command: command.map(String::from),
    }
}

fn pane(windows: Vec<WindowRecord>) -> RecordNode {
    RecordNode::Pane(PaneRecord {
        focused: 0,
        windows,
    })
}

fn golden_record() -> WorkspaceRecord {
    let root = "crucible-dev-env";
    WorkspaceRecord {
        name: "crucible-dev-env".into(),
        root: Some(root.into()),
        focused_pane: 1,
        output: None,
        tree: RecordNode::Split {
            axis: Axis::Horizontal,
            ratio: 0.5,
            first: Box::new(pane(vec![term(root, Some("cargo test"))])),
            second: Box::new(RecordNode::Split {
                axis: Axis::Vertical,
                ratio: 0.6,
                first: Box::new(pane(vec![term(root, None)])),
                second: Box::new(pane(vec![WindowRecord::App {
                    app_id: Some("firefox".into()),
                    cmdline: Some("firefox-esr".into()),
                }])),
            }),
        },
    }
}

#[test]
fn golden_file_parses_to_the_expected_record() {
    assert_eq!(WorkspaceRecord::from_kdl(GOLDEN).unwrap(), golden_record());
}

#[test]
fn golden_record_round_trips_through_kdl() {
    let text = golden_record().to_kdl().unwrap();
    assert!(text.starts_with("version 1\n"), "{text}");
    assert!(text.contains("axis=horizontal") || text.contains("axis=\"horizontal\""));
    assert_eq!(WorkspaceRecord::from_kdl(&text).unwrap(), golden_record());
    // Serialization is stable (a second pass is byte-identical).
    let again = WorkspaceRecord::from_kdl(&text).unwrap().to_kdl().unwrap();
    assert_eq!(text, again);
}

#[test]
fn optional_fields_are_omitted_when_none() {
    let rec = WorkspaceRecord {
        name: "w".into(),
        root: None,
        focused_pane: 0,
        output: None,
        tree: pane(vec![
            term("/a", None),
            WindowRecord::App {
                app_id: None,
                cmdline: None,
            },
        ]),
    };
    let text = rec.to_kdl().unwrap();
    assert!(!text.contains("root"), "{text}");
    assert!(!text.contains("command"), "{text}");
    assert!(
        !text.contains("app-id") && !text.contains("cmdline"),
        "{text}"
    );
    assert_eq!(WorkspaceRecord::from_kdl(&text).unwrap(), rec);
}

#[test]
fn awkward_strings_round_trip() {
    let nasty = "echo \"hi\\there\"\n'quoted' # not a comment {} \u{e9}";
    let rec = WorkspaceRecord {
        name: nasty.into(),
        root: Some(PathBuf::from("/p with space/\"q\"")),
        focused_pane: 0,
        output: None,
        tree: pane(vec![term("/c d", Some(nasty))]),
    };
    let text = rec.to_kdl().unwrap();
    assert_eq!(WorkspaceRecord::from_kdl(&text).unwrap(), rec);
}

#[test]
fn single_pane_workspace_has_no_split() {
    let rec = WorkspaceRecord {
        name: "solo".into(),
        root: None,
        focused_pane: 0,
        output: None,
        tree: pane(vec![]),
    };
    let text = rec.to_kdl().unwrap();
    assert!(!text.contains("split"));
    assert_eq!(WorkspaceRecord::from_kdl(&text).unwrap(), rec);
}

#[test]
fn world_to_kdl_to_layout_round_trip() {
    let mut w = World::new("1");
    w.split(Direction::Right);
    w.resize(Direction::Right, 0.1);
    w.split(Direction::Down);
    let ws = w.active_workspace_id();
    let panes = w.workspace(ws).unwrap().panes();
    w.open_window_at(panes[0], 0).unwrap();
    w.open_window_at(panes[0], 1).unwrap();
    w.set_stack_focus(panes[0], 1).unwrap();
    let rec = WorkspaceRecord::from_world(&w, ws, None, |_| term("/x", Some("vim 'a b'"))).unwrap();
    let back = WorkspaceRecord::from_kdl(&rec.to_kdl().unwrap()).unwrap();
    assert_eq!(back, rec);
    let snap = w.workspace_layout(ws).unwrap();
    assert_eq!(back.layout().root, snap.root.map_ref(|_| ()));
    assert_eq!(back.layout().focused_pane, snap.focused_pane);
    assert_eq!(back.panes().len(), 3);
    assert_eq!(back.panes()[0].focused, 1);
}

#[test]
fn non_utf8_paths_are_refused_not_mangled() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let bad = PathBuf::from(OsString::from_vec(vec![b'/', 0xff, 0xfe]));
    let rec = WorkspaceRecord {
        name: "w".into(),
        root: Some(bad),
        focused_pane: 0,
        output: None,
        tree: pane(vec![]),
    };
    assert!(matches!(rec.to_kdl(), Err(PersistError::NonUtf8Path(_))));
}

fn err(text: &str) -> PersistError {
    WorkspaceRecord::from_kdl(text).expect_err("should fail")
}

const OK_TAIL: &str = "workspace \"w\" { focused-pane 0\n pane focused=0 }";

#[test]
fn parse_errors_carry_position() {
    let e = err("version 1\nworkspace \"w\" {\n  pane focused=0 {{{\n}\n");
    let PersistError::Parse { line, .. } = e else {
        panic!("{e:?}");
    };
    assert!(line >= 1);
    assert!(e_msg(&err("version 1\n\"unterminated")).contains("line"));
}

fn e_msg(e: &PersistError) -> String {
    e.to_string()
}

#[test]
fn newer_version_is_refused() {
    let e = err(&format!("version 2\n{OK_TAIL}"));
    assert!(matches!(
        e,
        PersistError::UnsupportedVersion {
            found: 2,
            supported: 1
        }
    ));
    assert!(matches!(
        err(OK_TAIL),
        PersistError::Missing { ref field, .. } if field == "version"
    ));
    assert!(matches!(
        err(&format!("version 0\n{OK_TAIL}")),
        PersistError::Invalid { .. }
    ));
    assert!(matches!(
        err(&format!("version \"1\"\n{OK_TAIL}")),
        PersistError::Invalid { .. }
    ));
}

#[test]
fn unknown_nodes_and_properties_are_named() {
    let e = err(&format!("version 1\nbogus 1\n{OK_TAIL}"));
    assert!(matches!(e, PersistError::UnknownNode { ref name, line: 2 } if name == "bogus"));

    let e = err("version 1\nworkspace \"w\" { focused-pane 0\n pane focused=0 { floaty x=1 } }");
    assert!(matches!(e, PersistError::UnknownNode { ref name, .. } if name == "floaty"));

    let e = err("version 1\nworkspace \"w\" colour=\"red\" { focused-pane 0\n pane focused=0 }");
    assert!(matches!(
        e,
        PersistError::UnknownProperty { ref node, ref prop, .. } if node == "workspace" && prop == "colour"
    ));

    let e = err(
        "version 1\nworkspace \"w\" { focused-pane 0\n pane focused=0 { terminal cwd=\"/\" shell=\"zsh\" } }",
    );
    assert!(matches!(
        e,
        PersistError::UnknownProperty { ref prop, .. } if prop == "shell"
    ));
    assert!(e.to_string().contains("shell"));
}

#[test]
fn split_needs_exactly_two_children() {
    for kids in [
        "",
        "pane focused=0",
        "pane focused=0\n pane focused=0\n pane focused=0",
    ] {
        let text = format!(
            "version 1\nworkspace \"w\" {{ focused-pane 0\n split axis=\"horizontal\" ratio=0.5 {{ {kids} }} }}"
        );
        assert!(
            matches!(err(&text), PersistError::WrongChildCount { .. }),
            "{kids:?}"
        );
    }
}

#[test]
fn invalid_values_are_rejected_not_coerced() {
    let ws = |body: &str| format!("version 1\nworkspace \"w\" {{ {body} }}");
    let split = |attrs: &str| {
        ws(&format!(
            "focused-pane 0\n split {attrs} {{ pane focused=0\n pane focused=0 }}"
        ))
    };
    assert!(matches!(
        err(&split("axis=\"diagonal\" ratio=0.5")),
        PersistError::Invalid { .. }
    ));
    assert!(matches!(
        err(&split("ratio=0.5")),
        PersistError::Missing { .. }
    ));
    assert!(matches!(
        err(&split("axis=\"vertical\"")),
        PersistError::Missing { .. }
    ));
    for r in ["1.5", "0.0", "1.0", "-0.5", "1", "\"0.5\""] {
        assert!(
            matches!(
                err(&split(&format!("axis=\"vertical\" ratio={r}"))),
                PersistError::Invalid { .. }
            ),
            "{r}"
        );
    }
    // focused-pane / focused out of range, negative, or the wrong type.
    assert!(matches!(
        err(&ws("focused-pane 1\n pane focused=0")),
        PersistError::Invalid { .. }
    ));
    assert!(matches!(
        err(&ws("focused-pane -1\n pane focused=0")),
        PersistError::Invalid { .. }
    ));
    assert!(matches!(
        err(&ws("focused-pane 0\n pane focused=1")),
        PersistError::Invalid { .. }
    ));
    assert!(matches!(
        err(&ws("focused-pane 0\n pane")),
        PersistError::Missing { .. }
    ));
    assert!(matches!(
        err(&ws("pane focused=0")),
        PersistError::Missing { .. }
    ));
    assert!(matches!(
        err(&ws("focused-pane 0")),
        PersistError::Missing { .. }
    ));
    assert!(matches!(
        err(&ws("focused-pane 0\n pane focused=0 { terminal }")),
        PersistError::Missing { .. }
    ));
    assert!(matches!(
        err(&ws("focused-pane 0\n pane focused=0 { terminal cwd=1 }")),
        PersistError::Invalid { .. }
    ));
}

#[test]
fn duplicates_and_extra_workspaces_are_errors() {
    let e = err(&format!("version 1\nversion 1\n{OK_TAIL}"));
    assert!(matches!(e, PersistError::Duplicate { .. }));
    let e = err(&format!("version 1\n{OK_TAIL}\n{OK_TAIL}"));
    assert!(matches!(e, PersistError::Duplicate { ref name, .. } if name == "workspace"));
    let e = err("version 1\nworkspace \"w\" { focused-pane 0\n pane focused=0\n pane focused=0 }");
    assert!(matches!(e, PersistError::Duplicate { .. }));
}

#[test]
fn store_round_trip_of_the_golden_record() {
    let dir = std::env::temp_dir().join(format!("cde-golden-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = store::save(&dir, &golden_record()).unwrap();
    assert_eq!(store::load(&path).unwrap(), golden_record());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn last_viewed_output_round_trips_when_present() {
    let mut rec = golden_record();
    rec.output = Some("DP-1".into());
    let text = rec.to_kdl().unwrap();
    assert!(text.contains("output="), "{text}");
    assert_eq!(WorkspaceRecord::from_kdl(&text).unwrap(), rec);
    // Absent output is omitted entirely.
    assert!(!golden_record().to_kdl().unwrap().contains("output="));
}

#[test]
fn old_format_without_output_loads_with_none() {
    let old =
        "version 1\nworkspace \"w\" root=\"/r\" {\n    focused-pane 0\n    pane focused=0\n}\n";
    let rec = WorkspaceRecord::from_kdl(old).unwrap();
    assert_eq!(rec.output, None);
    assert_eq!(rec.root, Some(PathBuf::from("/r")));
}
