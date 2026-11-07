//! The overlay model: style and palette, the terminal programs CDE knows defaults for, and the
//! terminal args an overlay spawn gets.

use std::path::Path;

use crate::model::{Named, Rgb};

/// The four colours of one N1 palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Surround / background.
    pub bg: Rgb,
    /// Foreground text.
    pub text: Rgb,
    /// Selected-row background and rules.
    pub line: Rgb,
    /// Accent (selected-row text, overlay border).
    pub accent: Rgb,
}

/// The `n1-dark` palette.
pub const N1_DARK: Palette = Palette {
    bg: Rgb(0x0e, 0x0e, 0x10),
    text: Rgb(0xe9, 0xe9, 0xea),
    line: Rgb(0x37, 0x38, 0x3b),
    accent: Rgb(0xff, 0x8a, 0x3d),
};
/// The `n1-med` palette (the default style).
pub const N1_MED: Palette = Palette {
    bg: Rgb(0x3a, 0x3a, 0x3b),
    text: Rgb(0xf3, 0xf3, 0xf4),
    line: Rgb(0x66, 0x66, 0x68),
    accent: Rgb(0xff, 0x8a, 0x3d),
};
/// The `n1-light` palette.
pub const N1_LIGHT: Palette = Palette {
    bg: Rgb(0xf4, 0xf4, 0xf5),
    text: Rgb(0x19, 0x1a, 0x1b),
    line: Rgb(0xa5, 0xa5, 0xa8),
    accent: Rgb(0xf0, 0x77, 0x14),
};

/// The look of a CDE overlay (picker, launcher, keys): the terminal's own colours or one of the
/// N1 Console surrounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayStyle {
    /// The terminal's inherited look.
    Terminal,
    /// Opaque dark N1 surround.
    N1Dark,
    /// Opaque medium N1 surround (the default).
    N1Med,
    /// Opaque light N1 surround.
    N1Light,
}

impl Named for OverlayStyle {
    /// Every style with its config / environment spelling, in the order error messages list them.
    const NAMES: &'static [(Self, &'static str)] = &[
        (Self::Terminal, "terminal"),
        (Self::N1Dark, "n1-dark"),
        (Self::N1Med, "n1-med"),
        (Self::N1Light, "n1-light"),
    ];
}

impl OverlayStyle {
    /// The N1 palette the overlay TUIs paint and the compositor derives its border from, or
    /// `None` for [`OverlayStyle::Terminal`]. Its `bg` is the opaque surround foot gets.
    #[must_use]
    pub fn palette(self) -> Option<Palette> {
        match self {
            Self::Terminal => None,
            Self::N1Dark => Some(N1_DARK),
            Self::N1Med => Some(N1_MED),
            Self::N1Light => Some(N1_LIGHT),
        }
    }
}

/// Overlay presentation settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayConfig {
    /// Whether a border is drawn around the overlay.
    pub border: bool,
    /// The overlay style.
    pub style: OverlayStyle,
    /// Extra terminal arguments inserted before the exec flag on overlay spawns. `None` means not
    /// set: [`OverlayConfig::effective_terminal_args`] then applies the built-in ghostty or foot
    /// default for those terminals.
    /// `Some(list)` (even empty) replaces the default.
    pub terminal_args: Option<Vec<String>>,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            border: true,
            style: OverlayStyle::N1Med,
            terminal_args: None,
        }
    }
}

/// Overlay args CDE adds by default when the terminal program is ghostty.
pub const GHOSTTY_OVERLAY_DEFAULT_ARGS: [&str; 3] = [
    "--window-padding-color=extend-always",
    "--window-padding-balance=true",
    // The overlays (picker, launcher, settings) are CDE TUIs: no ghostty keybind may take a key
    // (Escape, Ctrl+Shift+…) before them. A CLI `keybind=clear` clears the user config's binds too.
    "--keybind=clear",
];

