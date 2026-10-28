//! The config grammar: [`parse`] and one parser per block, built on [`NodeReader`].

use std::path::PathBuf;

use kdl::KdlDocument;

use crate::error::{ConfigError, Location};
use crate::model::{
    ClassTiling, Config, HEADER_SIZE_MAX, HEADER_SIZE_MIN, HeaderConfig, HeaderMode,
    LauncherConfig, OUTPUT_SCALE_MAX, OUTPUT_SCALE_MIN, OUTPUT_SCALE_STEPS, OutputConfig,
    OverlayConfig, OverlayStyle, PickerConfig, Rgb, TerminalConfig, TilingConfig, clamp_ring,
};
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
    for node in Children::new(text, doc.nodes(), &["output"]) {
        let node = node?;
        match node.name() {
            "terminal" => parse_terminal(node, &mut config.terminal)?,
            "tiling" => config.tiling = parse_tiling(node)?,
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
            "restore-session" => {
                node.no_properties()?;
                config.restore_session = node.bool_arg()?;
            }
            "picker" => config.picker = parse_picker(node)?,
            "overlay" => config.overlay = parse_overlay(node)?,
            "launcher" => config.launcher = parse_launcher(node)?,
            "header" => config.header = parse_header(node)?,
            "output" => {
                let (name, out) = parse_output(node)?;
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
                let cmd = node.string_arg("a command string")?;
                node.nonempty("command", cmd)?;
                config.lock_command = cmd.to_string();
            }
            _ => return Err(node.unknown_node("the top level")),
        }
    }
    Ok(config)
}

fn parse_terminal(node: NodeReader<'_>, out: &mut TerminalConfig) -> Result<(), ConfigError> {
    node.allow_properties(&["exec-flag"])?;
    let cmd = node.string_arg("a command string")?;
    node.nonempty("command", cmd)?;
    out.command = Some(cmd.to_string());
    if let Some((flag, _)) = node.string_prop("exec-flag", "a string")? {
        node.nonempty("property `exec-flag`", flag)?;
        out.exec_flag = flag.to_string();
    }
    Ok(())
}

/// One `terminal` / `app` line of `tiling`: `gap`, `ring` and (apps only) `fill`.
fn parse_class_tiling(
    node: NodeReader<'_>,
    out: &mut ClassTiling,
    allow_fill: bool,
) -> Result<(), ConfigError> {
    node.allow_properties(if allow_fill {
        &["gap", "ring", "fill"]
    } else {
        &["gap", "ring"]
    })?;
    node.no_args()?;
    if let Some(gap) = node.u32_prop("gap")? {
        out.gap = gap;
    }
    if let Some(ring) = node.u32_prop("ring")? {
        out.ring = ring;
    }
    if let Some((s, at)) = node.string_prop("fill", "a \"#rrggbb\" string")? {
        out.fill = Rgb::from_hex(s).ok_or_else(|| {
            node.invalid_at(
                at,
                "property `fill`",
                format!("`{s}` is not a #rrggbb colour"),
            )
        })?;
    }
    Ok(())
}

fn parse_tiling(node: NodeReader<'_>) -> Result<TilingConfig, ConfigError> {
    let mut out = TilingConfig::default();
    for child in node.children(&[])? {
        let child = child?;
        match child.name() {
            "terminal" => parse_class_tiling(child, &mut out.terminal, false)?,
            "app" => parse_class_tiling(child, &mut out.app, true)?,
            "terminal-args" => {
                child.no_properties()?;
                out.terminal_args = child.string_list()?;
            }
            _ => return Err(child.unknown_node("tiling")),
        }
    }
    for (class, tiling) in [("terminal", &mut out.terminal), ("app", &mut out.app)] {
        let (ring, clamped) = clamp_ring(tiling.gap, tiling.ring);
        if clamped {
            tracing::warn!(
                class,
                gap = tiling.gap,
                ring = tiling.ring,
                "tiling ring is larger than gap; clamping ring to gap"
            );
            tiling.ring = ring;
        }
    }
    Ok(out)
}

