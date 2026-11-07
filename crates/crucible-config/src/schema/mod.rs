//! The config grammar: [`parse`] dispatches the top-level nodes to one parser per block, built on
//! [`NodeReader`]. `tiling`, `header` and `output` have their own modules; `overlay` also holds
//! the `picker` and `launcher` blocks (the TUIs the overlay hosts).

mod cursor;
mod header;
mod idle;
mod keyboard;
mod output;
mod overlay;
mod screenshot;
mod tiling;

use kdl::KdlDocument;

use crate::error::{ConfigError, Location};
use crate::model::{Config, TerminalConfig};
use crate::reader::{Children, NodeReader, locate};

/// Parses config text (KDL v2). Pure.
///
/// # Errors
/// A [`ConfigError`] with the line and column of the first problem found.
pub fn parse(text: &str) -> Result<Config, ConfigError> {
    let doc = KdlDocument::parse_v2(text).map_err(|err| {
        let (at, message) = err.diagnostics.first().map_or_else(
            || (Location { line: 1, column: 1 }, err.to_string()),
            |d| {
                (
                    locate(text, d.span.offset()),
                    d.message.clone().unwrap_or_else(|| err.to_string()),
                )
            },
        );
        ConfigError::Syntax { at, message }
    })?;

    let mut config = Config::default();
    for node in Children::new(text, doc.nodes(), &["output", "autostart"], None) {
        let node = node?;
        match node.name() {
            "terminal" => parse_terminal(node, &mut config.terminal)?,
            "tiling" => config.tiling = tiling::parse_tiling(node)?,
            "gaps" => {
                return Err(node.invalid(
                    "node",
                    "`gaps` was removed; use `tiling { terminal gap=… ring=…; app gap=… ring=… }`"
                        .to_string(),
                ));
            }
            "focus-follows-mouse" => {
                node.no_properties()?;
                config.focus_follows_mouse = node.bool_arg()?;
            }
            "confine-pointer-to-fullscreen" => {
                node.no_properties()?;
                config.confine_pointer_to_fullscreen = node.bool_arg()?;
            }
            "restore-session" => {
                node.no_properties()?;
                config.restore_session = node.bool_arg()?;
            }
            "picker" => config.picker = overlay::parse_picker(node)?,
            "overlay" => config.overlay = overlay::parse_overlay(node)?,
            "launcher" => config.launcher = overlay::parse_launcher(node)?,
            "header" => config.header = header::parse_header(node)?,
            "keyboard" => config.keyboard = keyboard::parse_keyboard(node)?,
            "cursor" => config.cursor = cursor::parse_cursor(node)?,
            "screenshot" => config.screenshot = screenshot::parse_screenshot(node)?,
            "idle" => config.idle = idle::parse_idle(node)?,
            "output" => {
                let (name, out) = output::parse_output(node)?;
                if config.outputs.insert(name.clone(), out).is_some() {
                    return Err(ConfigError::Duplicate {
                        at: node.at(),
                        name: format!("output \"{name}\""),
                    });
                }
            }
            "binds" => config.binds = parse_binds(node)?,
            "lock-command" => {
                node.no_properties()?;
                config.lock_command = node.nonempty_string_arg("a command string", "command")?;
            }
            "autostart" => {
                node.no_properties()?;
                config
                    .autostart
                    .push(node.nonempty_string_arg("a command string", "command")?);
            }
            _ => return Err(node.unknown_node("the top level")),
        }
    }
    Ok(config)
}

fn parse_terminal(node: NodeReader<'_>, out: &mut TerminalConfig) -> Result<(), ConfigError> {
    node.allow_properties(&["exec-flag"])?;
    out.command = Some(node.nonempty_string_arg("a command string", "command")?);
    if let Some((flag, _)) = node.string_prop("exec-flag", "a string")? {
        node.nonempty("property `exec-flag`", flag)?;
        out.exec_flag = flag.to_string();
    }
    Ok(())
}

fn parse_binds(node: NodeReader<'_>) -> Result<Vec<(String, String)>, ConfigError> {
    let mut binds = Vec::new();
    for child in node.children(&["bind"])? {
        let child = child?;
        if child.name() != "bind" {
            return Err(child.unknown_node("binds"));
        }
        child.no_properties()?;
        let a = child.string_args(&["a chord string", "an action string"])?;
        binds.push((a[0].to_string(), a[1].to_string()));
    }
    Ok(binds)
}

