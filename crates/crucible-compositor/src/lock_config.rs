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
    Config, LockBackground, LockConfig, Rgb, SurfaceBackground, ThemeConfig, config_path_from,
    surface_palette,
};
use tracing::{info, warn};

/// The "wrong password" red.
const WRONG: Rgb = Rgb(0xe0, 0x4f, 0x5f);

/// `rrggbbaa` for swaylock.
fn rgba(c: Rgb, alpha: u8) -> String {
    format!("{c:x}{alpha:02x}")
}

/// `rrggbb` for swaylock's `color=`.
fn rgb(c: Rgb) -> String {
    format!("{c:x}")
}

/// The entries of the wallpaper state text as `(output, path)`: `output` is `None` for a bare path
/// line or a `*\t<path>` line. A line that trims to nothing is blank; tab-separated fields are
/// taken verbatim (never trimmed); only a bare (no tab) line is trimmed; a tab line with an empty
/// path, or an empty output that is not `*`, is skipped. The one parser of the state file
/// (`wallpaper-rotate.sh` writes it). **Pure.**
pub(crate) fn wallpaper_entries(state: &str) -> Vec<(Option<&str>, &str)> {
    state
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| match line.split_once('\t') {
            Some(("*", path)) if !path.is_empty() => Some((None, path)),
            Some((output, path)) if !output.is_empty() && !path.is_empty() => {
                Some((Some(output), path))
            }
            Some(_) => None,
            None => Some((None, line.trim())),
        })
        .collect()
}

