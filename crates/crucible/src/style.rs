//! Overlay styling shared by the `pick`, `launch` and `keys` TUIs.
//!
//! The compositor sets `OVERLAY_STYLE_ENV` to `terminal`, `n1-dark`, `n1-med` or `n1-light` on
//! overlay children. `Terminal` (also the fallback for unset, empty or unknown values) makes every
//! helper return the empty string (or today's unstyled sequence), so output stays byte-identical
//! to the unstyled TUIs. The N1 styles paint truecolor base attributes before every clear and
//! re-apply them after every in-frame reset, so no cell falls back to the terminal's default
//! background.

use crucible_compositor::ipc::OVERLAY_STYLE_ENV;
use crucible_config::OverlayStyle;

/// An RGB triple.
pub type Rgb = (u8, u8, u8);

/// The four colours of one N1 palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub bg: Rgb,
    pub text: Rgb,
    pub line: Rgb,
    pub accent: Rgb,
}

pub const N1_DARK: Palette = Palette {
    bg: (0x0e, 0x0e, 0x10),
    text: (0xe9, 0xe9, 0xea),
    line: (0x37, 0x38, 0x3b),
    accent: (0xff, 0x8a, 0x3d),
};
pub const N1_MED: Palette = Palette {
    bg: (0x3a, 0x3a, 0x3b),
    text: (0xf3, 0xf3, 0xf4),
    line: (0x66, 0x66, 0x68),
    accent: (0xff, 0x8a, 0x3d),
};
pub const N1_LIGHT: Palette = Palette {
    bg: (0xf4, 0xf4, 0xf5),
    text: (0x19, 0x1a, 0x1b),
    line: (0xa5, 0xa5, 0xa8),
    accent: (0xf0, 0x77, 0x14),
};

/// The overlay style.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Terminal,
    N1(Palette),
}

impl From<OverlayStyle> for Style {
    fn from(style: OverlayStyle) -> Style {
        match style {
            OverlayStyle::Terminal => Style::Terminal,
            OverlayStyle::N1Dark => Style::N1(N1_DARK),
            OverlayStyle::N1Med => Style::N1(N1_MED),
            OverlayStyle::N1Light => Style::N1(N1_LIGHT),
        }
    }
}

fn fg((r, g, b): Rgb) -> String {
    format!("\x1b[38;2;{r};{g};{b}m")
}

fn bg((r, g, b): Rgb) -> String {
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

    /// The style from `OVERLAY_STYLE_ENV`.
    #[must_use]
    pub fn from_env() -> Style {
        Style::parse(std::env::var(OVERLAY_STYLE_ENV).ok().as_deref())
    }

    /// Base attributes: truecolor foreground and background (empty for `Terminal`).
    #[must_use]
    pub fn base(&self) -> String {
        match self {
            Style::Terminal => String::new(),
            Style::N1(p) => format!("{}{}", fg(p.text), bg(p.bg)),
        }
    }

    /// Attribute reset (empty for `Terminal`).
    #[must_use]
    pub fn reset(&self) -> String {
        match self {
            Style::Terminal => String::new(),
            Style::N1(_) => "\x1b[0m".to_string(),
        }
    }

    /// Start of a frame: home, base attributes, then erase-in-display (painted with the style
    /// background).
    #[must_use]
    pub fn frame_start(&self) -> String {
        format!("\x1b[H{}\x1b[2J", self.base())
    }

    /// A selected row: reverse video for `Terminal`; the `line` background with the `accent`
    /// foreground for N1, followed by the base attributes again.
    #[must_use]
    pub fn highlight(&self, text: &str) -> String {
        match self {
            Style::Terminal => format!("\x1b[7m{text}\x1b[0m"),
            Style::N1(p) => format!("{}{}{text}\x1b[0m{}", bg(p.line), fg(p.accent), self.base()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn config_surround_matches_palette_bg() {
        // crucible-config hands foot this colour as its overlay background; it must be the bg the
        // overlay TUIs paint, or the leftover cell-grid strip shows as a gap.
        for s in [
            OverlayStyle::N1Dark,
            OverlayStyle::N1Med,
            OverlayStyle::N1Light,
        ] {
            let Style::N1(p) = Style::from(s) else {
                unreachable!()
            };
            assert_eq!(s.surround_rgb(), Some(p.bg), "{}", s.as_str());
        }
        assert_eq!(OverlayStyle::Terminal.surround_rgb(), None);
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
    fn terminal_helpers_are_unstyled() {
        let s = Style::Terminal;
        assert_eq!(s.base(), "");
        assert_eq!(s.reset(), "");
        assert_eq!(s.frame_start(), "\x1b[H\x1b[2J");
        assert_eq!(s.highlight("x"), "\x1b[7mx\x1b[0m");
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
