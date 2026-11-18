//! The workspace header strip: its mode, active style, start state and settings.

use super::*;

/// When the workspace header strip is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderMode {
    /// Never shown.
    Off,
    /// Always shown; its height is reserved at the top of each output.
    Always,
    /// Hidden until `toggle-header` (Super+B) shows it; while shown its height is reserved at the top
    /// of each output (the default).
    Toggle,
}

impl Named for HeaderMode {
    /// Every mode with its config spelling, in the order error messages list them.
    const NAMES: &'static [(Self, &'static str)] = &[
        (Self::Off, "off"),
        (Self::Always, "always"),
        (Self::Toggle, "toggle"),
    ];
}

/// How the active workspace label is drawn in the header strip (`header { active }`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HeaderActive {
    /// A solid highlight block behind the label (the default).
    #[default]
    Fill,
    /// No block; the label is drawn in the highlight colour.
    Text,
    /// Like `Text`, in the bold face of the header font.
    TextBold,
}

impl Named for HeaderActive {
    /// Every value with its config spelling, in the order error messages list them.
    const NAMES: &'static [(Self, &'static str)] = &[
        (Self::Fill, "fill"),
        (Self::Text, "text"),
        (Self::TextBold, "text-bold"),
    ];
}

/// Whether a never-toggled output's `toggle`-mode strip starts shown or hidden (`header { start }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderStart {
    /// The strip starts shown.
    Shown,
    /// The strip starts hidden (the default).
    Hidden,
}

impl Named for HeaderStart {
    /// Every value with its config spelling, in the order error messages list them.
    const NAMES: &'static [(Self, &'static str)] =
        &[(Self::Shown, "shown"), (Self::Hidden, "hidden")];
}

impl HeaderStart {
    /// Whether this start state is "shown".
    #[must_use]
    pub fn is_shown(self) -> bool {
        self == Self::Shown
    }
}

/// Smallest accepted header font size, in logical pixels.
pub const HEADER_SIZE_MIN: u32 = 6;
/// Largest accepted header font size, in logical pixels.
pub const HEADER_SIZE_MAX: u32 = 96;
/// Largest accepted header divider width, in logical pixels (`0` = no divider).
pub const HEADER_DIVIDER_MAX: u32 = 16;
/// Largest accepted header divider inset (top and bottom), in logical pixels.
pub const HEADER_DIVIDER_INSET_MAX: u32 = 32;
/// Default header divider colour: the inactive label colour.
pub const DEFAULT_DIVIDER_COLOR: Rgb = Rgb(0xcd, 0xd0, 0xd6);
/// Default header font family (resolved through `fc-match`).
pub const DEFAULT_HEADER_FONT: &str = "DejaVu Sans";

/// Workspace header strip settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderConfig {
    /// When the strip is shown.
    pub mode: HeaderMode,
    /// Initial `toggle`-mode state of an output with no saved preference (`start`).
    pub start: HeaderStart,
    /// How the active workspace label is drawn (`active`).
    pub active: HeaderActive,
    /// A font file path (contains `/`) or a fontconfig family name.
    pub font: String,
    /// Font size in logical pixels, within [`HEADER_SIZE_MIN`]..=[`HEADER_SIZE_MAX`].
    pub size: u32,
    /// Width in logical pixels of the divider drawn between workspace labels, within
    /// `0..=`[`HEADER_DIVIDER_MAX`]; `0` draws none.
    pub divider: u32,
    /// Logical pixels trimmed from the top and bottom of each divider, within
    /// `0..=`[`HEADER_DIVIDER_INSET_MAX`]; no effect when `divider` is 0.
    pub divider_inset: u32,
    /// Explicit divider colour (`divider ... color=`), which always wins; `None` takes the label
    /// colour ([`DEFAULT_DIVIDER_COLOR`]) without a `style` and the palette `line` with one.
    pub divider_color: Option<Rgb>,
    /// Whether labels of slot-pinned workspaces carry a slot badge (`slots`).
    pub slots: bool,
    /// Strip, label and accent colours (`style`); `None` keeps the legacy header colours.
    pub style: Option<crate::surface::SurfaceStyle>,
    /// Highlight accent (`accent`); with no `style`, a non-default value replaces only the
    /// highlight colour.
    pub accent: crate::surface::SurfaceAccent,
}

impl Default for HeaderConfig {
    fn default() -> Self {
        Self {
            mode: HeaderMode::Toggle,
            start: HeaderStart::Hidden,
            active: HeaderActive::Fill,
            font: DEFAULT_HEADER_FONT.to_string(),
            size: 14,
            divider: 0,
            divider_inset: 0,
            divider_color: None,
            slots: false,
            style: None,
            accent: crate::surface::SurfaceAccent::Style,
        }
    }
}

impl HeaderConfig {
    /// The font as a path override when it contains `/`, else `None` (it is a family name).
    #[must_use]
    pub fn font_path(&self) -> Option<&Path> {
        self.font.contains('/').then(|| Path::new(&self.font))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tests::assert_names_round_trip;

    #[test]
    fn header_mode_names_round_trip() {
        // The exhaustive match fails to compile when a variant is added without listing it here.
        let _ = |m: HeaderMode| match m {
            HeaderMode::Off | HeaderMode::Always | HeaderMode::Toggle => (),
        };
        assert_names_round_trip(&[HeaderMode::Off, HeaderMode::Always, HeaderMode::Toggle]);
    }

    #[test]
    fn header_active_names_round_trip() {
        let _ = |m: HeaderActive| match m {
            HeaderActive::Fill | HeaderActive::Text | HeaderActive::TextBold => (),
        };
        assert_names_round_trip(&[
            HeaderActive::Fill,
            HeaderActive::Text,
            HeaderActive::TextBold,
        ]);
    }

    #[test]
    fn header_start_names_round_trip() {
        let _ = |m: HeaderStart| match m {
            HeaderStart::Shown | HeaderStart::Hidden => (),
        };
        assert_names_round_trip(&[HeaderStart::Shown, HeaderStart::Hidden]);
    }

    #[test]
    fn header_font_path_versus_family() {
        let mut h = HeaderConfig::default();
        assert_eq!(h.font_path(), None);
        h.font = "/usr/share/fonts/x.ttf".to_string();
        assert_eq!(h.font_path(), Some(Path::new("/usr/share/fonts/x.ttf")));
    }
}
