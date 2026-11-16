//! The generated swaylock config for the `lock` block: a pure generator ([`swaylock_config`]), the
//! path resolution, the argument rule for the `lock` action ([`locker_args`]) and the I/O edge
//! ([`write_swaylock_config`]).
//!
//! With `lock { .. }` set, every config apply (and every `lock` action, so a `user` background
//! follows wallpaper rotations) writes `$XDG_CONFIG_HOME/cde/swaylock/config` (atomic, skipped when
//! unchanged, a failure only warns); the `lock` action then passes it as `-C <file>` when the
//! locker is `swaylock`. Without the block nothing is written and the locker argv is untouched.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};

use crucible_config::{
    Config, LockConfig, Rgb, SurfaceBackground, ThemeConfig, config_path_from, resolve_palette,
};
use tracing::{info, warn};

/// The "wrong password" red.
const WRONG: Rgb = Rgb(0xe0, 0x4f, 0x5f);

/// `rrggbbaa` for swaylock.
fn rgba(c: Rgb, alpha: u8) -> String {
    format!("{:02x}{:02x}{:02x}{alpha:02x}", c.0, c.1, c.2)
}

/// `rrggbb` for swaylock's `color=`.
fn rgb(c: Rgb) -> String {
    format!("{:02x}{:02x}{:02x}", c.0, c.1, c.2)
}

/// The `image=` values for the wallpaper state text: one `<output>:<path>` per `<output>\t<path>`
/// line; a bare path line or a `*\t<path>` line gives a plain path. **Pure.**
fn wallpaper_images(state: &str) -> Vec<String> {
    state
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| match line.split_once('\t') {
            Some(("*", path)) if !path.is_empty() => Some(path.to_string()),
            Some((output, path)) if !output.is_empty() && !path.is_empty() => {
                Some(format!("{output}:{path}"))
            }
            Some(_) => None,
            None => Some(line.trim().to_string()),
        })
        .collect()
}

/// The swaylock config text for `lock`. `stock_path` is the installed stock PNG; `state_text` is
/// the wallpaper state file's content (`None` when missing or unreadable), used by the `user`
/// background. **Pure.**
#[must_use]
pub fn swaylock_config(
    lock: &LockConfig,
    theme: Option<&ThemeConfig>,
    stock_path: &Path,
    state_text: Option<&str>,
) -> String {
    let p = resolve_palette(lock.style, theme);
    let mut out = String::from("indicator-idle-visible\n");
    match lock.background {
        SurfaceBackground::Color(c) => {
            let _ = writeln!(out, "color={}", rgb(c));
        }
        SurfaceBackground::Stock => {
            let _ = writeln!(out, "color=000000");
            let _ = writeln!(out, "image={}", stock_path.display());
            let _ = writeln!(out, "scaling=fill");
        }
        SurfaceBackground::User => {
            let _ = writeln!(out, "color=000000");
            for image in state_text.map(wallpaper_images).unwrap_or_default() {
                let _ = writeln!(out, "image={image}");
            }
            let _ = writeln!(out, "scaling=fill");
        }
    }
    let lines = [
        ("inside-color", rgba(p.bg, 0xe6)),
        ("inside-clear-color", rgba(p.bg, 0xe6)),
        ("inside-caps-lock-color", rgba(p.bg, 0xe6)),
        ("inside-ver-color", rgba(p.bg, 0xe6)),
        ("inside-wrong-color", rgba(p.bg, 0xe6)),
        ("ring-color", rgba(p.line, 0xff)),
        ("ring-clear-color", rgba(p.text, 0xff)),
        ("ring-caps-lock-color", rgba(p.line, 0xff)),
        ("ring-ver-color", rgba(p.accent, 0xff)),
        ("ring-wrong-color", rgba(WRONG, 0xff)),
        ("line-color", "00000000".to_string()),
        ("separator-color", "00000000".to_string()),
        ("text-color", rgba(p.text, 0xff)),
        ("text-clear-color", rgba(p.text, 0xff)),
        ("text-caps-lock-color", rgba(p.text, 0xff)),
        ("text-ver-color", rgba(p.text, 0xff)),
        ("text-wrong-color", rgba(WRONG, 0xff)),
        ("key-hl-color", rgba(p.accent, 0xff)),
        ("bs-hl-color", rgba(WRONG, 0xff)),
    ];
    for (key, value) in lines {
        let _ = writeln!(out, "{key}={value}");
    }
    out
}

