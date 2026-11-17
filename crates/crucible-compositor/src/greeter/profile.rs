//! The greeter profile (`<dir>/profile.kdl`): everything the greeter shows, published by the
//! user session to a system-readable directory because the greeter cannot read the user's home.
//!
//! Pure: [`GreeterProfile::from_config`] + [`write_profile`] build the file (the user session calls
//! these), [`parse_profile`] reads it back, and [`load_profile`] is the one I/O edge (missing,
//! unreadable or invalid → [`GreeterProfile::fallback`] with one `warn!`).
//!
//! ```kdl
//! style "terminal"
//! background { image "/var/lib/cde-greeter/background.png"; }   // or { color "#000000"; }
//! palette { bg "#1d1f21"; text "#c5c8c6"; line "#666666"; accent "#81a2be"; }
//! alpha 0.85
//! position "lower"   // panel region: center|upper|lower|upper-left|upper-right|lower-left|lower-right
//! blank-after 600    // seconds of idle before the outputs blank
//! session "cde-session"
//! keyboard { layouts "us" "es"; }
//! bind "Super+E" "set-layout us"
//! ```

use std::path::{Path, PathBuf};

use crucible_config::{
    GreeterConfig, KeyboardConfig, Milli, N1_MED, Named, Palette, PanelPosition, Rgb,
    SurfaceBackground, SurfaceStyle, ThemeConfig, resolve_palette, surface_palette,
};
use kdl::{KdlDocument, KdlNode, KdlValue};

/// Seconds of idle before the greeter blanks when the user's config sets no `idle { blank-after }`.
pub const DEFAULT_BLANK_AFTER: u32 = 600;

/// The profile file name inside the `--profile` directory.
pub const PROFILE_FILE: &str = "profile.kdl";

/// The greeter's background: a solid colour or an absolute image path (never `stock`/`user`:
/// the publisher resolves those to a copied file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileBackground {
    /// A solid `#rrggbb` colour.
    Color(Rgb),
    /// An absolute image path, drawn by swaybg with `-m fill`.
    Image(PathBuf),
}

/// The parsed greeter profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GreeterProfile {
    /// The panel style (kept for the record; the colours are in `palette`).
    pub style: SurfaceStyle,
    /// The background swaybg draws.
    pub background: ProfileBackground,
    /// The resolved panel colours (style `terminal` carries the user's theme colours here).
    pub palette: Palette,
    /// The panel fill alpha (`1` opaque, below it the glass look).
    pub alpha: Milli,
    /// Where the login panel sits.
    pub position: PanelPosition,
    /// Seconds of idle before the outputs blank (the greeter never locks).
    pub blank_after: u32,
    /// The session command greetd starts.
    pub session: String,
    /// The xkb layouts, in group order (never empty).
    pub layouts: Vec<String>,
    /// The user's `set-layout` binds (`(chord, action)`), applied over the default binds.
    pub binds: Vec<(String, String)>,
}

/// Why a profile was rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProfileError {
    /// Not valid KDL.
    #[error("profile is not valid KDL: {0}")]
    Syntax(String),
    /// A node is unknown, misses its value or has a bad one.
    #[error("profile node `{node}`: {reason}")]
    Invalid {
        /// The node name.
        node: String,
        /// What is wrong.
        reason: String,
    },
}

impl GreeterProfile {
    /// The look without a readable profile: `n1-med` on `#000000`, opaque, default layouts.
    #[must_use]
    pub fn fallback() -> Self {
        Self {
            style: SurfaceStyle::N1Med,
            background: ProfileBackground::Color(Rgb(0, 0, 0)),
            palette: N1_MED,
            alpha: Milli(1000),
            position: crucible_config::DEFAULT_GREETER_POSITION,
            blank_after: DEFAULT_BLANK_AFTER,
            session: crucible_config::DEFAULT_GREETER_SESSION.to_string(),
            layouts: KeyboardConfig::default().layouts,
            binds: Vec::new(),
        }
    }

