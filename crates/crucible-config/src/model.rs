//! The parsed configuration model: plain data, defaults and the few pure helpers on it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The terminal CDE spawns for `Super+Return` / `Super+Shift+Return`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalConfig {
    /// The command to spawn. `None` keeps the built-in resolution (`$TERMINAL`, else `foot`).
    pub command: Option<String>,
    /// The flag a terminal takes to run a program (`-e` for most); used by the overlay and wrapper launches.
    pub exec_flag: String,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            command: None,
            exec_flag: "-e".to_string(),
        }
    }
}

/// An sRGB colour, parsed from `"#rrggbb"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// Parses exactly `#rrggbb` (hex digits, either case); anything else is `None`.
    #[must_use]
    pub fn from_hex(s: &str) -> Option<Self> {
        let digits = s.strip_prefix('#')?;
        if digits.len() != 6 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(&digits[i..i + 2], 16).ok();
        Some(Self(byte(0)?, byte(2)?, byte(4)?))
    }

    /// The opaque `[R, G, B, A]` float colour (`channel / 255`, alpha 1.0) the renderer takes.
    #[must_use]
    pub fn to_rgba_f32(self) -> [f32; 4] {
        [
            f32::from(self.0) / 255.0,
            f32::from(self.1) / 255.0,
            f32::from(self.2) / 255.0,
            1.0,
        ]
    }
}

/// Gap, focus-ring thickness and (for apps) inset fill of one window class, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassTiling {
    /// Space between the tile edge and the window content. For terminals the terminal draws it as
    /// its own padding; for apps CDE insets the window and paints the inset.
    pub gap: u32,
    /// Focus-ring thickness; never more than `gap` (clamped at parse time).
    pub ring: u32,
    /// Colour painted over the part of an app's inset the ring does not cover. Unused for terminals.
    pub fill: Rgb,
    /// Terminal only: distance the focus ring and `frame` are drawn inside the tile edge; with
    /// `ring` never more than `gap` (clamped at parse time). Always 0 for apps.
    pub ring_inset: u32,
    /// Terminal only: colour of the frame drawn at the ring position on unfocused tiles.
    /// `None` (default) draws no frame. Always `None` for apps.
    pub frame: Option<Rgb>,
}

/// Per-class tiling: terminals fill their tile, apps are inset by their gap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TilingConfig {
    /// CDE-spawned terminals.
    pub terminal: ClassTiling,
    /// Every other managed window.
    pub app: ClassTiling,
    /// Extra terminal arguments for terminal spawns (`{gap}` = the terminal gap, see
    /// [`substitute_gap`]), inserted before the exec flag.
    pub terminal_args: Vec<String>,
}

impl Default for TilingConfig {
    /// Terminal gap 6 / ring 3; app gap 0 / ring 0 with the muted chrome grey (`#737373`, the
    /// inactive stack-pip colour) as fill.
    fn default() -> Self {
        let fill = Rgb(0x73, 0x73, 0x73);
        Self {
            terminal: ClassTiling {
                gap: 6,
                ring: 3,
                fill,
                ring_inset: 0,
                frame: None,
            },
            app: ClassTiling {
                gap: 0,
                ring: 0,
                fill,
                ring_inset: 0,
                frame: None,
            },
            terminal_args: Vec::new(),
        }
    }
}

impl TilingConfig {
    /// [`Self::terminal_args`] with `{gap}` replaced by the terminal gap.
    #[must_use]
    pub fn substituted_terminal_args(&self) -> Vec<String> {
        substitute_gap(&self.terminal_args, self.terminal.gap)
    }
}

/// Replaces every `{gap}` in `args` with `gap`. Pure.
#[must_use]
pub fn substitute_gap(args: &[String], gap: u32) -> Vec<String> {
    let gap = gap.to_string();
    args.iter().map(|a| a.replace("{gap}", &gap)).collect()
}

/// `(ring, clamped)`: `ring` limited to `gap`; `clamped` is whether it was reduced. Pure.
#[must_use]
pub fn clamp_ring(gap: u32, ring: u32) -> (u32, bool) {
    if ring > gap {
        (gap, true)
    } else {
        (ring, false)
    }
}

