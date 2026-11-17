//! Overlay styling shared by the `pick`, `launch` and `keys` TUIs.
//!
//! The compositor sets `OVERLAY_STYLE_ENV` to `terminal`, `n1-dark`, `n1-med` or `n1-light` on
//! overlay children. `Terminal` (also the fallback for unset, empty or unknown values) makes every
//! helper return the empty string (or today's unstyled sequence), so output stays byte-identical
//! to the unstyled TUIs. The N1 styles paint truecolor base attributes before every clear and
//! re-apply them after every in-frame reset, so no cell falls back to the terminal's default
//! background. When the compositor also sets `OVERLAY_SURROUND_ENV` to `default` (the terminal's
//! default background already is the palette `bg`, so `overlay.alpha` can apply), the N1 base
//! background is SGR 49 instead of the explicit truecolor `bg`.

use crucible_compositor::ipc::{OVERLAY_STYLE_ENV, OVERLAY_SURROUND_ENV};
use crucible_config::{Named, OverlayStyle, Palette, Rgb};

/// The overlay style.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Terminal,
    /// An N1 palette whose surround is painted with an explicit truecolor background.
    N1(Palette),
    /// An N1 palette whose surround is the terminal's default background (SGR 49).
    N1Default(Palette),
}

impl From<OverlayStyle> for Style {
    fn from(style: OverlayStyle) -> Style {
        style.palette().map_or(Style::Terminal, Style::N1)
    }
}

/// The `OVERLAY_SURROUND_ENV` value meaning the terminal's default background is the palette bg.
const SURROUND_DEFAULT: &str = "default";

fn fg(Rgb(r, g, b): Rgb) -> String {
    format!("\x1b[38;2;{r};{g};{b}m")
}

fn bg(Rgb(r, g, b): Rgb) -> String {
    format!("\x1b[48;2;{r};{g};{b}m")
}

impl Style {
    /// Parses a style name through the config's [`OverlayStyle::from_name`]; unset, empty or
    /// unknown values explicitly give `Terminal`.
    #[must_use]
    pub fn parse(value: Option<&str>) -> Style {
        value
            .and_then(OverlayStyle::from_name)
            .map_or(Style::Terminal, Style::from)
    }

    /// [`Style::parse`] plus the surround: `surround == Some("default")` turns an N1 style's
    /// base background into SGR 49. Any other value, and the `Terminal` style, are unchanged.
    #[must_use]
    pub fn parse_with_surround(value: Option<&str>, surround: Option<&str>) -> Style {
        match (Style::parse(value), surround) {
            (Style::N1(p), Some(SURROUND_DEFAULT)) => Style::N1Default(p),
            (style, _) => style,
        }
    }

    /// The style from `OVERLAY_STYLE_ENV` and `OVERLAY_SURROUND_ENV`.
    #[must_use]
    pub fn from_env() -> Style {
        Style::parse_with_surround(
            std::env::var(OVERLAY_STYLE_ENV).ok().as_deref(),
            std::env::var(OVERLAY_SURROUND_ENV).ok().as_deref(),
        )
    }

    /// Base attributes: truecolor foreground and background, or SGR 49 for the background when
    /// the surround is the terminal's default (empty for `Terminal`).
    #[must_use]
    pub fn base(&self) -> String {
        match self {
            Style::Terminal => String::new(),
            Style::N1(p) => format!("{}{}", fg(p.text), bg(p.bg)),
            Style::N1Default(p) => format!("{}\x1b[49m", fg(p.text)),
        }
    }

    /// Attribute reset (empty for `Terminal`).
    #[must_use]
    pub fn reset(&self) -> String {
        match self {
            Style::Terminal => String::new(),
            Style::N1(_) | Style::N1Default(_) => "\x1b[0m".to_string(),
        }
    }

    /// Start of a frame: home, base attributes, then erase-in-display (painted with the style
    /// background).
    #[must_use]
    pub fn frame_start(&self) -> String {
        format!("\x1b[H{}\x1b[2J", self.base())
    }

    /// What opens a selected row: reverse video for `Terminal`; the `line` background with the
    /// `accent` foreground for N1.
    #[must_use]
    pub fn highlight_start(&self) -> String {
        match self {
            Style::Terminal => "\x1b[7m".to_string(),
            Style::N1(p) | Style::N1Default(p) => format!("{}{}", bg(p.line), fg(p.accent)),
        }
    }

    /// A selected row: [`Self::highlight_start`], the text, a reset, then (N1) the base attributes
    /// again.
    #[must_use]
    pub fn highlight(&self, text: &str) -> String {
        format!("{}{text}\x1b[0m{}", self.highlight_start(), self.base())
    }

