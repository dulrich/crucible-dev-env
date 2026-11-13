//! The `tiling { terminal …; app …; terminal-args … }` block.

use crate::error::ConfigError;
use crate::model::{AppTiling, TerminalTiling, TilingConfig, clamp_ring, clamp_ring_inset};
use crate::reader::NodeReader;

/// One `terminal` line of `tiling`: `gap`, `ring`, `ring-inset` and `frame`.
fn parse_terminal_tiling(
    node: NodeReader<'_>,
    out: &mut TerminalTiling,
) -> Result<(), ConfigError> {
    node.allow_properties(&["gap", "ring", "ring-inset", "frame"])?;
    node.no_args()?;
    if let Some(gap) = node.px_prop("gap")? {
        out.gap = gap;
    }
    if let Some(ring) = node.px_prop("ring")? {
        out.ring = ring;
    }
    if let Some(inset) = node.px_prop("ring-inset")? {
        out.ring_inset = inset;
    }
    if let Some(c) = node.hex_prop("frame")? {
        out.frame = Some(c);
    }
    Ok(())
}

/// One `app` line of `tiling`: `gap`, `ring` and `fill`.
fn parse_app_tiling(node: NodeReader<'_>, out: &mut AppTiling) -> Result<(), ConfigError> {
    node.allow_properties(&["gap", "ring", "fill"])?;
    node.no_args()?;
    if let Some(gap) = node.px_prop("gap")? {
        out.gap = gap;
    }
    if let Some(ring) = node.px_prop("ring")? {
        out.ring = ring;
    }
    if let Some(c) = node.hex_prop("fill")? {
        out.fill = c;
    }
    Ok(())
}

/// Clamps `ring` to `gap`, warning when it was reduced.
fn clamp_ring_warn(class: &str, gap: u32, ring: &mut u32) {
    let (clamped_ring, clamped) = clamp_ring(gap, *ring);
    if clamped {
        tracing::warn!(
            class,
            gap,
            ring = *ring,
            "tiling ring is larger than gap; clamping ring to gap"
        );
        *ring = clamped_ring;
    }
}

