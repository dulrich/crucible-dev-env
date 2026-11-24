//! The parsed configuration model: plain data, defaults and the few pure helpers on it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::overlay::{OverlayConfig, TerminalKind};

mod header;
mod input_device;
mod tiling;

pub use header::{
    DEFAULT_DIVIDER_COLOR, DEFAULT_HEADER_FONT, HEADER_DIVIDER_INSET_MAX, HEADER_DIVIDER_MAX,
    HEADER_SIZE_MAX, HEADER_SIZE_MIN, HeaderActive, HeaderConfig, HeaderMode, HeaderStart,
};
pub use input_device::{InputDeviceConfig, InputDeviceLayer, KeyMapEntry, KeyMapTarget};
pub use tiling::{
    AppTiling, TerminalTiling, TilingConfig, clamp_ring, clamp_ring_inset, substitute_gap,
};

/// A closed set of config spellings: one `NAMES` table, in the order error messages list them,
/// drives both parsing ([`Named::from_name`]) and printing ([`Named::as_str`]).
pub trait Named: Copy + Eq + 'static {
    /// Every variant with its config spelling. A variant missing here is a programming error
    /// (each implementor's test matches its variants exhaustively).
    const NAMES: &'static [(Self, &'static str)];

    /// The config spelling.
    ///
    /// # Panics
    /// If `self` is missing from [`Named::NAMES`].
    #[must_use]
    fn as_str(self) -> &'static str {
        Self::NAMES
            .iter()
            .find_map(|(value, name)| (*value == self).then_some(*name))
            .expect("every variant is listed in Named::NAMES")
    }

    /// Parses the exact spelling; anything else is `None`.
    #[must_use]
    fn from_name(name: &str) -> Option<Self> {
        Self::NAMES
            .iter()
            .find_map(|(value, n)| (*n == name).then_some(*value))
    }
}

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

/// An sRGB colour, parsed from `"#rrggbb"`. Formats as lowercase `#rrggbb` with `{}` and as the
/// bare lowercase `rrggbb` digits with `{:x}` (the one formatter every config consumer uses).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl std::fmt::LowerHex for Rgb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }
}

impl std::fmt::Display for Rgb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{self:x}")
    }
}

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

/// Lowest accepted `output` position coordinate (logical pixels).
pub const OUTPUT_POSITION_MIN: i32 = -65536;
/// Highest accepted `output` position coordinate (logical pixels).
pub const OUTPUT_POSITION_MAX: i32 = 65536;

/// One output's settings (`output "<connector>" { … }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputConfig {
    /// The scale in 1/120 steps (120 = 1.0), quantized from the configured number.
    pub scale_120: u32,
    /// The desired top-left origin in logical pixels (`position x y`), if configured.
    pub position: Option<(i32, i32)>,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            scale_120: OUTPUT_SCALE_STEPS,
            position: None,
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

/// What opening an already-live workspace does (`workspace-open { live ... }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LiveOpen {
    /// Switch to the workspace where it lives, focusing its output (the default).
    #[default]
    Jump,
    /// Move the workspace onto the focused output.
    Bring,
}

impl Named for LiveOpen {
    const NAMES: &'static [(Self, &'static str)] = &[(Self::Jump, "jump"), (Self::Bring, "bring")];
}

/// Where opening a saved (not live) workspace restores it (`workspace-open { saved ... }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SavedOpen {
    /// Onto the focused output at open time (the default).
    #[default]
    Focused,
    /// Onto its recorded last-viewed output, falling back to the focused one.
    LastOutput,
}

impl Named for SavedOpen {
    const NAMES: &'static [(Self, &'static str)] = &[
        (Self::Focused, "focused"),
        (Self::LastOutput, "last-output"),
    ];
}

/// Placement of user-initiated opens of an existing workspace
/// (`workspace-open { live "jump"; saved "focused" }`). Startup restore and brand-new folder
/// workspaces are not governed by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WorkspaceOpenConfig {
    /// Opening a live workspace.
    pub live: LiveOpen,
    /// Opening a saved workspace.
    pub saved: SavedOpen,
}

/// The lowest night-light temperature in kelvin, for the config and the `night-light` verb.
pub const NIGHT_LIGHT_MIN_KELVIN: u32 = 2500;
/// The identity night-light temperature in kelvin: no tint, and the highest accepted value.
pub const NIGHT_LIGHT_IDENTITY_KELVIN: u32 = 6500;
/// The default night temperature in kelvin (a `night-light` block without `temperature`, and what
/// the `on` verb forces without a block).
pub const NIGHT_LIGHT_DEFAULT_KELVIN: u32 = 4500;

/// Built-in night light (`night-light { temperature 4500; start "21:00"; end "07:00"; fade-minutes 30 }`).
/// `start`/`end` are local minutes of the day (`0..1440`), `start != end`; the window may wrap midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NightLightConfig {
    /// Colour temperature in kelvin inside the window. `2500..=6500`, default 4500.
    pub temperature: u32,
    /// Colour temperature in kelvin outside the window (and the end the fades run to).
    /// `temperature..=6500`, default 6500 (no tint).
    pub day_temperature: u32,
    /// Window start, minutes after local midnight (`0..1440`).
    pub start: u16,
    /// Window end (exclusive), minutes after local midnight (`0..1440`); equal to `start` means all day.
    pub end: u16,
    /// Minutes of linear fade before `start` and after `end`. `0..=180`, default 30; 0 is a hard step.
    pub fade_minutes: u16,
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

