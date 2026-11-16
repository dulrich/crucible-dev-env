//! The `lock { style; background }` and `greeter { style; background; session }` blocks, and the
//! shared `style` / `background` child parsers.

use crate::error::ConfigError;
use crate::model::Rgb;
use crate::reader::NodeReader;
use crate::surface::{GreeterConfig, LockConfig, SurfaceBackground, SurfaceStyle};

/// `style "<terminal|n1-dark|n1-med|n1-light>"`.
pub(super) fn parse_style(child: &NodeReader<'_>) -> Result<SurfaceStyle, ConfigError> {
    child.enum_arg("a style string")
}

/// `background "stock"`, `background "user"` or `background "color" "#rrggbb"`.
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
            Rgb::from_hex(a[1])
                .map(SurfaceBackground::Color)
                .ok_or_else(|| {
                    child.invalid("argument 2", format!("`{}` is not a #rrggbb colour", a[1]))
                })
        }
        other => Err(child.invalid(
            "argument",
            format!("`{other}` is not one of color, stock, user"),
        )),
    }
}

pub(super) fn parse_lock(node: NodeReader<'_>) -> Result<LockConfig, ConfigError> {
    let mut out = LockConfig::default();
    for child in node.children_strict(&[], &[])? {
        let child = child?;
        match child.name() {
            "style" => out.style = parse_style(&child)?,
            "background" => out.background = parse_background(&child)?,
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
    use crate::overlay::OverlayStyle;
    use crate::surface::{GreeterConfig, LockConfig, SurfaceBackground};

    #[test]
    fn absent_blocks_are_none_and_empty_blocks_are_defaults() {
        assert_eq!(Config::default().lock, None);
        assert_eq!(Config::default().greeter, None);
        let c = parse("lock {\n}\ngreeter {\n}\n").unwrap();
        let lock = c.lock.unwrap();
        assert_eq!(lock, LockConfig::default());
        assert_eq!(
            (lock.style, lock.background),
            (OverlayStyle::N1Med, SurfaceBackground::Color(Rgb(0, 0, 0)))
        );
        let g = c.greeter.unwrap();
        assert_eq!(g, GreeterConfig::default());
        assert_eq!(
            (g.style, g.background, g.session.as_str()),
            (OverlayStyle::N1Med, SurfaceBackground::Stock, "cde-session")
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
            (OverlayStyle::N1Dark, SurfaceBackground::User)
        );
        let g = c.greeter.unwrap();
        assert_eq!(
            (g.style, g.background, g.session.as_str()),
            (
                OverlayStyle::Terminal,
                SurfaceBackground::Color(Rgb(0x10, 0x20, 0x30)),
                "sway"
            )
        );
        let c = parse("lock { background \"stock\"; }\n").unwrap();
        assert_eq!(c.lock.unwrap().background, SurfaceBackground::Stock);
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
    fn repeats_and_unknowns_are_errors() {
        for text in [
            "lock { style \"n1-med\"; style \"n1-dark\"; }\n",
            "greeter { session \"a\"; session \"b\"; }\n",
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