pub(super) fn parse_tiling(node: NodeReader<'_>) -> Result<TilingConfig, ConfigError> {
    let mut out = TilingConfig::default();
    for child in node.children(&["terminal-args"])? {
        let child = child?;
        match child.name() {
            "terminal" => parse_terminal_tiling(child, &mut out.terminal)?,
            "app" => parse_app_tiling(child, &mut out.app)?,
            "terminal-args" => {
                child.allow_properties(&["program"])?;
                let args = child.string_list()?;
                match child.string_prop("program", "a string")? {
                    None => out.terminal_args = Some(args),
                    Some((program, _)) => {
                        child.nonempty("property `program`", program)?;
                        out.program_terminal_args.retain(|(p, _)| p != program);
                        out.program_terminal_args.push((program.to_string(), args));
                    }
                }
            }
            _ => return Err(child.unknown_node("tiling")),
        }
    }
    let t = &mut out.terminal;
    clamp_ring_warn("terminal", t.gap, &mut t.ring);
    let (inset, clamped) = clamp_ring_inset(t.gap, t.ring, t.ring_inset);
    if clamped {
        tracing::warn!(
            class = "terminal",
            gap = t.gap,
            ring = t.ring,
            ring_inset = t.ring_inset,
            "tiling ring-inset plus ring is larger than gap; clamping ring-inset to gap - ring"
        );
        t.ring_inset = inset;
    }
    clamp_ring_warn("app", out.app.gap, &mut out.app.ring);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::{err_at, parse};
    use crate::error::{ConfigError, Location};
    use crate::model::Rgb;
    use crate::model::TilingConfig;

    #[test]
    fn unknown_property() {
        let (err, at) = err_at("tiling {\n    terminal gap=(px)1 middle=2\n}\n");
        assert!(
            matches!(&err, ConfigError::UnknownProperty { name, node, .. }
                if name == "middle" && node == "terminal"),
            "{err}"
        );
        assert_eq!(
            at,
            Location {
                line: 2,
                column: 24
            }
        );
        // `fill` is an app-only property.
        let (err, _) = err_at("tiling {\n    terminal fill=\"#000000\"\n}\n");
        assert!(matches!(err, ConfigError::UnknownProperty { .. }));
        let (err, _) = err_at("tiling {\n    window gap=(px)1\n}\n");
        assert!(matches!(&err, ConfigError::UnknownNode { context, .. } if *context == "tiling"));
        let (err, _) = err_at("terminal \"foot\" flag=\"-e\"\n");
        assert!(matches!(err, ConfigError::UnknownProperty { .. }));
    }

    #[test]
    fn ring_clamps_to_gap_in_parse() {
        let c =
            parse("tiling {\n    terminal gap=(px)2 ring=(px)9\n    app gap=(px)1 ring=(px)5\n}\n")
                .unwrap();
        assert_eq!(c.tiling.terminal.ring, 2);
        assert_eq!(c.tiling.app.ring, 1);
    }

    #[test]
    fn terminal_ring_inset_and_frame_parse_clamp_and_reject() {
        let c = parse(
            "tiling {\n    terminal gap=(px)9 ring=(px)1 ring-inset=(px)4 frame=\"#3a3a3a\"\n}\n",
        )
        .unwrap();
        let t = c.tiling.terminal;
        assert_eq!((t.ring, t.ring_inset), (1, 4));
        assert_eq!(t.frame, Some(Rgb(0x3a, 0x3a, 0x3a)));
        let d = TilingConfig::default().terminal;
        assert_eq!((d.ring_inset, d.frame), (0, None));
        let c = parse("tiling {\n    terminal gap=(px)6 ring=(px)3 ring-inset=(px)5\n}\n").unwrap();
        assert_eq!(c.tiling.terminal.ring_inset, 3);
        let (err, _) = err_at("tiling {\n    terminal frame=\"red\"\n}\n");
        assert!(matches!(err, ConfigError::InvalidValue { .. }), "{err:?}");
        for bad in ["app ring-inset=(px)1", "app frame=\"#000000\""] {
            let (err, _) = err_at(&format!("tiling {{\n    {bad}\n}}\n"));
            assert!(
                matches!(err, ConfigError::UnknownProperty { .. }),
                "{err:?}"
            );
        }
    }

    /// Characterization: the exact message and location of the property checks.
    #[test]
    fn property_errors_pin_message_and_location() {
        let msg = |text: &str| parse(text).unwrap_err().to_string();
        assert_eq!(
            msg("tiling {\n    app fill=\"red\"\n}\n"),
            "2:9: `app` property `fill`: `red` is not a #rrggbb colour"
        );
        assert_eq!(
            msg("tiling {\n    terminal frame=\"red\"\n}\n"),
            "2:14: `terminal` property `frame`: `red` is not a #rrggbb colour"
        );
        assert_eq!(
            msg("tiling {\n    terminal gap=(px)-1\n}\n"),
            "2:14: `terminal` property `gap`: (px)-1 is outside 0..=4294967295"
        );
        assert_eq!(
            msg("tiling {\n    terminal fill=\"#000000\"\n}\n"),
            "2:14: unknown property `fill` on `terminal`"
        );
        assert_eq!(
            msg("tiling {\n    app frame=\"#000000\"\n}\n"),
            "2:9: unknown property `frame` on `app`"
        );
        assert_eq!(
            msg("tiling {\n    app ring-inset=(px)1\n}\n"),
            "2:9: unknown property `ring-inset` on `app`"
        );
    }

    #[test]
    fn size_properties_need_px() {
        let msg = |text: &str| parse(text).unwrap_err().to_string();
        assert_eq!(
            msg("tiling {\n    terminal gap=6\n}\n"),
            "2:14: `terminal` property `gap`: `gap` needs a unit: (px)6"
        );
        assert_eq!(
            msg("tiling {\n    terminal gap=(pt)6\n}\n"),
            "2:14: `terminal` property `gap`: `gap` accepts only (px), e.g. (px)6"
        );
        assert_eq!(
            msg("tiling {\n    app ring=(em)2\n}\n"),
            "2:9: `app` property `ring`: `ring` has unknown unit (em); use (px)2"
        );
        assert!(matches!(
            parse("tiling {\n    terminal ring-inset=(px)1.5\n}\n").unwrap_err(),
            ConfigError::WrongType { .. }
        ));
    }

    fn args(text: &str) -> TilingConfig {
        parse(&format!("tiling {{\n{text}\n}}\n")).unwrap().tiling
    }

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn terminal_args_program_match_wins_over_program_less() {
        let t =
            args("terminal-args \"--a\"\nterminal-args program=\"urxvt\" \"-b\" \"{gap}\" \"-x\"");
        assert_eq!(
            t.effective_terminal_args("urxvt", true),
            v(&["-b", "6", "-x"])
        );
        assert_eq!(
            t.effective_terminal_args("urxvt", false),
            v(&["-b", "6", "-x"])
        );
        assert_eq!(t.effective_terminal_args("foot", true), v(&["--a"]));
    }

    #[test]
    fn terminal_args_program_less_applies_only_to_configured_terminal() {
        let t = args("terminal-args \"--a\"");
        assert_eq!(t.effective_terminal_args("kitty", true), v(&["--a"]));
        assert!(t.effective_terminal_args("kitty", false).is_empty());
        // Not the configured terminal: the built-in default still applies.
        assert_eq!(
            t.effective_terminal_args("foot", false),
            v(&["-o", "pad=6x6 center"])
        );
    }

    #[test]
    fn terminal_args_built_in_defaults_per_basename() {
        let t = TilingConfig::default();
        let pad = v(&["-o", "pad=6x6 center"]);
        for p in ["cterm", "foot"] {
            assert_eq!(t.effective_terminal_args(p, true), pad, "{p}");
            assert_eq!(t.effective_terminal_args(p, false), pad, "{p}");
        }
        for p in ["urxvt", "rxvt-unicode"] {
            assert_eq!(t.effective_terminal_args(p, true), v(&["-b", "6"]), "{p}");
        }
        for p in ["ghostty", "ghostty-cde"] {
            assert_eq!(
                t.effective_terminal_args(p, true),
                v(&[
                    "--window-padding-x=6",
                    "--window-padding-y=6",
                    "--window-padding-color=extend-always",
                    "--window-padding-balance=true",
                ]),
                "{p}"
            );
        }
    }

    #[test]
    fn terminal_args_unknown_basename_gets_none() {
        let t = TilingConfig::default();
        for p in ["footclient", "urxvtc", "kitty", ""] {
            assert!(t.effective_terminal_args(p, true).is_empty(), "{p}");
        }
    }

    #[test]
    fn terminal_args_empty_program_less_node_restores_pre_0_6() {
        let t = args("terminal-args");
        assert_eq!(t.terminal_args, Some(vec![]));
        assert!(t.effective_terminal_args("foot", true).is_empty());
        // Still defaulted for a program that is not the configured terminal.
        assert!(!t.effective_terminal_args("foot", false).is_empty());
    }

    #[test]
    fn terminal_args_empty_program_node_disables_that_default() {
        let t = args("terminal-args program=\"foot\"");
        assert!(t.effective_terminal_args("foot", true).is_empty());
        assert!(t.effective_terminal_args("foot", false).is_empty());
        assert_eq!(t.effective_terminal_args("urxvt", true), v(&["-b", "6"]));
    }

    #[test]
    fn terminal_args_repeated_nodes_keep_the_last() {
        let t = args("terminal-args \"--a\"\nterminal-args \"--b\"");
        assert_eq!(t.effective_terminal_args("kitty", true), v(&["--b"]));
        let t =
            args("terminal-args program=\"foot\" \"--a\"\nterminal-args program=\"foot\" \"--b\"");
        assert_eq!(t.effective_terminal_args("foot", true), v(&["--b"]));
        assert_eq!(t.program_terminal_args.len(), 1);
    }

    #[test]
    fn terminal_args_gap_is_substituted() {
        let t =
            args("terminal gap=(px)9\nterminal-args program=\"kitty\" \"{gap}\" \"{gap}x{gap}\"");
        assert_eq!(t.effective_terminal_args("kitty", true), v(&["9", "9x9"]));
        assert_eq!(
            t.effective_terminal_args("foot", true),
            v(&["-o", "pad=9x9 center"])
        );
    }

    #[test]
    fn terminal_args_property_validation() {
        let (err, _) = err_at("tiling {\n    terminal-args flag=\"x\"\n}\n");
        assert!(
            matches!(&err, ConfigError::UnknownProperty { name, node, .. }
            if name == "flag" && node == "terminal-args"),
            "{err}"
        );
        let (err, at) = err_at("tiling {\n    terminal-args program=\"\" \"--a\"\n}\n");
        assert!(matches!(&err, ConfigError::InvalidValue { .. }), "{err}");
        assert_eq!(at.line, 2);
        let (err, _) = err_at("tiling {\n    terminal-args program=1\n}\n");
        assert!(matches!(err, ConfigError::WrongType { .. }));
    }
}