/// The foot (1.28) `[key-bindings]` actions the overlay defaults set to `none`, in emission order.
///
/// The overlays (picker, launcher, settings) are CDE TUIs: no foot key binding may take a key
/// (Escape, Ctrl+Shift+...) before them. foot has no `keybind=clear`, so every default action
/// (and the other plain actions) is set to `none` with a `-o key-bindings.<action>=none` pair;
/// this also overrides the user's foot.ini for these actions (`pipe-*`, `regex-*` and
/// `color-theme-switch-*` binds there are not cleared).
/// foot 1.28's own defaults in `[key-bindings]` are scrollback-up/down-page, clipboard-copy/paste,
/// primary-paste, search-start, font-increase/decrease/reset, spawn-terminal, show-urls-launch,
/// unicode-input and prompt-prev/next; the rest here are the actions that have no default but
/// that a user config could bind. `[search-bindings]` and `[url-bindings]` are only reachable
/// through `search-start` / `show-urls-*` (disabled here), and `[mouse-bindings]` take no keyboard
/// key (modifiers only), so none of those are touched.
/// [`OverlayConfig::effective_terminal_args`] appends the N1 style's opaque surround as foot's
/// background (see [`OverlayStyle::palette`]).
pub const FOOT_UNBOUND_ACTIONS: [&str; 28] = [
    "scrollback-up-page",
    "scrollback-up-half-page",
    "scrollback-up-line",
    "scrollback-down-page",
    "scrollback-down-half-page",
    "scrollback-down-line",
    "scrollback-home",
    "scrollback-end",
    "clipboard-copy",
    "clipboard-paste",
    "primary-paste",
    "search-start",
    "font-increase",
    "font-decrease",
    "font-reset",
    "spawn-terminal",
    "minimize",
    "maximize",
    "fullscreen",
    "show-urls-launch",
    "show-urls-copy",
    "show-urls-persistent",
    "prompt-prev",
    "prompt-next",
    "unicode-input",
    "color-theme-toggle",
    "noop",
    "quit",
];

/// The terminal programs CDE has overlay defaults for, classified by exact basename.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalKind {
    /// `ghostty` or `ghostty-cde`.
    Ghostty,
    /// `foot`.
    Foot,
    /// `cterm` (crucible-term): no default-binding args, but with an N1 style the opaque surround
    /// as its background plus zero centred padding (see [`Self::overlay_args`]); its terminal
    /// spawns get a window-identity grant.
    Cterm,
    /// `urxvt` or `rxvt-unicode`: no scrollbar (`+sb`) whatever the style, and with an N1 style
    /// the opaque surround as its background (`-bg #rrggbb`) and a zero internal border (`-b 0`).
    Urxvt,
    /// Anything else (including `footclient`, `ghostty-foo`, an empty program): no defaults.
    Other,
}

impl TerminalKind {
    /// Classifies the terminal `program` (a bare name or a path) by its exact basename.
    #[must_use]
    pub fn of(program: &str) -> Self {
        match Path::new(program).file_name().and_then(|n| n.to_str()) {
            Some("ghostty" | "ghostty-cde") => Self::Ghostty,
            Some("foot") => Self::Foot,
            Some("cterm") => Self::Cterm,
            Some("urxvt" | "rxvt-unicode") => Self::Urxvt,
            _ => Self::Other,
        }
    }

    /// The args CDE adds by default for this terminal, whatever the overlay style.
    #[must_use]
    pub fn overlay_default_args(self) -> Vec<String> {
        match self {
            Self::Ghostty => GHOSTTY_OVERLAY_DEFAULT_ARGS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            Self::Foot => FOOT_UNBOUND_ACTIONS
                .iter()
                .flat_map(|action| ["-o".to_string(), format!("key-bindings.{action}=none")])
                .collect(),
            // An overlay never scrolls back; urxvt shows a scrollbar by default.
            Self::Urxvt => vec!["+sb".to_string()],
            Self::Cterm | Self::Other => Vec::new(),
        }
    }