/// `(ring_inset, clamped)`: `ring_inset` limited to `gap - ring` (callers clamp `ring` to `gap`
/// first); `clamped` is whether it was reduced. Pure.
#[must_use]
pub fn clamp_ring_inset(gap: u32, ring: u32, ring_inset: u32) -> (u32, bool) {
    let max = gap.saturating_sub(ring);
    if ring_inset > max {
        (max, true)
    } else {
        (ring_inset, false)
    }
}

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

impl OverlayStyle {
    /// The config / environment spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::N1Dark => "n1-dark",
            Self::N1Med => "n1-med",
            Self::N1Light => "n1-light",
        }
    }

    /// The opaque N1 surround colour (`(r, g, b)`), or `None` for [`OverlayStyle::Terminal`]. Must
    /// match the palette `bg` the overlay TUIs paint (`crucible::style`, which tests the match).
    #[must_use]
    pub fn surround_rgb(self) -> Option<(u8, u8, u8)> {
        match self {
            Self::Terminal => None,
            Self::N1Dark => Some((0x0e, 0x0e, 0x10)),
            Self::N1Med => Some((0x3a, 0x3a, 0x3b)),
            Self::N1Light => Some((0xf4, 0xf4, 0xf5)),
        }
    }

    /// Parses the exact spelling; anything else is `None`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "terminal" => Some(Self::Terminal),
            "n1-dark" => Some(Self::N1Dark),
            "n1-med" => Some(Self::N1Med),
            "n1-light" => Some(Self::N1Light),
            _ => None,
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

/// Overlay args CDE adds by default when the terminal program is ghostty.
pub const GHOSTTY_OVERLAY_DEFAULT_ARGS: [&str; 3] = [
    "--window-padding-color=extend-always",
    "--window-padding-balance=true",
    // The overlays (picker, launcher, settings) are CDE TUIs: no ghostty keybind may take a key
    // (Escape, Ctrl+Shift+…) before them. A CLI `keybind=clear` clears the user config's binds too.
    "--keybind=clear",
];

/// Overlay args CDE adds by default when the terminal program is foot (1.28).
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
/// background (see [`OverlayStyle::surround_rgb`]).
pub const FOOT_OVERLAY_DEFAULT_ARGS: [&str; 56] = [
    "-o",
    "key-bindings.scrollback-up-page=none",
    "-o",
    "key-bindings.scrollback-up-half-page=none",
    "-o",
    "key-bindings.scrollback-up-line=none",
    "-o",
    "key-bindings.scrollback-down-page=none",
    "-o",
    "key-bindings.scrollback-down-half-page=none",
    "-o",
    "key-bindings.scrollback-down-line=none",
    "-o",
    "key-bindings.scrollback-home=none",
    "-o",
    "key-bindings.scrollback-end=none",
    "-o",
    "key-bindings.clipboard-copy=none",
    "-o",
    "key-bindings.clipboard-paste=none",
    "-o",
    "key-bindings.primary-paste=none",
    "-o",
    "key-bindings.search-start=none",
    "-o",
    "key-bindings.font-increase=none",
    "-o",
    "key-bindings.font-decrease=none",
    "-o",
    "key-bindings.font-reset=none",
    "-o",
    "key-bindings.spawn-terminal=none",
    "-o",
    "key-bindings.minimize=none",
    "-o",
    "key-bindings.maximize=none",
    "-o",
    "key-bindings.fullscreen=none",
    "-o",
    "key-bindings.show-urls-launch=none",
    "-o",
    "key-bindings.show-urls-copy=none",
    "-o",
    "key-bindings.show-urls-persistent=none",
    "-o",
    "key-bindings.prompt-prev=none",
    "-o",
    "key-bindings.prompt-next=none",
    "-o",
    "key-bindings.unicode-input=none",
    "-o",
    "key-bindings.color-theme-toggle=none",
    "-o",
    "key-bindings.noop=none",
    "-o",
    "key-bindings.quit=none",
];

