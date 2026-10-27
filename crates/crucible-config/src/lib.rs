//! Configuration for Crucible Dev Env (CDE).
//!
//! CDE is configured by a declarative KDL file (config-file-only for the MVP, settings UI on the
//! roadmap) at `$XDG_CONFIG_HOME/cde/config.kdl` (fallback `~/.config/cde/config.kdl`). This crate
//! parses and validates it into a [`Config`]; a missing file is [`Config::default`], which equals
//! CDE's pre-config behaviour. An invalid file is a typed [`ConfigError`] carrying the line and
//! column of the offending node or entry.
//!
//! [`parse`] is pure; [`load`] adds exactly one file read. The crate knows nothing about chords or
//! actions: `binds` entries are carried as raw `(chord, action)` strings and validated by
//! `crucible-keys` (`Bindings::with_overrides`). Hot reload (WP-5) is the compositor's concern.
//! The full schema is documented in `docs/config.md`.

use std::path::{Path, PathBuf};

use kdl::{KdlDocument, KdlNode, KdlValue};

/// A 1-based line/column position in the config source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    /// 1-based line number.
    pub line: usize,
    /// 1-based column (in characters).
    pub column: usize,
}

impl std::fmt::Display for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.line, self.column)
    }
}

/// Why a config file could not be loaded. Every parse-level variant carries a [`Location`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file exists but could not be read.
    #[error("cannot read {}: {source}", path.display())]
    Io {
        /// The config path.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The text is not valid KDL.
    #[error("{at}: invalid KDL: {message}")]
    Syntax {
        /// Where the syntax error is.
        at: Location,
        /// The parser's message.
        message: String,
    },
    /// A node name the schema does not define (at the top level or inside `picker`/`binds`).
    #[error("{at}: unknown node `{name}` in {context}")]
    UnknownNode {
        /// Where the node is.
        at: Location,
        /// The node name.
        name: String,
        /// `"the top level"`, `"picker"` or `"binds"`.
        context: &'static str,
    },
    /// A property the node does not accept.
    #[error("{at}: unknown property `{name}` on `{node}`")]
    UnknownProperty {
        /// Where the property is.
        at: Location,
        /// The node it appears on.
        node: String,
        /// The property name.
        name: String,
    },
    /// More arguments (or any arguments, where none are allowed) than the node takes.
    #[error("{at}: unexpected extra argument on `{node}`")]
    UnexpectedArgument {
        /// Where the argument is.
        at: Location,
        /// The node it appears on.
        node: String,
    },
    /// A required argument is absent.
    #[error("{at}: `{node}` is missing {expected}")]
    MissingArgument {
        /// Where the node is.
        at: Location,
        /// The node name.
        node: String,
        /// What is missing, e.g. `"a command string"`.
        expected: &'static str,
    },
    /// An argument or property has the wrong KDL type.
    #[error("{at}: `{node}` {what} must be {expected}")]
    WrongType {
        /// Where the value is.
        at: Location,
        /// The node it appears on.
        node: String,
        /// Which value, e.g. `"argument"` or ``"property `inner`"``.
        what: String,
        /// The required type, e.g. `"a string"`.
        expected: &'static str,
    },
    /// A value of the right type but outside what the schema allows (negative or oversized gap,
    /// empty string).
    #[error("{at}: `{node}` {what}: {message}")]
    InvalidValue {
        /// Where the value is.
        at: Location,
        /// The node it appears on.
        node: String,
        /// Which value.
        what: String,
        /// What is wrong with it.
        message: String,
    },
    /// A single-use node appears twice.
    #[error("{at}: duplicate `{name}` node")]
    Duplicate {
        /// Where the second occurrence is.
        at: Location,
        /// The node name.
        name: String,
    },
}

/// The terminal CDE spawns for `Super+Return` / `Super+Shift+Return`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalConfig {
    /// The command to spawn. `None` keeps the built-in resolution (`$TERMINAL`, else `foot`).
    pub command: Option<String>,
    /// The flag a terminal takes to run a program (`-e` for most); used by WP-5/WP-6 launchers.
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
            },
            app: ClassTiling {
                gap: 0,
                ring: 0,
                fill,
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
    /// Extra terminal arguments inserted before the exec flag on overlay spawns.
    pub terminal_args: Vec<String>,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            border: true,
            style: OverlayStyle::N1Med,
            terminal_args: Vec::new(),
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
    /// Whether labels of slot-pinned workspaces carry a slot badge (`slots`).
    pub slots: bool,
}

