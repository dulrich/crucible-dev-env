//! The `theme { font "<family>" size=(pt)N; foreground "#rrggbb"; background "#rrggbb";
//! cursor "#rrggbb"; alpha 0.9; palette <0-15> "#rrggbb" }` block.

use crate::error::ConfigError;
use crate::model::Rgb;
use crate::reader::NodeReader;
use crate::theme::{Milli, ThemeConfig, ThemeFont};

/// The largest font size in points.
const MAX_FONT_PT: f64 = 200.0;

fn parse_font(child: &NodeReader<'_>) -> Result<ThemeFont, ConfigError> {
    child.allow_properties(&["size"])?;
    let family = child.nonempty_string_arg("a font family string", "argument")?;
    let size = match child.pt_prop("size")? {
        None => None,
        Some((pt, at)) => Some(
            Milli::from_f64(pt)
                .filter(|m| m.0 > 0 && pt <= MAX_FONT_PT)
                .ok_or_else(|| {
                    child.invalid_at(
                        at,
                        "property `size`",
                        format!("{pt} is outside (0, {MAX_FONT_PT}] points"),
                    )
                })?,
        ),
    };
    Ok(ThemeFont { family, size })
}

pub(super) fn parse_theme(node: NodeReader<'_>) -> Result<ThemeConfig, ConfigError> {
    let mut theme = ThemeConfig::default();
    for child in node.children_strict(&["palette"], &["font"])? {
        let child = child?;
        match child.name() {
            "font" => theme.font = Some(parse_font(&child)?),
            "foreground" => theme.foreground = Some(child.hex_arg()?),
            "background" => theme.background = Some(child.hex_arg()?),
            "cursor" => theme.cursor = Some(child.hex_arg()?),
            "alpha" => {
                let a = child.float_arg()?;
                theme.alpha =
                    Some(Milli::from_f64(a).filter(|m| m.0 <= 1000).ok_or_else(|| {
                        child.invalid("argument", format!("{a} is outside 0.0..=1.0"))
                    })?);
            }
            "palette" => {
                let (index, at, s) =
                    child.int_string_args(["an index 0-15", "a \"#rrggbb\" string"])?;
                let slot = usize::try_from(index)
                    .ok()
                    .filter(|i| *i < 16)
                    .ok_or_else(|| {
                        child.invalid_at(at, "argument 1", format!("{index} is outside 0..=15"))
                    })?;
                let colour = Rgb::from_hex(s).ok_or_else(|| {
                    child.invalid("argument 2", format!("`{s}` is not a #rrggbb colour"))
                })?;
                if theme.palette[slot].replace(colour).is_some() {
                    return Err(ConfigError::Duplicate {
                        at: child.at(),
                        name: format!("palette {slot}"),
                    });
                }
            }
            _ => return Err(child.unknown_node("theme")),
        }
    }
    Ok(theme)
}

#[cfg(test)]
mod tests {
    use super::super::{err_at, parse};
    use crate::error::ConfigError;
    use crate::model::{Config, Rgb};
    use crate::theme::{Milli, ThemeConfig, ThemeFont};

    #[test]
    fn absent_block_is_none_and_empty_block_is_empty_theme() {
        assert_eq!(Config::default().theme, None);
        assert_eq!(parse("").unwrap().theme, None);
        assert_eq!(
            parse("theme {\n}\n").unwrap().theme,
            Some(ThemeConfig::default())
        );
    }

    #[test]
    fn full_block_parses() {
        let t = parse(
            "theme {\n font \"JetBrains Mono\" size=(pt)11.5\n foreground \"#dcdccc\"\n \
             background \"#111111\"\n cursor \"#FF0080\"\n alpha 0.9\n palette 0 \"#242424\"\n \
             palette 15 \"#ffffff\"\n}\n",
        )
        .unwrap()
        .theme
        .unwrap();
        assert_eq!(
            t.font,
            Some(ThemeFont {
                family: "JetBrains Mono".into(),
                size: Some(Milli(11_500))
            })
        );
        assert_eq!(t.cursor, Some(Rgb(0xff, 0, 0x80)));
        assert_eq!(t.alpha, Some(Milli(900)));
        assert_eq!(t.palette[0], Some(Rgb(0x24, 0x24, 0x24)));
        assert_eq!(t.palette[15], Some(Rgb(0xff, 0xff, 0xff)));
        assert_eq!(t.palette[1], None);
        // Bare sizes are points too; size is optional; integer alpha is accepted.
        let t = parse("theme { font \"Mono\" size=12; alpha 1; }\n")
            .unwrap()
            .theme
            .unwrap();
        assert_eq!(t.font.unwrap().size, Some(Milli(12_000)));
        assert_eq!(t.alpha, Some(Milli(1000)));
        let t = parse("theme { font \"Mono\"; }\n").unwrap().theme.unwrap();
        assert_eq!(t.font.unwrap().size, None);
    }

    #[test]
    fn bad_values_are_located_errors() {
        for bad in [
            "alpha 1.01",
            "alpha -0.1",
            "alpha \"0.5\"",
            "foreground \"#12345\"",
            "background \"red\"",
            "cursor 123",
            "palette 16 \"#000000\"",
            "palette -1 \"#000000\"",
            "palette 1 \"#00000g\"",
            "palette \"1\" \"#000000\"",
            "font \"Mono\" size=(px)12",
            "font \"Mono\" size=0",
            "font \"Mono\" size=(pt)201",
            "font \"Mono\" size=\"12\"",
            "font \"\"",
        ] {
            let (err, at) = err_at(&format!("\ntheme {{\n {bad}\n}}\n"));
            assert!(
                matches!(
                    err,
                    ConfigError::InvalidValue { .. } | ConfigError::WrongType { .. }
                ),
                "{bad}: {err:?}"
            );
            assert_eq!(at.line, 3, "{bad}");
        }
    }

    #[test]
    fn repeats_and_unknowns_are_errors() {
        let (err, at) = err_at("theme {\n palette 1 \"#000000\"\n palette 1 \"#111111\"\n}\n");
        assert!(matches!(err, ConfigError::Duplicate { .. }), "{err:?}");
        assert_eq!(at.line, 3);
        let (err, _) = err_at("theme { alpha 0.5; alpha 0.6; }\n");
        assert!(matches!(err, ConfigError::Duplicate { .. }), "{err:?}");
        let (err, _) = err_at("theme { opacity 0.5; }\n");
        assert!(matches!(err, ConfigError::UnknownNode { .. }), "{err:?}");
        let (err, _) = err_at("theme { font \"Mono\" weight=1; }\n");
        assert!(
            matches!(err, ConfigError::UnknownProperty { .. }),
            "{err:?}"
        );
        let (err, _) = err_at("theme { }\ntheme { }\n");
        assert!(matches!(err, ConfigError::Duplicate { .. }), "{err:?}");
        let (err, _) = err_at("theme { palette 1; }\n");
        assert!(
            matches!(err, ConfigError::MissingArgument { .. }),
            "{err:?}"
        );
    }
}
