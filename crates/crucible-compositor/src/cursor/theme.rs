//! XCursor theme loading for the compositor-drawn cursor.
//!
//! Pure pieces first ([`resolve`], [`icon_candidates`], [`pick_image`], [`build_image`]), then
//! [`ThemeState`], which owns the loaded [`CursorTheme`] and turns a [`CursorIcon`] into a
//! ready-to-upload [`ThemeImage`]. Every failure (missing theme, missing icon, bad file) ends in
//! `None`, which the caller draws as the built-in arrow; nothing here panics.
//!
//! Byte order: the `xcursor` crate's `pixels_rgba` field is the file's pixel order, and XCursor
//! files store each pixel as a little-endian ARGB32, so those bytes are already `[B, G, R, A]`
//! (the order [`Fourcc::Argb8888`](smithay::backend::allocator::Fourcc::Argb8888) wants). They are
//! copied as-is; `pixels_argb` (`[A, R, G, B]`) is not used.

use std::collections::HashSet;

use crucible_config::CursorConfig;
use smithay::{
    input::pointer::CursorIcon,
    utils::{Logical, Physical, Point, Size},
};
use tracing::warn;
use xcursor::{CursorTheme, parser::Image};

use crate::physical::padded_dims;

/// The nominal size used when neither the config nor `XCURSOR_SIZE` gives one.
pub const DEFAULT_SIZE: u32 = 24;
/// The theme used when neither the config nor `XCURSOR_THEME` names one.
pub const DEFAULT_THEME: &str = "default";

/// The theme name and nominal size: config, then `XCURSOR_THEME`/`XCURSOR_SIZE` (an empty theme
/// or an unparsable or zero size is ignored), then `"default"`/24.
#[must_use]
pub fn resolve(config: &CursorConfig, env: impl Fn(&str) -> Option<String>) -> (String, u32) {
    let name = config
        .theme
        .clone()
        .or_else(|| env("XCURSOR_THEME").filter(|t| !t.trim().is_empty()))
        .unwrap_or_else(|| DEFAULT_THEME.to_string());
    let size = config
        .size
        .or_else(|| {
            env("XCURSOR_SIZE")
                .and_then(|s| s.trim().parse::<u32>().ok())
                .filter(|&n| n > 0)
        })
        .unwrap_or(DEFAULT_SIZE);
    (name, size)
}

/// The file names tried for `icon`, in order: its own name, its `alt_names()`, `default`, then
/// `left_ptr`. The returned count says how many leading entries are the icon's own names; the
/// rest are fallbacks to another shape.
#[must_use]
pub fn icon_candidates(icon: CursorIcon) -> (Vec<&'static str>, usize) {
    let mut names: Vec<&'static str> = vec![icon.name()];
    for alt in icon.alt_names() {
        if !names.contains(alt) {
            names.push(alt);
        }
    }
    let own = names.len();
    for fallback in ["default", "left_ptr"] {
        if !names.contains(&fallback) {
            names.push(fallback);
        }
    }
    (names, own)
}

/// The image whose nominal size is nearest `target` (ties go to the larger one); of several frames
/// at that size, the first. `None` for an empty list.
#[must_use]
pub fn pick_image(images: &[Image], target: u32) -> Option<&Image> {
    let best = images
        .iter()
        .min_by_key(|img| (img.size.abs_diff(target), std::cmp::Reverse(img.size)))?
        .size;
    images.iter().find(|img| img.size == best)
}

/// A theme image ready for upload: the padded `[B, G, R, A]` bytes and its 1:1 display geometry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeImage {
    /// `[B, G, R, A]` bytes, `phys.w * phys.h * 4` of them, the image at the top-left and
    /// transparent padding right/bottom.
    pub bytes: Vec<u8>,
    /// The padded bitmap's physical size.
    pub phys: Size<i32, Physical>,
    /// The logical size it is displayed at so `round(logical * s) == phys`.
    pub logical: Size<i32, Logical>,
    /// The hotspot in physical pixels (image pixels; drawn 1:1).
    pub hotspot: Point<i32, Physical>,
}