impl Default for HeaderConfig {
    fn default() -> Self {
        Self {
            mode: HeaderMode::Toggle,
            font: DEFAULT_HEADER_FONT.to_string(),
            size: 14,
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
    /// Directories whose immediate subdirectories are workspace-picker candidates.
    pub picker_roots: Vec<PathBuf>,
    /// Whether the workspace picker shows `⌘<n>` badges on pinned rows (`picker { slots }`).
    pub picker_slots: bool,
    /// Overlay border, style and terminal arguments.
    pub overlay: OverlayConfig,
    /// Workspace header strip mode, font and size.
    pub header: HeaderConfig,
    /// The screen locker the `lock` action spawns (`lock-command`), split on whitespace into
    /// program + arguments (no shell quoting). Default `"swaylock"`.
    pub lock_command: String,
    /// Binding overrides as raw `(chord, action)` strings, in file order.
    pub binds: Vec<(String, String)>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            terminal: TerminalConfig::default(),
            tiling: TilingConfig::default(),
            focus_follows_mouse: true,
            restore_session: true,
            picker_roots: Vec::new(),
            picker_slots: false,
            overlay: OverlayConfig::default(),
            header: HeaderConfig::default(),
            binds: Vec::new(),
            lock_command: "swaylock".to_string(),
        }
    }
}

/// Resolves the config path from the two environment values (`None` or empty = unset).
#[must_use]
pub fn config_path_from(xdg_config_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    fn nonempty(v: Option<&str>) -> Option<&str> {
        v.filter(|s| !s.is_empty())
    }
    if let Some(xdg) = nonempty(xdg_config_home) {
        return Some(Path::new(xdg).join("cde").join("config.kdl"));
    }
    nonempty(home).map(|h| Path::new(h).join(".config").join("cde").join("config.kdl"))
}

/// `$XDG_CONFIG_HOME/cde/config.kdl`, else `$HOME/.config/cde/config.kdl`; `None` when neither
/// variable is set.
#[must_use]
pub fn config_path() -> Option<PathBuf> {
    config_path_from(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// Reads and parses the config at `path`; a missing file is `Ok(Config::default())`.
///
/// # Errors
/// [`ConfigError::Io`] for an unreadable file, otherwise as [`parse`].
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(source) => Err(ConfigError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Parses config text (KDL v2). Pure.
///
/// # Errors
/// A [`ConfigError`] with the line and column of the first problem found.
pub fn parse(text: &str) -> Result<Config, ConfigError> {
    let doc = KdlDocument::parse_v2(text).map_err(|err| {
        let (at, message) = err.diagnostics.first().map_or_else(
            || (Location { line: 1, column: 1 }, err.to_string()),
            |d| {
                (
                    locate(text, d.span.offset()),
                    d.message.clone().unwrap_or_else(|| err.to_string()),
                )
            },
        );
        ConfigError::Syntax { at, message }
    })?;

    let mut config = Config::default();
    let mut seen: Vec<&str> = Vec::new();
    for node in doc.nodes() {
        let name = node.name().value();
        let at = locate(text, node.span().offset());
        if matches!(
            name,
            "terminal"
                | "tiling"
                | "focus-follows-mouse"
                | "restore-session"
                | "picker"
                | "overlay"
                | "header"
                | "binds"
                | "lock-command"
        ) && seen.contains(&name)
        {
            return Err(ConfigError::Duplicate {
                at,
                name: name.to_string(),
            });
        }
        match name {
            "terminal" => parse_terminal(text, node, &mut config.terminal)?,
            "tiling" => config.tiling = parse_tiling(text, node)?,
            "gaps" => {
                return Err(ConfigError::InvalidValue {
                    at,
                    node: "gaps".to_string(),
                    what: "node".to_string(),
                    message: "`gaps` was removed; use `tiling { terminal gap=… ring=…; app gap=… ring=… }`"
                        .to_string(),
                });
            }
            "focus-follows-mouse" => {
                no_properties(text, node)?;
                config.focus_follows_mouse = single_arg(text, node, "a #true or #false")?
                    .as_bool()
                    .ok_or_else(|| wrong_type(text, node, "argument".into(), "#true or #false"))?;
            }
            "restore-session" => {
                no_properties(text, node)?;
                config.restore_session = single_arg(text, node, "a #true or #false")?
                    .as_bool()
                    .ok_or_else(|| wrong_type(text, node, "argument".into(), "#true or #false"))?;
            }
            "picker" => (config.picker_roots, config.picker_slots) = parse_picker(text, node)?,
            "overlay" => config.overlay = parse_overlay(text, node)?,
            "header" => config.header = parse_header(text, node)?,
            "binds" => config.binds = parse_binds(text, node)?,
            "lock-command" => {
                no_properties(text, node)?;
                let cmd = string_args(text, node, &["a command string"])?[0];
                nonempty(text, node, "command", cmd)?;
                config.lock_command = cmd.to_string();
            }
            other => {
                return Err(ConfigError::UnknownNode {
                    at,
                    name: other.to_string(),
                    context: "the top level",
                });
            }
        }
        seen.push(name);
    }
    Ok(config)
}

/// Converts a char offset into a 1-based line/column.
fn locate(text: &str, offset: usize) -> Location {
    let (mut line, mut column) = (1, 1);
    for ch in text.chars().take(offset) {
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    Location { line, column }
}

fn node_name(node: &KdlNode) -> String {
    node.name().value().to_string()
}

fn wrong_type(text: &str, node: &KdlNode, what: String, expected: &'static str) -> ConfigError {
    ConfigError::WrongType {
        at: locate(text, node.span().offset()),
        node: node_name(node),
        what,
        expected,
    }
}

/// The node's positional arguments with their locations.
fn args<'a>(text: &str, node: &'a KdlNode) -> Vec<(&'a KdlValue, Location)> {
    node.entries()
        .iter()
        .filter(|e| e.name().is_none())
        .map(|e| (e.value(), locate(text, e.span().offset())))
        .collect()
}

/// Exactly one positional argument.
fn single_arg<'a>(
    text: &str,
    node: &'a KdlNode,
    expected: &'static str,
) -> Result<&'a KdlValue, ConfigError> {
    let mut all = args(text, node).into_iter();
    let (first, _) = all.next().ok_or_else(|| ConfigError::MissingArgument {
        at: locate(text, node.span().offset()),
        node: node_name(node),
        expected,
    })?;
    if let Some((_, at)) = all.next() {
        return Err(ConfigError::UnexpectedArgument {
            at,
            node: node_name(node),
        });
    }
    Ok(first)
}

/// Rejects any property (for nodes that take none).
fn no_properties(text: &str, node: &KdlNode) -> Result<(), ConfigError> {
    allow_properties(text, node, &[])
}

fn allow_properties(text: &str, node: &KdlNode, allowed: &[&str]) -> Result<(), ConfigError> {
    for entry in node.entries() {
        if let Some(key) = entry.name()
            && !allowed.contains(&key.value())
        {
            return Err(ConfigError::UnknownProperty {
                at: locate(text, entry.span().offset()),
                node: node_name(node),
                name: key.value().to_string(),
            });
        }
    }
    Ok(())
}

/// A required-string argument at `index` (0-based), exactly `count` arguments total.
fn string_args<'a>(
    text: &str,
    node: &'a KdlNode,
    labels: &[&'static str],
) -> Result<Vec<&'a str>, ConfigError> {
    let all = args(text, node);
    let mut out = Vec::new();
    for (i, label) in labels.iter().enumerate() {
        let Some((value, at)) = all.get(i) else {
            return Err(ConfigError::MissingArgument {
                at: locate(text, node.span().offset()),
                node: node_name(node),
                expected: label,
            });
        };
        let s = value.as_string().ok_or_else(|| ConfigError::WrongType {
            at: *at,
            node: node_name(node),
            what: format!("argument {}", i + 1),
            expected: "a string",
        })?;
        out.push(s);
    }
    if let Some((_, at)) = all.get(labels.len()) {
        return Err(ConfigError::UnexpectedArgument {
            at: *at,
            node: node_name(node),
        });
    }
    Ok(out)
}

