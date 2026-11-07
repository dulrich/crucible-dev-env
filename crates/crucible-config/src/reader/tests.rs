//! Tests for [`NodeReader`] and its helpers.

use super::*;

fn doc(text: &str) -> KdlDocument {
    KdlDocument::parse_v2(text).unwrap()
}

#[test]
fn once_only_children_raise_duplicate_at_the_second_child() {
    let text = "block {\n    a 1\n    b 2\n    a 3\n}\n";
    let doc = doc(text);
    let block = NodeReader::new(text, &doc.nodes()[0]);
    let mut it = block.children(&[]).unwrap();
    assert_eq!(it.next().unwrap().unwrap().name(), "a");
    assert_eq!(it.next().unwrap().unwrap().name(), "b");
    match it.next().unwrap() {
        Err(ConfigError::Duplicate { at, name }) => {
            assert_eq!(name, "a");
            assert_eq!(at, Location { line: 4, column: 5 });
        }
        other => panic!("expected Duplicate, got {:?}", other.map(|r| r.name())),
    }
}

#[test]
fn repeatable_children_may_repeat() {
    let text = "block {\n    root 1\n    root 2\n}\n";
    let doc = doc(text);
    let block = NodeReader::new(text, &doc.nodes()[0]);
    let names: Vec<_> = block
        .children(&["root"])
        .unwrap()
        .map(|c| c.unwrap().name())
        .collect();
    assert_eq!(names, ["root", "root"]);
}

#[test]
fn bool_arg_type_error_message() {
    let text = "flag \"yes\"\n";
    let doc = doc(text);
    let err = NodeReader::new(text, &doc.nodes()[0])
        .bool_arg()
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "1:1: `flag` argument must be #true or #false"
    );
    let text = "flag #true\n";
    let doc = self::doc(text);
    assert!(NodeReader::new(text, &doc.nodes()[0]).bool_arg().unwrap());
}

#[test]
fn block_node_itself_takes_no_arguments_or_properties() {
    let text = "block 1 {\n}\nother k=1 {\n}\n";
    let doc = doc(text);
    let err = NodeReader::new(text, &doc.nodes()[0])
        .children(&[])
        .err()
        .unwrap();
    assert!(matches!(err, ConfigError::UnexpectedArgument { .. }));
    let err = NodeReader::new(text, &doc.nodes()[1])
        .children(&[])
        .err()
        .unwrap();
    assert!(matches!(err, ConfigError::UnknownProperty { .. }));
}

fn read_err(text: &str, f: impl Fn(NodeReader<'_>) -> Result<(), ConfigError>) -> String {
    let doc = doc(text);
    f(NodeReader::new(text, &doc.nodes()[0]))
        .unwrap_err()
        .to_string()
}

#[test]
fn u32_arg_in_checks_the_range_and_quotes_the_integer() {
    let ok = |text: &str| {
        let doc = doc(text);
        NodeReader::new(text, &doc.nodes()[0]).u32_arg_in(8..=64)
    };
    assert_eq!(ok("n 8\n").unwrap(), 8);
    assert_eq!(ok("n 64\n").unwrap(), 64);
    let f = |r: NodeReader<'_>| r.u32_arg_in(8..=64).map(|_| ());
    assert_eq!(
        read_err("n -1\n", f),
        "1:1: `n` argument: -1 is outside 8..=64"
    );
    assert_eq!(
        read_err("n 65\n", f),
        "1:1: `n` argument: 65 is outside 8..=64"
    );
    assert!(read_err("n 1.5\n", f).contains("must be an integer"));
}

#[test]
fn hex_prop_validates_at_the_property() {
    let f = |r: NodeReader<'_>| r.hex_prop("c").map(|_| ());
    assert_eq!(
        read_err("n c=\"red\"\n", f),
        "1:3: `n` property `c`: `red` is not a #rrggbb colour"
    );
    let text = "n c=\"#010203\"\n";
    let d = doc(text);
    assert_eq!(
        NodeReader::new(text, &d.nodes()[0]).hex_prop("c").unwrap(),
        Some(Rgb(1, 2, 3))
    );
    assert_eq!(
        NodeReader::new(text, &d.nodes()[0]).hex_prop("z").unwrap(),
        None
    );
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Thing(u8);

impl Named for Thing {
    const NAMES: &'static [(Self, &'static str)] =
        &[(Self(1), "a"), (Self(2), "b"), (Self(3), "c")];
}

#[test]
fn enum_arg_lists_names_in_table_order() {
    let text = "n \"b\"\nm \"z\"\n";
    let d = doc(text);
    assert_eq!(
        NodeReader::new(text, &d.nodes()[0])
            .enum_arg::<Thing>("a thing")
            .unwrap()
            .0,
        2
    );
    assert_eq!(
        NodeReader::new(text, &d.nodes()[1])
            .enum_arg::<Thing>("a thing")
            .err()
            .unwrap()
            .to_string(),
        "2:1: `m` argument: `z` is not one of a, b, c"
    );
}

#[test]
fn nonempty_string_arg_rejects_blank_with_the_what() {
    let f = |r: NodeReader<'_>| r.nonempty_string_arg("a name", "name").map(|_| ());
    assert_eq!(read_err("n \" \"\n", f), "1:1: `n` name: must not be empty");
    let text = "n \"x\"\n";
    let d = doc(text);
    assert_eq!(
        NodeReader::new(text, &d.nodes()[0])
            .nonempty_string_arg("a name", "name")
            .unwrap(),
        "x"
    );
}

#[test]
fn children_strict_rejects_child_properties_except_exempt() {
    let text = "block {\n    a k=1\n    d k=1\n}\n";
    let d = doc(text);
    let block = NodeReader::new(text, &d.nodes()[0]);
    let mut it = block.children_strict(&[], &["d"]).unwrap();
    assert!(matches!(
        it.next().unwrap(),
        Err(ConfigError::UnknownProperty { .. })
    ));
    let mut it = block.children_strict(&[], &["a", "d"]).unwrap();
    assert_eq!(it.next().unwrap().unwrap().name(), "a");
    assert_eq!(it.next().unwrap().unwrap().name(), "d");
}
