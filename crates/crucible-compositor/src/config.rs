//! Applying the user's `crucible-config` [`Config`] to the running compositor.
//!
//! [`load_startup_config`] reads the config file once at startup (a parse error is logged with its
//! line/column and falls back to defaults). [`CrucibleState::apply_config`] swaps bindings, tiling,
//! terminal and focus-follows-mouse on a live state; it is the single entry point the persist tick
//! calls on a config reload. The decision logic is in the pure [`plan_bindings`]/[`pick_terminal`] so it is
//! unit-tested without a compositor.

use crucible_config::{Config, ConfigError};
use crucible_keys::{BindError, Bindings};
use tracing::{error, info, warn};

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarmKey {
    /// The resolved terminal program.
    pub terminal: String,
    /// The terminal's exec flag.
    pub exec_flag: String,
    /// The effective overlay terminal args for `terminal`.
    pub overlay_args: Vec<String>,
    /// The launcher's TUI entries.
    pub launcher_tui: Vec<String>,
    /// The configured `(output name, scale_120)` pairs.
    pub output_scales: Vec<(String, u32)>,
}

/// The arguments an overlay spawn of `program` puts before the exec flag: the effective
/// `overlay.terminal-args`, then the `theme`'s spawn arguments (ghostty's `--config-file=` into
/// `theme_dir`; nothing without a `theme`). **Pure.**
#[must_use]
pub fn overlay_terminal_args(
    config: &Config,
    program: &str,
    theme_dir: Option<&std::path::Path>,
) -> Vec<String> {
    let mut args = config.overlay.effective_terminal_args(program);
    args.extend(crucible_config::theme_spawn_args(
        config.theme.as_ref(),
        program,
        theme_dir,
    ));
    args
}

/// The [`WarmKey`] of `config` given the resolved `terminal` program and the theme fragment
/// directory (the theme's ghostty `--config-file=` is part of the overlay args).
#[must_use]
pub fn warm_key(config: &Config, terminal: String, theme_dir: Option<&std::path::Path>) -> WarmKey {
    let overlay_args = overlay_terminal_args(config, &terminal, theme_dir);
    WarmKey {
        terminal,
        exec_flag: config.terminal.exec_flag.clone(),
        overlay_args,
        launcher_tui: config.launcher.tui.clone(),
        output_scales: config
            .outputs
            .iter()
            .map(|(name, output)| (name.clone(), output.scale_120))
            .collect(),
    }
}

/// What [`ConfigDelta::between`] needs beyond the two configs: the live state the decisions are
/// made against and the environment the resolutions read, injected so the decisions stay pure.
pub struct LiveEnv<'a> {
    /// The cursor theme and size currently loaded (`None` before the first apply).
    pub cursor_loaded: Option<(&'a str, u32)>,
    /// Environment lookup for the cursor theme resolution (`XCURSOR_THEME` / `XCURSOR_SIZE`).
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// The terminal the keys crate resolves when the config names none.
    pub fallback_terminal: &'a str,
    /// The `theme` fragment directory ([`crucible_config::theme_dir`]).
    pub theme_dir: Option<&'a std::path::Path>,
}

/// The decisions of applying `new` over `old`, computed once by [`Self::between`] so
/// [`CrucibleState::apply_config`] acts on them as one delta.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigDelta {
    /// The tiling insets or the header changed: the applied projection must be forgotten so
    /// every window is re-placed (and re-configured) with the new geometry.
    pub relayout: bool,
    /// The `keyboard` layouts changed: the keymap is recompiled (the active group resets to the
    /// first layout).
    pub keyboard: bool,
    /// The `(old, new)` maximum configured output scale, for the XWayland scale note.
    pub max_scale: (f64, f64),
    /// The idle timers changed: the idle machinery is re-armed.
    pub idle: bool,
    /// The resolved `(theme, size)` to load, when it differs from what is loaded (startup:
    /// nothing loaded yet); `None` keeps the loaded theme.
    pub cursor: Option<(String, u32)>,
    /// The overlay host's warm key changed (terminal, exec flag, overlay args, launcher entries or
    /// output scales): the hidden warm overlay host restarts.
    pub warm: bool,
}