fn nonempty(text: &str, node: &KdlNode, what: &str, s: &str) -> Result<(), ConfigError> {
    if s.trim().is_empty() {
        return Err(ConfigError::InvalidValue {
            at: locate(text, node.span().offset()),
            node: node_name(node),
            what: what.to_string(),
            message: "must not be empty".to_string(),
        });
    }
    Ok(())
}

fn parse_terminal(text: &str, node: &KdlNode, out: &mut TerminalConfig) -> Result<(), ConfigError> {
    allow_properties(text, node, &["exec-flag"])?;
    let cmd = string_args(text, node, &["a command string"])?[0];
    nonempty(text, node, "command", cmd)?;
    out.command = Some(cmd.to_string());
    if let Some(entry) = node.entry("exec-flag") {
        let at = locate(text, entry.span().offset());
        let flag = entry
            .value()
            .as_string()
            .ok_or_else(|| ConfigError::WrongType {
                at,
                node: node_name(node),
                what: "property `exec-flag`".to_string(),
                expected: "a string",
            })?;
        nonempty(text, node, "property `exec-flag`", flag)?;
        out.exec_flag = flag.to_string();
    }
    Ok(())
}

/// The node's positional arguments, all strings (`terminal-args`).
fn string_list(text: &str, node: &KdlNode) -> Result<Vec<String>, ConfigError> {
    let mut list = Vec::new();
    for (value, arg_at) in args(text, node) {
        let s = value.as_string().ok_or_else(|| ConfigError::WrongType {
            at: arg_at,
            node: node_name(node),
            what: "argument".to_string(),
            expected: "a string",
        })?;
        list.push(s.to_string());
    }
    Ok(list)
}

/// A non-negative integer property `key`, if present.
fn u32_prop(text: &str, node: &KdlNode, key: &str) -> Result<Option<u32>, ConfigError> {
    let Some(entry) = node.entry(key) else {
        return Ok(None);
    };
    let at = locate(text, entry.span().offset());
    let what = format!("property `{key}`");
    let n = entry
        .value()
        .as_integer()
        .ok_or_else(|| ConfigError::WrongType {
            at,
            node: node_name(node),
            what: what.clone(),
            expected: "an integer",
        })?;
    u32::try_from(n)
        .map(Some)
        .map_err(|_| ConfigError::InvalidValue {
            at,
            node: node_name(node),
            what,
            message: format!("{n} is outside 0..={}", u32::MAX),
        })
}

