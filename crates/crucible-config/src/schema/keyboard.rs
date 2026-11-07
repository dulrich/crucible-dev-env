//! The `keyboard { layouts "us" "es" }` block.

use crate::error::ConfigError;
use crate::model::KeyboardConfig;
use crate::reader::NodeReader;

pub(super) fn parse_keyboard(node: NodeReader<'_>) -> Result<KeyboardConfig, ConfigError> {
    let mut out = KeyboardConfig::default();
    for child in node.children_strict(&[], &[])? {
        let child = child?;
        match child.name() {
            "layouts" => {
                let layouts = child.string_list()?;
                if layouts.is_empty() {
                    return Err(child.missing("at least one layout name string"));
                }
                for layout in &layouts {
                    child.nonempty("layout name", layout)?;
                    if layout.contains(',') {
                        return Err(child.invalid(
                            "layout name",
                            format!("`{layout}` contains `,` (list layouts as separate arguments)"),
                        ));
                    }
                }
                out.layouts = layouts;
            }
            _ => return Err(child.unknown_node("keyboard")),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::{err_at, parse};
    use crate::error::ConfigError;
    use crate::model::Config;

    #[test]
    fn keyboard_parses_layouts_and_defaults_to_us() {
        assert_eq!(Config::default().keyboard.layouts, ["us"]);
        let c = parse("keyboard { layouts \"us\" \"es\"; }\n").unwrap();
        assert_eq!(c.keyboard.layouts, ["us", "es"]);
        assert_eq!(c.keyboard.xkb_layout(), "us,es");
        assert_eq!(c.keyboard.layout_index("es"), Some(1));
        assert_eq!(c.keyboard.layout_index("de"), None);
        // A block without `layouts` keeps the default.
        assert_eq!(parse("keyboard { }\n").unwrap().keyboard.layouts, ["us"]);
    }

    #[test]
    fn keyboard_rejects_empty_blank_comma_and_unknown() {
        let (err, at) = err_at("keyboard { layouts; }\n");
        assert!(
            matches!(err, ConfigError::MissingArgument { .. }),
            "{err:?}"
        );
        assert_eq!(at.line, 1);
        for bad in ["\"\"", "\"us,es\""] {
            let (err, at) = err_at(&format!("\n\nkeyboard {{ layouts {bad}; }}\n"));
            assert!(
                matches!(err, ConfigError::InvalidValue { .. }),
                "{bad}: {err:?}"
            );
            assert_eq!(at.line, 3);
        }
        let (err, _) = err_at("keyboard { variant \"x\"; }\n");
        assert!(matches!(err, ConfigError::UnknownNode { .. }));
    }
}
