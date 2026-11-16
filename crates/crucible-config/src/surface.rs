//! The shared surface-style model: one [`SurfaceStyle`] (the overlay style names) and one
//! [`SurfaceBackground`], used by `header { style }`, the `lock` block and the `greeter` block.
//!
//! [`resolve_palette`] turns a style plus the optional `theme` block into the [`Palette`] a
//! surface paints. Rendering lives in the compositor and the greeter; this module is data only.

use crate::model::Rgb;
use crate::overlay::{N1_MED, OverlayStyle, Palette};
use crate::theme::ThemeConfig;

/// The look of a CDE surface (header, overlay, lock screen, greeter): `terminal | n1-dark |
/// n1-med | n1-light`. The same type as [`OverlayStyle`], so the names, parser and
/// [`OverlayStyle::palette`] table are shared.
pub type SurfaceStyle = OverlayStyle;

/// What a full-screen surface (lock screen, greeter) shows behind its content.
///
/// KDL: `background "color" "#rrggbb"`, `background "stock"` or `background "user"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceBackground {
    /// A solid colour.
    Color(Rgb),
    /// The stock CDE background image shipped with every deploy.
    Stock,
    /// The user's current per-output wallpaper.
    User,
}

/// The default `lock` style.
pub const DEFAULT_LOCK_STYLE: SurfaceStyle = OverlayStyle::N1Med;
/// The default `lock` background: solid black.
pub const DEFAULT_LOCK_BACKGROUND: SurfaceBackground = SurfaceBackground::Color(Rgb(0, 0, 0));
/// The default `greeter` style.
pub const DEFAULT_GREETER_STYLE: SurfaceStyle = OverlayStyle::N1Med;
/// The default `greeter` background.
pub const DEFAULT_GREETER_BACKGROUND: SurfaceBackground = SurfaceBackground::Stock;
/// The default `greeter` session command.
pub const DEFAULT_GREETER_SESSION: &str = "cde-session";

/// The `lock { style …; background … }` block: how the generated swaylock config looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockConfig {
    /// Indicator colours (default [`DEFAULT_LOCK_STYLE`]).
    pub style: SurfaceStyle,
    /// Lock-screen background (default [`DEFAULT_LOCK_BACKGROUND`]).
    pub background: SurfaceBackground,
}

impl Default for LockConfig {
    fn default() -> Self {
        Self {
            style: DEFAULT_LOCK_STYLE,
            background: DEFAULT_LOCK_BACKGROUND,
        }
    }
}

/// The `greeter { style …; background …; session "<command>" }` block for the optional greetd
/// greeter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GreeterConfig {
    /// Greeter colours (default [`DEFAULT_GREETER_STYLE`]).
    pub style: SurfaceStyle,
    /// Greeter background (default [`DEFAULT_GREETER_BACKGROUND`]).
    pub background: SurfaceBackground,
    /// The session command started after login, non-empty (default
    /// [`DEFAULT_GREETER_SESSION`]).
    pub session: String,
}

impl Default for GreeterConfig {
    fn default() -> Self {
        Self {
            style: DEFAULT_GREETER_STYLE,
            background: DEFAULT_GREETER_BACKGROUND,
            session: DEFAULT_GREETER_SESSION.to_string(),
        }
    }
}

/// The effective palette of `style`. The N1 styles return their fixed palette. `terminal` takes
/// its colours from `theme`: `bg` = `theme.background`, `text` = `theme.foreground`, `line` =
/// `theme.palette 8` (bright black), `accent` = `theme.cursor`, each falling back to the
/// [`N1_MED`] value when unset; with no `theme` block, `terminal` is exactly [`N1_MED`].
#[must_use]
pub fn resolve_palette(style: SurfaceStyle, theme: Option<&ThemeConfig>) -> Palette {
    if let Some(p) = style.palette() {
        return p;
    }
    let Some(t) = theme else {
        return N1_MED;
    };
    Palette {
        bg: t.background.unwrap_or(N1_MED.bg),
        text: t.foreground.unwrap_or(N1_MED.text),
        line: t.palette[8].unwrap_or(N1_MED.line),
        accent: t.cursor.unwrap_or(N1_MED.accent),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::{N1_DARK, N1_LIGHT};

    #[test]
    fn n1_styles_ignore_the_theme() {
        let t = ThemeConfig {
            background: Some(Rgb(1, 2, 3)),
            ..ThemeConfig::default()
        };
        for (s, p) in [
            (OverlayStyle::N1Dark, N1_DARK),
            (OverlayStyle::N1Med, N1_MED),
            (OverlayStyle::N1Light, N1_LIGHT),
        ] {
            assert_eq!(resolve_palette(s, Some(&t)), p);
            assert_eq!(resolve_palette(s, None), p);
        }
    }

    #[test]
    fn terminal_takes_theme_colours_and_falls_back_to_n1_med() {
        assert_eq!(resolve_palette(OverlayStyle::Terminal, None), N1_MED);
        assert_eq!(
            resolve_palette(OverlayStyle::Terminal, Some(&ThemeConfig::default())),
            N1_MED
        );
        let mut t = ThemeConfig {
            background: Some(Rgb(1, 1, 1)),
            foreground: Some(Rgb(2, 2, 2)),
            cursor: Some(Rgb(4, 4, 4)),
            ..ThemeConfig::default()
        };
        t.palette[8] = Some(Rgb(3, 3, 3));
        assert_eq!(
            resolve_palette(OverlayStyle::Terminal, Some(&t)),
            Palette {
                bg: Rgb(1, 1, 1),
                text: Rgb(2, 2, 2),
                line: Rgb(3, 3, 3),
                accent: Rgb(4, 4, 4),
            }
        );
    }
}