impl ConfigDelta {
    /// The delta from `old` to `new` against the live state in `live`.
    #[must_use]
    pub fn between(old: &Config, new: &Config, live: &LiveEnv<'_>) -> Self {
        let header_changed = old.header != new.header;
        let (theme, size) = crate::cursor::theme::resolve(&new.cursor, live.env);
        let cursor = (live.cursor_loaded != Some((theme.as_str(), size))).then_some((theme, size));
        let terminal = |c: &Config| {
            pick_terminal(
                c.terminal.command.as_deref(),
                live.fallback_terminal.to_string(),
            )
        };
        Self {
            relayout: old.tiling != new.tiling || header_changed,
            keyboard: old.keyboard != new.keyboard,
            max_scale: (old.max_output_scale(), new.max_output_scale()),
            idle: old.idle != new.idle,
            cursor,
            warm: warm_key(old, terminal(old), live.theme_dir)
                != warm_key(new, terminal(new), live.theme_dir),
        }
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
    /// Public so the persist tick's config reload can call it; safe to call before any output exists.
    pub fn apply_config(&mut self, config: Config) {
        let fallback_terminal = crucible_keys::terminal_command();
        let theme_dir = crucible_config::theme_dir();
        let delta = ConfigDelta::between(
            &self.config,
            &config,
            &LiveEnv {
                cursor_loaded: self.cursor.loaded(),
                env: &|k| std::env::var(k).ok(),
                fallback_terminal: &fallback_terminal,
                theme_dir: theme_dir.as_deref(),
            },
        );
        let (bindings, bind_error) = plan_bindings(&self.bindings, &config);
        if let Some(err) = bind_error {
            error!(%err, "invalid binds in config; keeping the previous bindings");
        }
        self.bindings = bindings;
        // Class insets or the header reservation changed: forget the applied projection so every
        // window is re-placed (and re-configured), once. The stored `toggle` state is kept
        // (it has no effect outside `toggle` mode), so toggle -> always -> toggle restores it.
        if delta.relayout {
            self.invalidate_projection();
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
        if delta.keyboard {
            self.reload_keyboard_layouts(&config);
        }
        crate::xwayland::note_xwayland_scale_change(delta.max_scale.0, delta.max_scale.1);
        // Build (startup: nothing loaded yet) or rebuild (reload) the cursor theme; this clears
        // the image cache.
        if let Some((theme, size)) = &delta.cursor {
            self.cursor.set_theme(theme, *size);
        }
        let night_light_changed = self.config.night_light != config.night_light;
        if let Some(theme) = &config.theme {
            self.write_terminal_theme(theme);
        }
        self.config = config;
        if delta.idle {
            self.idle.machine.reset();
        }
        self.apply_output_scales();
        if delta.warm {
            self.warm_config_changed();
        }
        if night_light_changed {
            self.evaluate_night_light();
        }
        self.apply_layout();
    }

    /// Writes the `theme` fragments (unchanged files are left alone). When the urxvt fragment
    /// changed and XWayland is already running, it is re-merged so new urxvt windows pick it up.
    /// Failures are logged at `warn!`; a config apply never fails on them.
    fn write_terminal_theme(&self, theme: &crucible_config::ThemeConfig) {
        let Some(dir) = crucible_config::theme_dir() else {
            warn!("neither XDG_CONFIG_HOME nor HOME is set; terminal theme files not written");
            return;
        };
        match crate::terminal_theme::write_fragments(theme, &dir) {
            Ok(changed) => {
                if !changed.is_empty() {
                    info!(dir = %dir.display(), ?changed, "terminal theme files written");
                }
                if changed.contains(&crucible_config::ThemeTarget::Urxvt)
                    && let Some(display) = self.xdisplay
                {
                    crate::xwayland::remerge_xresources(
                        display,
                        &dir.join(crucible_config::ThemeTarget::Urxvt.file_name()),
                    );
                }
            }
            Err(err) => warn!(dir = %dir.display(), %err, "cannot write the terminal theme files"),
        }
    }

    /// Recompiles the keymap from `config.keyboard.layouts`. A reload resets the active layout
    /// group to the first configured layout. A layout string xkb cannot compile is logged and
    /// the previous keymap is kept.
    fn reload_keyboard_layouts(&mut self, config: &Config) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let layout = config.keyboard.xkb_layout();
        let xkb = smithay::input::keyboard::XkbConfig {
            layout: &layout,
            ..Default::default()
        };
        match keyboard.set_xkb_config(self, xkb) {
            Ok(()) => info!(layout, "keyboard layouts applied"),
            Err(err) => {
                error!(%err, layout, "keyboard layouts did not compile; keeping the previous keymap")
            }
        }
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

    fn header_mode(mode: crucible_config::HeaderMode) -> Config {
        let mut c = Config::default();
        c.header.mode = mode;
        c
    }

    /// A live state with the default cursor loaded, no environment and `foot` as the fallback.
    fn live() -> LiveEnv<'static> {
        LiveEnv {
            cursor_loaded: Some(("default", 24)),
            env: &|_| None,
            fallback_terminal: "foot",
            theme_dir: Some(std::path::Path::new("/c/cde/terminal-theme")),
        }
    }

    fn delta(old: &Config, new: &Config) -> ConfigDelta {
        ConfigDelta::between(old, new, &live())
    }

    #[test]
    fn delta_no_change_is_inert() {
        let c = Config::default();
        let d = delta(&c, &c.clone());
        assert!(!d.relayout);
        assert!(!d.idle && !d.warm && d.cursor.is_none());
        assert_eq!(d.max_scale, (1.0, 1.0));
    }

    #[test]
    fn delta_tiling_only_relayouts() {
        let old = Config::default();
        let mut new = old.clone();
        new.tiling.app.gap = old.tiling.app.gap + 1;
        assert!(delta(&old, &new).relayout);
    }

    #[test]
    fn delta_header_mode_change_relayouts_in_both_directions() {
        use crucible_config::HeaderMode;
        for (from, to) in [
            (HeaderMode::Toggle, HeaderMode::Always),
            (HeaderMode::Toggle, HeaderMode::Off),
            (HeaderMode::Always, HeaderMode::Toggle),
        ] {
            let d = delta(&header_mode(from), &header_mode(to));
            assert!(d.relayout, "{from:?} -> {to:?}");
        }
    }

    #[test]
    fn delta_both_changed_and_scale_pair() {
        use crucible_config::HeaderMode;
        let old = header_mode(HeaderMode::Toggle);
        let mut new = header_mode(HeaderMode::Always);
        new.tiling.app.gap = old.tiling.app.gap + 1;
        new.outputs = crucible_config::parse("output \"DP-1\" { scale 2; }\n")
            .unwrap()
            .outputs;
        let d = delta(&old, &new);
        assert!(d.relayout);
        assert_eq!(d.max_scale, (1.0, 2.0));
    }

    #[test]
    fn delta_idle_follows_the_idle_timers_only() {
        let old = Config::default();
        let mut new = old.clone();
        new.idle.lock_after = Some(300);
        assert!(delta(&old, &new).idle);
        assert!(!delta(&new, &new.clone()).idle);
        assert!(!delta(&old, &header_mode(crucible_config::HeaderMode::Off)).idle);
    }

    #[test]
    fn delta_cursor_is_the_resolved_theme_when_it_differs_from_loaded() {
        let old = Config::default();
        let mut new = old.clone();
        // The loaded default theme resolves again: nothing to do.
        assert_eq!(delta(&old, &new).cursor, None);
        new.cursor.theme = Some("Adwaita".into());
        new.cursor.size = Some(32);
        assert_eq!(delta(&old, &new).cursor, Some(("Adwaita".to_string(), 32)));
        // Nothing loaded yet (startup): the resolved default is loaded.
        let startup = LiveEnv {
            cursor_loaded: None,
            ..live()
        };
        assert_eq!(
            ConfigDelta::between(&old, &old, &startup).cursor,
            Some(("default".to_string(), 24))
        );
        // The environment feeds the resolution.
        let env = |k: &str| (k == "XCURSOR_THEME").then(|| "Env".to_string());
        let with_env = LiveEnv {
            env: &env,
            ..live()
        };
        assert_eq!(
            ConfigDelta::between(&old, &old, &with_env).cursor,
            Some(("Env".to_string(), 24))
        );
    }

    #[test]
    fn delta_warm_tracks_the_resolved_terminal_and_overlay_inputs() {
        let old = Config::default();
        let mut new = old.clone();
        new.launcher.tui = vec!["k9s".into()];
        assert!(delta(&old, &new).warm);
        // A configured terminal equal to the fallback resolves to the same warm key.
        let mut same = old.clone();
        same.terminal.command = Some("foot".into());
        assert!(!delta(&old, &same).warm);
        let mut other = old.clone();
        other.terminal.command = Some("ghostty".into());
        assert!(delta(&old, &other).warm);
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
        assert_eq!(
            warm_key(&a, "t".into(), None),
            warm_key(&a.clone(), "t".into(), None)
        );
        assert_ne!(
            warm_key(&a, "t".into(), None),
            warm_key(&b, "t".into(), None)
        );
    }

    #[test]
    fn warm_key_carries_effective_overlay_args() {
        let c = Config::default();
        let defaults = crucible_config::TerminalKind::Ghostty.overlay_default_args();
        assert_eq!(
            warm_key(&c, "/tmp/cde/ghostty-cde".into(), None).overlay_args,
            defaults
        );
        let foot = crucible_config::TerminalKind::Foot.overlay_default_args();
        assert_eq!(
            warm_key(&c, "foot".into(), None).overlay_args[..foot.len()],
            foot[..]
        );
        assert!(
            warm_key(&c, "footclient".into(), None)
                .overlay_args
                .is_empty()
        );
        let mut opt_out = c.clone();
        opt_out.overlay.terminal_args = Some(vec![]);
        assert_ne!(
            warm_key(&c, "ghostty".into(), None),
            warm_key(&opt_out, "ghostty".into(), None)
        );
    }

    #[test]
    fn theme_adds_ghostty_config_file_to_overlay_args_and_restarts_the_warm_host() {
        let plain = Config::default();
        let themed = crucible_config::parse("theme { alpha 0.9; }\n").unwrap();
        let dir = std::path::Path::new("/c/cde/terminal-theme");
        let defaults = crucible_config::TerminalKind::Ghostty.overlay_default_args();
        // No theme: byte-identical to the overlay args alone.
        assert_eq!(
            overlay_terminal_args(&plain, "ghostty", Some(dir)),
            defaults
        );
        let mut want = defaults.clone();
        want.push("--config-file=/c/cde/terminal-theme/ghostty.conf".into());
        assert_eq!(overlay_terminal_args(&themed, "ghostty", Some(dir)), want);
        assert_eq!(
            overlay_terminal_args(&themed, "foot", Some(dir)),
            overlay_terminal_args(&plain, "foot", Some(dir))
        );
        // Adding the theme restarts a ghostty warm host; a theme edit does not.
        // The fallback terminal here is foot, whose overlay args a theme does not touch.
        assert!(!delta(&plain, &themed).warm);
        let mut on_ghostty = plain.clone();
        on_ghostty.terminal.command = Some("ghostty".into());
        let mut themed_ghostty = themed.clone();
        themed_ghostty.terminal.command = Some("ghostty".into());
        assert!(delta(&on_ghostty, &themed_ghostty).warm);
        let mut edited = themed_ghostty.clone();
        edited.theme.as_mut().unwrap().alpha = None;
        assert!(!delta(&themed_ghostty, &edited).warm);
    }

    #[test]
    fn warm_key_tracks_output_scales() {
        let one = crucible_config::parse("output \"DP-1\" { scale 1; }\n").unwrap();
        let scaled = crucible_config::parse("output \"DP-1\" { scale 1.4167; }\n").unwrap();
        let again = crucible_config::parse("output \"DP-1\" { scale 1.4167; }\n").unwrap();
        // A rescale restarts the hidden overlay host; an unchanged scale keeps it warm.
        assert_ne!(
            warm_key(&one, "t".into(), None),
            warm_key(&scaled, "t".into(), None)
        );
        assert_eq!(
            warm_key(&scaled, "t".into(), None),
            warm_key(&again, "t".into(), None)
        );
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