/// One `terminal` / `app` line of `tiling`: `gap`, `ring` and (apps only) `fill`.
fn parse_class_tiling(
    text: &str,
    node: &KdlNode,
    out: &mut ClassTiling,
    allow_fill: bool,
) -> Result<(), ConfigError> {
    allow_properties(
        text,
        node,
        if allow_fill {
            &["gap", "ring", "fill"]
        } else {
            &["gap", "ring"]
        },
    )?;
    if let Some((_, at)) = args(text, node).first() {
        return Err(ConfigError::UnexpectedArgument {
            at: *at,
            node: node_name(node),
        });
    }
    if let Some(gap) = u32_prop(text, node, "gap")? {
        out.gap = gap;
    }
    if let Some(ring) = u32_prop(text, node, "ring")? {
        out.ring = ring;
    }
    if let Some(entry) = node.entry("fill") {
        let at = locate(text, entry.span().offset());
        let s = entry
            .value()
            .as_string()
            .ok_or_else(|| ConfigError::WrongType {
                at,
                node: node_name(node),
                what: "property `fill`".to_string(),
                expected: "a \"#rrggbb\" string",
            })?;
        out.fill = Rgb::from_hex(s).ok_or_else(|| ConfigError::InvalidValue {
            at,
            node: node_name(node),
            what: "property `fill`".to_string(),
            message: format!("`{s}` is not a #rrggbb colour"),
        })?;
    }
    Ok(())
}

fn parse_tiling(text: &str, node: &KdlNode) -> Result<TilingConfig, ConfigError> {
    let mut out = TilingConfig::default();
    let mut seen: Vec<&str> = Vec::new();
    for child in block_children(text, node)? {
        let name = child.name().value();
        let at = locate(text, child.span().offset());
        if seen.contains(&name) {
            return Err(ConfigError::Duplicate {
                at,
                name: name.to_string(),
            });
        }
        match name {
            "terminal" => parse_class_tiling(text, child, &mut out.terminal, false)?,
            "app" => parse_class_tiling(text, child, &mut out.app, true)?,
            "terminal-args" => {
                no_properties(text, child)?;
                out.terminal_args = string_list(text, child)?;
            }
            other => {
                return Err(ConfigError::UnknownNode {
                    at,
                    name: other.to_string(),
                    context: "tiling",
                });
            }
        }
        seen.push(name);
    }
    for (class, tiling) in [("terminal", &mut out.terminal), ("app", &mut out.app)] {
        let (ring, clamped) = clamp_ring(tiling.gap, tiling.ring);
        if clamped {
            tracing::warn!(
                class,
                gap = tiling.gap,
                ring = tiling.ring,
                "tiling ring is larger than gap; clamping ring to gap"
            );
            tiling.ring = ring;
        }
    }
    Ok(out)
}

/// Children of a block node, rejecting arguments/properties on the block itself.
fn block_children<'a>(text: &str, node: &'a KdlNode) -> Result<&'a [KdlNode], ConfigError> {
    no_properties(text, node)?;
    if let Some((_, at)) = args(text, node).first() {
        return Err(ConfigError::UnexpectedArgument {
            at: *at,
            node: node_name(node),
        });
    }
    Ok(node.children().map_or(&[][..], KdlDocument::nodes))
}

/// Parses the `picker` block into its `root` directories and the `slots` badge setting.
fn parse_picker(text: &str, node: &KdlNode) -> Result<(Vec<PathBuf>, bool), ConfigError> {
    let mut roots = Vec::new();
    let mut slots: Option<bool> = None;
    for child in block_children(text, node)? {
        let at = locate(text, child.span().offset());
        match child.name().value() {
            "root" => {
                no_properties(text, child)?;
                let dir = string_args(text, child, &["a directory string"])?[0];
                nonempty(text, child, "directory", dir)?;
                roots.push(PathBuf::from(dir));
            }
            "slots" => {
                if slots.is_some() {
                    return Err(ConfigError::Duplicate {
                        at,
                        name: "slots".to_string(),
                    });
                }
                no_properties(text, child)?;
                slots = Some(
                    single_arg(text, child, "a #true or #false")?
                        .as_bool()
                        .ok_or_else(|| {
                            wrong_type(text, child, "argument".into(), "#true or #false")
                        })?,
                );
            }
            _ => {
                return Err(ConfigError::UnknownNode {
                    at,
                    name: node_name(child),
                    context: "picker",
                });
            }
        }
    }
    Ok((roots, slots.unwrap_or(false)))
}