/// Pads `img` to [`padded_dims`] at scale `s` for a 1:1 draw. `None` for an image whose pixel
/// buffer does not match its dimensions or whose dimensions overflow `i32`.
#[must_use]
pub fn build_image(img: &Image, s: f64) -> Option<ThemeImage> {
    let (w, h) = (
        i32::try_from(img.width).ok()?,
        i32::try_from(img.height).ok()?,
    );
    if img.pixels_rgba.len() != (w as usize) * (h as usize) * 4 {
        return None;
    }
    let (phys, logical) = padded_dims(w, h, s, s);
    let mut bytes = vec![0u8; (phys.w as usize) * (phys.h as usize) * 4];
    let (src_row, dst_row) = ((w as usize) * 4, (phys.w as usize) * 4);
    for y in 0..h as usize {
        bytes[y * dst_row..y * dst_row + src_row]
            .copy_from_slice(&img.pixels_rgba[y * src_row..(y + 1) * src_row]);
    }
    let hotspot = (
        i32::try_from(img.xhot).ok()?.min(w),
        i32::try_from(img.yhot).ok()?.min(h),
    )
        .into();
    Some(ThemeImage {
        bytes,
        phys,
        logical,
        hotspot,
    })
}

/// The loaded cursor theme and the set of icons already warned about.
#[derive(Debug)]
pub struct ThemeState {
    /// Resolved theme name.
    pub name: String,
    /// Resolved nominal size, logical pixels.
    pub size: u32,
    theme: CursorTheme,
    warned: HashSet<CursorIcon>,
}

impl ThemeState {
    /// Loads theme `name` (search path from `XCURSOR_PATH`/XDG/`~/.icons`). A missing theme is not
    /// an error here; its icons simply fail to resolve.
    #[must_use]
    pub fn load(name: &str, size: u32) -> Self {
        Self::with_theme(CursorTheme::load(name), name, size)
    }

    /// A state over an already-loaded `theme`.
    #[must_use]
    pub fn with_theme(theme: CursorTheme, name: &str, size: u32) -> Self {
        Self {
            name: name.to_string(),
            size,
            theme,
            warned: HashSet::new(),
        }
    }

    /// Whether the theme provides a pointer for the default shape at all (otherwise the built-in
    /// arrow is what every shape ends up as).
    #[must_use]
    pub fn has_default_pointer(&self) -> bool {
        icon_candidates(CursorIcon::Default)
            .0
            .iter()
            .any(|n| self.theme.load_icon(n).is_some())
    }

    /// The theme image for `icon` at output scale `s`, or `None` to draw the built-in arrow.
    /// Logs one `warn!` per icon when its own names cannot be loaded.
    pub fn image_for(&mut self, icon: CursorIcon, s: f64) -> Option<ThemeImage> {
        let target = (f64::from(self.size) * s).round().max(1.0) as u32;
        let (candidates, own) = icon_candidates(icon);
        for (i, name) in candidates.iter().enumerate() {
            let Some(path) = self.theme.load_icon(name) else {
                continue;
            };
            let parsed = std::fs::read(&path)
                .ok()
                .and_then(|bytes| xcursor::parser::parse_xcursor(&bytes))
                .and_then(|images| build_image(pick_image(&images, target)?, s));
            match parsed {
                Some(image) => {
                    if i >= own {
                        self.warn_once(icon, "not in the theme; using a fallback shape");
                    }
                    return Some(image);
                }
                None => self.warn_once(icon, "cursor file could not be parsed"),
            }
        }
        self.warn_once(icon, "no cursor found; using the built-in arrow");
        None
    }

    fn warn_once(&mut self, icon: CursorIcon, what: &str) {
        if self.warned.insert(icon) {
            warn!(theme = %self.name, icon = icon.name(), "cursor: {what}");
        }
    }
}

#[cfg(test)]
pub(crate) mod fixture {
    //! A tempdir XCursor theme built with a minimal in-test writer of the XCursor binary format.

    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use xcursor::CursorTheme;

    /// `XCURSOR_PATH` is process-global: loads serialize on this.
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// One image to write: nominal `size`, `w`x`h`, hotspot, and every pixel `[B, G, R, A]`.
    pub(crate) struct Img {
        pub(crate) size: u32,
        pub(crate) w: u32,
        pub(crate) h: u32,
        pub(crate) hot: (u32, u32),
        pub(crate) pixel: [u8; 4],
    }

    /// The bytes of an XCursor file holding `imgs` (header `Xcur`, TOC, one chunk per image).
    pub(crate) fn xcursor_file(imgs: &[Img]) -> Vec<u8> {
        let mut out = Vec::new();
        let u32le = |out: &mut Vec<u8>, v: u32| out.extend_from_slice(&v.to_le_bytes());
        out.extend_from_slice(b"Xcur");
        u32le(&mut out, 16);
        u32le(&mut out, 0x1_0000);
        u32le(&mut out, imgs.len() as u32);
        let mut pos = 16 + 12 * imgs.len() as u32;
        for img in imgs {
            u32le(&mut out, 0xfffd_0002);
            u32le(&mut out, img.size);
            u32le(&mut out, pos);
            pos += 36 + img.w * img.h * 4;
        }
        for img in imgs {
            for v in [
                36,
                0xfffd_0002,
                img.size,
                1,
                img.w,
                img.h,
                img.hot.0,
                img.hot.1,
                0,
            ] {
                u32le(&mut out, v);
            }
            for _ in 0..img.w * img.h {
                out.extend_from_slice(&img.pixel);
            }
        }
        out
    }

