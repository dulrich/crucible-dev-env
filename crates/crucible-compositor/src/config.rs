//! Applying the user's `crucible-config` [`Config`] to the running compositor.
//!
//! [`load_startup_config`] reads the config file once at startup (a parse error is logged with its
//! line/column and falls back to defaults). [`CrucibleState::apply_config`] swaps bindings, tiling,
//! terminal and focus-follows-mouse on a live state; it is the single entry point the persist tick
//! calls on a config reload. The decision logic is in the pure [`plan_bindings`]/[`pick_terminal`] so it is
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

/// What a pre-warmed overlay host bakes in at spawn time: resolved terminal program, exec flag,
/// overlay terminal args, the launcher's TUI entries, and the configured output scales (the
/// hidden host is on no output, so it never sees a rescale and would open sized for the old
/// scale). A change in any of them invalidates the warm host.
pub type WarmKey = (String, String, Vec<String>, Vec<String>, Vec<(String, u32)>);

/// The [`WarmKey`] of `config` given the resolved `terminal` program.
#[must_use]
pub fn warm_key(config: &Config, terminal: String) -> WarmKey {
    let overlay_args = config.overlay.effective_terminal_args(&terminal);
    (
        terminal,
        config.terminal.exec_flag.clone(),
        overlay_args,
        config.launcher.tui.clone(),
        config
            .outputs
            .iter()
            .map(|(name, output)| (name.clone(), output.scale_120))
            .collect(),
    )
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
    /// Public so the persist tick's config reload can call it; safe to call before any output exists.
    pub fn apply_config(&mut self, config: Config) {
        let old_overlay_terminal = warm_key(&self.config, self.terminal_program());
        let (bindings, bind_error) = plan_bindings(&self.bindings, &config);
        if let Some(err) = bind_error {
            error!(%err, "invalid binds in config; keeping the previous bindings");
        }
        self.bindings = bindings;
        if config.tiling != self.config.tiling {
            // Class insets changed: forget the applied projection so every window is re-placed (and
            // re-configured) with the new geometry instead of diffing to nothing.
            self.invalidate_projection();
        }
        let mut hide_header = false;
        if config.header != self.config.header {
            // The reservation may have changed height: re-place every window.
            self.invalidate_projection();
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
        crate::xwayland::note_xwayland_scale_change(
            self.config.max_output_scale(),
            config.max_output_scale(),
        );
        self.config = config;
        self.apply_output_scales();
        if old_overlay_terminal != warm_key(&self.config, self.terminal_program()) {
            self.warm_config_changed();
        }
        if hide_header {
            // Same path as `toggle-header` (resets the projection, re-applies the layout).
            self.set_header_visible(false);
        }
        self.apply_layout();
    }

    /// Re-applies the configured scale to every mapped output; when any output's scale changed,
    /// reflows the outputs through the backend ([`Backend::outputs_changed`]) and re-places an open
    /// overlay. A scale change is a geometry change (plan 12 invariant).
    pub fn apply_output_scales(&mut self) {
        let mut changed = false;
        for output in self.space.outputs() {
            let wanted = configured_scale(&self.config, &output.name());
            if output.current_scale().fractional_scale() != wanted.fractional_scale() {
                info!(output = %output.name(), scale = wanted.fractional_scale(), "output scale");
                output.change_current_state(None, None, Some(wanted), None);
                changed = true;
            }
        }
        if changed {
            self.invalidate_projection();
            B::outputs_changed(self);
            self.replace_mapped_overlay();
            // Repaint every output so the next frame's walk sends the new preferred scales.
            let outputs: Vec<_> = self.space.outputs().cloned().collect();
            for output in &outputs {
                self.backend_data.reset_buffers(output);
            }
        }
    }
}

/// The scale `connector` gets from `config`: `Scale::Fractional` of its `output` block, 1 when
/// unlisted. Smithay derives the `wl_output` integer scale (ceil) and the xdg-output logical size
/// from it.
#[must_use]
pub fn configured_scale(config: &Config, connector: &str) -> smithay::output::Scale {
    let scale = config.output_scale(connector);
    if scale == 1.0 {
        // Exactly today's default output state when no `output` block applies.
        smithay::output::Scale::Integer(1)
    } else {
        smithay::output::Scale::Fractional(scale)
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
    fn warm_key_tracks_launcher_entries() {
        let a = Config::default();
        let b = Config {
            launcher: crucible_config::LauncherConfig {
                tui: vec!["k9s".into()],
            },
            ..Config::default()
        };
        assert_eq!(warm_key(&a, "t".into()), warm_key(&a.clone(), "t".into()));
        assert_ne!(warm_key(&a, "t".into()), warm_key(&b, "t".into()));
    }

    #[test]
    fn warm_key_carries_effective_overlay_args() {
        let c = Config::default();
        let defaults: Vec<String> = crucible_config::GHOSTTY_OVERLAY_DEFAULT_ARGS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert_eq!(warm_key(&c, "/tmp/cde/ghostty-cde".into()).2, defaults);
        assert!(warm_key(&c, "foot".into()).2.is_empty());
        let mut opt_out = c.clone();
        opt_out.overlay.terminal_args = Some(vec![]);
        assert_ne!(
            warm_key(&c, "ghostty".into()),
            warm_key(&opt_out, "ghostty".into())
        );
    }

    #[test]
    fn warm_key_tracks_output_scales() {
        let one = crucible_config::parse("output \"DP-1\" { scale 1; }\n").unwrap();
        let scaled = crucible_config::parse("output \"DP-1\" { scale 1.4167; }\n").unwrap();
        let again = crucible_config::parse("output \"DP-1\" { scale 1.4167; }\n").unwrap();
        // A rescale restarts the hidden overlay host; an unchanged scale keeps it warm.
        assert_ne!(warm_key(&one, "t".into()), warm_key(&scaled, "t".into()));
        assert_eq!(warm_key(&scaled, "t".into()), warm_key(&again, "t".into()));
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
    fn output_scale_resolves_by_connector_name() {
        let config = crucible_config::parse("output \"DP-1\" { scale 1.4167; }\n").unwrap();
        let dp1 = configured_scale(&config, "DP-1");
        assert!(matches!(dp1, smithay::output::Scale::Fractional(_)));
        assert_eq!(dp1.fractional_scale(), 170.0 / 120.0);
        assert_eq!(dp1.integer_scale(), 2);
        assert!(matches!(
            configured_scale(&config, "DP-3"),
            smithay::output::Scale::Integer(1)
        ));
        assert!(matches!(
            configured_scale(&Config::default(), "winit"),
            smithay::output::Scale::Integer(1)
        ));
    }

    #[test]
    fn configured_terminal_wins_over_the_fallback() {
        assert_eq!(pick_terminal(Some("ghostty"), "foot".into()), "ghostty");
        assert_eq!(pick_terminal(None, "foot".into()), "foot");
    }
}