    /// Builds the profile the user session publishes. `background_image` is the copied file
    /// that stands for `stock`/`user` (without one those fall back to black); only `set-layout`
    /// entries of `binds` are kept. `blank_after` is the user's `idle { blank-after }`; `None` →
    /// [`DEFAULT_BLANK_AFTER`].
    #[must_use]
    pub fn from_config(
        greeter: &GreeterConfig,
        theme: Option<&ThemeConfig>,
        alpha: Milli,
        blank_after: Option<u32>,
        keyboard: &KeyboardConfig,
        binds: &[(String, String)],
        background_image: Option<&Path>,
    ) -> Self {
        let background = match (greeter.background, background_image) {
            (SurfaceBackground::Color(c), _) => ProfileBackground::Color(c),
            (_, Some(p)) => ProfileBackground::Image(p.to_path_buf()),
            (_, None) => ProfileBackground::Color(Rgb(0, 0, 0)),
        };
        Self {
            style: greeter.style,
            background,
            palette: surface_palette(greeter.style, greeter.accent, theme),
            alpha,
            position: greeter.position,
            blank_after: blank_after.unwrap_or(DEFAULT_BLANK_AFTER),
            session: greeter.session.clone(),
            layouts: keyboard.layouts.clone(),
            binds: binds
                .iter()
                .filter(|(_, a)| is_layout_bind(a))
                .cloned()
                .collect(),
        }
    }

    /// The keyboard config the greeter seat is built from.
    #[must_use]
    pub fn keyboard(&self) -> KeyboardConfig {
        KeyboardConfig {
            layouts: self.layouts.clone(),
        }
    }
}

fn is_layout_bind(action: &str) -> bool {
    action.split_whitespace().next() == Some("set-layout")
}

fn hex(c: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2)
}

fn node1(name: &str, value: impl Into<KdlValue>) -> KdlNode {
    let mut n = KdlNode::new(name);
    n.push(kdl::KdlEntry::new(value));
    n
}

fn block(name: &str, children: Vec<KdlNode>) -> KdlNode {
    let mut n = KdlNode::new(name);
    let mut doc = KdlDocument::new();
    doc.nodes_mut().extend(children);
    n.set_children(doc);
    n
}

/// Serializes `p` as `profile.kdl` text (round-trips through [`parse_profile`]).
#[must_use]
pub fn write_profile(p: &GreeterProfile) -> String {
    let mut doc = KdlDocument::new();
    let nodes = doc.nodes_mut();
    nodes.push(node1("style", p.style.as_str()));
    let bg = match &p.background {
        ProfileBackground::Color(c) => node1("color", hex(*c)),
        ProfileBackground::Image(path) => node1("image", path.to_string_lossy().into_owned()),
    };
    nodes.push(block("background", vec![bg]));
    let pal = &p.palette;
    nodes.push(block(
        "palette",
        vec![
            node1("bg", hex(pal.bg)),
            node1("text", hex(pal.text)),
            node1("line", hex(pal.line)),
            node1("accent", hex(pal.accent)),
        ],
    ));
    nodes.push(node1("alpha", f64::from(p.alpha.0) / 1000.0));
    nodes.push(node1("position", p.position.as_str()));
    nodes.push(node1("blank-after", i128::from(p.blank_after)));
    nodes.push(node1("session", p.session.clone()));
    let mut layouts = KdlNode::new("layouts");
    for l in &p.layouts {
        layouts.push(kdl::KdlEntry::new(l.clone()));
    }
    nodes.push(block("keyboard", vec![layouts]));
    for (chord, action) in &p.binds {
        let mut b = node1("bind", chord.clone());
        b.push(kdl::KdlEntry::new(action.clone()));
        nodes.push(b);
    }
    doc.autoformat();
    doc.to_string()
}

fn invalid(node: &str, reason: impl Into<String>) -> ProfileError {
    ProfileError::Invalid {
        node: node.to_string(),
        reason: reason.into(),
    }
}

fn args(n: &KdlNode) -> Vec<&KdlValue> {
    n.entries()
        .iter()
        .filter(|e| e.name().is_none())
        .map(kdl::KdlEntry::value)
        .collect()
}

fn str_arg<'a>(n: &'a KdlNode, name: &str) -> Result<&'a str, ProfileError> {
    match args(n).as_slice() {
        [v] => v
            .as_string()
            .ok_or_else(|| invalid(name, "expects one string")),
        _ => Err(invalid(name, "expects one string")),
    }
}

fn color_arg(n: &KdlNode, name: &str) -> Result<Rgb, ProfileError> {
    let s = str_arg(n, name)?;
    Rgb::from_hex(s).ok_or_else(|| invalid(name, format!("`{s}` is not #rrggbb")))
}

fn children(n: &KdlNode) -> &[KdlNode] {
    n.children().map_or(&[], KdlDocument::nodes)
}

