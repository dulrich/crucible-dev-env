//! Configuration for Crucible Dev Env (CDE).
//!
//! CDE is configured by a declarative KDL file (config-file-only for the MVP, settings UI on the
//! roadmap) at `$XDG_CONFIG_HOME/cde/config.kdl` (fallback `~/.config/cde/config.kdl`). This crate
//! parses and validates it into a [`Config`]; a missing file is [`Config::default`], which equals
//! CDE's pre-config behaviour. An invalid file is a typed [`ConfigError`] carrying the line and
//! column of the offending node or entry.
//!
//! [`parse`] is pure; [`load`] adds exactly one file read. The crate knows nothing about chords or
//! actions: `binds` entries are carried as raw `(chord, action)` strings and validated by
//! `crucible-keys` (`Bindings::with_overrides`). Hot reload is the compositor's concern.
//! The full schema is documented in `docs/config.md`.
//!
//! Layout: `error` (typed errors), `model` (the parsed data), `overlay` (overlay style, palette
//! and terminal defaults), `reader` (a located view of one KDL node) and `schema` (the grammar
//! built on the reader).

mod error;
mod model;
mod overlay;
mod reader;
mod schema;
mod theme;

use std::path::{Path, PathBuf};

pub use error::{ConfigError, Location};
pub use model::{
    AppTiling, Config, CursorConfig, DEFAULT_DIVIDER_COLOR, DEFAULT_HEADER_FONT,
    HEADER_DIVIDER_INSET_MAX, HEADER_DIVIDER_MAX, HEADER_SIZE_MAX, HEADER_SIZE_MIN, HeaderConfig,
    HeaderMode, HeaderStart, IdleConfig, KeyboardConfig, LauncherConfig, Named, NightLightConfig,
    PickerConfig, Rgb, ScreenshotConfig, ScreenshotDestination, TerminalConfig, TerminalTiling,
    TilingConfig, clamp_ring, substitute_gap,
};
pub use overlay::{
    FOOT_UNBOUND_ACTIONS, GHOSTTY_OVERLAY_DEFAULT_ARGS, N1_DARK, N1_LIGHT, N1_MED, OverlayConfig,
    OverlayStyle, Palette, TerminalKind,
};
pub use schema::parse;
pub use theme::{
    Milli, ThemeConfig, ThemeFont, ThemeTarget, render as render_theme, theme_dir, theme_dir_from,
    theme_spawn_args,
};

/// Resolves the config path from the two environment values (`None` or empty = unset).
#[must_use]
pub fn config_path_from(xdg_config_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    fn nonempty(v: Option<&str>) -> Option<&str> {
        v.filter(|s| !s.is_empty())
    }
    if let Some(xdg) = nonempty(xdg_config_home) {
        return Some(Path::new(xdg).join("cde").join("config.kdl"));
    }
    nonempty(home).map(|h| Path::new(h).join(".config").join("cde").join("config.kdl"))
}

