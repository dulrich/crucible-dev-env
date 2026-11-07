//! The `screenshot { destination "clipboard"; directory "~/Pictures/Screenshots" }` block.

use std::path::PathBuf;

use crate::error::ConfigError;
use crate::model::{ScreenshotConfig, ScreenshotDestination};
use crate::reader::NodeReader;

pub(super) fn parse_screenshot(node: NodeReader<'_>) -> Result<ScreenshotConfig, ConfigError> {
    let mut out = ScreenshotConfig::default();
    for child in node.children(&[])? {
        let child = child?;
        child.no_properties()?;
        match child.name() {
            "destination" => {
                let value = child.string_arg("\"clipboard\", \"file\" or \"both\"")?;
                out.destination = match value {
                    "clipboard" => ScreenshotDestination::Clipboard,
                    "file" => ScreenshotDestination::File,
                    "both" => ScreenshotDestination::Both,
                    other => {
                        return Err(child.invalid(
                            "destination",
                            format!("`{other}` is not one of clipboard, file, both"),
                        ));
                    }
                };
            }
            "directory" => {
                let path = child.string_arg("a directory path")?;
                child.nonempty("directory", path)?;
                out.directory = PathBuf::from(path);
            }
            _ => return Err(child.unknown_node("screenshot")),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::{err_at, parse};
    use crate::error::ConfigError;
    use crate::model::{Config, ScreenshotConfig, ScreenshotDestination};
    use std::path::PathBuf;

    #[test]
    fn screenshot_defaults_to_clipboard_and_pictures() {
        let d = ScreenshotConfig::default();
        assert_eq!(d.destination, ScreenshotDestination::Clipboard);
        assert_eq!(d.directory, PathBuf::from("~/Pictures/Screenshots"));
        assert_eq!(Config::default().screenshot, d);
        assert_eq!(parse("").unwrap().screenshot, d);
        assert_eq!(parse("screenshot { }\n").unwrap().screenshot, d);
    }

    #[test]
    fn screenshot_parses_each_destination_and_the_directory() {
        for (text, want) in [
            ("clipboard", ScreenshotDestination::Clipboard),
            ("file", ScreenshotDestination::File),
            ("both", ScreenshotDestination::Both),
        ] {
            let c = parse(&format!("screenshot {{ destination \"{text}\"; }}\n")).unwrap();
            assert_eq!(c.screenshot.destination, want, "{text}");
        }
        let c = parse("screenshot { directory \"/tmp/shots\"; }\n").unwrap();
        assert_eq!(c.screenshot.directory, PathBuf::from("/tmp/shots"));
        assert_eq!(c.screenshot.destination, ScreenshotDestination::Clipboard);
    }

    #[test]
    fn screenshot_rejects_bad_values_with_a_line() {
        for bad in [
            "destination \"clip\"",
            "destination \"\"",
            "destination \"File\"",
            "destination 5",
            "directory \"\"",
            "directory \"  \"",
            "directory 5",
        ] {
            let (err, at) = err_at(&format!("\nscreenshot {{ {bad}; }}\n"));
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
    fn screenshot_rejects_unknown_child_and_repeats() {
        let (err, _) = err_at("screenshot { format \"png\"; }\n");
        assert!(matches!(err, ConfigError::UnknownNode { .. }), "{err:?}");
        let (err, _) = err_at("screenshot { destination \"file\"; destination \"both\"; }\n");
        assert!(matches!(err, ConfigError::Duplicate { .. }), "{err:?}");
    }
}