/// Parses `profile.kdl` text. Every node is optional (missing ones keep the fallback value);
/// an unknown node or a bad value is an error.
pub fn parse_profile(text: &str) -> Result<GreeterProfile, ProfileError> {
    let doc = KdlDocument::parse(text).map_err(|e| ProfileError::Syntax(e.to_string()))?;
    let mut p = GreeterProfile::fallback();
    let mut palette = None;
    for n in doc.nodes() {
        let name = n.name().value();
        match name {
            "style" => {
                let s = str_arg(n, name)?;
                p.style = SurfaceStyle::from_name(s)
                    .ok_or_else(|| invalid(name, format!("unknown style `{s}`")))?;
            }
            "background" => {
                let [bg] = children(n) else {
                    return Err(invalid(name, "expects one `color` or `image` child"));
                };
                p.background = match bg.name().value() {
                    "color" => ProfileBackground::Color(color_arg(bg, "color")?),
                    "image" => {
                        let path = PathBuf::from(str_arg(bg, "image")?);
                        if !path.is_absolute() {
                            return Err(invalid("image", "the path must be absolute"));
                        }
                        ProfileBackground::Image(path)
                    }
                    other => return Err(invalid(name, format!("unknown child `{other}`"))),
                };
            }
            "palette" => {
                let mut pal = N1_MED;
                for c in children(n) {
                    let key = c.name().value();
                    let slot = match key {
                        "bg" => &mut pal.bg,
                        "text" => &mut pal.text,
                        "line" => &mut pal.line,
                        "accent" => &mut pal.accent,
                        other => return Err(invalid(name, format!("unknown child `{other}`"))),
                    };
                    *slot = color_arg(c, key)?;
                }
                palette = Some(pal);
            }
            "alpha" => {
                let v = match args(n).as_slice() {
                    [v] => v.as_float().or_else(|| {
                        #[allow(clippy::cast_precision_loss)]
                        v.as_integer().map(|i| i as f64)
                    }),
                    _ => None,
                };
                p.alpha = v
                    .filter(|a| (0.0..=1.0).contains(a))
                    .and_then(Milli::from_f64)
                    .ok_or_else(|| invalid(name, "expects a number in 0.0..=1.0"))?;
            }
            "position" => {
                let s = str_arg(n, name)?;
                p.position = PanelPosition::from_name(s)
                    .ok_or_else(|| invalid(name, format!("unknown position `{s}`")))?;
            }
            "blank-after" => {
                p.blank_after = match args(n).as_slice() {
                    [v] => v
                        .as_integer()
                        .and_then(|i| u32::try_from(i).ok())
                        .filter(|s| *s >= 1),
                    _ => None,
                }
                .ok_or_else(|| invalid(name, "expects one whole number of seconds, 1 or more"))?;
            }
            "session" => {
                let s = str_arg(n, name)?;
                if s.split_whitespace().next().is_none() {
                    return Err(invalid(name, "must not be empty"));
                }
                p.session = s.to_string();
            }
            "keyboard" => {
                for c in children(n) {
                    if c.name().value() != "layouts" {
                        return Err(invalid(name, "expects only `layouts`"));
                    }
                    let layouts: Option<Vec<String>> = args(c)
                        .iter()
                        .map(|v| v.as_string().map(str::to_string))
                        .collect();
                    match layouts {
                        Some(l) if !l.is_empty() => p.layouts = l,
                        _ => return Err(invalid("layouts", "expects one or more strings")),
                    }
                }
            }
            "bind" => {
                let (chord, action) = match args(n).as_slice() {
                    [c, a] => c.as_string().zip(a.as_string()),
                    _ => None,
                }
                .ok_or_else(|| invalid(name, "expects a chord and an action string"))?;
                if !is_layout_bind(action) {
                    return Err(invalid(name, "only `set-layout` binds are allowed"));
                }
                p.binds.push((chord.to_string(), action.to_string()));
            }
            other => return Err(invalid(other, "unknown node")),
        }
    }
    p.palette = palette.unwrap_or_else(|| resolve_palette(p.style, None));
    Ok(p)
}

/// The profile file for `--profile <dir>`; `None` without `--profile` (never `$HOME` or
/// `$XDG_CONFIG_HOME`).
#[must_use]
pub fn profile_path(dir: Option<&Path>) -> Option<PathBuf> {
    dir.map(|d| d.join(PROFILE_FILE))
}

