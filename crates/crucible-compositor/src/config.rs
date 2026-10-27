//! Applying the user's `crucible-config` [`Config`] to the running compositor.
//!
//! [`load_startup_config`] reads the config file once at startup (a parse error is logged with its
//! line/column and falls back to defaults). [`CrucibleState::apply_config`] swaps bindings, tiling,
//! terminal and focus-follows-mouse on a live state; it is the single entry point WP-5's reload
//! timer calls. The decision logic is in the pure [`plan_bindings`]/[`pick_terminal`] so it is
//! unit-tested without a compositor.

use crucible_config::{Config, ConfigError};
use crucible_keys::{BindError, Bindings};
use tracing::{error, info};

use crate::state::{Backend, CrucibleState};

/// Loads the config at [`crucible_config::config_path`] for startup. Never fails: a missing file or
/// unset `$HOME` is the defaults, and an invalid file logs an `error!` with its location and also
/// falls back to the defaults.
#[must_use]
pub fn load_startup_config() -> Config {
    let Some(path) = crucible_config::config_path() else {
        info!("neither XDG_CONFIG_HOME nor HOME is set; using the default config");
        return Config::default();
    };
    match crucible_config::load(&path) {
        Ok(config) => {
            info!(path = %path.display(), "config loaded");
            config
        }
        Err(err) => {
            log_config_error(&path, &err);
            Config::default()
        }
    }
}

/// Logs a config load error at `error!` (the message already carries line:column for parse errors).
pub fn log_config_error(path: &std::path::Path, err: &ConfigError) {
    error!(path = %path.display(), %err, "invalid config; falling back to defaults");
}

/// The bindings to use for `config`: rebuilt from its `binds`, or on a [`BindError`] the `current`
/// table kept and the error returned for logging.
#[must_use]
pub fn plan_bindings(current: &Bindings, config: &Config) -> (Bindings, Option<BindError>) {
    match Bindings::with_overrides(&config.binds) {
        Ok(next) => (next, None),
        Err(err) => (current.clone(), Some(err)),
    }
}

/// The terminal command to spawn: the configured one when set, else the keys crate's resolution.
#[must_use]
pub fn pick_terminal(configured: Option<&str>, fallback: String) -> String {
    configured.map_or(fallback, str::to_string)
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Applies `config` to the live compositor: rebuilds the chord table (a bad `binds` entry keeps
    /// the old table and logs), swaps the tiling and re-applies the layout, and stores the terminal
    /// command/exec flag and focus-follows-mouse.
    ///
    /// Public so WP-5's reload timer can call it; safe to call before any output exists.
    pub fn apply_config(&mut self, config: Config) {
        let (bindings, bind_error) = plan_bindings(&self.bindings, &config);
        if let Some(err) = bind_error {
            error!(%err, "invalid binds in config; keeping the previous bindings");
        }
        self.bindings = bindings;
        if config.tiling != self.config.tiling {
            // Class insets changed: forget the applied projection so every window is re-placed (and
            // re-configured) with the new geometry instead of diffing to nothing.
            self.projection = crucible_layout::Projection::default();
        }
        let mut hide_header = false;
        if config.header != self.config.header {
            // The reservation may have changed height: re-place every window.
            self.projection = crucible_layout::Projection::default();
            hide_header = config.header.mode != crucible_config::HeaderMode::Toggle;
        }
        // Load lazily (also on the first apply, when the header equals the default): only when
        // the strip can ever show and the font spec differs from what is loaded.
        if config.header.mode != crucible_config::HeaderMode::Off
            && self.header.font_loaded.as_deref() != Some(config.header.font.as_str())
        {
            self.header_text =
                crate::text::TextRenderer::load(config.header.font_path(), &config.header.font);
            self.header.font_loaded = Some(config.header.font.clone());
        }
        self.focus_follows_mouse = config.focus_follows_mouse;
        self.config = config;
        if hide_header {
            // Same path as `toggle-header` (resets the projection, re-applies the layout).
            self.set_header_visible(false);
        }
        self.apply_layout();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_keys::{Action, Keysym, Modifiers};

    fn sup() -> Modifiers {
        Modifiers {
            logo: true,
            ..Default::default()
        }
    }

    #[test]
    fn plan_bindings_applies_valid_overrides() {
        let config = Config {
            binds: vec![("Super+X".into(), "quit".into())],
            ..Config::default()
        };
        let (b, err) = plan_bindings(&Bindings::default(), &config);
        assert!(err.is_none());
        assert_eq!(b.get(sup(), Keysym::x), Some(&Action::Quit));
    }

    #[test]
    fn plan_bindings_keeps_the_old_table_on_error() {
        let current = Bindings::with_overrides(&[("Super+Y".into(), "quit".into())]).unwrap();
        let config = Config {
            binds: vec![("Super+X".into(), "bogus".into())],
            ..Config::default()
        };
        let (b, err) = plan_bindings(&current, &config);
        assert!(matches!(err, Some(BindError::BadAction { .. })));
        assert_eq!(b, current);
    }

    #[test]
    fn configured_terminal_wins_over_the_fallback() {
        assert_eq!(pick_terminal(Some("ghostty"), "foot".into()), "ghostty");
        assert_eq!(pick_terminal(None, "foot".into()), "foot");
    }
}
