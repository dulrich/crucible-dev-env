//! The shared surface-style model: one [`SurfaceStyle`] (the overlay style names) and one
//! [`SurfaceBackground`], used by `header { style }`, the `lock` block and the `greeter` block.
//!
//! [`resolve_palette`] turns a style plus the optional `theme` block into the [`Palette`] a
//! surface paints. Rendering lives in the compositor and the greeter; this module is data only.

use crate::model::{Named, Rgb};
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

/// Which accent a surface paints. KDL: `accent "style" | "theme" | "#rrggbb"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SurfaceAccent {
    /// The surface style's own accent (today's behaviour).
    #[default]
    Style,
    /// The `terminal` style's resolved accent: `theme.accent`, then `theme.cursor`, then n1-med.
    Theme,
    /// A fixed colour.
    Color(Rgb),
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

/// Where the greeter's login panel sits: the centre of a screen region, as fractions of the output.
///
/// KDL: `position "<name>"` in the `greeter` block (default [`PanelPosition::Lower`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelPosition {
    /// (50 %, 50 %).
    Center,
    /// (50 %, 25 %).
    Upper,
    /// (50 %, 75 %).
    Lower,
    /// (25 %, 25 %).
    UpperLeft,
    /// (75 %, 25 %).
    UpperRight,
    /// (25 %, 75 %).
    LowerLeft,
    /// (75 %, 75 %).
    LowerRight,
}

impl Named for PanelPosition {
    const NAMES: &'static [(Self, &'static str)] = &[
        (Self::Center, "center"),
        (Self::Upper, "upper"),
        (Self::Lower, "lower"),
        (Self::UpperLeft, "upper-left"),
        (Self::UpperRight, "upper-right"),
        (Self::LowerLeft, "lower-left"),
        (Self::LowerRight, "lower-right"),
    ];
}

impl PanelPosition {
    /// The region centre as (x, y) fractions of the output size.
    #[must_use]
    pub const fn fractions(self) -> (f64, f64) {
        match self {
            Self::Center => (0.5, 0.5),
            Self::Upper => (0.5, 0.25),
            Self::Lower => (0.5, 0.75),
            Self::UpperLeft => (0.25, 0.25),
            Self::UpperRight => (0.75, 0.25),
            Self::LowerLeft => (0.25, 0.75),
            Self::LowerRight => (0.75, 0.75),
        }
    }
}

/// The default `greeter` panel position.
pub const DEFAULT_GREETER_POSITION: PanelPosition = PanelPosition::Lower;

/// The `lock { style …; background … }` block: how the generated swaylock config looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockConfig {
    /// Indicator colours (default [`DEFAULT_LOCK_STYLE`]).
    pub style: SurfaceStyle,
    /// Lock-screen background (default [`DEFAULT_LOCK_BACKGROUND`]).
    pub background: SurfaceBackground,
    /// Indicator accent (default [`SurfaceAccent::Style`]).
    pub accent: SurfaceAccent,
}

impl Default for LockConfig {
    fn default() -> Self {
        Self {
            style: DEFAULT_LOCK_STYLE,
            background: DEFAULT_LOCK_BACKGROUND,
            accent: SurfaceAccent::Style,
        }
    }
}

/// The `greeter { style …; background …; position …; session "<command>" }` block for the optional greetd
/// greeter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GreeterConfig {
    /// Greeter colours (default [`DEFAULT_GREETER_STYLE`]).
    pub style: SurfaceStyle,
    /// Greeter background (default [`DEFAULT_GREETER_BACKGROUND`]).
    pub background: SurfaceBackground,
    /// Where the login panel sits (default [`DEFAULT_GREETER_POSITION`]).
    pub position: PanelPosition,
    /// The session command started after login, non-empty (default
    /// [`DEFAULT_GREETER_SESSION`]).
    pub session: String,
    /// Panel accent (default [`SurfaceAccent::Style`]).
    pub accent: SurfaceAccent,
}

impl Default for GreeterConfig {
    fn default() -> Self {
        Self {
            style: DEFAULT_GREETER_STYLE,
            background: DEFAULT_GREETER_BACKGROUND,
            position: DEFAULT_GREETER_POSITION,
            session: DEFAULT_GREETER_SESSION.to_string(),
            accent: SurfaceAccent::Style,
        }
    }
}

/// The effective palette of `style`. The N1 styles return their fixed palette. `terminal` takes
/// its colours from `theme`: `bg` = `theme.background`, `text` = `theme.foreground`, `line` =
/// `theme.palette 8` (bright black), `accent` = `theme.accent`, then `theme.cursor`, each falling back to the
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
        accent: t.accent.or(t.cursor).unwrap_or(N1_MED.accent),
    }
}

/// The accent colour `accent` selects for `style`: [`resolve_palette`]'s accent under
/// [`SurfaceAccent::Style`], the `terminal` accent under [`SurfaceAccent::Theme`], else the colour.
#[must_use]
pub fn resolve_accent(
    style: SurfaceStyle,
    accent: SurfaceAccent,
    theme: Option<&ThemeConfig>,
) -> Rgb {
    match accent {
        SurfaceAccent::Style => resolve_palette(style, theme).accent,
        SurfaceAccent::Theme => resolve_palette(OverlayStyle::Terminal, theme).accent,
        SurfaceAccent::Color(c) => c,
    }
}

/// A surface's palette: [`resolve_palette`] with its accent chosen by `accent`
/// ([`resolve_accent`]). The one place a surface's accent is resolved.
#[must_use]
pub fn surface_palette(
    style: SurfaceStyle,
    accent: SurfaceAccent,
    theme: Option<&ThemeConfig>,
) -> Palette {
    Palette {
        accent: resolve_accent(style, accent, theme),
        ..resolve_palette(style, theme)
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
    fn surface_palette_over_style_accent_and_theme() {
        let teal = Rgb(0x33, 0xcc, 0xcc);
        let cursor_only = ThemeConfig {
            cursor: Some(Rgb(4, 4, 4)),
            ..ThemeConfig::default()
        };
        let both = ThemeConfig {
            accent: Some(Rgb(5, 5, 5)),
            ..cursor_only.clone()
        };
        // `terminal` prefers theme.accent, then cursor, then n1-med.
        assert_eq!(
            resolve_palette(OverlayStyle::Terminal, Some(&both)).accent,
            Rgb(5, 5, 5)
        );
        for style in [
            OverlayStyle::Terminal,
            OverlayStyle::N1Dark,
            OverlayStyle::N1Light,
        ] {
            for theme in [None, Some(&cursor_only), Some(&both)] {
                let base = resolve_palette(style, theme);
                assert_eq!(surface_palette(style, SurfaceAccent::Style, theme), base);
                let want = match theme {
                    None => N1_MED.accent,
                    Some(t) => t.accent.or(t.cursor).unwrap(),
                };
                let p = surface_palette(style, SurfaceAccent::Theme, theme);
                assert_eq!(
                    p,
                    Palette {
                        accent: want,
                        ..base
                    },
                    "{style:?}"
                );
                let p = surface_palette(style, SurfaceAccent::Color(teal), theme);
                assert_eq!(
                    p,
                    Palette {
                        accent: teal,
                        ..base
                    }
                );
            }
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