/// Reads the profile under `dir`; missing, unreadable or invalid → the fallback, with one `warn!`.
#[must_use]
pub fn load_profile(dir: Option<&Path>) -> GreeterProfile {
    let Some(path) = profile_path(dir) else {
        tracing::warn!("no --profile given; using the fallback greeter look");
        return GreeterProfile::fallback();
    };
    let parsed = std::fs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|t| parse_profile(&t).map_err(|e| e.to_string()));
    match parsed {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "greeter profile unusable; using the fallback look");
            GreeterProfile::fallback()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_profile_round_trips() {
        let mut theme = ThemeConfig {
            background: Some(Rgb(1, 2, 3)),
            foreground: Some(Rgb(200, 201, 202)),
            ..ThemeConfig::default()
        };
        theme.palette[8] = Some(Rgb(9, 9, 9));
        let greeter = GreeterConfig {
            style: SurfaceStyle::Terminal,
            background: SurfaceBackground::User,
            position: PanelPosition::UpperRight,
            session: "cde-session --x".into(),
            ..GreeterConfig::default()
        };
        let kb = KeyboardConfig {
            layouts: vec!["us".into(), "es".into()],
        };
        let binds = vec![
            ("Super+E".to_string(), "set-layout us".to_string()),
            ("Super+Return".to_string(), "run-terminal".to_string()),
        ];
        let p = GreeterProfile::from_config(
            &greeter,
            Some(&theme),
            Milli(850),
            Some(90),
            &kb,
            &binds,
            Some(Path::new("/var/lib/cde-greeter/background.png")),
        );
        assert_eq!((p.position, p.blank_after), (PanelPosition::UpperRight, 90));
        assert!(write_profile(&p).contains("blank-after 90"));
        let none = GreeterProfile::from_config(&greeter, None, Milli(1000), None, &kb, &[], None);
        assert_eq!(none.blank_after, 600);
        assert_eq!(
            (
                GreeterProfile::fallback().position,
                GreeterProfile::fallback().blank_after
            ),
            (PanelPosition::Lower, 600)
        );
        assert_eq!(
            parse_profile("style \"n1-med\"\n").unwrap().position,
            PanelPosition::Lower,
            "old profiles keep the fallback"
        );
        assert_eq!(p.binds.len(), 1, "only set-layout binds are published");
        assert_eq!(p.palette.bg, Rgb(1, 2, 3));
        let text = write_profile(&p);
        assert_eq!(parse_profile(&text).unwrap(), p, "{text}");
        // A colour background round-trips too.
        let mut q = GreeterProfile::fallback();
        q.background = ProfileBackground::Color(Rgb(0x12, 0x34, 0x56));
        assert_eq!(parse_profile(&write_profile(&q)).unwrap(), q);
    }

    #[test]
    fn profile_palette_carries_the_resolved_accent() {
        let kb = KeyboardConfig {
            layouts: vec!["us".into()],
        };
        let theme = ThemeConfig {
            accent: Some(Rgb(0x33, 0xcc, 0xcc)),
            ..ThemeConfig::default()
        };
        let plain = GreeterConfig::default();
        let p =
            GreeterProfile::from_config(&plain, Some(&theme), Milli(1000), None, &kb, &[], None);
        assert_eq!(
            p.palette,
            crucible_config::N1_MED,
            "default accent: today's palette"
        );
        let g = GreeterConfig {
            accent: crucible_config::SurfaceAccent::Theme,
            ..GreeterConfig::default()
        };
        let p = GreeterProfile::from_config(&g, Some(&theme), Milli(1000), None, &kb, &[], None);
        assert_eq!(p.palette.accent, Rgb(0x33, 0xcc, 0xcc));
        assert_eq!(p.palette.bg, crucible_config::N1_MED.bg);
        let text = write_profile(&p);
        assert_eq!(parse_profile(&text).unwrap(), p, "{text}");
    }

    #[test]
    fn missing_or_invalid_profile_falls_back() {
        let dir = std::env::temp_dir().join(format!("cde-greeter-profile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Missing file, and no --profile at all.
        assert_eq!(load_profile(Some(&dir)), GreeterProfile::fallback());
        assert_eq!(load_profile(None), GreeterProfile::fallback());
        assert_eq!(profile_path(None), None);
        assert_eq!(
            profile_path(Some(&dir)),
            Some(dir.join("profile.kdl")),
            "only --profile decides the path"
        );
        // Invalid values.
        std::fs::write(dir.join(PROFILE_FILE), "style \"nope\"\n").unwrap();
        assert_eq!(load_profile(Some(&dir)), GreeterProfile::fallback());
        for bad in [
            "alpha 1.5",
            "position \"middle\"",
            "position",
            "blank-after 0",
            "blank-after -5",
            "blank-after 1.5",
            "blank-after \"9\"",
            "background { image \"relative.png\"; }",
            "bind \"Super+Return\" \"run-terminal\"",
            "keyboard { layouts; }",
            "{",
        ] {
            assert!(parse_profile(bad).is_err(), "{bad}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
        let f = GreeterProfile::fallback();
        assert_eq!(
            (f.style, f.alpha, f.layouts.as_slice()),
            (
                SurfaceStyle::N1Med,
                Milli(1000),
                ["us".to_string()].as_slice()
            )
        );
        assert_eq!(f.background, ProfileBackground::Color(Rgb(0, 0, 0)));
    }
}