    /// Dimmed text (a column header): `SGR 2` .. `SGR 22` for `Terminal`; for N1 a muted
    /// foreground (the per-channel midpoint of `text` and `bg`) and then the text colour again --
    /// never `\x1b[0m`, which would drop the N1 background.
    #[must_use]
    pub fn dim(&self, text: &str) -> String {
        match self {
            Style::Terminal => format!("\x1b[2m{text}\x1b[22m"),
            Style::N1(p) | Style::N1Default(p) => {
                let mid =
                    |a: u8, b: u8| u8::try_from((u16::from(a) + u16::from(b)) / 2).unwrap_or(a);
                let (Rgb(tr, tg, tb), Rgb(br, bgc, bb)) = (p.text, p.bg);
                let muted = Rgb(mid(tr, br), mid(tg, bgc), mid(tb, bb));
                format!("{}{text}{}", fg(muted), fg(p.text))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_config::{N1_DARK, N1_LIGHT, N1_MED};

    #[test]
    fn parses_each_value_and_falls_back() {
        assert_eq!(Style::parse(Some("terminal")), Style::Terminal);
        assert_eq!(Style::parse(Some("n1-dark")), Style::N1(N1_DARK));
        assert_eq!(Style::parse(Some("n1-med")), Style::N1(N1_MED));
        assert_eq!(Style::parse(Some("n1-light")), Style::N1(N1_LIGHT));
        assert_eq!(Style::parse(Some("bogus")), Style::Terminal);
        assert_eq!(Style::parse(Some("")), Style::Terminal);
        assert_eq!(Style::parse(None), Style::Terminal);
    }

    #[test]
    fn from_overlay_style_covers_every_variant() {
        assert_eq!(Style::from(OverlayStyle::Terminal), Style::Terminal);
        assert_eq!(Style::from(OverlayStyle::N1Dark), Style::N1(N1_DARK));
        assert_eq!(Style::from(OverlayStyle::N1Med), Style::N1(N1_MED));
        assert_eq!(Style::from(OverlayStyle::N1Light), Style::N1(N1_LIGHT));
        // The names the compositor exports round-trip through the config's spelling.
        for s in [
            OverlayStyle::Terminal,
            OverlayStyle::N1Dark,
            OverlayStyle::N1Med,
            OverlayStyle::N1Light,
        ] {
            assert_eq!(Style::parse(Some(s.as_str())), Style::from(s));
        }
    }

    #[test]
    fn surround_default_makes_the_n1_base_background_sgr_49() {
        let n1 = |surround| Style::parse_with_surround(Some("n1-dark"), surround);
        // Unset or any other value: unchanged from today.
        assert_eq!(n1(None), Style::N1(N1_DARK));
        assert_eq!(n1(Some("")), Style::N1(N1_DARK));
        assert_eq!(n1(Some("opaque")), Style::N1(N1_DARK));
        let d = n1(Some("default"));
        assert_eq!(d, Style::N1Default(N1_DARK));
        assert_eq!(d.base(), "\x1b[38;2;233;233;234m\x1b[49m");
        assert_eq!(
            d.frame_start(),
            "\x1b[H\x1b[38;2;233;233;234m\x1b[49m\x1b[2J"
        );
        // Selected rows keep the explicit line background; the base is re-applied after.
        assert_eq!(
            d.highlight("x"),
            "\x1b[48;2;55;56;59m\x1b[38;2;255;138;61mx\x1b[0m\x1b[38;2;233;233;234m\x1b[49m"
        );
        assert_eq!(d.highlight_start(), n1(None).highlight_start());
        assert_eq!(d.reset(), "\x1b[0m");
        assert_eq!(d.dim("x"), n1(None).dim("x"));
        // The Terminal style ignores the surround.
        assert_eq!(
            Style::parse_with_surround(Some("terminal"), Some("default")),
            Style::Terminal
        );
        assert_eq!(
            Style::parse_with_surround(None, Some("default")),
            Style::Terminal
        );
    }

    #[test]
    fn terminal_helpers_are_unstyled() {
        let s = Style::Terminal;
        assert_eq!(s.base(), "");
        assert_eq!(s.reset(), "");
        assert_eq!(s.frame_start(), "\x1b[H\x1b[2J");
        assert_eq!(s.highlight("x"), "\x1b[7mx\x1b[0m");
    }

    #[test]
    fn dim_is_sgr_dim_for_terminal_and_a_muted_foreground_for_n1() {
        assert_eq!(Style::Terminal.dim("x"), "\x1b[2mx\x1b[22m");
        assert_eq!(Style::Terminal.highlight_start(), "\x1b[7m");
        let s = Style::parse(Some("n1-dark"));
        // Midpoint of text (233,233,234) and bg (14,14,16), then the text colour; no reset.
        assert_eq!(s.dim("x"), "\x1b[38;2;123;123;125mx\x1b[38;2;233;233;234m");
        assert!(!s.dim("x").contains("\x1b[0m"));
        assert!(s.highlight("x").starts_with(&s.highlight_start()));
    }

    #[test]
    fn n1_helpers_emit_exact_truecolor() {
        let s = Style::parse(Some("n1-dark"));
        assert_eq!(s.base(), "\x1b[38;2;233;233;234m\x1b[48;2;14;14;16m");
        assert_eq!(s.reset(), "\x1b[0m");
        assert_eq!(
            s.frame_start(),
            "\x1b[H\x1b[38;2;233;233;234m\x1b[48;2;14;14;16m\x1b[2J"
        );
        assert_eq!(
            s.highlight("x"),
            "\x1b[48;2;55;56;59m\x1b[38;2;255;138;61mx\x1b[0m\x1b[38;2;233;233;234m\x1b[48;2;14;14;16m"
        );
        let l = Style::parse(Some("n1-light"));
        assert_eq!(l.base(), "\x1b[38;2;25;26;27m\x1b[48;2;244;244;245m");
    }
}
