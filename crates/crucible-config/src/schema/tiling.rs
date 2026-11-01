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
    if let Some(gap) = node.u32_prop("gap")? {
        out.gap = gap;
    }
    if let Some(ring) = node.u32_prop("ring")? {
        out.ring = ring;
    }
    if let Some(inset) = node.u32_prop("ring-inset")? {
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
    if let Some(gap) = node.u32_prop("gap")? {
        out.gap = gap;
    }
    if let Some(ring) = node.u32_prop("ring")? {
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
    for child in node.children(&[])? {
        let child = child?;
        match child.name() {
            "terminal" => parse_terminal_tiling(child, &mut out.terminal)?,
            "app" => parse_app_tiling(child, &mut out.app)?,
            "terminal-args" => {
                child.no_properties()?;
                out.terminal_args = child.string_list()?;
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
    fn ring_clamps_to_gap_in_parse() {
        let c = parse("tiling {\n    terminal gap=2 ring=9\n    app gap=1 ring=5\n}\n").unwrap();
        assert_eq!(c.tiling.terminal.ring, 2);
        assert_eq!(c.tiling.app.ring, 1);
    }

    #[test]
    fn terminal_ring_inset_and_frame_parse_clamp_and_reject() {
        let c = parse("tiling {\n    terminal gap=9 ring=1 ring-inset=4 frame=\"#3a3a3a\"\n}\n")
            .unwrap();
        let t = c.tiling.terminal;
        assert_eq!((t.ring, t.ring_inset), (1, 4));
        assert_eq!(t.frame, Some(Rgb(0x3a, 0x3a, 0x3a)));
        let d = TilingConfig::default().terminal;
        assert_eq!((d.ring_inset, d.frame), (0, None));
        let c = parse("tiling {\n    terminal gap=6 ring=3 ring-inset=5\n}\n").unwrap();
        assert_eq!(c.tiling.terminal.ring_inset, 3);
        let (err, _) = err_at("tiling {\n    terminal frame=\"red\"\n}\n");
        assert!(matches!(err, ConfigError::InvalidValue { .. }), "{err:?}");
        for bad in ["app ring-inset=1", "app frame=\"#000000\""] {
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
            msg("tiling {\n    terminal gap=-1\n}\n"),
            "2:14: `terminal` property `gap`: -1 is outside 0..=4294967295"
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
            msg("tiling {\n    app ring-inset=1\n}\n"),
            "2:9: unknown property `ring-inset` on `app`"
        );
    }
}
