//! The `cursor { theme "Adwaita"; size (px)24 }` block.

use crate::error::ConfigError;
use crate::model::CursorConfig;
use crate::reader::NodeReader;

/// Largest accepted cursor size, in logical pixels.
const MAX_SIZE: u32 = 256;

pub(super) fn parse_cursor(node: NodeReader<'_>) -> Result<CursorConfig, ConfigError> {
    let mut out = CursorConfig::default();
    for child in node.children(&[])? {
        let child = child?;
        child.no_properties()?;
        match child.name() {
            "theme" => {
                let name = child.string_arg("a theme name")?;
                child.nonempty("theme", name)?;
                out.theme = Some(name.to_string());
            }
            "size" => out.size = Some(child.px_arg_in(1..=MAX_SIZE)?),
            _ => return Err(child.unknown_node("cursor")),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::{err_at, parse};
    use crate::error::ConfigError;
    use crate::model::{Config, CursorConfig};

    #[test]
    fn cursor_defaults_to_unset() {
        assert_eq!(Config::default().cursor, CursorConfig::default());
        assert_eq!(parse("").unwrap().cursor, CursorConfig::default());
        assert_eq!(
            parse("cursor { }\n").unwrap().cursor,
            CursorConfig::default()
        );
    }

    #[test]
    fn cursor_parses_both_children() {
        let c = parse("cursor { theme \"Adwaita\"; size (px)32; }\n").unwrap();
        assert_eq!(c.cursor.theme.as_deref(), Some("Adwaita"));
        assert_eq!(c.cursor.size, Some(32));
        let c = parse("cursor { size (px)256; }\n").unwrap();
        assert_eq!((c.cursor.theme, c.cursor.size), (None, Some(256)));
    }

    #[test]
    fn cursor_rejects_bad_values_with_a_line() {
        for bad in [
            "theme 5",
            "theme \"\"",
            "theme \"  \"",
            "size (px)0",
            "size (px)257",
            "size (px)-3",
            "size (px)1.5",
            "size \"24\"",
        ] {
            let (err, at) = err_at(&format!("\ncursor {{ {bad}; }}\n"));
            assert!(
                matches!(
                    err,
                    ConfigError::InvalidValue { .. } | ConfigError::WrongType { .. }
                ),
                "{bad}: {err:?}"
            );
            assert_eq!(at.line, 2, "{bad}");
        }
    }

    #[test]
    fn cursor_rejects_unknown_child_and_repeats() {
        let (err, _) = err_at("cursor { shape 5; }\n");
        assert!(matches!(err, ConfigError::UnknownNode { .. }), "{err:?}");
        let (err, _) = err_at("cursor { size (px)5; size (px)6; }\n");
        assert!(matches!(err, ConfigError::Duplicate { .. }), "{err:?}");
        let (err, _) = err_at("cursor { theme \"a\"; theme \"b\"; }\n");
        assert!(matches!(err, ConfigError::Duplicate { .. }), "{err:?}");
    }

    #[test]
    fn size_needs_px() {
        for bad in ["size 24", "size (pt)24", "size (em)24"] {
            let (err, _) = err_at(&format!("cursor {{ {bad}; }}\n"));
            assert!(
                matches!(err, ConfigError::InvalidValue { .. }),
                "{bad}: {err:?}"
            );
        }
    }
}