/// `$XDG_CONFIG_HOME/cde/config.kdl`, else `$HOME/.config/cde/config.kdl`; `None` when neither
/// variable is set.
#[must_use]
pub fn config_path() -> Option<PathBuf> {
    config_path_from(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// Expands a leading `~` (alone or `~/...`) to `home`. Anything else is returned unchanged
/// (literal): `~user/...`, a `~` that is not the first component, and any path when `home` is
/// `None` or empty.
#[must_use]
pub fn expand_tilde(path: &Path, home: Option<&str>) -> PathBuf {
    match (path.strip_prefix("~"), home) {
        (Ok(rest), Some(home)) if !home.is_empty() => {
            if rest.as_os_str().is_empty() {
                PathBuf::from(home)
            } else {
                Path::new(home).join(rest)
            }
        }
        _ => path.to_path_buf(),
    }
}

/// [`parse`], then `picker.roots` and `screenshot.directory` are resolved against `home` with
/// [`expand_tilde`], so no consumer ever sees a `~` the config meant as `$HOME`. `parse(text)` is
/// `parse_with_home(text, None)`.
///
/// # Errors
/// As [`parse`].
pub fn parse_with_home(text: &str, home: Option<&str>) -> Result<Config, ConfigError> {
    let mut config = parse(text)?;
    for root in &mut config.picker.roots {
        *root = expand_tilde(root, home);
    }
    config.screenshot.directory = expand_tilde(&config.screenshot.directory, home);
    Ok(config)
}

/// Reads and parses the config at `path`, resolving `~` against `$HOME` (read once); a missing
/// file is the default config, resolved the same way.
///
/// # Errors
/// [`ConfigError::Io`] for an unreadable file, otherwise as [`parse`].
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    load_with_home(path, std::env::var("HOME").ok().as_deref())
}

fn load_with_home(path: &Path, home: Option<&str>) -> Result<Config, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_with_home(&text, home),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => parse_with_home("", home),
        Err(source) => Err(ConfigError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_path_prefers_xdg_then_home() {
        assert_eq!(
            config_path_from(Some("/x"), Some("/h")),
            Some(PathBuf::from("/x/cde/config.kdl"))
        );
        assert_eq!(
            config_path_from(None, Some("/h")),
            Some(PathBuf::from("/h/.config/cde/config.kdl"))
        );
        assert_eq!(
            config_path_from(Some(""), Some("/h")),
            Some(PathBuf::from("/h/.config/cde/config.kdl"))
        );
        assert_eq!(config_path_from(None, None), None);
    }

    #[test]
    fn load_missing_file_is_default_and_bad_file_errors() {
        let dir = std::env::temp_dir().join(format!("cde-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("absent.kdl");
        assert_eq!(load_with_home(&missing, None).unwrap(), Config::default());

        let good = dir.join("good.kdl");
        std::fs::write(&good, "tiling {\n    terminal gap=(px)2 ring=(px)1\n}\n").unwrap();
        assert_eq!(load_with_home(&good, None).unwrap().tiling.terminal.gap, 2);

        let bad = dir.join("bad.kdl");
        std::fs::write(&bad, "nope\n").unwrap();
        assert!(matches!(
            load_with_home(&bad, None),
            Err(ConfigError::UnknownNode { .. })
        ));

        // A directory in place of the file is an I/O error, not "missing".
        assert!(matches!(
            load_with_home(&dir, None),
            Err(ConfigError::Io { .. })
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tilde_expands_only_as_a_leading_component() {
        let h = Some("/home/u");
        assert_eq!(
            expand_tilde(Path::new("~/Pictures/Screenshots"), h),
            Path::new("/home/u/Pictures/Screenshots")
        );
        assert_eq!(expand_tilde(Path::new("~"), h), Path::new("/home/u"));
        assert_eq!(
            expand_tilde(Path::new("/abs/~/x"), h),
            Path::new("/abs/~/x")
        );
        assert_eq!(
            expand_tilde(Path::new("~other/x"), h),
            Path::new("~other/x")
        );
        assert_eq!(expand_tilde(Path::new("~/x"), None), Path::new("~/x"));
        assert_eq!(expand_tilde(Path::new("~/x"), Some("")), Path::new("~/x"));
    }

    #[test]
    fn picker_roots_and_screenshot_directory_resolve_at_the_boundary() {
        let text =
            "picker { root \"~/code\"; root \"/abs\"; }\nscreenshot { directory \"~/shots\"; }\n";
        let c = parse_with_home(text, Some("/home/u")).unwrap();
        assert_eq!(
            c.picker.roots,
            [PathBuf::from("/home/u/code"), PathBuf::from("/abs")]
        );
        assert_eq!(c.screenshot.directory, PathBuf::from("/home/u/shots"));
        // The default screenshot directory resolves too.
        assert_eq!(
            parse_with_home("", Some("/home/u"))
                .unwrap()
                .screenshot
                .directory,
            PathBuf::from("/home/u/Pictures/Screenshots")
        );
        // No home: literal, exactly what `parse` returns.
        let literal = parse_with_home(text, None).unwrap();
        assert_eq!(literal.picker.roots[0], PathBuf::from("~/code"));
        assert_eq!(literal, parse(text).unwrap());
    }
}
