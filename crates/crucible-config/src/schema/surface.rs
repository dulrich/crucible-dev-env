//! The `lock { style; background }` and `greeter { style; background; position; session }` blocks, and the
//! shared `style` / `background` child parsers.

use crate::error::ConfigError;
use crate::model::Rgb;
use crate::reader::NodeReader;
use crate::surface::{
    GreeterConfig, LockBackground, LockConfig, SurfaceAccent, SurfaceBackground, SurfaceStyle,
};

/// `style "<terminal|n1-dark|n1-med|n1-light>"`.
pub(super) fn parse_style(child: &NodeReader<'_>) -> Result<SurfaceStyle, ConfigError> {
    child.enum_arg("a style string")
}

/// `accent "style" | "theme" | "#rrggbb"`.
pub(super) fn parse_accent(child: &NodeReader<'_>) -> Result<SurfaceAccent, ConfigError> {
    match child.string_arg("an accent string")? {
        "style" => Ok(SurfaceAccent::Style),
        "theme" => Ok(SurfaceAccent::Theme),
        s => Rgb::from_hex(s).map(SurfaceAccent::Color).ok_or_else(|| {
            child.invalid(
                "argument",
                format!("`{s}` is not one of style, theme or a #rrggbb colour"),
            )
        }),
    }
}

/// `background "stock"`, `background "user"` or `background "color" "#rrggbb"` (the shared forms;
/// `glass` is lock-only and parsed by [`parse_lock_background`], so the greeter rejects it).
fn parse_background(child: &NodeReader<'_>) -> Result<SurfaceBackground, ConfigError> {
    let kind = child.string_args_min("a background kind string")?;
    match kind {
        "stock" | "user" => {
            child.string_args(&["a background kind string"])?;
            Ok(if kind == "stock" {
                SurfaceBackground::Stock
            } else {
                SurfaceBackground::User
            })
        }
        "color" => {
            let a = child.string_args(&["a background kind string", "a \"#rrggbb\" string"])?;
            child
                .hex_value("argument 2", a[1])
                .map(SurfaceBackground::Color)
        }
        other => Err(child.invalid(
            "argument",
            format!("`{other}` is not one of color, stock, user"),
        )),
    }
}

/// The `lock` background: `background "glass"` / `background "glass" alpha=<0.0..=1.0>` (lock-only,
/// never clamped), else the shared forms (which take no properties).
fn parse_lock_background(child: &NodeReader<'_>) -> Result<LockBackground, ConfigError> {
    if child.string_args_min("a background kind string")? != "glass" {
        child.no_properties()?;
        return parse_background(child).map(LockBackground::Surface);
    }
    child.allow_properties(&["alpha"])?;
    child.string_args(&["a background kind string"])?;
    Ok(LockBackground::Glass {
        alpha: child.alpha_prop("alpha")?,
    })
}

pub(super) fn parse_lock(node: NodeReader<'_>) -> Result<LockConfig, ConfigError> {
    let mut out = LockConfig::default();
    for child in node.children_strict(&[], &["background"])? {
        let child = child?;
        match child.name() {
            "style" => out.style = parse_style(&child)?,
            "accent" => out.accent = parse_accent(&child)?,
            "background" => out.background = parse_lock_background(&child)?,
            _ => return Err(child.unknown_node("lock")),
        }
    }
    Ok(out)
}

