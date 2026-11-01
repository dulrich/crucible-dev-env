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
//! Layout: `error` (typed errors), `model` (the parsed data), `reader` (a located view of one
//! KDL node) and `schema` (the grammar built on the reader).

mod error;
mod model;
mod reader;
mod schema;

use std::path::{Path, PathBuf};

pub use error::{ConfigError, Location};
pub use model::{
    ClassTiling, Config, DEFAULT_DIVIDER_COLOR, DEFAULT_HEADER_FONT, FOOT_OVERLAY_DEFAULT_ARGS,
    GHOSTTY_OVERLAY_DEFAULT_ARGS, HEADER_DIVIDER_INSET_MAX, HEADER_DIVIDER_MAX, HEADER_SIZE_MAX,
    HEADER_SIZE_MIN, HeaderConfig, HeaderMode, LauncherConfig, OverlayConfig, OverlayStyle,
    PickerConfig, Rgb, TerminalConfig, TilingConfig, clamp_ring, substitute_gap,
};
pub use schema::parse;

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

/// Reads and parses the config at `path`; a missing file is `Ok(Config::default())`.
///
/// # Errors
/// [`ConfigError::Io`] for an unreadable file, otherwise as [`parse`].
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
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
        assert_eq!(load(&missing).unwrap(), Config::default());

        let good = dir.join("good.kdl");
        std::fs::write(&good, "tiling {\n    terminal gap=2 ring=1\n}\n").unwrap();
        assert_eq!(load(&good).unwrap().tiling.terminal.gap, 2);

        let bad = dir.join("bad.kdl");
        std::fs::write(&bad, "nope\n").unwrap();
        assert!(matches!(load(&bad), Err(ConfigError::UnknownNode { .. })));

        // A directory in place of the file is an I/O error, not "missing".
        assert!(matches!(load(&dir), Err(ConfigError::Io { .. })));
        std::fs::remove_dir_all(&dir).ok();
    }
}