fn nonempty(v: Option<&str>) -> Option<&str> {
    v.filter(|s| !s.is_empty())
}

/// `$XDG_CONFIG_HOME/cde/swaylock/config`, else `$HOME/.config/cde/swaylock/config`. **Pure.**
#[must_use]
pub fn fragment_path_from(xdg_config_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    config_path_from(xdg_config_home, home)
        .and_then(|p| p.parent().map(|cde| cde.join("swaylock").join("config")))
}

/// `$XDG_DATA_HOME/cde/backgrounds/stock.png`, else `$HOME/.local/share/...`. **Pure.**
#[must_use]
pub fn stock_path_from(xdg_data_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let base = match nonempty(xdg_data_home) {
        Some(x) => PathBuf::from(x),
        None => Path::new(nonempty(home)?).join(".local").join("share"),
    };
    Some(base.join("cde").join("backgrounds").join("stock.png"))
}

/// `${XDG_STATE_HOME:-~/.local/state}/wallpaper-rotate/current`. **Pure.**
#[must_use]
pub fn wallpaper_state_path_from(
    xdg_state_home: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    let base = match nonempty(xdg_state_home) {
        Some(x) => PathBuf::from(x),
        None => Path::new(nonempty(home)?).join(".local").join("state"),
    };
    Some(base.join("wallpaper-rotate").join("current"))
}