    /// [`Self::overlay_default_args`] plus what depends on the overlay `style`. foot has no
    /// ghostty `--window-padding-color=extend-always`: the strip left where the cell grid does
    /// not divide the overlay shows foot's own background, so that becomes the opaque N1
    /// surround (no gap between the surround and the border).
    #[must_use]
    pub fn overlay_args(self, style: OverlayStyle) -> Vec<String> {
        let mut args = self.overlay_default_args();
        if let Some(Rgb(r, g, b)) = style.palette().map(|p| p.bg) {
            match self {
                Self::Foot => args.extend([
                    "-o".to_string(),
                    format!("colors-dark.background={r:02x}{g:02x}{b:02x}"),
                    "-o".to_string(),
                    "colors-dark.alpha=1.0".to_string(),
                ]),
                // cterm paints its whole buffer (padding and the cell-grid remainder) in
                // `theme.bg` at `alpha`, so the same opaque surround closes the gap; zero
                // padding with `center` splits the remainder evenly on every side.
                Self::Cterm => args.extend([
                    "-o".to_string(),
                    format!("colors.background={r:02x}{g:02x}{b:02x}"),
                    "-o".to_string(),
                    "colors.alpha=1.0".to_string(),
                    "-o".to_string(),
                    "pad=0x0 center".to_string(),
                ]),
                // urxvt's internal border and the strip the cell grid leaves show its own
                // background, so the opaque surround closes the gap; `-b 0` drops the border.
                Self::Urxvt => args.extend([
                    "-bg".to_string(),
                    format!("#{r:02x}{g:02x}{b:02x}"),
                    "-b".to_string(),
                    "0".to_string(),
                ]),
                Self::Ghostty | Self::Other => {}
            }
        }
        args
    }
}

