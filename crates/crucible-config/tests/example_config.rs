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
}