impl Named for ScreenshotDestination {
    const NAMES: &'static [(Self, &'static str)] = &[
        (Self::Clipboard, "clipboard"),
        (Self::File, "file"),
        (Self::Both, "both"),
    ];
}

/// Screenshot delivery (`screenshot { destination "clipboard"; directory "~/Pictures/Screenshots" }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenshotConfig {
    /// Where a capture goes.
    pub destination: ScreenshotDestination,
    /// The directory files are written to, as written in the config (a leading `~` or `~/` is expanded to
    /// `$HOME` by [`crate::load`] when `$HOME` is set). Never empty. Created on first write.
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
    /// Where opening an existing workspace lands (`workspace-open`).
    pub workspace_open: WorkspaceOpenConfig,
    /// Night light schedule; `None` (no `night-light` block) leaves the display ramp untouched.
    pub night_light: Option<NightLightConfig>,
    /// Terminal theme (`theme`); `None` writes no fragment files and adds no spawn arguments.
    pub theme: Option<crate::theme::ThemeConfig>,
    /// Lock-screen style (`lock`); `None` leaves `lock-command` spawned exactly as written.
    pub lock: Option<crate::surface::LockConfig>,
    /// Optional greetd greeter settings (`greeter`); `None` when the block is absent.
    pub greeter: Option<crate::surface::GreeterConfig>,
    /// Pointer cursor theme and size (both unset by default).
    pub cursor: CursorConfig,
    /// Screenshot destination and directory.
    pub screenshot: ScreenshotConfig,
    /// The screen locker the `lock` action spawns (`lock-command`), split on whitespace into
    /// program + arguments (no shell quoting). Default
    /// `"swaylock --indicator-idle-visible"` (shows the keyboard layout name while idle).
    pub lock_command: String,
    /// Shell command lines run once per CDE start (`autostart`, repeatable), in file order, via
    /// `/bin/sh -c`. Default empty.
    pub autostart: Vec<String>,
    /// Extra TUI program names for the launcher.
    pub launcher: LauncherConfig,
    /// Binding overrides as raw `(chord, action)` strings, in file order.
    pub binds: Vec<(String, String)>,
    /// Per-device key remap tables (`input-device`, repeatable), in file order; one per
    /// vendor:product.
    pub input_devices: Vec<InputDeviceConfig>,
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

    /// The configured logical-pixel origin for the output with connector name `connector`, or
    /// `None` when no `output` block names it or the block has no `position`.
    #[must_use]
    pub fn output_position(&self, connector: &str) -> Option<(i32, i32)> {
        self.outputs.get(connector).and_then(|o| o.position)
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
            workspace_open: WorkspaceOpenConfig::default(),
            night_light: None,
            theme: None,
            lock: None,
            greeter: None,
            cursor: CursorConfig::default(),
            screenshot: ScreenshotConfig::default(),
            launcher: LauncherConfig::default(),
            binds: Vec::new(),
            outputs: BTreeMap::new(),
            input_devices: Vec::new(),
            lock_command: "swaylock --indicator-idle-visible".to_string(),
            autostart: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::SurfaceStyle;

    /// Every `all` variant is in `NAMES` (same count, each round-trips through its spelling).
    pub(super) fn assert_names_round_trip<T: Named + std::fmt::Debug>(all: &[T]) {
        assert_eq!(T::NAMES.len(), all.len(), "NAMES lists every variant");
        for v in all {
            assert_eq!(T::from_name(v.as_str()), Some(*v), "{v:?}");
        }
        assert_eq!(T::from_name("no-such-name"), None);
    }
    #[test]
    fn overlay_style_names_round_trip() {
        let _ = |m: SurfaceStyle| match m {
            SurfaceStyle::Terminal
            | SurfaceStyle::N1Dark
            | SurfaceStyle::N1Med
            | SurfaceStyle::N1Light => (),
        };
        assert_names_round_trip(&[
            SurfaceStyle::Terminal,
            SurfaceStyle::N1Dark,
            SurfaceStyle::N1Med,
            SurfaceStyle::N1Light,
        ]);
    }

    #[test]
    fn screenshot_destination_names_round_trip() {
        let _ = |m: ScreenshotDestination| match m {
            ScreenshotDestination::Clipboard
            | ScreenshotDestination::File
            | ScreenshotDestination::Both => (),
        };
        assert_names_round_trip(&[
            ScreenshotDestination::Clipboard,
            ScreenshotDestination::File,
            ScreenshotDestination::Both,
        ]);
    }

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
        assert_eq!(c.tiling.terminal_args, None);
        assert!(c.tiling.program_terminal_args.is_empty());
        assert!(c.focus_follows_mouse);
        assert!(c.confine_pointer_to_fullscreen);
        assert!(c.picker.roots.is_empty());
        assert!(!c.picker.slots);
        assert!(c.launcher.tui.is_empty());
        assert!(c.binds.is_empty());
    }

    #[test]
    fn rgb_parses_exactly_six_hex_digits() {
        assert_eq!(Rgb::from_hex("#10aBcD"), Some(Rgb(0x10, 0xab, 0xcd)));
        assert_eq!(Rgb::from_hex("10aBcD"), None);
        assert_eq!(Rgb::from_hex("#10ab"), None);
    }
}