impl OverlayConfig {
    /// The terminal args to use for overlay spawns with terminal `program`: the explicit list when
    /// set (even if empty), else the [`TerminalKind`] defaults, else nothing.
    #[must_use]
    pub fn effective_terminal_args(&self, program: &str) -> Vec<String> {
        match &self.terminal_args {
            Some(list) => list.clone(),
            None => TerminalKind::of(program).overlay_args(self.style),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_effective_args_resolution() {
        let defaults: Vec<String> = GHOSTTY_OVERLAY_DEFAULT_ARGS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let unset = OverlayConfig::default();
        for p in ["ghostty", "/usr/bin/ghostty", "/tmp/cde/ghostty-cde"] {
            assert_eq!(unset.effective_terminal_args(p), defaults, "{p}");
        }
        let foot = TerminalKind::Foot.overlay_default_args();
        assert_eq!(foot.len(), 56);
        assert!(
            foot.chunks(2).all(|c| c[0] == "-o"
                && c[1].starts_with("key-bindings.")
                && c[1].ends_with("=none"))
        );
        // The default n1-med style adds its opaque surround as foot's background.
        let mut foot_med = foot.clone();
        foot_med.extend(
            [
                "-o",
                "colors-dark.background=3a3a3b",
                "-o",
                "colors-dark.alpha=1.0",
            ]
            .map(String::from),
        );
        for p in ["foot", "/x/foot", "/home/u/.local/opt/foot/bin/foot"] {
            assert_eq!(unset.effective_terminal_args(p), foot_med, "{p}");
        }
        for p in ["urxvt", "/usr/bin/urxvt", "rxvt-unicode"] {
            assert_eq!(
                unset.effective_terminal_args(p),
                ["+sb", "-bg", "#3a3a3b", "-b", "0"].map(String::from),
                "{p}"
            );
        }
        let term_style = OverlayConfig {
            style: OverlayStyle::Terminal,
            ..OverlayConfig::default()
        };
        assert_eq!(term_style.effective_terminal_args("foot"), foot);
        assert_eq!(
            term_style.effective_terminal_args("urxvt"),
            ["+sb"].map(String::from)
        );
        assert_eq!(term_style.effective_terminal_args("ghostty"), defaults);
        let dark = OverlayConfig {
            style: OverlayStyle::N1Dark,
            ..OverlayConfig::default()
        };
        assert!(
            dark.effective_terminal_args("foot")
                .contains(&"colors-dark.background=0e0e10".to_string())
        );
        // cterm: opaque N1 surround, zero padding centred; none for the terminal style.
        let cterm_med: Vec<String> = [
            "-o",
            "colors.background=3a3a3b",
            "-o",
            "colors.alpha=1.0",
            "-o",
            "pad=0x0 center",
        ]
        .map(String::from)
        .to_vec();
        for p in ["cterm", "/x/cterm"] {
            assert_eq!(unset.effective_terminal_args(p), cterm_med, "{p}");
        }
        assert!(term_style.effective_terminal_args("cterm").is_empty());
        assert!(
            dark.effective_terminal_args("cterm")
                .contains(&"colors.background=0e0e10".to_string())
        );
        for p in [
            "footclient",
            "foot-wrapper",
            "alacritty",
            "ghostty-foo",
            "myghostty",
            "",
            "/a/..",
        ] {
            assert!(unset.effective_terminal_args(p).is_empty(), "{p}");
        }
        let empty = OverlayConfig {
            terminal_args: Some(vec![]),
            ..OverlayConfig::default()
        };
        assert!(empty.effective_terminal_args("ghostty").is_empty());
        assert!(empty.effective_terminal_args("foot").is_empty());
        assert!(empty.effective_terminal_args("cterm").is_empty());
        let x = OverlayConfig {
            terminal_args: Some(vec!["--x".into()]),
            ..OverlayConfig::default()
        };
        assert_eq!(x.effective_terminal_args("ghostty"), vec!["--x"]);
        assert_eq!(x.effective_terminal_args("foot"), vec!["--x"]);
        assert_eq!(x.effective_terminal_args("cterm"), vec!["--x"]);
    }

    #[test]
    fn terminal_kind_classifies_by_exact_basename() {
        for p in ["ghostty", "/usr/bin/ghostty", "/tmp/cde/ghostty-cde"] {
            assert_eq!(TerminalKind::of(p), TerminalKind::Ghostty, "{p}");
        }
        for p in ["foot", "/x/foot", "/home/u/.local/opt/foot/bin/foot"] {
            assert_eq!(TerminalKind::of(p), TerminalKind::Foot, "{p}");
        }
        for p in [
            "footclient",
            "foot-wrapper",
            "alacritty",
            "ghostty-foo",
            "myghostty",
            "",
            "/a/..",
        ] {
            assert_eq!(TerminalKind::of(p), TerminalKind::Other, "{p}");
        }
        assert!(TerminalKind::Other.overlay_default_args().is_empty());
    }

    #[test]
    fn foot_defaults_keep_todays_order() {
        let foot = TerminalKind::Foot.overlay_default_args();
        assert_eq!(
            foot[..4],
            [
                "-o",
                "key-bindings.scrollback-up-page=none",
                "-o",
                "key-bindings.scrollback-up-half-page=none"
            ]
        );
        assert_eq!(
            foot[52..],
            [
                "-o",
                "key-bindings.noop=none",
                "-o",
                "key-bindings.quit=none"
            ]
        );
    }

    #[test]
    fn palette_is_the_n1_table_and_terminal_has_none() {
        assert_eq!(OverlayStyle::Terminal.palette(), None);
        assert_eq!(OverlayStyle::N1Dark.palette(), Some(N1_DARK));
        assert_eq!(OverlayStyle::N1Med.palette(), Some(N1_MED));
        assert_eq!(OverlayStyle::N1Light.palette(), Some(N1_LIGHT));
        // The compositor's border accents: dark/med #ff8a3d, light #f07714.
        assert_eq!(N1_DARK.accent, Rgb(0xff, 0x8a, 0x3d));
        assert_eq!(N1_LIGHT.accent, Rgb(0xf0, 0x77, 0x14));
    }

    #[test]
    fn style_names_roundtrip_in_table_order() {
        let names: Vec<&str> = OverlayStyle::NAMES.iter().map(|(_, n)| *n).collect();
        assert_eq!(names, ["terminal", "n1-dark", "n1-med", "n1-light"]);
        for &(style, name) in OverlayStyle::NAMES {
            assert_eq!(style.as_str(), name);
            assert_eq!(OverlayStyle::from_name(name), Some(style));
        }
        assert_eq!(OverlayStyle::from_name("N1-Med"), None);
    }
}