/// [`fragment_path_from`] the process environment.
#[must_use]
pub fn fragment_path() -> Option<PathBuf> {
    fragment_path_from(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// The fragment text for `config`'s `lock` block from the process environment (reads the
/// wallpaper state for a `user` background); `None` without a `lock` block.
#[must_use]
pub fn render_for_env(config: &Config) -> Option<String> {
    let lock = config.lock.as_ref()?;
    let home = std::env::var("HOME").ok();
    let stock = stock_path_from(
        std::env::var("XDG_DATA_HOME").ok().as_deref(),
        home.as_deref(),
    )
    .unwrap_or_else(|| PathBuf::from("stock.png"));
    let state = wallpaper_state_path_from(
        std::env::var("XDG_STATE_HOME").ok().as_deref(),
        home.as_deref(),
    )
    .and_then(|p| std::fs::read_to_string(p).ok());
    Some(swaylock_config(
        lock,
        config.theme.as_ref(),
        &stock,
        state.as_deref(),
    ))
}

/// Writes the swaylock config for `config` (nothing without a `lock` block). A failure only
/// warns. Returns the file's path when a `lock` block is set and a path resolves.
pub fn write_swaylock_config(config: &Config) -> Option<PathBuf> {
    let text = render_for_env(config)?;
    let Some(path) = fragment_path() else {
        warn!("neither XDG_CONFIG_HOME nor HOME is set; swaylock config not written");
        return None;
    };
    match write_file(&path, &text) {
        Ok(true) => info!(path = %path.display(), "swaylock config written"),
        Ok(false) => {}
        Err(err) => warn!(path = %path.display(), %err, "cannot write the swaylock config"),
    }
    Some(path)
}

fn write_file(path: &Path, text: &str) -> io::Result<bool> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::terminal_theme::write_if_changed(path, text)
}

/// The locker arguments: `words` unchanged, except that with a generated `fragment` and a
/// program whose basename is exactly `swaylock` and no `-C`/`--config`/`--config=…` among
/// `words`, `-C <fragment>` is inserted first. **Pure.**
#[must_use]
pub fn locker_args(program: &str, words: &[&str], fragment: Option<&Path>) -> Vec<OsString> {
    let mut args: Vec<OsString> = words.iter().map(OsString::from).collect();
    let is_swaylock = Path::new(program)
        .file_name()
        .is_some_and(|n| n == "swaylock");
    let has_config = words
        .iter()
        .any(|w| w.starts_with("-C") || *w == "--config" || w.starts_with("--config="));
    if let Some(fragment) = fragment
        && is_swaylock
        && !has_config
    {
        args.splice(
            0..0,
            [OsString::from("-C"), fragment.as_os_str().to_owned()],
        );
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_config::OverlayStyle;

    fn lock(style: OverlayStyle, background: SurfaceBackground) -> LockConfig {
        LockConfig { style, background }
    }

    #[test]
    fn colour_background_and_style_colours() {
        let l = lock(
            OverlayStyle::N1Dark,
            SurfaceBackground::Color(Rgb(0x10, 0x20, 0x30)),
        );
        let text = swaylock_config(&l, None, Path::new("/s.png"), None);
        assert!(
            text.starts_with("indicator-idle-visible\ncolor=102030\n"),
            "{text}"
        );
        assert!(!text.contains("image="), "{text}");
        let p = crucible_config::N1_DARK;
        assert!(
            text.contains(&format!("key-hl-color={}ff\n", rgb(p.accent))),
            "{text}"
        );
        assert!(
            text.contains(&format!("ring-ver-color={}ff\n", rgb(p.accent))),
            "{text}"
        );
        assert!(text.contains("ring-wrong-color=e04f5fff\n"), "{text}");
    }

    #[test]
    fn stock_background_points_at_the_installed_png() {
        let l = lock(OverlayStyle::N1Med, SurfaceBackground::Stock);
        let text = swaylock_config(&l, None, Path::new("/d/cde/backgrounds/stock.png"), None);
        assert!(
            text.contains("color=000000\nimage=/d/cde/backgrounds/stock.png\nscaling=fill\n"),
            "{text}"
        );
    }

    #[test]
    fn user_background_reads_the_state_lines() {
        let l = lock(OverlayStyle::N1Med, SurfaceBackground::User);
        let state = "DP-1\t/w/a.png\nHDMI-A-1\t/w/b.png\n";
        let text = swaylock_config(&l, None, Path::new("/s.png"), Some(state));
        assert!(
            text.contains("image=DP-1:/w/a.png\nimage=HDMI-A-1:/w/b.png\nscaling=fill\n"),
            "{text}"
        );
        for mirror in ["/w/a.png\n", "*\t/w/a.png\n"] {
            let text = swaylock_config(&l, None, Path::new("/s.png"), Some(mirror));
            assert!(text.contains("image=/w/a.png\nscaling=fill\n"), "{text}");
        }
        for missing in [None, Some("")] {
            let text = swaylock_config(&l, None, Path::new("/s.png"), missing);
            assert!(!text.contains("image="), "{text}");
        }
    }

    #[test]
    fn terminal_style_uses_the_theme() {
        let theme = ThemeConfig {
            foreground: Some(Rgb(1, 2, 3)),
            ..ThemeConfig::default()
        };
        let l = lock(
            OverlayStyle::Terminal,
            SurfaceBackground::Color(Rgb(0, 0, 0)),
        );
        let text = swaylock_config(&l, Some(&theme), Path::new("/s.png"), None);
        assert!(text.contains("text-color=010203ff\n"), "{text}");
    }

    #[test]
    fn paths_resolve_from_the_environment_values() {
        assert_eq!(
            fragment_path_from(None, Some("/h")),
            Some(PathBuf::from("/h/.config/cde/swaylock/config"))
        );
        assert_eq!(
            stock_path_from(None, Some("/h")),
            Some(PathBuf::from("/h/.local/share/cde/backgrounds/stock.png"))
        );
        assert_eq!(
            stock_path_from(Some("/x"), None),
            Some(PathBuf::from("/x/cde/backgrounds/stock.png"))
        );
        assert_eq!(
            wallpaper_state_path_from(None, Some("/h")),
            Some(PathBuf::from("/h/.local/state/wallpaper-rotate/current"))
        );
    }

    #[test]
    fn dash_c_goes_only_to_a_plain_swaylock() {
        let f = Path::new("/c/cde/swaylock/config");
        let os = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            locker_args("swaylock", &["-f"], Some(f)),
            os(&["-C", "/c/cde/swaylock/config", "-f"])
        );
        assert_eq!(
            locker_args("/usr/bin/swaylock", &[], Some(f)),
            os(&["-C", "/c/cde/swaylock/config"])
        );
        // No lock block, another locker, or the user's own config: unchanged.
        assert_eq!(locker_args("swaylock", &["-f"], None), os(&["-f"]));
        assert_eq!(
            locker_args("swaylock-effects", &["-f"], Some(f)),
            os(&["-f"])
        );
        assert_eq!(locker_args("i3lock", &[], Some(f)), os(&[]));
        for given in [
            &["-C", "/mine"][..],
            &["--config", "/mine"],
            &["--config=/mine"],
            &["-C/mine"],
        ] {
            assert_eq!(locker_args("swaylock", given, Some(f)), os(given));
        }
    }
}