    /// A fresh empty directory under the system temp dir.
    pub(crate) fn tempdir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("cde-cursor-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Writes `<root>/<theme>/cursors/<name>`.
    pub(crate) fn write_icon(root: &Path, theme: &str, name: &str, imgs: &[Img]) {
        let dir = root.join(theme).join("cursors");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), xcursor_file(imgs)).unwrap();
    }

    /// Loads theme `name` with the search path pinned to `root` alone (never the host's dirs).
    pub(crate) fn load_theme(root: &Path, name: &str) -> CursorTheme {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("XCURSOR_PATH");
        // SAFETY: serialized on ENV_LOCK; only this fixture touches XCURSOR_PATH in tests.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("XCURSOR_PATH", root);
        }
        let theme = CursorTheme::load(name);
        #[allow(unsafe_code)]
        unsafe {
            match prev {
                Some(v) => std::env::set_var("XCURSOR_PATH", v),
                None => std::env::remove_var("XCURSOR_PATH"),
            }
        }
        theme
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{Img, load_theme, tempdir, write_icon};
    use super::*;
    use std::collections::HashMap;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        let map: HashMap<&str, &str> = pairs.iter().copied().collect();
        move |k| map.get(k).map(|v| (*v).to_string())
    }

    fn img(size: u32) -> Img {
        Img {
            size,
            w: 4,
            h: 4,
            hot: (1, 2),
            pixel: [1, 2, 3, 4],
        }
    }

    fn fake_image(size: u32, tag: u8) -> Image {
        Image {
            size,
            width: 1,
            height: 1,
            xhot: 0,
            yhot: 0,
            delay: 0,
            pixels_rgba: vec![tag; 4],
            pixels_argb: vec![tag; 4],
        }
    }

    #[test]
    fn resolve_priority_config_then_env_then_default() {
        let cfg = CursorConfig {
            theme: Some("Cfg".into()),
            size: Some(48),
        };
        let env = env_of(&[("XCURSOR_THEME", "Env"), ("XCURSOR_SIZE", "32")]);
        assert_eq!(resolve(&cfg, &env), ("Cfg".to_string(), 48));
        assert_eq!(
            resolve(&CursorConfig::default(), &env),
            ("Env".to_string(), 32)
        );
        assert_eq!(
            resolve(&CursorConfig::default(), env_of(&[])),
            ("default".to_string(), 24)
        );
        // Each field falls through independently.
        let theme_only = CursorConfig {
            theme: Some("Cfg".into()),
            size: None,
        };
        assert_eq!(resolve(&theme_only, &env), ("Cfg".to_string(), 32));
    }

    #[test]
    fn resolve_ignores_bad_env_size() {
        for bad in ["0", "abc", "-4", "", "2.5"] {
            let pairs = [("XCURSOR_SIZE", bad)];
            let env = env_of(&pairs);
            assert_eq!(resolve(&CursorConfig::default(), env).1, 24, "{bad:?}");
        }
    }

    #[test]
    fn candidates_are_name_alts_default_left_ptr() {
        let (names, own) = icon_candidates(CursorIcon::Text);
        assert_eq!(names[0], "text");
        assert!(
            names[own..].ends_with(&["default", "left_ptr"]),
            "{names:?}"
        );
        // The default shape's alt names already include left_ptr; no duplicates.
        let (names, _) = icon_candidates(CursorIcon::Default);
        assert_eq!(names.iter().filter(|n| **n == "left_ptr").count(), 1);
        assert_eq!(names.iter().filter(|n| **n == "default").count(), 1);
    }

    #[test]
    fn pick_nearest_size_with_ties_to_the_larger() {
        let imgs = [fake_image(24, 1), fake_image(32, 2), fake_image(48, 3)];
        assert_eq!(pick_image(&imgs, 30).unwrap().size, 32);
        assert_eq!(pick_image(&imgs, 1).unwrap().size, 24);
        assert_eq!(pick_image(&imgs, 400).unwrap().size, 48);
        // 28 is 4 from both 24 and 32: the larger wins.
        assert_eq!(pick_image(&imgs, 28).unwrap().size, 32);
        // Several frames at the winning size: the first.
        let frames = [fake_image(24, 7), fake_image(24, 8)];
        assert_eq!(pick_image(&frames, 24).unwrap().pixels_rgba[0], 7);
        assert!(pick_image(&[], 24).is_none());
    }

    #[test]
    fn build_image_keeps_bytes_hotspot_and_pads_to_one_to_one() {
        let i = xcursor::parser::parse_xcursor(&super::fixture::xcursor_file(&[img(4)])).unwrap();
        let built = build_image(&i[0], 1.0).unwrap();
        assert_eq!(built.hotspot, Point::from((1, 2)));
        assert_eq!((built.phys.w, built.phys.h), (4, 4));
        // ARGB byte order: the fixture pixel [B, G, R, A] comes through untouched.
        assert_eq!(&built.bytes[0..4], &[1, 2, 3, 4]);
        // A fractional scale pads right/bottom with transparent pixels; the image stays at (0,0).
        let s = 170.0 / 120.0;
        let built = build_image(&i[0], s).unwrap();
        let (phys, logical) = padded_dims(4, 4, s, s);
        assert_eq!((built.phys, built.logical), (phys, logical));
        assert_eq!(built.bytes.len(), (phys.w * phys.h * 4) as usize);
        assert_eq!(&built.bytes[0..4], &[1, 2, 3, 4]);
        assert_eq!(built.hotspot, Point::from((1, 2)));
        // A truncated pixel buffer is rejected, not panicked on.
        let mut bad = i[0].clone();
        bad.pixels_rgba.pop();
        assert!(build_image(&bad, 1.0).is_none());
    }

    #[test]
    fn lookup_chain_requested_alt_default_then_arrow() {
        let root = tempdir();
        // The theme has `left_ptr` and `text`; `pointer` exists only under its alt name `hand2`.
        write_icon(&root, "T", "left_ptr", &[img(24)]);
        write_icon(
            &root,
            "T",
            "text",
            &[Img {
                pixel: [9, 9, 9, 9],
                ..img(24)
            }],
        );
        write_icon(
            &root,
            "T",
            "hand2",
            &[Img {
                pixel: [5, 5, 5, 5],
                ..img(24)
            }],
        );
        let mut st = ThemeState::with_theme(load_theme(&root, "T"), "T", 24);
        // Requested icon present.
        assert_eq!(st.image_for(CursorIcon::Text, 1.0).unwrap().bytes[0], 9);
        // Found through an alt name.
        assert_eq!(st.image_for(CursorIcon::Pointer, 1.0).unwrap().bytes[0], 5);
        // Missing: falls back to left_ptr (via the default candidates).
        assert_eq!(st.image_for(CursorIcon::Wait, 1.0).unwrap().bytes[0], 1);
        assert!(st.has_default_pointer());
        assert!(st.warned.contains(&CursorIcon::Wait));
        assert!(!st.warned.contains(&CursorIcon::Text));
        // Nothing at all in the theme (the search path is the tempdir only).
        let mut empty = ThemeState::with_theme(load_theme(&root, "Missing"), "Missing", 24);
        assert!(empty.image_for(CursorIcon::Text, 1.0).is_none());
        assert!(!empty.has_default_pointer());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn default_theme_is_the_fallback_through_inherits() {
        let root = tempdir();
        write_icon(&root, "default", "default", &[img(24)]);
        std::fs::create_dir_all(root.join("Child")).unwrap();
        // No index.theme: xcursor inherits from "default".
        let mut st = ThemeState::with_theme(load_theme(&root, "Child"), "Child", 24);
        assert_eq!(
            st.image_for(CursorIcon::Crosshair, 1.0).unwrap().bytes[0],
            1
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_corrupt_cursor_file_falls_through_without_panicking() {
        let root = tempdir();
        let dir = root.join("T").join("cursors");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("text"), b"not an xcursor").unwrap();
        write_icon(&root, "T", "left_ptr", &[img(24)]);
        let mut st = ThemeState::with_theme(load_theme(&root, "T"), "T", 24);
        assert!(st.image_for(CursorIcon::Text, 1.0).is_some());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn size_scales_the_nominal_target() {
        let root = tempdir();
        write_icon(
            &root,
            "T",
            "left_ptr",
            &[
                img(24),
                Img {
                    pixel: [7, 7, 7, 7],
                    ..img(48)
                },
            ],
        );
        let mut st = ThemeState::with_theme(load_theme(&root, "T"), "T", 24);
        assert_eq!(st.image_for(CursorIcon::Default, 2.0).unwrap().bytes[0], 7);
        assert_eq!(st.image_for(CursorIcon::Default, 1.0).unwrap().bytes[0], 1);
        std::fs::remove_dir_all(&root).ok();
    }
}
