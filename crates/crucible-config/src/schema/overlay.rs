//! The `overlay` block, and the `picker` and `launcher` blocks of the TUIs it hosts.

use std::path::PathBuf;

use crate::error::ConfigError;
use crate::model::{LauncherConfig, PickerConfig};
use crate::overlay::{OverlayConfig, OverlayStyle};
use crate::reader::NodeReader;

/// Parses the `picker` block into its `root` directories and the `slots` badge setting.
pub(super) fn parse_picker(node: NodeReader<'_>) -> Result<PickerConfig, ConfigError> {
    let mut out = PickerConfig::default();
    for child in node.children(&["root"])? {
        let child = child?;
        match child.name() {
            "root" => {
                child.no_properties()?;
                let dir = child.string_arg("a directory string")?;
                child.nonempty("directory", dir)?;
                out.roots.push(PathBuf::from(dir));
            }
            "slots" => {
                child.no_properties()?;
                out.slots = child.bool_arg()?;
            }
            _ => return Err(child.unknown_node("picker")),
        }
    }
    Ok(out)
}

/// Parses `launcher { tui "name" "glob-*" … }` into the deduped entry list.
pub(super) fn parse_launcher(node: NodeReader<'_>) -> Result<LauncherConfig, ConfigError> {
    let mut out = LauncherConfig::default();
    for child in node.children(&["tui"])? {
        let child = child?;
        if child.name() != "tui" {
            return Err(child.unknown_node("launcher"));
        }
        child.no_properties()?;
        let entries = child.string_list()?;
        if entries.is_empty() {
            return Err(child.missing("a program name"));
        }
        for entry in entries {
            child.nonempty("program name", &entry)?;
            if entry.contains('/') || entry.contains(':') {
                return Err(child.invalid(
                    &format!("argument `{entry}`"),
                    "must be a bare program name or glob, without `/` or `:`".to_string(),
                ));
            }
            if !out.tui.contains(&entry) {
                out.tui.push(entry);
            }
        }
    }
    Ok(out)
}

pub(super) fn parse_overlay(node: NodeReader<'_>) -> Result<OverlayConfig, ConfigError> {
    let mut out = OverlayConfig::default();
    for child in node.children(&[])? {
        let child = child?;
        child.no_properties()?;
        match child.name() {
            "border" => out.border = child.bool_arg()?,
            "style" => out.style = child.enum_arg("a style string", &OverlayStyle::NAMES)?,
            "terminal-args" => out.terminal_args = Some(child.string_list()?),
            _ => return Err(child.unknown_node("overlay")),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::{err_at, parse};
    use crate::error::ConfigError;
    use crate::model::Config;
    use crate::overlay::OverlayStyle;
    use std::path::PathBuf;

    #[test]
    fn overlay_defaults_when_absent() {
        let c = parse("").unwrap();
        assert!(c.overlay.border);
        assert_eq!(c.overlay.style, OverlayStyle::N1Med);
        assert_eq!(c.overlay.terminal_args, None);
    }

    #[test]
    fn overlay_parses_all_children() {
        let c = parse(
            "overlay {\n border #false\n style \"n1-light\"\n terminal-args \"--a=1\" \"--b\"\n}\n",
        )
        .unwrap();
        assert!(!c.overlay.border);
        assert_eq!(c.overlay.style, OverlayStyle::N1Light);
        assert_eq!(
            c.overlay.terminal_args,
            Some(vec!["--a=1".to_string(), "--b".to_string()])
        );
    }

    #[test]
    fn overlay_empty_terminal_args_is_some_empty() {
        let c = parse("overlay {\n terminal-args\n}\n").unwrap();
        assert_eq!(c.overlay.terminal_args, Some(vec![]));
    }

    #[test]
    fn overlay_partial_keeps_other_defaults() {
        let c = parse("overlay {\n style \"terminal\"\n}\n").unwrap();
        assert!(c.overlay.border);
        assert_eq!(c.overlay.style, OverlayStyle::Terminal);
    }

    #[test]
    fn overlay_every_style_roundtrips() {
        for style in ["terminal", "n1-dark", "n1-med", "n1-light"] {
            let c = parse(&format!("overlay {{\n style \"{style}\"\n}}\n")).unwrap();
            assert_eq!(c.overlay.style.as_str(), style);
        }
    }

    #[test]
    fn overlay_unknown_style_is_error() {
        let err = parse("overlay {\n style \"N1-Med\"\n}\n").unwrap_err();
        assert!(matches!(err, ConfigError::InvalidValue { .. }), "{err:?}");
    }

    #[test]
    fn overlay_unknown_child_is_error() {
        let err = parse("overlay {\n frob 1\n}\n").unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::UnknownNode {
                    context: "overlay",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn overlay_border_must_be_bool() {
        let err = parse("overlay {\n border \"yes\"\n}\n").unwrap_err();
        assert!(matches!(err, ConfigError::WrongType { .. }), "{err:?}");
    }

    #[test]
    fn overlay_terminal_args_must_be_strings() {
        let err = parse("overlay {\n terminal-args 3\n}\n").unwrap_err();
        assert!(matches!(err, ConfigError::WrongType { .. }), "{err:?}");
    }

    #[test]
    fn launcher_block_parses_and_validates() {
        let c =
            parse("launcher {\n    tui \"codex-*\"\n    tui \"lazydocker\" \"k9s\" \"k9s\"\n}\n")
                .unwrap();
        assert_eq!(c.launcher.tui, vec!["codex-*", "lazydocker", "k9s"]);
        assert!(parse("").unwrap().launcher.tui.is_empty());
        for bad in [
            "launcher {\n    tui \"\"\n}\n",
            "launcher {\n    tui \"a/b\"\n}\n",
            "launcher {\n    tui \"a:b\"\n}\n",
            "launcher {\n    tui\n}\n",
        ] {
            let (err, _) = err_at(bad);
            assert!(
                matches!(
                    err,
                    ConfigError::InvalidValue { .. } | ConfigError::MissingArgument { .. }
                ),
                "{bad}: {err:?}"
            );
        }
        let (err, _) = err_at("launcher {\n    tui \"a\"\n}\nlauncher {\n}\n");
        assert!(matches!(err, ConfigError::Duplicate { .. }));
        let (err, _) = err_at("launcher {\n    app \"a\"\n}\n");
        assert!(matches!(&err, ConfigError::UnknownNode { context, .. } if *context == "launcher"));
    }

    #[test]
    fn picker_slots_setting() {
        assert!(!Config::default().picker.slots);
        assert!(!parse("picker {\n root \"/x\"\n}\n").unwrap().picker.slots);
        let c = parse("picker {\n slots #true\n root \"/x\"\n}\n").unwrap();
        assert!(c.picker.slots);
        assert_eq!(c.picker.roots, vec![PathBuf::from("/x")]);
        assert!(!parse("picker {\n slots #false\n}\n").unwrap().picker.slots);
        let err = parse("picker {\n slots \"yes\"\n}\n").unwrap_err();
        assert!(matches!(err, ConfigError::WrongType { .. }), "{err:?}");
        let err = parse("picker {\n slots #true\n slots #false\n}\n").unwrap_err();
        assert!(matches!(err, ConfigError::Duplicate { .. }), "{err:?}");
    }

    /// Characterization: the exact message and location of an unknown style.
    #[test]
    fn unknown_style_pins_message_and_location() {
        assert_eq!(
            parse("overlay {\n style \"bogus\"\n}\n")
                .unwrap_err()
                .to_string(),
            "2:2: `style` argument: `bogus` is not one of terminal, n1-dark, n1-med, n1-light"
        );
    }
}