fn parse_overlay(text: &str, node: &KdlNode) -> Result<OverlayConfig, ConfigError> {
    let mut out = OverlayConfig::default();
    let mut seen: Vec<&str> = Vec::new();
    for child in block_children(text, node)? {
        let name = child.name().value();
        let at = locate(text, child.span().offset());
        if seen.contains(&name) {
            return Err(ConfigError::Duplicate {
                at,
                name: name.to_string(),
            });
        }
        no_properties(text, child)?;
        match name {
            "border" => {
                out.border = single_arg(text, child, "a #true or #false")?
                    .as_bool()
                    .ok_or_else(|| wrong_type(text, child, "argument".into(), "#true or #false"))?;
            }
            "style" => {
                let s = string_args(text, child, &["a style string"])?[0];
                out.style =
                    OverlayStyle::from_name(s).ok_or_else(|| ConfigError::InvalidValue {
                        at,
                        node: node_name(child),
                        what: "argument".to_string(),
                        message: format!("`{s}` is not one of terminal, n1-dark, n1-med, n1-light"),
                    })?;
            }
            "terminal-args" => {
                out.terminal_args = string_list(text, child)?;
            }
            other => {
                return Err(ConfigError::UnknownNode {
                    at,
                    name: other.to_string(),
                    context: "overlay",
                });
            }
        }
        seen.push(name);
    }
    Ok(out)
}

fn parse_header(text: &str, node: &KdlNode) -> Result<HeaderConfig, ConfigError> {
    let mut out = HeaderConfig::default();
    let mut seen: Vec<&str> = Vec::new();
    for child in block_children(text, node)? {
        let name = child.name().value();
        let at = locate(text, child.span().offset());
        if seen.contains(&name) {
            return Err(ConfigError::Duplicate {
                at,
                name: name.to_string(),
            });
        }
        no_properties(text, child)?;
        match name {
            "mode" => {
                let s = string_args(text, child, &["a mode string"])?[0];
                out.mode = HeaderMode::from_name(s).ok_or_else(|| ConfigError::InvalidValue {
                    at,
                    node: node_name(child),
                    what: "argument".to_string(),
                    message: format!("`{s}` is not one of off, always, toggle"),
                })?;
            }
            "font" => {
                let s = string_args(text, child, &["a font path or family string"])?[0];
                nonempty(text, child, "font", s)?;
                out.font = s.to_string();
            }
            "size" => {
                let v = single_arg(text, child, "an integer")?;
                let n = v
                    .as_integer()
                    .ok_or_else(|| wrong_type(text, child, "argument".into(), "an integer"))?;
                out.size = u32::try_from(n)
                    .ok()
                    .filter(|n| (HEADER_SIZE_MIN..=HEADER_SIZE_MAX).contains(n))
                    .ok_or_else(|| ConfigError::InvalidValue {
                        at,
                        node: node_name(child),
                        what: "argument".to_string(),
                        message: format!("{n} is outside {HEADER_SIZE_MIN}..={HEADER_SIZE_MAX}"),
                    })?;
            }
            "slots" => {
                out.slots = single_arg(text, child, "a #true or #false")?
                    .as_bool()
                    .ok_or_else(|| wrong_type(text, child, "argument".into(), "#true or #false"))?;
            }
            other => {
                return Err(ConfigError::UnknownNode {
                    at,
                    name: other.to_string(),
                    context: "header",
                });
            }
        }
        seen.push(name);
    }
    Ok(out)
}