impl OverlayConfig {
    /// The terminal args to use for overlay spawns with terminal `program`: the explicit list when
    /// set (even if empty), else the ghostty defaults when the basename is exactly `ghostty` or
    /// `ghostty-cde`, else the foot defaults when it is exactly `foot`, else nothing.
    #[must_use]
    pub fn effective_terminal_args(&self, program: &str) -> Vec<String> {
        if let Some(list) = &self.terminal_args {
            return list.clone();
        }
        let name = std::path::Path::new(program).file_name();
        let is_foot = name.is_some_and(|n| n == "foot");
        let defaults: &[&str] = if name.is_some_and(|n| n == "ghostty" || n == "ghostty-cde") {
            &GHOSTTY_OVERLAY_DEFAULT_ARGS
        } else if is_foot {
            &FOOT_OVERLAY_DEFAULT_ARGS
        } else {
            &[]
        };
        let mut args: Vec<String> = defaults.iter().map(|s| (*s).to_string()).collect();
        // foot has no ghostty `--window-padding-color=extend-always`: the strip left where the cell
        // grid does not divide the overlay shows foot's own background, so make that the opaque
        // N1 surround (no gap between the surround and the border).
        if is_foot && let Some((r, g, b)) = self.style.surround_rgb() {
            args.extend([
                "-o".to_string(),
                format!("colors-dark.background={r:02x}{g:02x}{b:02x}"),
                "-o".to_string(),
                "colors-dark.alpha=1.0".to_string(),
            ]);
        }
        args
    }
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

impl HeaderMode {
    /// The config spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Always => "always",
            Self::Toggle => "toggle",
        }
    }

    /// Parses the exact spelling; anything else is `None`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "off" => Some(Self::Off),
            "always" => Some(Self::Always),
            "toggle" => Some(Self::Toggle),
            _ => None,
        }
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
    /// Divider colour (`divider ... color=`); defaults to [`DEFAULT_DIVIDER_COLOR`].
    pub divider_color: Rgb,
    /// Whether labels of slot-pinned workspaces carry a slot badge (`slots`).
    pub slots: bool,
}

impl Default for HeaderConfig {
    fn default() -> Self {
        Self {
            mode: HeaderMode::Toggle,
            font: DEFAULT_HEADER_FONT.to_string(),
            size: 14,
            divider: 0,
            divider_inset: 0,
            divider_color: DEFAULT_DIVIDER_COLOR,
            slots: false,
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

/// The workspace picker's settings (`picker { … }`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PickerConfig {
    /// Directories whose immediate subdirectories are workspace-picker candidates.
    pub roots: Vec<PathBuf>,
    /// Whether the workspace picker shows `⌘<n>` badges on pinned rows (`slots`).
    pub slots: bool,
}

/// The launcher's settings (`launcher { … }`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LauncherConfig {
    /// Extra TUI program names / `*` globs from `tui …`, in file order, first occurrence kept on
    /// duplicates. Each entry is non-empty with no `/` and no `:`.
    pub tui: Vec<String>,
}

/// Lowest accepted `output` scale.
pub const OUTPUT_SCALE_MIN: f64 = 0.5;
/// Highest accepted `output` scale.
pub const OUTPUT_SCALE_MAX: f64 = 4.0;
/// Fractional-scale steps per 1.0 (the `wp_fractional_scale_v1` unit).
pub const OUTPUT_SCALE_STEPS: u32 = 120;

/// One output's settings (`output "<connector>" { … }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputConfig {
    /// The scale in 1/120 steps (120 = 1.0), quantized from the configured number.
    pub scale_120: u32,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            scale_120: OUTPUT_SCALE_STEPS,
        }
    }
}

impl OutputConfig {
    /// The scale as a float (`scale_120 / 120`).
    #[must_use]
    pub fn scale(&self) -> f64 {
        f64::from(self.scale_120) / f64::from(OUTPUT_SCALE_STEPS)
    }
}