/// Test helper: the error `text` fails with and where.
#[cfg(test)]
pub(crate) fn err_at(text: &str) -> (ConfigError, Location) {
    let err = parse(text).unwrap_err();
    let at = match &err {
        ConfigError::Syntax { at, .. }
        | ConfigError::UnknownNode { at, .. }
        | ConfigError::UnknownProperty { at, .. }
        | ConfigError::UnexpectedArgument { at, .. }
        | ConfigError::MissingArgument { at, .. }
        | ConfigError::WrongType { at, .. }
        | ConfigError::InvalidValue { at, .. }
        | ConfigError::Duplicate { at, .. } => *at,
        ConfigError::Io { .. } => panic!("unexpected io error"),
    };
    (err, at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Rgb;
    use std::path::PathBuf;

    const FULL: &str = r##"
terminal "/tmp/cde/ghostty-cde" exec-flag="-e"
tiling {
    terminal gap=(px)4 ring=(px)2
    app gap=(px)3 ring=(px)1 fill="#102030"
    terminal-args "--window-padding-x={gap}" "--b"
}
focus-follows-mouse #false
confine-pointer-to-fullscreen #false
picker {
    root "<code-root>"
    root "/srv/work"
}
binds {
    bind "Super+P" "open-picker"
    bind "Super+Backslash" "split right"
    bind "Super+Shift+Q" "none"
}
"##;

    #[test]
    fn empty_text_is_default() {
        assert_eq!(parse("").unwrap(), Config::default());
    }

    #[test]
    fn lock_command_defaults_to_swaylock_and_rejects_empty_or_duplicate() {
        assert_eq!(Config::default().lock_command, "swaylock");
        let c = parse("lock-command \"swaylock -f -c 000000\"\n").unwrap();
        assert_eq!(c.lock_command, "swaylock -f -c 000000");
        let (err, _) = err_at("lock-command \"  \"\n");
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
        let (err, _) = err_at("lock-command \"a\"\nlock-command \"b\"\n");
        assert!(matches!(err, ConfigError::Duplicate { .. }));
    }

    #[test]
    fn autostart_repeats_in_order_and_rejects_empty_or_extra_args() {
        assert!(Config::default().autostart.is_empty());
        let c = parse("autostart \"b; c\"\nautostart \"a\"\nautostart \"b; c\"\n").unwrap();
        assert_eq!(c.autostart, vec!["b; c", "a", "b; c"]);
        let (err, _) = err_at("autostart \"  \"\n");
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
        let (err, at) = err_at("autostart \"a\" \"b\"\n");
        assert!(!matches!(err, ConfigError::InvalidValue { .. }));
        assert_eq!(at.line, 1);
    }

    #[test]
    fn restore_session_defaults_on_parses_and_rejects_non_booleans() {
        assert!(Config::default().restore_session);
        assert!(!parse("restore-session #false\n").unwrap().restore_session);
        assert!(parse("restore-session #true\n").unwrap().restore_session);
        let (err, _) = err_at("restore-session \"no\"\n");
        assert!(matches!(err, ConfigError::WrongType { .. }));
        let (err, _) = err_at("restore-session\n");
        assert!(matches!(err, ConfigError::MissingArgument { .. }));
        let (err, _) = err_at("restore-session #true\nrestore-session #false\n");
        assert!(matches!(err, ConfigError::Duplicate { .. }));
    }

    #[test]
    fn full_example_parses() {
        let c = parse(FULL).unwrap();
        assert_eq!(c.terminal.command.as_deref(), Some("/tmp/cde/ghostty-cde"));
        assert_eq!(c.terminal.exec_flag, "-e");
        assert_eq!((c.tiling.terminal.gap, c.tiling.terminal.ring), (4, 2));
        assert_eq!((c.tiling.app.gap, c.tiling.app.ring), (3, 1));
        assert_eq!(c.tiling.app.fill, Rgb(0x10, 0x20, 0x30));
        assert_eq!(
            c.tiling.substituted_terminal_args(),
            vec!["--window-padding-x=4".to_string(), "--b".to_string()]
        );
        assert!(!c.focus_follows_mouse);
        assert!(!c.confine_pointer_to_fullscreen);
        assert_eq!(
            c.picker.roots,
            vec![
                PathBuf::from("<code-root>"),
                PathBuf::from("/srv/work")
            ]
        );
        assert_eq!(
            c.binds,
            vec![
                ("Super+P".to_string(), "open-picker".to_string()),
                ("Super+Backslash".to_string(), "split right".to_string()),
                ("Super+Shift+Q".to_string(), "none".to_string()),
            ]
        );
    }

    #[test]
    fn confine_pointer_to_fullscreen_defaults_on_and_parses_off() {
        assert!(parse("").unwrap().confine_pointer_to_fullscreen);
        assert!(
            !parse("confine-pointer-to-fullscreen #false\n")
                .unwrap()
                .confine_pointer_to_fullscreen
        );
    }

    #[test]
    fn partial_files_keep_other_defaults() {
        let c = parse("tiling {\n    terminal gap=(px)0 ring=(px)0\n}\n").unwrap();
        assert_eq!(c.tiling.terminal.gap, 0);
        assert_eq!((c.tiling.app.gap, c.tiling.app.ring), (0, 0));
        assert!(c.focus_follows_mouse);
        let c = parse("terminal \"alacritty\"\n").unwrap();
        assert_eq!(c.terminal.exec_flag, "-e");
        let c = parse("terminal \"wezterm\" exec-flag=\"start\"\n").unwrap();
        assert_eq!(c.terminal.exec_flag, "start");
    }

    #[test]
    fn syntax_error_has_location() {
        let (err, at) = err_at("tiling {\n}\nterminal \"unterminated\n");
        assert!(matches!(err, ConfigError::Syntax { .. }), "{err}");
        assert!(at.line >= 2, "{at}");
    }

    #[test]
    fn unknown_top_level_node() {
        let (err, at) = err_at("tiling {\n}\n\nwallpaper \"x.png\"\n");
        assert!(
            matches!(&err, ConfigError::UnknownNode { name, context, .. }
                if name == "wallpaper" && *context == "the top level"),
            "{err}"
        );
        assert_eq!(at, Location { line: 4, column: 1 });
    }

    #[test]
    fn unknown_nodes_inside_blocks() {
        let (err, at) = err_at("picker {\n    dir \"/x\"\n}\n");
        assert!(matches!(&err, ConfigError::UnknownNode { context, .. } if *context == "picker"));
        assert_eq!(at.line, 2);
        let (err, _) = err_at("binds {\n    key \"Super+P\" \"quit\"\n}\n");
        assert!(matches!(&err, ConfigError::UnknownNode { context, .. } if *context == "binds"));
    }

    #[test]
    fn wrong_value_types() {
        let (err, _) = err_at("tiling {\n    app gap=(px)\"big\"\n}\n");
        assert!(
            matches!(&err, ConfigError::WrongType { expected, .. } if *expected == "an integer")
        );
        let (err, _) = err_at("terminal 5\n");
        assert!(matches!(&err, ConfigError::WrongType { expected, .. } if *expected == "a string"));
        let (err, _) = err_at("focus-follows-mouse \"yes\"\n");
        assert!(matches!(&err, ConfigError::WrongType { .. }));
        let (err, _) = err_at("confine-pointer-to-fullscreen \"yes\"\n");
        assert!(matches!(&err, ConfigError::WrongType { .. }));
        let (err, _) = err_at("terminal \"foot\" exec-flag=1\n");
        assert!(matches!(&err, ConfigError::WrongType { what, .. } if what.contains("exec-flag")));
        let (err, _) = err_at("binds {\n    bind \"Super+P\" 3\n}\n");
        assert!(matches!(&err, ConfigError::WrongType { .. }));
    }

    #[test]
    fn out_of_range_and_empty_values() {
        let (err, _) = err_at("tiling {\n    terminal gap=(px)-1\n}\n");
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
        let (err, _) = err_at("tiling {\n    app ring=(px)99999999999\n}\n");
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
        let (err, _) = err_at("tiling {\n    app fill=\"red\"\n}\n");
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
        let (err, _) = err_at("terminal \"\"\n");
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
    }

    #[test]
    fn missing_and_extra_arguments() {
        let (err, _) = err_at("terminal\n");
        assert!(matches!(err, ConfigError::MissingArgument { .. }));
        let (err, _) = err_at("focus-follows-mouse\n");
        assert!(matches!(err, ConfigError::MissingArgument { .. }));
        let (err, _) = err_at("confine-pointer-to-fullscreen\n");
        assert!(matches!(err, ConfigError::MissingArgument { .. }));
        let (err, _) = err_at("binds {\n    bind \"Super+P\"\n}\n");
        assert!(matches!(err, ConfigError::MissingArgument { .. }));
        let (err, at) = err_at("terminal \"a\" \"b\"\n");
        assert!(matches!(err, ConfigError::UnexpectedArgument { .. }));
        assert_eq!(at.column, 14);
        let (err, _) = err_at("tiling 4\n");
        assert!(matches!(err, ConfigError::UnexpectedArgument { .. }));
    }

    #[test]
    fn duplicate_single_use_node() {
        let (err, at) = err_at("tiling {\n}\ntiling {\n}\n");
        assert!(matches!(&err, ConfigError::Duplicate { name, .. } if name == "tiling"));
        assert_eq!(at.line, 3);
    }

    #[test]
    fn gaps_node_is_an_error_naming_tiling() {
        let (err, at) = err_at("gaps inner=4\n");
        assert!(err.to_string().contains("tiling"), "{err}");
        assert_eq!(at.line, 1);
    }

    #[test]
    fn errors_display_their_location() {
        let (err, _) = err_at("\n  wallpaper \"x\"\n");
        let msg = err.to_string();
        assert!(msg.starts_with("2:3:"), "{msg}");
    }
}