fn parse_binds(text: &str, node: &KdlNode) -> Result<Vec<(String, String)>, ConfigError> {
    let mut binds = Vec::new();
    for child in block_children(text, node)? {
        if child.name().value() != "bind" {
            return Err(ConfigError::UnknownNode {
                at: locate(text, child.span().offset()),
                name: node_name(child),
                context: "binds",
            });
        }
        no_properties(text, child)?;
        let a = string_args(text, child, &["a chord string", "an action string"])?;
        binds.push((a[0].to_string(), a[1].to_string()));
    }
    Ok(binds)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r##"
terminal "/tmp/cde/ghostty-cde" exec-flag="-e"
tiling {
    terminal gap=4 ring=2
    app gap=3 ring=1 fill="#102030"
    terminal-args "--window-padding-x={gap}" "--b"
}
focus-follows-mouse #false
picker {
    root "<code-root>"
    root "/srv/work"
}
binds {
    bind "Super+P" "open-picker"
    bind "Super+Backslash" "split right"
    bind "Super+Shift+Q" "none"
}
"##;

    #[test]
    fn empty_text_is_default() {
        assert_eq!(parse("").unwrap(), Config::default());
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
        assert!(c.picker_roots.is_empty());
        assert!(c.binds.is_empty());
    }

    #[test]
    fn lock_command_defaults_to_swaylock_and_rejects_empty_or_duplicate() {
        assert_eq!(Config::default().lock_command, "swaylock");
        let c = parse("lock-command \"swaylock -f -c 000000\"\n").unwrap();
        assert_eq!(c.lock_command, "swaylock -f -c 000000");
        let (err, _) = err_at("lock-command \"  \"\n");
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
        let (err, _) = err_at("lock-command \"a\"\nlock-command \"b\"\n");
        assert!(matches!(err, ConfigError::Duplicate { .. }));
    }

    #[test]
    fn restore_session_defaults_on_parses_and_rejects_non_booleans() {
        assert!(Config::default().restore_session);
        assert!(!parse("restore-session #false\n").unwrap().restore_session);
        assert!(parse("restore-session #true\n").unwrap().restore_session);
        let (err, _) = err_at("restore-session \"no\"\n");
        assert!(matches!(err, ConfigError::WrongType { .. }));
        let (err, _) = err_at("restore-session\n");
        assert!(matches!(err, ConfigError::MissingArgument { .. }));
        let (err, _) = err_at("restore-session #true\nrestore-session #false\n");
        assert!(matches!(err, ConfigError::Duplicate { .. }));
    }

    #[test]
    fn full_example_parses() {
        let c = parse(FULL).unwrap();
        assert_eq!(c.terminal.command.as_deref(), Some("/tmp/cde/ghostty-cde"));
        assert_eq!(c.terminal.exec_flag, "-e");
        assert_eq!((c.tiling.terminal.gap, c.tiling.terminal.ring), (4, 2));
        assert_eq!((c.tiling.app.gap, c.tiling.app.ring), (3, 1));
        assert_eq!(c.tiling.app.fill, Rgb(0x10, 0x20, 0x30));
        assert_eq!(
            c.tiling.substituted_terminal_args(),
            vec!["--window-padding-x=4".to_string(), "--b".to_string()]
        );
        assert!(!c.focus_follows_mouse);
        assert_eq!(
            c.picker_roots,
            vec![
                PathBuf::from("<code-root>"),
                PathBuf::from("/srv/work")
            ]
        );
        assert_eq!(
            c.binds,
            vec![
                ("Super+P".to_string(), "open-picker".to_string()),
                ("Super+Backslash".to_string(), "split right".to_string()),
                ("Super+Shift+Q".to_string(), "none".to_string()),
            ]
        );
    }

    #[test]
    fn partial_files_keep_other_defaults() {
        let c = parse("tiling {\n    terminal gap=0 ring=0\n}\n").unwrap();
        assert_eq!(c.tiling.terminal.gap, 0);
        assert_eq!((c.tiling.app.gap, c.tiling.app.ring), (0, 0));
        assert!(c.focus_follows_mouse);
        let c = parse("terminal \"alacritty\"\n").unwrap();
        assert_eq!(c.terminal.exec_flag, "-e");
        let c = parse("terminal \"wezterm\" exec-flag=\"start\"\n").unwrap();
        assert_eq!(c.terminal.exec_flag, "start");
    }

    fn err_at(text: &str) -> (ConfigError, Location) {
        let err = parse(text).unwrap_err();
        let at = match &err {
            ConfigError::Syntax { at, .. }
            | ConfigError::UnknownNode { at, .. }
            | ConfigError::UnknownProperty { at, .. }
            | ConfigError::UnexpectedArgument { at, .. }
            | ConfigError::MissingArgument { at, .. }
            | ConfigError::WrongType { at, .. }
            | ConfigError::InvalidValue { at, .. }
            | ConfigError::Duplicate { at, .. } => *at,
            ConfigError::Io { .. } => panic!("unexpected io error"),
        };
        (err, at)
    }

    #[test]
    fn syntax_error_has_location() {
        let (err, at) = err_at("tiling {\n}\nterminal \"unterminated\n");
        assert!(matches!(err, ConfigError::Syntax { .. }), "{err}");
        assert!(at.line >= 2, "{at}");
    }

    #[test]
    fn unknown_top_level_node() {
        let (err, at) = err_at("tiling {\n}\n\nwallpaper \"x.png\"\n");
        assert!(
            matches!(&err, ConfigError::UnknownNode { name, context, .. }
                if name == "wallpaper" && *context == "the top level"),
            "{err}"
        );
        assert_eq!(at, Location { line: 4, column: 1 });
    }

    #[test]
    fn unknown_nodes_inside_blocks() {
        let (err, at) = err_at("picker {\n    dir \"/x\"\n}\n");
        assert!(matches!(&err, ConfigError::UnknownNode { context, .. } if *context == "picker"));
        assert_eq!(at.line, 2);
        let (err, _) = err_at("binds {\n    key \"Super+P\" \"quit\"\n}\n");
        assert!(matches!(&err, ConfigError::UnknownNode { context, .. } if *context == "binds"));
    }

    #[test]
    fn unknown_property() {
        let (err, at) = err_at("tiling {\n    terminal gap=1 middle=2\n}\n");
        assert!(
            matches!(&err, ConfigError::UnknownProperty { name, node, .. }
                if name == "middle" && node == "terminal"),
            "{err}"
        );
        assert_eq!(
            at,
            Location {
                line: 2,
                column: 20
            }
        );
        // `fill` is an app-only property.
        let (err, _) = err_at("tiling {\n    terminal fill=\"#000000\"\n}\n");
        assert!(matches!(err, ConfigError::UnknownProperty { .. }));
        let (err, _) = err_at("tiling {\n    window gap=1\n}\n");
        assert!(matches!(&err, ConfigError::UnknownNode { context, .. } if *context == "tiling"));
        let (err, _) = err_at("terminal \"foot\" flag=\"-e\"\n");
        assert!(matches!(err, ConfigError::UnknownProperty { .. }));
    }

    #[test]
    fn wrong_value_types() {
        let (err, _) = err_at("tiling {\n    app gap=\"big\"\n}\n");
        assert!(
            matches!(&err, ConfigError::WrongType { expected, .. } if *expected == "an integer")
        );
        let (err, _) = err_at("terminal 5\n");
        assert!(matches!(&err, ConfigError::WrongType { expected, .. } if *expected == "a string"));
        let (err, _) = err_at("focus-follows-mouse \"yes\"\n");
        assert!(matches!(&err, ConfigError::WrongType { .. }));
        let (err, _) = err_at("terminal \"foot\" exec-flag=1\n");
        assert!(matches!(&err, ConfigError::WrongType { what, .. } if what.contains("exec-flag")));
        let (err, _) = err_at("binds {\n    bind \"Super+P\" 3\n}\n");
        assert!(matches!(&err, ConfigError::WrongType { .. }));
    }

    #[test]
    fn out_of_range_and_empty_values() {
        let (err, _) = err_at("tiling {\n    terminal gap=-1\n}\n");
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
        let (err, _) = err_at("tiling {\n    app ring=99999999999\n}\n");
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
        let (err, _) = err_at("tiling {\n    app fill=\"red\"\n}\n");
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
        let (err, _) = err_at("terminal \"\"\n");
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
    }

    #[test]
    fn missing_and_extra_arguments() {
        let (err, _) = err_at("terminal\n");
        assert!(matches!(err, ConfigError::MissingArgument { .. }));
        let (err, _) = err_at("focus-follows-mouse\n");
        assert!(matches!(err, ConfigError::MissingArgument { .. }));
        let (err, _) = err_at("binds {\n    bind \"Super+P\"\n}\n");
        assert!(matches!(err, ConfigError::MissingArgument { .. }));
        let (err, at) = err_at("terminal \"a\" \"b\"\n");
        assert!(matches!(err, ConfigError::UnexpectedArgument { .. }));
        assert_eq!(at.column, 14);
        let (err, _) = err_at("tiling 4\n");
        assert!(matches!(err, ConfigError::UnexpectedArgument { .. }));
    }

    #[test]
    fn duplicate_single_use_node() {
        let (err, at) = err_at("tiling {\n}\ntiling {\n}\n");
        assert!(matches!(&err, ConfigError::Duplicate { name, .. } if name == "tiling"));
        assert_eq!(at.line, 3);
    }

    #[test]
    fn gaps_node_is_an_error_naming_tiling() {
        let (err, at) = err_at("gaps inner=4\n");
        assert!(err.to_string().contains("tiling"), "{err}");
        assert_eq!(at.line, 1);
    }

    #[test]
    fn ring_clamps_to_gap_and_gap_substitutes() {
        let c = parse("tiling {\n    terminal gap=2 ring=9\n    app gap=1 ring=5\n}\n").unwrap();
        assert_eq!(c.tiling.terminal.ring, 2);
        assert_eq!(c.tiling.app.ring, 1);
        assert_eq!(clamp_ring(6, 3), (3, false));
        assert_eq!(clamp_ring(2, 9), (2, true));
        assert_eq!(
            substitute_gap(&["a={gap}-{gap}".to_string(), "b".to_string()], 7),
            vec!["a=7-7".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn errors_display_their_location() {
        let (err, _) = err_at("\n  wallpaper \"x\"\n");
        let msg = err.to_string();
        assert!(msg.starts_with("2:3:"), "{msg}");
    }

    #[test]
    fn config_path_prefers_xdg_then_home() {
        assert_eq!(
            config_path_from(Some("/x"), Some("/h")),
            Some(PathBuf::from("/x/cde/config.kdl"))
        );
        assert_eq!(
            config_path_from(None, Some("/h")),
            Some(PathBuf::from("/h/.config/cde/config.kdl"))
        );
        assert_eq!(
            config_path_from(Some(""), Some("/h")),
            Some(PathBuf::from("/h/.config/cde/config.kdl"))
        );
        assert_eq!(config_path_from(None, None), None);
    }

    #[test]
    fn load_missing_file_is_default_and_bad_file_errors() {
        let dir = std::env::temp_dir().join(format!("cde-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("absent.kdl");
        assert_eq!(load(&missing).unwrap(), Config::default());

        let good = dir.join("good.kdl");
        std::fs::write(&good, "tiling {\n    terminal gap=2 ring=1\n}\n").unwrap();
        assert_eq!(load(&good).unwrap().tiling.terminal.gap, 2);

        let bad = dir.join("bad.kdl");
        std::fs::write(&bad, "nope\n").unwrap();
        assert!(matches!(load(&bad), Err(ConfigError::UnknownNode { .. })));

        // A directory in place of the file is an I/O error, not "missing".
        assert!(matches!(load(&dir), Err(ConfigError::Io { .. })));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn overlay_defaults_when_absent() {
        let c = parse("").unwrap();
        assert!(c.overlay.border);
        assert_eq!(c.overlay.style, OverlayStyle::N1Med);
        assert!(c.overlay.terminal_args.is_empty());
    }

    #[test]
    fn overlay_parses_all_children() {
        let c = parse(
            "overlay {\n border #false\n style \"n1-light\"\n terminal-args \"--a=1\" \"--b\"\n}\n",
        )
        .unwrap();
        assert!(!c.overlay.border);
        assert_eq!(c.overlay.style, OverlayStyle::N1Light);
        assert_eq!(c.overlay.terminal_args, vec!["--a=1", "--b"]);
    }

    #[test]
    fn overlay_partial_keeps_other_defaults() {
        let c = parse("overlay {\n style \"terminal\"\n}\n").unwrap();
        assert!(c.overlay.border);
        assert_eq!(c.overlay.style, OverlayStyle::Terminal);
    }

    #[test]
    fn overlay_every_style_roundtrips() {
        for style in ["terminal", "n1-dark", "n1-med", "n1-light"] {
            let c = parse(&format!("overlay {{\n style \"{style}\"\n}}\n")).unwrap();
            assert_eq!(c.overlay.style.as_str(), style);
        }
    }

    #[test]
    fn overlay_unknown_style_is_error() {
        let err = parse("overlay {\n style \"N1-Med\"\n}\n").unwrap_err();
        assert!(matches!(err, ConfigError::InvalidValue { .. }), "{err:?}");
    }

    #[test]
    fn overlay_unknown_child_is_error() {
        let err = parse("overlay {\n frob 1\n}\n").unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::UnknownNode {
                    context: "overlay",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn overlay_border_must_be_bool() {
        let err = parse("overlay {\n border \"yes\"\n}\n").unwrap_err();
        assert!(matches!(err, ConfigError::WrongType { .. }), "{err:?}");
    }

    #[test]
    fn overlay_terminal_args_must_be_strings() {
        let err = parse("overlay {\n terminal-args 3\n}\n").unwrap_err();
        assert!(matches!(err, ConfigError::WrongType { .. }), "{err:?}");
    }

    #[test]
    fn header_defaults_and_modes() {
        let d = Config::default().header;
        assert_eq!(
            (d.mode, d.size, d.font.as_str()),
            (HeaderMode::Toggle, 14, "DejaVu Sans")
        );
        for (name, mode) in [
            ("off", HeaderMode::Off),
            ("always", HeaderMode::Always),
            ("toggle", HeaderMode::Toggle),
        ] {
            let c = parse(&format!("header {{\n mode \"{name}\"\n}}\n")).unwrap();
            assert_eq!(c.header.mode, mode);
        }
        // The old `alt` spelling is not an alias.
        for bad in ["sometimes", "alt"] {
            let err = parse(&format!("header {{\n mode \"{bad}\"\n}}\n")).unwrap_err();
            assert!(err.to_string().contains("off, always, toggle"), "{err}");
        }
    }

    #[test]
    fn picker_slots_setting() {
        assert!(!Config::default().picker_slots);
        assert!(!parse("picker {\n root \"/x\"\n}\n").unwrap().picker_slots);
        let c = parse("picker {\n slots #true\n root \"/x\"\n}\n").unwrap();
        assert!(c.picker_slots);
        assert_eq!(c.picker_roots, vec![PathBuf::from("/x")]);
        assert!(!parse("picker {\n slots #false\n}\n").unwrap().picker_slots);
        let err = parse("picker {\n slots \"yes\"\n}\n").unwrap_err();
        assert!(matches!(err, ConfigError::WrongType { .. }), "{err:?}");
        let err = parse("picker {\n slots #true\n slots #false\n}\n").unwrap_err();
        assert!(matches!(err, ConfigError::Duplicate { .. }), "{err:?}");
    }

    #[test]
    fn header_slots_setting() {
        assert!(!Config::default().header.slots);
        assert!(parse("header {\n slots #true\n}\n").unwrap().header.slots);
        assert!(!parse("header {\n slots #false\n}\n").unwrap().header.slots);
        let err = parse("header {\n slots \"yes\"\n}\n").unwrap_err();
        assert!(matches!(err, ConfigError::WrongType { .. }), "{err:?}");
    }

    #[test]
    fn header_size_bounds() {
        assert_eq!(parse("header {\n size 6\n}\n").unwrap().header.size, 6);
        assert_eq!(parse("header {\n size 96\n}\n").unwrap().header.size, 96);
        for bad in ["5", "97", "0", "-3"] {
            let err = parse(&format!("header {{\n size {bad}\n}}\n")).unwrap_err();
            assert!(
                matches!(err, ConfigError::InvalidValue { .. }),
                "{bad}: {err:?}"
            );
        }
        let err = parse("header {\n size 1.5\n}\n").unwrap_err();
        assert!(matches!(err, ConfigError::WrongType { .. }), "{err:?}");
    }

    #[test]
    fn header_font_path_versus_family() {
        let c = parse("header {\n font \"/usr/share/fonts/x.ttf\"\n}\n").unwrap();
        assert_eq!(
            c.header.font_path(),
            Some(Path::new("/usr/share/fonts/x.ttf"))
        );
        let c = parse("header {\n font \"Noto Sans\"\n}\n").unwrap();
        assert_eq!(c.header.font_path(), None);
        assert_eq!(c.header.font, "Noto Sans");
    }
}