/// Parses the `picker` block into its `root` directories and the `slots` badge setting.
fn parse_picker(node: NodeReader<'_>) -> Result<PickerConfig, ConfigError> {
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
fn parse_launcher(node: NodeReader<'_>) -> Result<LauncherConfig, ConfigError> {
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

fn parse_overlay(node: NodeReader<'_>) -> Result<OverlayConfig, ConfigError> {
    let mut out = OverlayConfig::default();
    for child in node.children(&[])? {
        let child = child?;
        child.no_properties()?;
        match child.name() {
            "border" => out.border = child.bool_arg()?,
            "style" => {
                let s = child.string_arg("a style string")?;
                out.style = OverlayStyle::from_name(s).ok_or_else(|| {
                    child.invalid(
                        "argument",
                        format!("`{s}` is not one of terminal, n1-dark, n1-med, n1-light"),
                    )
                })?;
            }
            "terminal-args" => out.terminal_args = child.string_list()?,
            _ => return Err(child.unknown_node("overlay")),
        }
    }
    Ok(out)
}

fn parse_header(node: NodeReader<'_>) -> Result<HeaderConfig, ConfigError> {
    let mut out = HeaderConfig::default();
    for child in node.children(&[])? {
        let child = child?;
        child.no_properties()?;
        match child.name() {
            "mode" => {
                let s = child.string_arg("a mode string")?;
                out.mode = HeaderMode::from_name(s).ok_or_else(|| {
                    child.invalid(
                        "argument",
                        format!("`{s}` is not one of off, always, toggle"),
                    )
                })?;
            }
            "font" => {
                let s = child.string_arg("a font path or family string")?;
                child.nonempty("font", s)?;
                out.font = s.to_string();
            }
            "size" => {
                let n = child.int_arg()?;
                out.size = u32::try_from(n)
                    .ok()
                    .filter(|n| (HEADER_SIZE_MIN..=HEADER_SIZE_MAX).contains(n))
                    .ok_or_else(|| {
                        child.invalid(
                            "argument",
                            format!("{n} is outside {HEADER_SIZE_MIN}..={HEADER_SIZE_MAX}"),
                        )
                    })?;
            }
            "slots" => out.slots = child.bool_arg()?,
            _ => return Err(child.unknown_node("header")),
        }
    }
    Ok(out)
}

fn parse_output(node: NodeReader<'_>) -> Result<(String, OutputConfig), ConfigError> {
    node.no_properties()?;
    let name = node.string_arg("a connector name string")?;
    node.nonempty("connector name", name)?;
    let mut out = OutputConfig::default();
    for child in node.children_keeping_args(&[])? {
        let child = child?;
        child.no_properties()?;
        match child.name() {
            "scale" => {
                let n = child.float_arg()?;
                if !(OUTPUT_SCALE_MIN..=OUTPUT_SCALE_MAX).contains(&n) {
                    return Err(child.invalid(
                        "argument",
                        format!("{n} is outside {OUTPUT_SCALE_MIN}..={OUTPUT_SCALE_MAX}"),
                    ));
                }
                let steps = (n * f64::from(OUTPUT_SCALE_STEPS)).round();
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                {
                    out.scale_120 = steps as u32;
                }
            }
            _ => return Err(child.unknown_node("output")),
        }
    }
    Ok((name.to_string(), out))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const FULL: &str = r##"
terminal "/tmp/cde/ghostty-cde" exec-flag="-e"
tiling {
    terminal gap=4 ring=2
    app gap=3 ring=1 fill="#102030"
    terminal-args "--window-padding-x={gap}" "--b"
}
focus-follows-mouse #false
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
    fn partial_files_keep_other_defaults() {
        let c = parse("tiling {\n    terminal gap=0 ring=0\n}\n").unwrap();
        assert_eq!(c.tiling.terminal.gap, 0);
        assert_eq!((c.tiling.app.gap, c.tiling.app.ring), (0, 0));
        assert!(c.focus_follows_mouse);
        let c = parse("terminal \"alacritty\"\n").unwrap();
        assert_eq!(c.terminal.exec_flag, "-e");
        let c = parse("terminal \"wezterm\" exec-flag=\"start\"\n").unwrap();
        assert_eq!(c.terminal.exec_flag, "start");
    }

    #[test]
    fn output_scale_parses_float_and_integer_and_quantizes() {
        let c =
            parse("output \"DP-1\" { scale 1.4167; }\noutput \"HDMI-A-1\" { scale 2; }\n").unwrap();
        assert_eq!(c.outputs["DP-1"].scale_120, 170);
        assert_eq!(c.outputs["HDMI-A-1"].scale_120, 240);
        assert!((c.output_scale("DP-1") - 170.0 / 120.0).abs() < 1e-12);
        assert!((c.output_scale("HDMI-A-1") - 2.0).abs() < 1e-12);
        assert!((c.max_output_scale() - 2.0).abs() < 1e-12);
    }

    #[test]
    fn output_scale_unlisted_or_unspecified_is_one() {
        let c = parse("output \"DP-1\" { }\n").unwrap();
        assert_eq!(c.outputs["DP-1"].scale_120, 120);
        assert!((c.output_scale("DP-2") - 1.0).abs() < 1e-12);
        assert!((Config::default().max_output_scale() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn output_scale_range_is_inclusive_and_enforced() {
        assert_eq!(
            parse("output \"A\" { scale 0.5; }\n").unwrap().outputs["A"].scale_120,
            60
        );
        assert_eq!(
            parse("output \"A\" { scale 4; }\n").unwrap().outputs["A"].scale_120,
            480
        );
        for bad in ["0.49", "4.01", "0", "-1", "#nan"] {
            let (err, at) = err_at(&format!("output \"A\" {{ scale {bad}; }}\n"));
            assert!(matches!(err, ConfigError::InvalidValue { .. }), "{bad}");
            assert_eq!(at.line, 1);
        }
    }

    #[test]
    fn output_errors_duplicate_unknown_child_missing_name_wrong_type() {
        let (err, at) = err_at("output \"A\" { scale 1; }\noutput \"A\" { scale 2; }\n");
        assert!(matches!(err, ConfigError::Duplicate { .. }));
        assert_eq!(at.line, 2);
        let (err, _) = err_at("output \"A\" { mode 1; }\n");
        assert!(matches!(
            err,
            ConfigError::UnknownNode {
                context: "output",
                ..
            }
        ));
        let (err, _) = err_at("output { scale 1; }\n");
        assert!(matches!(err, ConfigError::MissingArgument { .. }));
        let (err, _) = err_at("output \"A\" { scale \"big\"; }\n");
        assert!(matches!(err, ConfigError::WrongType { .. }));
    }

    fn err_at(text: &str) -> (ConfigError, Location) {
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
    fn unknown_nodes_inside_blocks() {
        let (err, at) = err_at("picker {\n    dir \"/x\"\n}\n");
        assert!(matches!(&err, ConfigError::UnknownNode { context, .. } if *context == "picker"));
        assert_eq!(at.line, 2);
        let (err, _) = err_at("binds {\n    key \"Super+P\" \"quit\"\n}\n");
        assert!(matches!(&err, ConfigError::UnknownNode { context, .. } if *context == "binds"));
    }

    #[test]
    fn unknown_property() {
        let (err, at) = err_at("tiling {\n    terminal gap=1 middle=2\n}\n");
        assert!(
            matches!(&err, ConfigError::UnknownProperty { name, node, .. }
                if name == "middle" && node == "terminal"),
            "{err}"
        );
        assert_eq!(
            at,
            Location {
                line: 2,
                column: 20
            }
        );
        // `fill` is an app-only property.
        let (err, _) = err_at("tiling {\n    terminal fill=\"#000000\"\n}\n");
        assert!(matches!(err, ConfigError::UnknownProperty { .. }));
        let (err, _) = err_at("tiling {\n    window gap=1\n}\n");
        assert!(matches!(&err, ConfigError::UnknownNode { context, .. } if *context == "tiling"));
        let (err, _) = err_at("terminal \"foot\" flag=\"-e\"\n");
        assert!(matches!(err, ConfigError::UnknownProperty { .. }));
    }

    #[test]
    fn wrong_value_types() {
        let (err, _) = err_at("tiling {\n    app gap=\"big\"\n}\n");
        assert!(
            matches!(&err, ConfigError::WrongType { expected, .. } if *expected == "an integer")
        );
        let (err, _) = err_at("terminal 5\n");
        assert!(matches!(&err, ConfigError::WrongType { expected, .. } if *expected == "a string"));
        let (err, _) = err_at("focus-follows-mouse \"yes\"\n");
        assert!(matches!(&err, ConfigError::WrongType { .. }));
        let (err, _) = err_at("terminal \"foot\" exec-flag=1\n");
        assert!(matches!(&err, ConfigError::WrongType { what, .. } if what.contains("exec-flag")));
        let (err, _) = err_at("binds {\n    bind \"Super+P\" 3\n}\n");
        assert!(matches!(&err, ConfigError::WrongType { .. }));
    }

    #[test]
    fn out_of_range_and_empty_values() {
        let (err, _) = err_at("tiling {\n    terminal gap=-1\n}\n");
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
        let (err, _) = err_at("tiling {\n    app ring=99999999999\n}\n");
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
    fn ring_clamps_to_gap_in_parse() {
        let c = parse("tiling {\n    terminal gap=2 ring=9\n    app gap=1 ring=5\n}\n").unwrap();
        assert_eq!(c.tiling.terminal.ring, 2);
        assert_eq!(c.tiling.app.ring, 1);
    }

    #[test]
    fn errors_display_their_location() {
        let (err, _) = err_at("\n  wallpaper \"x\"\n");
        let msg = err.to_string();
        assert!(msg.starts_with("2:3:"), "{msg}");
    }

    #[test]
    fn overlay_defaults_when_absent() {
        let c = parse("").unwrap();
        assert!(c.overlay.border);
        assert_eq!(c.overlay.style, OverlayStyle::N1Med);
        assert!(c.overlay.terminal_args.is_empty());
    }

    #[test]
    fn overlay_parses_all_children() {
        let c = parse(
            "overlay {\n border #false\n style \"n1-light\"\n terminal-args \"--a=1\" \"--b\"\n}\n",
        )
        .unwrap();
        assert!(!c.overlay.border);
        assert_eq!(c.overlay.style, OverlayStyle::N1Light);
        assert_eq!(c.overlay.terminal_args, vec!["--a=1", "--b"]);
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
    fn header_defaults_and_modes() {
        let d = Config::default().header;
        assert_eq!(
            (d.mode, d.size, d.font.as_str()),
            (HeaderMode::Toggle, 14, "DejaVu Sans")
        );
        for (name, mode) in [
            ("off", HeaderMode::Off),
            ("always", HeaderMode::Always),
            ("toggle", HeaderMode::Toggle),
        ] {
            let c = parse(&format!("header {{\n mode \"{name}\"\n}}\n")).unwrap();
            assert_eq!(c.header.mode, mode);
        }
        // The old `alt` spelling is not an alias.
        for bad in ["sometimes", "alt"] {
            let err = parse(&format!("header {{\n mode \"{bad}\"\n}}\n")).unwrap_err();
            assert!(err.to_string().contains("off, always, toggle"), "{err}");
        }
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

    #[test]
    fn header_slots_setting() {
        assert!(!Config::default().header.slots);
        assert!(parse("header {\n slots #true\n}\n").unwrap().header.slots);
        assert!(!parse("header {\n slots #false\n}\n").unwrap().header.slots);
        let err = parse("header {\n slots \"yes\"\n}\n").unwrap_err();
        assert!(matches!(err, ConfigError::WrongType { .. }), "{err:?}");
    }

    #[test]
    fn header_size_bounds() {
        assert_eq!(parse("header {\n size 6\n}\n").unwrap().header.size, 6);
        assert_eq!(parse("header {\n size 96\n}\n").unwrap().header.size, 96);
        for bad in ["5", "97", "0", "-3"] {
            let err = parse(&format!("header {{\n size {bad}\n}}\n")).unwrap_err();
            assert!(
                matches!(err, ConfigError::InvalidValue { .. }),
                "{bad}: {err:?}"
            );
        }
        let err = parse("header {\n size 1.5\n}\n").unwrap_err();
        assert!(matches!(err, ConfigError::WrongType { .. }), "{err:?}");
    }

    #[test]
    fn header_font_path_versus_family() {
        let c = parse("header {\n font \"/usr/share/fonts/x.ttf\"\n}\n").unwrap();
        assert_eq!(
            c.header.font_path(),
            Some(Path::new("/usr/share/fonts/x.ttf"))
        );
        let c = parse("header {\n font \"Noto Sans\"\n}\n").unwrap();
        assert_eq!(c.header.font_path(), None);
        assert_eq!(c.header.font, "Noto Sans");
    }
}