/// The parsed CDE configuration. `Default` is CDE's behaviour with no config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Terminal command and exec flag.
    pub terminal: TerminalConfig,
    /// Per-class gap, ring and terminal arguments.
    pub tiling: TilingConfig,
    /// Whether pointer motion moves focus.
    pub focus_follows_mouse: bool,
    /// Whether startup reopens the workspaces open at the last exit (`restore-session`).
    pub restore_session: bool,
    /// Workspace-picker roots and badge setting.
    pub picker: PickerConfig,
    /// Overlay border, style and terminal arguments.
    pub overlay: OverlayConfig,
    /// Workspace header strip mode, font and size.
    pub header: HeaderConfig,
    /// The screen locker the `lock` action spawns (`lock-command`), split on whitespace into
    /// program + arguments (no shell quoting). Default `"swaylock"`.
    pub lock_command: String,
    /// Shell command lines run once per CDE start (`autostart`, repeatable), in file order, via
    /// `/bin/sh -c`. Default empty.
    pub autostart: Vec<String>,
    /// Extra TUI program names for the launcher.
    pub launcher: LauncherConfig,
    /// Binding overrides as raw `(chord, action)` strings, in file order.
    pub binds: Vec<(String, String)>,
    /// Per-output settings keyed by connector name (`output "DP-1" { … }`).
    pub outputs: BTreeMap<String, OutputConfig>,
}

impl Config {
    /// The configured scale for the output with connector name `connector`, or `1.0` when no
    /// `output` block names it.
    #[must_use]
    pub fn output_scale(&self, connector: &str) -> f64 {
        self.outputs.get(connector).map_or(1.0, OutputConfig::scale)
    }

    /// The largest scale among all configured `output` blocks (mapped or not), or `1.0` with none.
    #[must_use]
    pub fn max_output_scale(&self) -> f64 {
        self.outputs
            .values()
            .map(OutputConfig::scale)
            .fold(1.0, f64::max)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            terminal: TerminalConfig::default(),
            tiling: TilingConfig::default(),
            focus_follows_mouse: true,
            restore_session: true,
            picker: PickerConfig::default(),
            overlay: OverlayConfig::default(),
            header: HeaderConfig::default(),
            launcher: LauncherConfig::default(),
            binds: Vec::new(),
            outputs: BTreeMap::new(),
            lock_command: "swaylock".to_string(),
            autostart: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_to_rgba_f32_is_opaque_and_unit_scaled() {
        assert_eq!(Rgb(0, 0, 0).to_rgba_f32(), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(Rgb(255, 255, 255).to_rgba_f32(), [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(
            Rgb(0x3a, 0x80, 0x01).to_rgba_f32(),
            [58.0 / 255.0, 128.0 / 255.0, 1.0 / 255.0, 1.0]
        );
    }

    #[test]
    fn defaults_match_todays_behaviour() {
        let c = Config::default();
        assert_eq!(c.terminal.command, None);
        assert_eq!(c.terminal.exec_flag, "-e");
        assert_eq!(c.tiling.terminal.gap, 6);
        assert_eq!(c.tiling.terminal.ring, 3);
        assert_eq!((c.tiling.app.gap, c.tiling.app.ring), (0, 0));
        assert_eq!(c.tiling.app.fill, Rgb(0x73, 0x73, 0x73));
        assert!(c.tiling.terminal_args.is_empty());
        assert!(c.focus_follows_mouse);
        assert!(c.picker.roots.is_empty());
        assert!(!c.picker.slots);
        assert!(c.launcher.tui.is_empty());
        assert!(c.binds.is_empty());
    }

    #[test]
    fn ring_clamps_to_gap_and_gap_substitutes() {
        assert_eq!(clamp_ring(6, 3), (3, false));
        assert_eq!(clamp_ring(2, 9), (2, true));
        assert_eq!(clamp_ring_inset(9, 1, 4), (4, false));
        assert_eq!(clamp_ring_inset(6, 3, 5), (3, true));
        assert_eq!(clamp_ring_inset(2, 2, 1), (0, true));
        assert_eq!(
            substitute_gap(&["a={gap}-{gap}".to_string(), "b".to_string()], 7),
            vec!["a=7-7".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn rgb_parses_exactly_six_hex_digits() {
        assert_eq!(Rgb::from_hex("#10aBcD"), Some(Rgb(0x10, 0xab, 0xcd)));
        assert_eq!(Rgb::from_hex("10aBcD"), None);
        assert_eq!(Rgb::from_hex("#10ab"), None);
    }

    #[test]
    fn header_font_path_versus_family() {
        let mut h = HeaderConfig::default();
        assert_eq!(h.font_path(), None);
        h.font = "/usr/share/fonts/x.ttf".to_string();
        assert_eq!(h.font_path(), Some(Path::new("/usr/share/fonts/x.ttf")));
    }
}
