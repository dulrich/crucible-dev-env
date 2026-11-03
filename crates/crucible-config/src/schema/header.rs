//! The `header { mode; start; font; size; divider …; slots }` block.

use crate::error::ConfigError;
use crate::model::{
    HEADER_DIVIDER_INSET_MAX, HEADER_DIVIDER_MAX, HEADER_SIZE_MAX, HEADER_SIZE_MIN, HeaderConfig,
    HeaderMode, HeaderStart,
};
use crate::reader::NodeReader;

pub(super) fn parse_header(node: NodeReader<'_>) -> Result<HeaderConfig, ConfigError> {
    let mut out = HeaderConfig::default();
    for child in node.children(&[])? {
        let child = child?;
        if child.name() != "divider" {
            child.no_properties()?;
        }
        match child.name() {
            "mode" => out.mode = child.enum_arg("a mode string", &HeaderMode::NAMES)?,
            "start" => out.start = child.enum_arg("a start string", &HeaderStart::NAMES)?,
            "font" => {
                let s = child.string_arg("a font path or family string")?;
                child.nonempty("font", s)?;
                out.font = s.to_string();
            }
            "size" => out.size = child.u32_arg_in(HEADER_SIZE_MIN..=HEADER_SIZE_MAX)?,
            "divider" => {
                child.allow_properties(&["inset", "color"])?;
                out.divider = child.u32_arg_in(0..=HEADER_DIVIDER_MAX)?;
                if let Some(inset) = child.u32_prop_in("inset", 0..=HEADER_DIVIDER_INSET_MAX)? {
                    out.divider_inset = inset;
                }
                if let Some(c) = child.hex_prop("color")? {
                    out.divider_color = c;
                }
            }
            "slots" => out.slots = child.bool_arg()?,
            _ => return Err(child.unknown_node("header")),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::parse;
    use crate::error::ConfigError;
    use crate::model::{Config, HeaderMode, Rgb};

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
        let c = parse("header {\n font \"Noto Sans\"\n}\n").unwrap();
        assert_eq!(
            (c.header.font.as_str(), c.header.font_path()),
            ("Noto Sans", None)
        );
        // The old `alt` spelling is not an alias.
        for bad in ["sometimes", "alt"] {
            let err = parse(&format!("header {{\n mode \"{bad}\"\n}}\n")).unwrap_err();
            assert!(err.to_string().contains("off, always, toggle"), "{err}");
        }
    }

    #[test]
    fn header_start_default_and_parse() {
        use crate::model::HeaderStart;
        assert_eq!(Config::default().header.start, HeaderStart::Hidden);
        for (name, start) in [
            ("shown", HeaderStart::Shown),
            ("hidden", HeaderStart::Hidden),
        ] {
            let c = parse(&format!("header {{\n start \"{name}\"\n}}\n")).unwrap();
            assert_eq!(c.header.start, start);
            assert_eq!(start.as_str(), name);
        }
        assert!(HeaderStart::Shown.is_shown() && !HeaderStart::Hidden.is_shown());
    }

    #[test]
    fn header_start_rejects_other_values() {
        for bad in ["\"sometimes\"", "\"Shown\"", "#true", "1"] {
            assert!(
                parse(&format!("header {{\n start {bad}\n}}\n")).is_err(),
                "{bad}"
            );
        }
        let err = parse("header {\n start \"x\"\n}\n").unwrap_err();
        assert_eq!(
            err.to_string(),
            "2:2: `start` argument: `x` is not one of shown, hidden"
        );
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
    fn header_divider_inset_color_defaults_and_parse() {
        let h = parse("").unwrap().header;
        assert_eq!(h.divider_inset, 0);
        assert_eq!(h.divider_color, Rgb(0xcd, 0xd0, 0xd6));
        let h = parse("header {\n divider 1 inset=6 color=\"#5c6370\"\n}\n")
            .unwrap()
            .header;
        assert_eq!((h.divider, h.divider_inset), (1, 6));
        assert_eq!(h.divider_color, Rgb(0x5c, 0x63, 0x70));
        // Properties are accepted (and inert) with a zero width.
        assert_eq!(
            parse("header {\n divider 0 inset=32\n}\n")
                .unwrap()
                .header
                .divider_inset,
            32
        );
    }

    #[test]
    fn header_divider_rejects_bad_properties() {
        for bad in [
            "divider 1 inset=33",
            "divider 1 inset=-1",
            "divider 1 color=\"red\"",
            "divider 1 color=\"#12345\"",
        ] {
            let err = parse(&format!("header {{\n {bad}\n}}\n")).unwrap_err();
            assert!(
                matches!(err, ConfigError::InvalidValue { .. }),
                "{bad}: {err:?}"
            );
        }
        for bad in [
            "divider 1 inset=1.5",
            "divider 1 inset=\"2\"",
            "divider 1 color=3",
        ] {
            let err = parse(&format!("header {{\n {bad}\n}}\n")).unwrap_err();
            assert!(
                matches!(err, ConfigError::WrongType { .. }),
                "{bad}: {err:?}"
            );
        }
        for bad in ["divider 1 foo=1", "size 14 x=1"] {
            assert!(parse(&format!("header {{\n {bad}\n}}\n")).is_err(), "{bad}");
        }
    }

    #[test]
    fn header_divider_default_and_bounds() {
        assert_eq!(parse("").unwrap().header.divider, 0);
        assert_eq!(
            parse("header {\n divider 2\n}\n").unwrap().header.divider,
            2
        );
        assert_eq!(
            parse("header {\n divider 0\n}\n").unwrap().header.divider,
            0
        );
        assert_eq!(
            parse("header {\n divider 16\n}\n").unwrap().header.divider,
            16
        );
        for bad in ["-1", "17"] {
            let err = parse(&format!("header {{\n divider {bad}\n}}\n")).unwrap_err();
            assert!(
                matches!(err, ConfigError::InvalidValue { .. }),
                "{bad}: {err:?}"
            );
        }
        for bad in ["1.5", "\"2\""] {
            let err = parse(&format!("header {{\n divider {bad}\n}}\n")).unwrap_err();
            assert!(
                matches!(err, ConfigError::WrongType { .. }),
                "{bad}: {err:?}"
            );
        }
    }

    /// Characterization: the exact message and location of each checked value.
    #[test]
    fn checked_value_errors_pin_message_and_location() {
        let msg = |text: &str| parse(text).unwrap_err().to_string();
        assert_eq!(
            msg("header {\n mode \"x\"\n}\n"),
            "2:2: `mode` argument: `x` is not one of off, always, toggle"
        );
        assert_eq!(
            msg("header {\n size -1\n}\n"),
            "2:2: `size` argument: -1 is outside 6..=96"
        );
        assert_eq!(
            msg("header {\n size 97\n}\n"),
            "2:2: `size` argument: 97 is outside 6..=96"
        );
        assert_eq!(
            msg("header {\n divider 17\n}\n"),
            "2:2: `divider` argument: 17 is outside 0..=16"
        );
        assert_eq!(
            msg("header {\n divider 1 inset=33\n}\n"),
            "2:2: `divider` property `inset`: 33 is outside 0..=32"
        );
        assert_eq!(
            msg("header {\n divider 1 inset=-1\n}\n"),
            "2:12: `divider` property `inset`: -1 is outside 0..=4294967295"
        );
        assert_eq!(
            msg("header {\n divider 1 color=\"red\"\n}\n"),
            "2:12: `divider` property `color`: `red` is not a #rrggbb colour"
        );
    }

    #[test]
    fn mode_names_roundtrip_in_table_order() {
        let names: Vec<&str> = HeaderMode::NAMES.iter().map(|(_, n)| *n).collect();
        assert_eq!(names, ["off", "always", "toggle"]);
        for (mode, name) in HeaderMode::NAMES {
            assert_eq!(mode.as_str(), name);
            assert_eq!(HeaderMode::from_name(name), Some(mode));
        }
    }
}