pub(super) fn parse_greeter(node: NodeReader<'_>) -> Result<GreeterConfig, ConfigError> {
    let mut out = GreeterConfig::default();
    for child in node.children_strict(&[], &[])? {
        let child = child?;
        match child.name() {
            "style" => out.style = parse_style(&child)?,
            "background" => out.background = parse_background(&child)?,
            "accent" => out.accent = parse_accent(&child)?,
            "position" => out.position = child.enum_arg("a position string")?,
            "session" => {
                out.session = child.nonempty_string_arg("a command string", "session")?;
            }
            _ => return Err(child.unknown_node("greeter")),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::{err_at, parse};
    use crate::error::ConfigError;
    use crate::model::{Config, Rgb};
    use crate::surface::SurfaceStyle;
    use crate::surface::{
        DEFAULT_LOCK_BACKGROUND, GreeterConfig, LockBackground, LockConfig, SurfaceBackground,
    };
    use crate::theme::{Alpha, ThemeConfig};

    #[test]
    fn background_colour_error_keeps_its_text_and_location() {
        let (err, _) = err_at("lock {\n   background \"color\" \"#zzzzzz\"\n}\n");
        assert_eq!(
            err.to_string(),
            "2:4: `background` argument 2: `#zzzzzz` is not a #rrggbb colour"
        );
    }

    #[test]
    fn absent_blocks_are_none_and_empty_blocks_are_defaults() {
        assert_eq!(Config::default().lock, None);
        assert_eq!(Config::default().greeter, None);
        let c = parse("lock {\n}\ngreeter {\n}\n").unwrap();
        let lock = c.lock.unwrap();
        assert_eq!(lock, LockConfig::default());
        assert_eq!(
            (lock.style, lock.background),
            (
                SurfaceStyle::N1Med,
                LockBackground::Surface(SurfaceBackground::Color(Rgb(0, 0, 0)))
            )
        );
        let g = c.greeter.unwrap();
        assert_eq!(g, GreeterConfig::default());
        assert_eq!(
            (g.style, g.background, g.session.as_str()),
            (
                SurfaceStyle::N1Dark,
                SurfaceBackground::Stock,
                "cde-session"
            )
        );
    }

    #[test]
    fn full_blocks_parse() {
        let c = parse(
            "lock {\n style \"n1-dark\"\n background \"user\"\n}\n\
             greeter {\n style \"terminal\"\n background \"color\" \"#102030\"\n \
             session \"sway\"\n}\n",
        )
        .unwrap();
        let lock = c.lock.unwrap();
        assert_eq!(
            (lock.style, lock.background),
            (
                SurfaceStyle::N1Dark,
                LockBackground::Surface(SurfaceBackground::User)
            )
        );
        let g = c.greeter.unwrap();
        assert_eq!(
            (g.style, g.background, g.session.as_str()),
            (
                SurfaceStyle::Terminal,
                SurfaceBackground::Color(Rgb(0x10, 0x20, 0x30)),
                "sway"
            )
        );
        let c = parse("lock { background \"stock\"; }\n").unwrap();
        assert_eq!(
            c.lock.unwrap().background,
            LockBackground::Surface(SurfaceBackground::Stock)
        );
    }

    fn lock_bg(body: &str) -> LockBackground {
        parse(&format!("lock {{\n {body}\n}}\n"))
            .unwrap()
            .lock
            .unwrap()
            .background
    }

    #[test]
    fn glass_parses_with_and_without_alpha() {
        assert_eq!(
            lock_bg("background \"glass\""),
            LockBackground::Glass { alpha: None }
        );
        assert_eq!(
            lock_bg("background \"glass\" alpha=0.5"),
            LockBackground::Glass {
                alpha: Alpha::from_milli(500)
            }
        );
        // Integer bounds are numbers too.
        assert_eq!(
            lock_bg("background \"glass\" alpha=1"),
            LockBackground::Glass {
                alpha: Some(Alpha::OPAQUE)
            }
        );
        assert_eq!(
            lock_bg("background \"glass\" alpha=0"),
            LockBackground::Glass {
                alpha: Alpha::from_milli(0)
            }
        );
    }

    #[test]
    fn glass_rejects_bad_alpha_and_properties_with_a_location() {
        for bad in [
            "background \"glass\" alpha=1.5",
            "background \"glass\" alpha=-0.1",
            "background \"glass\" alpha=\"0.5\"",
            "background \"glass\" alpha=nan",
            "background \"glass\" tint=0.5",
            "background \"glass\" \"extra\"",
            "background \"stock\" alpha=0.5",
            "background \"color\" \"#000000\" alpha=0.5",
        ] {
            let (err, at) = err_at(&format!("\nlock {{\n {bad}\n}}\n"));
            assert_eq!(at.line, 3, "{bad}: {err:?}");
        }
        let (err, _) = err_at("lock {\n   background \"glass\" alpha=1.5\n}\n");
        assert_eq!(
            err.to_string(),
            "2:23: `background` property `alpha`: 1.5 is outside 0.0..=1.0"
        );
    }

    #[test]
    fn glass_is_lock_only() {
        for bad in ["background \"glass\"", "background \"glass\" alpha=0.5"] {
            let (err, at) = err_at(&format!("\ngreeter {{\n {bad}\n}}\n"));
            assert_eq!(at.line, 3, "{bad}: {err:?}");
        }
    }

    #[test]
    fn glass_tint_resolves_alpha_then_theme_then_default() {
        let theme = |alpha: Option<u32>, bg: Option<Rgb>| ThemeConfig {
            alpha: alpha.and_then(Alpha::from_milli),
            background: bg,
            ..ThemeConfig::default()
        };
        let g = |alpha: Option<u32>| LockBackground::Glass {
            alpha: alpha.and_then(Alpha::from_milli),
        };
        // No theme: black at 0.8 (round(0.8 * 255) = 204).
        assert_eq!(g(None).glass_tint(None), Some((Rgb(0, 0, 0), 204)));
        // The theme's alpha and background.
        let t = theme(Some(500), Some(Rgb(1, 2, 3)));
        assert_eq!(g(None).glass_tint(Some(&t)), Some((Rgb(1, 2, 3), 128)));
        // An explicit alpha wins over the theme's.
        assert_eq!(
            g(Some(1000)).glass_tint(Some(&t)),
            Some((Rgb(1, 2, 3), 255))
        );
        assert_eq!(g(Some(0)).glass_tint(Some(&t)), Some((Rgb(1, 2, 3), 0)));
        // A theme without either falls back to black / 0.8.
        let t = theme(None, None);
        assert_eq!(g(None).glass_tint(Some(&t)), Some((Rgb(0, 0, 0), 204)));
        // Only glass has a tint.
        assert_eq!(DEFAULT_LOCK_BACKGROUND.glass_tint(None), None);
        assert!(g(None).is_glass() && !DEFAULT_LOCK_BACKGROUND.is_glass());
    }

    #[test]
    fn bad_values_are_located_errors() {
        for (block, bad) in [
            ("lock", "style \"N1-Med\""),
            ("lock", "style 1"),
            ("lock", "background \"image\""),
            ("lock", "background \"color\" \"#12345\""),
            ("lock", "background \"color\""),
            ("lock", "background \"stock\" \"#000000\""),
            ("lock", "background \"color\" 7"),
            ("lock", "background"),
            ("greeter", "session \"  \""),
            ("greeter", "session 3"),
            ("greeter", "position \"middle\""),
            ("greeter", "position"),
            ("greeter", "position 1"),
            ("greeter", "position \"lower\" \"upper\""),
            ("greeter", "background \"user\" \"x\""),
        ] {
            let (err, at) = err_at(&format!("\n{block} {{\n {bad}\n}}\n"));
            assert!(
                matches!(
                    err,
                    ConfigError::InvalidValue { .. }
                        | ConfigError::WrongType { .. }
                        | ConfigError::MissingArgument { .. }
                        | ConfigError::UnexpectedArgument { .. }
                ),
                "{bad}: {err:?}"
            );
            assert_eq!(at.line, 3, "{bad}");
        }
    }

    #[test]
    fn accent_parses_in_every_surface_block_and_rejects_bad_values() {
        use crate::surface::SurfaceAccent;
        let c = parse(
            "lock { accent \"#33CCcc\"; }\ngreeter { accent \"theme\"; }\n\
             overlay { accent \"style\"; }\nheader { accent \"#010203\"; }\ntheme { accent \"#0a0b0c\"; }\n",
        )
        .unwrap();
        assert_eq!(
            c.lock.unwrap().accent,
            SurfaceAccent::Color(Rgb(0x33, 0xcc, 0xcc))
        );
        assert_eq!(c.greeter.unwrap().accent, SurfaceAccent::Theme);
        assert_eq!(c.overlay.accent, SurfaceAccent::Style);
        assert_eq!(c.header.accent, SurfaceAccent::Color(Rgb(1, 2, 3)));
        assert_eq!(c.theme.unwrap().accent, Some(Rgb(0x0a, 0x0b, 0x0c)));
        let d = parse("lock { }\n").unwrap();
        assert_eq!(d.lock.unwrap().accent, SurfaceAccent::Style);
        assert_eq!(Config::default().overlay.accent, SurfaceAccent::Style);
        assert_eq!(Config::default().header.accent, SurfaceAccent::Style);
        for block in ["lock", "greeter", "overlay", "header"] {
            for bad in ["accent \"teal\"", "accent 1", "accent \"#12345\"", "accent"] {
                let (err, at) = err_at(&format!("\n{block} {{\n {bad}\n}}\n"));
                assert!(
                    matches!(
                        err,
                        ConfigError::InvalidValue { .. }
                            | ConfigError::WrongType { .. }
                            | ConfigError::MissingArgument { .. }
                    ),
                    "{block} {bad}: {err:?}"
                );
                assert_eq!(at.line, 3, "{block} {bad}");
            }
            let (err, _) = err_at(&format!(
                "{block} {{ accent \"style\"; accent \"theme\"; }}\n"
            ));
            assert!(matches!(err, ConfigError::Duplicate { .. }), "{block}");
        }
        for bad in [
            "accent \"theme\"",
            "accent 1",
            "accent \"#33cccc\"\n accent \"#33cccc\"",
        ] {
            assert!(parse(&format!("theme {{\n {bad}\n}}\n")).is_err(), "{bad}");
        }
    }

    #[test]
    fn repeats_and_unknowns_are_errors() {
        for text in [
            "lock { style \"n1-med\"; style \"n1-dark\"; }\n",
            "greeter { session \"a\"; session \"b\"; }\n",
            "greeter { position \"lower\"; position \"upper\"; }\n",
            "lock { }\nlock { }\n",
            "greeter { }\ngreeter { }\n",
        ] {
            let (err, _) = err_at(text);
            assert!(matches!(err, ConfigError::Duplicate { .. }), "{text}");
        }
        let (err, _) = err_at("lock { image \"x\"; }\n");
        assert!(matches!(&err, ConfigError::UnknownNode { context, .. } if *context == "lock"));
        let (err, _) = err_at("greeter { user \"x\"; }\n");
        assert!(matches!(&err, ConfigError::UnknownNode { context, .. } if *context == "greeter"));
        let (err, _) = err_at("greeter { session \"x\" user=1; }\n");
        assert!(
            matches!(err, ConfigError::UnknownProperty { .. }),
            "{err:?}"
        );
    }
}
