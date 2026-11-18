//! Keeps `docs/config.example.kdl` honest: its live lines are exactly the built-in defaults, and
//! every slashdashed (`/-`) example in it is a valid setting once enabled.

use crucible_config::{Config, parse};

const EXAMPLE: &str = include_str!("../../../docs/config.example.kdl");

#[test]
fn example_live_lines_are_the_defaults() {
    let parsed = parse(EXAMPLE).expect("docs/config.example.kdl must parse");
    assert_eq!(
        parsed,
        Config::default(),
        "a live line in docs/config.example.kdl differs from the built-in default"
    );
}

#[test]
fn example_slashdashed_settings_are_valid_when_enabled() {
    let enabled = EXAMPLE.replace("/-", "");
    let parsed = parse(&enabled).expect("every `/-` example must be a valid setting");
    assert!(parsed.night_light.is_some());
    assert!(parsed.terminal.command.is_some());
    assert!(!parsed.binds.is_empty());
    assert_eq!(parsed.input_devices.len(), 1);
    assert!(parsed.input_devices[0].layer.is_some());
}

#[test]
fn docs_input_device_examples_parse() {
    let doc = include_str!("../../../docs/config.md");
    let blocks: Vec<&str> = doc
        .split("```kdl\n")
        .skip(1)
        .filter_map(|b| b.split("```").next())
        .filter(|b| b.contains("input-device \""))
        .collect();
    assert_eq!(blocks.len(), 2, "the starting and reversed tables");
    for block in blocks {
        let parsed = parse(block).expect("docs input-device example parses");
        let d = &parsed.input_devices[0];
        assert_eq!((d.vendor, d.product), (0x1532, 0x0207));
        assert!(d.maps.len() >= 15);
    }
}