/// The `image=` values for the wallpaper state text: one `<output>:<path>` per `<output>\t<path>`
/// line; a bare path line or a `*\t<path>` line gives a plain path. **Pure.**
fn wallpaper_images(state: &str) -> Vec<String> {
    wallpaper_entries(state)
        .into_iter()
        .map(|(output, path)| output.map_or_else(|| path.to_string(), |o| format!("{o}:{path}")))
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
    let p = surface_palette(lock.style, lock.accent, theme);
    let mut out = String::from("indicator-idle-visible\n");
    let background = match lock.background {
        LockBackground::Glass { .. } => {
            if let Some((c, alpha)) = lock.background.glass_tint(theme) {
                let _ = writeln!(out, "color={}{alpha:02x}", rgb(c));
            }
            None
        }
        LockBackground::Surface(b) => Some(b),
    };
    match background {
        None => {}
        Some(SurfaceBackground::Color(c)) => {
            let _ = writeln!(out, "color={}", rgb(c));
        }
        Some(SurfaceBackground::Stock) => {
            let _ = writeln!(out, "color=000000");
            let _ = writeln!(out, "image={}", stock_path.display());
            let _ = writeln!(out, "scaling=fill");
        }
        Some(SurfaceBackground::User) => {
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

/// The prepared swaylock fragment: where it lives and whether its current content is on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fragment {
    /// The fragment's path.
    pub path: PathBuf,
    /// `true` when the content was written or was already identical on disk; `false` after a
    /// write error (the file may be missing or stale).
    pub on_disk: bool,
}

/// Writes the swaylock config for `config` (nothing without a `lock` block). A failure only
/// warns. Returns the fragment when a `lock` block is set and a path resolves; its
/// [`Fragment::on_disk`] is `false` after a write error, so a caller that trusts the content
/// (the glass gate) fails closed.
pub fn write_swaylock_config(config: &Config) -> Option<Fragment> {
    let text = render_for_env(config)?;
    let Some(path) = fragment_path() else {
        warn!("neither XDG_CONFIG_HOME nor HOME is set; swaylock config not written");
        return None;
    };
    let on_disk = match write_file(&path, &text) {
        Ok(true) => {
            info!(path = %path.display(), "swaylock config written");
            true
        }
        Ok(false) => true,
        Err(err) => {
            warn!(path = %path.display(), %err, "cannot write the swaylock config");
            false
        }
    };
    Some(Fragment { path, on_disk })
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
    insert_fragment(program, words, fragment).0
}

/// [`locker_args`] plus whether `-C <fragment>` was actually inserted. **Pure.**
#[must_use]
pub fn insert_fragment(
    program: &str,
    words: &[&str],
    fragment: Option<&Path>,
) -> (Vec<OsString>, bool) {
    let mut args: Vec<OsString> = words.iter().map(OsString::from).collect();
    let is_swaylock = Path::new(program)
        .file_name()
        .is_some_and(|n| n == "swaylock");
    let has_config = words
        .iter()
        .any(|w| w.starts_with("-C") || *w == "--config" || w.starts_with("--config="));
    let mut inserted = false;
    if let Some(fragment) = fragment
        && is_swaylock
        && !has_config
    {
        args.splice(
            0..0,
            [OsString::from("-C"), fragment.as_os_str().to_owned()],
        );
        inserted = true;
    }
    (args, inserted)
}

/// Whether a locker launch may be bound to glass: the `lock` block is glass, its fragment content
/// is on disk, and `-C <fragment>` was inserted into the argv (a locker with its own config, or
/// not a plain `swaylock`, never reads the glass fragment). **Pure.**
#[must_use]
pub fn glass_launch_eligible(
    background: LockBackground,
    fragment: Option<&Fragment>,
    inserted: bool,
) -> bool {
    background.is_glass() && inserted && fragment.is_some_and(|f| f.on_disk)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_config::SurfaceStyle;

    fn lock(style: SurfaceStyle, background: SurfaceBackground) -> LockConfig {
        LockConfig {
            style,
            background: LockBackground::Surface(background),
            ..LockConfig::default()
        }
    }

    #[test]
    fn colour_background_and_style_colours() {
        let l = lock(
            SurfaceStyle::N1Dark,
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

    fn glass(alpha: Option<u32>) -> LockConfig {
        LockConfig {
            background: LockBackground::Glass {
                alpha: alpha.and_then(crucible_config::Alpha::from_milli),
            },
            ..LockConfig::default()
        }
    }

    #[test]
    fn glass_emits_the_translucent_tint_and_no_image() {
        // Default: black at 0.8 -> cc; indicator colours are the style's, unchanged.
        let text = swaylock_config(&glass(None), None, Path::new("/s.png"), Some("/w/a.png\n"));
        assert!(
            text.starts_with("indicator-idle-visible\ncolor=000000cc\ninside-color="),
            "{text}"
        );
        assert!(
            !text.contains("image=") && !text.contains("scaling="),
            "{text}"
        );
        let plain = swaylock_config(
            &lock(SurfaceStyle::N1Med, SurfaceBackground::Color(Rgb(0, 0, 0))),
            None,
            Path::new("/s.png"),
            None,
        );
        assert_eq!(
            text.replace("color=000000cc\n", "color=000000\n"),
            plain,
            "only the colour line differs"
        );
        // alpha= and the theme: aa = round(alpha * 255), two lowercase hex digits.
        let theme = ThemeConfig {
            background: Some(Rgb(0x10, 0xab, 0xff)),
            ..ThemeConfig::default()
        };
        for (alpha, aa) in [(500, "80"), (0, "00"), (1000, "ff"), (50, "0d")] {
            let text =
                swaylock_config(&glass(Some(alpha)), Some(&theme), Path::new("/s.png"), None);
            assert!(
                text.contains(&format!("\ncolor=10abff{aa}\n")),
                "{alpha}: {text}"
            );
        }
        // The theme's alpha applies when `alpha=` is not given.
        let theme = ThemeConfig {
            alpha: crucible_config::Alpha::from_milli(500),
            ..ThemeConfig::default()
        };
        let text = swaylock_config(&glass(None), Some(&theme), Path::new("/s.png"), None);
        assert!(text.contains("\ncolor=00000080\n"), "{text}");
    }

    #[test]
    fn glass_launch_needs_a_glass_block_a_prepared_fragment_and_an_inserted_c() {
        let frag = |on_disk| Fragment {
            path: PathBuf::from("/c/cde/swaylock/config"),
            on_disk,
        };
        let g = glass(None).background;
        let plain = LockConfig::default().background;
        assert!(glass_launch_eligible(g, Some(&frag(true)), true));
        // A write failure: the content is not on disk.
        assert!(!glass_launch_eligible(g, Some(&frag(false)), true));
        // No fragment path, or `-C` not inserted (own config / another locker).
        assert!(!glass_launch_eligible(g, None, true));
        assert!(!glass_launch_eligible(g, Some(&frag(true)), false));
        // Not a glass block.
        assert!(!glass_launch_eligible(plain, Some(&frag(true)), true));
        // The insertion report follows `locker_args`.
        let f = Path::new("/c/cde/swaylock/config");
        assert!(insert_fragment("swaylock", &["-f"], Some(f)).1);
        for (program, words) in [
            ("swaylock", &["-C", "/mine"][..]),
            ("swaylock", &["--config=/mine"]),
            ("swaylock-effects", &[]),
            ("i3lock", &[]),
        ] {
            assert!(!insert_fragment(program, words, Some(f)).1, "{program}");
        }
        assert!(!insert_fragment("swaylock", &[], None).1);
    }

    #[test]
    fn stock_background_points_at_the_installed_png() {
        let l = lock(SurfaceStyle::N1Med, SurfaceBackground::Stock);
        let text = swaylock_config(&l, None, Path::new("/d/cde/backgrounds/stock.png"), None);
        assert!(
            text.contains("color=000000\nimage=/d/cde/backgrounds/stock.png\nscaling=fill\n"),
            "{text}"
        );
    }

    #[test]
    fn user_background_reads_the_state_lines() {
        let l = lock(SurfaceStyle::N1Med, SurfaceBackground::User);
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
            SurfaceStyle::Terminal,
            SurfaceBackground::Color(Rgb(0, 0, 0)),
        );
        let text = swaylock_config(&l, Some(&theme), Path::new("/s.png"), None);
        assert!(text.contains("text-color=010203ff\n"), "{text}");
    }

    #[test]
    fn lock_accent_colours_the_ring() {
        let base = lock(SurfaceStyle::N1Med, SurfaceBackground::Color(Rgb(0, 0, 0)));
        let l = LockConfig {
            accent: crucible_config::SurfaceAccent::Color(Rgb(0x33, 0xcc, 0xcc)),
            ..base
        };
        let text = swaylock_config(&l, None, Path::new("/s.png"), None);
        assert!(text.contains("33ccccff"), "{text}");
        let plain = swaylock_config(&base, None, Path::new("/s.png"), None);
        assert!(!plain.contains("33cccc"), "{plain}");
        assert_eq!(
            text.replace("33cccc", &rgb(crucible_config::N1_MED.accent)),
            plain
        );
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
