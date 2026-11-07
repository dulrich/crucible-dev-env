//! The parsed configuration model: plain data, defaults and the few pure helpers on it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::overlay::OverlayConfig;

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

/// Tiling of CDE-spawned terminals, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalTiling {
    /// Space between the tile edge and the terminal content; the terminal draws it as its own
    /// padding.
    pub gap: u32,
    /// Focus-ring thickness; never more than `gap` (clamped at parse time).
    pub ring: u32,
    /// Distance the focus ring and `frame` are drawn inside the tile edge; with `ring` never more
    /// than `gap` (clamped at parse time).
    pub ring_inset: u32,
    /// Colour of the frame drawn at the ring position on unfocused tiles. `None` (default) draws
    /// no frame.
    pub frame: Option<Rgb>,
}

/// Tiling of every other managed window, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppTiling {
    /// Space between the tile edge and the window; CDE insets the window and paints the inset.
    pub gap: u32,
    /// Focus-ring thickness; never more than `gap` (clamped at parse time).
    pub ring: u32,
    /// Colour painted over the part of the inset the ring does not cover.
    pub fill: Rgb,
}

/// Per-class tiling: terminals fill their tile, apps are inset by their gap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TilingConfig {
    /// CDE-spawned terminals.
    pub terminal: TerminalTiling,
    /// Every other managed window.
    pub app: AppTiling,
    /// Extra terminal arguments for terminal spawns (`{gap}` = the terminal gap, see
    /// [`substitute_gap`]), inserted before the exec flag.
    pub terminal_args: Vec<String>,
}

impl Default for TilingConfig {
    /// Terminal gap 6 / ring 3; app gap 0 / ring 0 with the muted chrome grey (`#737373`, the
    /// inactive stack-pip colour) as fill.
    fn default() -> Self {
        Self {
            terminal: TerminalTiling {
                gap: 6,
                ring: 3,
                ring_inset: 0,
                frame: None,
            },
            app: AppTiling {
                gap: 0,
                ring: 0,
                fill: Rgb(0x73, 0x73, 0x73),
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
    /// Every mode with its config spelling, in the order error messages list them.
    pub const NAMES: [(Self, &'static str); 3] = [
        (Self::Off, "off"),
        (Self::Always, "always"),
        (Self::Toggle, "toggle"),
    ];

    /// The config spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        Self::NAMES
            .iter()
            .find_map(|(mode, name)| (*mode == self).then_some(*name))
            .unwrap_or("toggle")
    }

    /// Parses the exact spelling; anything else is `None`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::NAMES
            .iter()
            .find_map(|(mode, n)| (*n == name).then_some(*mode))
    }
}

/// Whether a never-toggled output's `toggle`-mode strip starts shown or hidden (`header { start }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderStart {
    /// The strip starts shown.
    Shown,
    /// The strip starts hidden (the default).
    Hidden,
}

impl HeaderStart {
    /// Every value with its config spelling, in the order error messages list them.
    pub const NAMES: [(Self, &'static str); 2] = [(Self::Shown, "shown"), (Self::Hidden, "hidden")];

    /// The config spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shown => "shown",
            Self::Hidden => "hidden",
        }
    }

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
            start: HeaderStart::Hidden,
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

/// Keyboard settings (`keyboard { layouts "us" "es" }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyboardConfig {
    /// The xkb layout names, in group order; `set-layout <name>` selects by position here.
    /// Never empty. Default `["us"]`.
    pub layouts: Vec<String>,
}

impl Default for KeyboardConfig {
    fn default() -> Self {
        Self {
            layouts: vec!["us".to_string()],
        }
    }
}

impl KeyboardConfig {
    /// The comma-joined xkb layout string (`"us,es"`).
    #[must_use]
    pub fn xkb_layout(&self) -> String {
        self.layouts.join(",")
    }

    /// The group index of the layout named `name`, if configured.
    #[must_use]
    pub fn layout_index(&self, name: &str) -> Option<usize> {
        self.layouts.iter().position(|l| l == name)
    }
}

/// Idle timers (`idle { lock-after 300; blank-after 600 }`); `None` leaves that timer off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IdleConfig {
    /// Seconds of inactivity before the locker is spawned. Positive.
    pub lock_after: Option<u32>,
    /// Seconds of inactivity before outputs are blanked (powered off). Positive.
    pub blank_after: Option<u32>,
}

/// Pointer cursor theme and size (`cursor { theme "Adwaita"; size (px)24 }`); `None` defers to the
/// `XCURSOR_THEME`/`XCURSOR_SIZE` environment, then to `default`/24.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CursorConfig {
    /// XCursor theme name. Never empty when set.
    pub theme: Option<String>,
    /// Nominal size in logical pixels, 1..=256.
    pub size: Option<u32>,
}

/// Where a screenshot goes (`screenshot { destination ... }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScreenshotDestination {
    /// Publish a PNG as the clipboard selection (the default).
    #[default]
    Clipboard,
    /// Write a PNG file into the screenshot directory.
    File,
    /// Both the clipboard and a file.
    Both,
}

/// Screenshot delivery (`screenshot { destination "clipboard"; directory "~/Pictures/Screenshots" }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenshotConfig {
    /// Where a capture goes.
    pub destination: ScreenshotDestination,
    /// The directory files are written to, as written in the config (a leading `~` is expanded to
    /// `$HOME` by the compositor at use). Never empty. Created on first write.
    pub directory: PathBuf,
}

impl Default for ScreenshotConfig {
    fn default() -> Self {
        Self {
            destination: ScreenshotDestination::default(),
            directory: PathBuf::from("~/Pictures/Screenshots"),
        }
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
    /// Whether a keyboard-focused fullscreen window confines the pointer to its output
    /// (`confine-pointer-to-fullscreen`, TTY backend only). Default on.
    pub confine_pointer_to_fullscreen: bool,
    /// Whether startup reopens the workspaces open at the last exit (`restore-session`).
    pub restore_session: bool,
    /// Workspace-picker roots and badge setting.
    pub picker: PickerConfig,
    /// Overlay border, style and terminal arguments.
    pub overlay: OverlayConfig,
    /// Workspace header strip mode, font and size.
    pub header: HeaderConfig,
    /// Keyboard layouts.
    pub keyboard: KeyboardConfig,
    /// Idle lock and blank timers (both off by default).
    pub idle: IdleConfig,
    /// Pointer cursor theme and size (both unset by default).
    pub cursor: CursorConfig,
    /// Screenshot destination and directory.
    pub screenshot: ScreenshotConfig,
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
            confine_pointer_to_fullscreen: true,
            restore_session: true,
            picker: PickerConfig::default(),
            overlay: OverlayConfig::default(),
            header: HeaderConfig::default(),
            keyboard: KeyboardConfig::default(),
            idle: IdleConfig::default(),
            cursor: CursorConfig::default(),
            screenshot: ScreenshotConfig::default(),
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
        assert!(c.confine_pointer_to_fullscreen);
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
