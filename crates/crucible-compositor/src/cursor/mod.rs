//! The compositor-drawn pointer cursor.
//!
//! Focus-ring borders and stack indicators live in [`crate::chrome`]; this module owns only the
//! cursor.
//!
//! ## The pointer cursor
//! [`Cursor`] is the single owner of the compositor's cursor: the current [`CursorImageStatus`]
//! (written by the seat's `cursor_image` callback, including the shapes clients request through
//! `wp_cursor_shape_v1`) and the cached cursor buffers. It draws either a client-provided cursor
//! surface (when an application sets one via `wl_pointer.set_cursor`) or, for a named shape, the
//! image of that shape from the installed XCursor theme ([`theme`]: config, then `XCURSOR_THEME` /
//! `XCURSOR_SIZE`, then `default`/24), offset by the image's hotspot. When no theme image can be
//! loaded for a shape it draws CDE's built-in [arrow](default_cursor_bitmap), whose tip is the
//! hotspot. Animated cursors draw their first frame.
//!
//! Images are drawn texel-for-texel at the output's physical resolution (one buffer per icon and
//! quantized scale), so they stay crisp at fractional scales; the theme image nearest
//! `size x scale` is chosen and never resampled.

use std::collections::HashMap;

use smithay::{
    backend::renderer::{
        ImportAll, ImportMem, Renderer,
        element::{
            Kind,
            memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            surface::WaylandSurfaceRenderElement,
        },
    },
    input::pointer::{CursorImageStatus, CursorImageSurfaceData},
    render_elements,
    utils::{Logical, Physical, Point, Rectangle, Scale, Size},
    wayland::compositor,
};

use crucible_config::CursorConfig;
use smithay::input::pointer::CursorIcon;
use tracing::info;

use crate::physical::{padded_dims, quantize_scale};

pub mod theme;

/// The environment variables to add to a spawned child so it draws cursors like CDE: only the
/// ones the config sets (`XCURSOR_THEME`, `XCURSOR_SIZE`), none when it sets neither.
#[must_use]
pub fn child_env(config: &CursorConfig) -> Vec<(&'static str, String)> {
    let mut vars = Vec::new();
    if let Some(theme) = &config.theme {
        vars.push(("XCURSOR_THEME", theme.clone()));
    }
    if let Some(size) = config.size {
        vars.push(("XCURSOR_SIZE", size.to_string()));
    }
    vars
}

/// The hotspot a client set with `wl_pointer.set_cursor` (logical, surface-local), else `(0, 0)`.
fn cursor_hotspot(
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
) -> Point<i32, Logical> {
    compositor::with_states(surface, |states| {
        states
            .data_map
            .get::<CursorImageSurfaceData>()
            .and_then(|m| m.lock().ok().map(|a| a.hotspot))
            .unwrap_or_default()
    })
}

/// The built-in arrow's artwork: `#` outline, `.` fill, anything else transparent. One character
/// per logical pixel; the tip is the top-left cell.
const ARROW: [&str; 17] = [
    "#",
    "##",
    "#.#",
    "#..#",
    "#...#",
    "#....#",
    "#.....#",
    "#......#",
    "#.......#",
    "#........#",
    "#.....#####",
    "#..#..#",
    "#.# #..#",
    "##  #..#",
    "#    #..#",
    "     #..#",
    "      ##",
];
/// Width, in logical pixels, of the arrow artwork.
const ARROW_W: i32 = 11;
/// Height, in logical pixels, of the arrow artwork.
const ARROW_H: i32 = ARROW.len() as i32;

/// Outline colour of the arrow, `[B, G, R, A]`.
const BORDER_PIXEL: [u8; 4] = [0x20, 0x20, 0x20, 0xff];
/// Fill colour of the arrow, `[B, G, R, A]`.
const INTERIOR_PIXEL: [u8; 4] = [0xf0, 0xf0, 0xf0, 0xff];

/// The arrow's physical extent `(w, h) = (round(11·s), round(17·s))` (each at least 1) at scale `s`.
#[must_use]
fn arrow_extent(s: f64) -> (i32, i32) {
    let side = |n: i32| ((f64::from(n) * s).round() as i32).max(1);
    (side(ARROW_W), side(ARROW_H))
}

/// The arrow pixel (`[B, G, R, A]`, transparent outside the arrow) at physical pixel `(x, y)`,
/// sampling the artwork at the pixel's centre so outline and fill stay crisp at any scale.
fn arrow_pixel(x: i32, y: i32, s: f64) -> [u8; 4] {
    let cell = |v: i32| ((f64::from(v) + 0.5) / s).floor() as usize;
    let (cx, cy) = (cell(x), cell(y));
    match ARROW.get(cy).and_then(|row| row.as_bytes().get(cx)) {
        Some(b'#') => BORDER_PIXEL,
        Some(b'.') => INTERIOR_PIXEL,
        _ => [0, 0, 0, 0],
    }
}

/// Builds CDE's built-in fallback arrow at scale `s` as a packed `Argb8888` bitmap.
///
/// A classic pointer: dark outline, light fill, tip at the top-left pixel `(0, 0)` (its hotspot).
/// The artwork is sampled at `round(11·s) x round(17·s)` physical pixels and padded with
/// transparent pixels right/bottom to [`padded_dims`]'s physical size so it displays 1:1. Returns
/// `(bytes, phys, logical)` (the padded physical size and the logical size it displays at); the
/// bytes are 4 per pixel in the byte order expected by
/// [`Fourcc::Argb8888`](smithay::backend::allocator::Fourcc::Argb8888) on little-endian hosts
/// (`[B, G, R, A]`).
#[must_use]
pub fn default_cursor_bitmap(s: f64) -> (Vec<u8>, Size<i32, Physical>, Size<i32, Logical>) {
    let (aw, ah) = arrow_extent(s);
    let (phys, logical) = padded_dims(aw, ah, s, s);
    let (w, h) = (phys.w, phys.h);
    let mut data = vec![0u8; (w * h * 4) as usize];
    for y in 0..ah {
        for x in 0..aw {
            let idx = ((y * w + x) * 4) as usize;
            data[idx..idx + 4].copy_from_slice(&arrow_pixel(x, y, s));
        }
    }
    (data, phys, logical)
}

/// One uploaded cursor bitmap (theme image or arrow) and its 1:1 display geometry.
#[derive(Debug)]
struct CursorBuffer {
    /// The bitmap (buffer scale 1).
    buffer: MemoryRenderBuffer,
    /// Its physical size (the padded bitmap).
    phys: Size<i32, Physical>,
    /// The logical size it is displayed at, so `round(logical * s) == phys`.
    logical: Size<i32, Logical>,
    /// The hotspot, physical pixels from the bitmap's top-left.
    hotspot: Point<i32, Physical>,
}

impl CursorBuffer {
    fn upload(
        bytes: &[u8],
        phys: Size<i32, Physical>,
        logical: Size<i32, Logical>,
        hotspot: Point<i32, Physical>,
    ) -> Self {
        Self {
            buffer: MemoryRenderBuffer::from_slice(
                bytes,
                smithay::backend::allocator::Fourcc::Argb8888,
                (phys.w, phys.h),
                1,
                smithay::utils::Transform::Normal,
                None,
            ),
            phys,
            logical,
            hotspot,
        }
    }
}

/// The built-in arrow for scale `s` in `arrows`, rasterized on first use per quantized scale.
fn arrow_in(arrows: &mut HashMap<u32, CursorBuffer>, s: f64) -> &CursorBuffer {
    arrows.entry(quantize_scale(s)).or_insert_with(|| {
        let (bytes, phys, logical) = default_cursor_bitmap(s);
        CursorBuffer::upload(&bytes, phys, logical, Point::default())
    })
}

/// The compositor's cursor: the single owner of the cursor state.
///
/// Tracks the current [`CursorImageStatus`] (hidden, a named/default shape, or a client-provided
/// surface) and renders the appropriate element each frame. Stored on
/// [`crate::state::CrucibleState`] and updated from the seat's `cursor_image` callback.
#[derive(Debug)]
pub struct Cursor {
    /// The cursor state most recently requested by the focused client (or the default).
    pub status: CursorImageStatus,
    /// The loaded theme and its resolved `(name, size)`; `None` until [`Self::set_theme`] (the
    /// arrow is drawn meanwhile).
    theme: Option<theme::ThemeState>,
    /// The built-in arrow, keyed by [`quantize_scale`] of the output's scale.
    arrows: HashMap<u32, CursorBuffer>,
    /// Theme images keyed by (icon, [`quantize_scale`]); `None` records a failed lookup so the
    /// disk is not re-read every frame (the arrow is drawn).
    named: HashMap<(CursorIcon, u32), Option<CursorBuffer>>,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            status: CursorImageStatus::default_named(),
            theme: None,
            arrows: HashMap::new(),
            named: HashMap::new(),
        }
    }
}

impl Cursor {
    /// The `(theme, size)` currently loaded, `None` before the first [`Self::set_theme`].
    #[must_use]
    pub fn loaded(&self) -> Option<(&str, u32)> {
        self.theme.as_ref().map(|t| (t.name.as_str(), t.size))
    }

    /// Loads theme `name` at nominal `size` and drops every cached image. Logs one `info!` line
    /// naming the theme, the size and whether the built-in arrow is what gets drawn.
    pub fn set_theme(&mut self, name: &str, size: u32) {
        let state = theme::ThemeState::load(name, size);
        let arrow_fallback = !state.has_default_pointer();
        info!(theme = name, size, arrow_fallback, "cursor theme");
        self.theme = Some(state);
        self.named.clear();
    }

    /// Makes sure the buffer for `icon` at scale `s` is cached (theme image, else `None`).
    fn ensure_named(&mut self, icon: CursorIcon, s: f64) {
        let key = (icon, quantize_scale(s));
        if self.named.contains_key(&key) {
            return;
        }
        let buffer = self
            .theme
            .as_mut()
            .and_then(|t| t.image_for(icon, s))
            .map(|img| CursorBuffer::upload(&img.bytes, img.phys, img.logical, img.hotspot));
        self.named.insert(key, buffer);
    }

    /// The buffer to draw for `icon` at scale `s`: the theme image, else the built-in arrow.
    fn buffer_for(&mut self, icon: CursorIcon, s: f64) -> &CursorBuffer {
        self.ensure_named(icon, s);
        // Disjoint fields, so the `named` borrow can be returned while `arrows` is the fallback.
        let Self { named, arrows, .. } = self;
        match named.get(&(icon, quantize_scale(s))) {
            Some(Some(buffer)) => buffer,
            _ => arrow_in(arrows, s),
        }
    }

    /// Renders the cursor with its hotspot at the output-local physical `location`.
    pub fn render_elements<R>(
        &mut self,
        renderer: &mut R,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Vec<PointerRenderElement<R>>
    where
        R: Renderer + ImportAll + ImportMem,
        R::TextureId: Send + Clone + 'static,
    {
        match &self.status {
            // Nothing to draw when the client explicitly hides the cursor.
            CursorImageStatus::Hidden => vec![],
            // A named shape: draw its theme image (else the built-in arrow) 1:1, hotspot at the
            // pointer location.
            CursorImageStatus::Named(icon) => {
                let icon = *icon;
                let buf = self.buffer_for(icon, scale.x);
                match MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    (location - buf.hotspot).to_f64(),
                    &buf.buffer,
                    Some(alpha),
                    Some(Rectangle::from_size(
                        (f64::from(buf.phys.w), f64::from(buf.phys.h)).into(),
                    )),
                    Some(buf.logical),
                    Kind::Cursor,
                ) {
                    Ok(element) => vec![PointerRenderElement::<R>::from(element)],
                    Err(_) => vec![],
                }
            }
            // A client-provided cursor surface: draw its surface tree.
            CursorImageStatus::Surface(surface) => {
                // The pointer position is the hotspot, so the surface's top-left sits `hotspot` up-left
                // of it (unshifted, an I-beam drew right/below where clicks land).
                let location = location - cursor_hotspot(surface).to_physical_precise_round(scale);
                smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                    renderer,
                    surface,
                    location,
                    scale,
                    alpha,
                    Kind::Cursor,
                )
            }
        }
    }
}

render_elements! {
    /// Render element for the compositor cursor: either a client cursor surface tree or the
    /// built-in default-cursor bitmap.
    pub PointerRenderElement<R> where R: ImportAll + ImportMem;
    /// A client-provided cursor surface.
    Surface=WaylandSurfaceRenderElement<R>,
    /// CDE's built-in default-cursor bitmap.
    Memory=MemoryRenderBufferRenderElement<R>,
}

impl<R: Renderer> std::fmt::Debug for PointerRenderElement<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Surface(arg0) => f.debug_tuple("Surface").field(arg0).finish(),
            Self::Memory(arg0) => f.debug_tuple("Memory").field(arg0).finish(),
            Self::_GenericCatcher(arg0) => f.debug_tuple("_GenericCatcher").field(arg0).finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(data: &[u8], w: i32, x: i32, y: i32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        data[i..i + 4].try_into().unwrap()
    }

    /// Checks the arrow at scale `s`: size, 1:1 rule, tip, filled interior, transparent outside.
    fn check(s: f64) {
        let (aw, ah) = arrow_extent(s);
        let (data, phys, logical) = default_cursor_bitmap(s);
        assert_eq!(padded_dims(aw, ah, s, s), (phys, logical));
        let (w, h) = (phys.w, phys.h);
        assert!(w >= aw && h >= ah);
        assert_eq!((f64::from(logical.w) * s).round() as i32, w);
        assert_eq!((f64::from(logical.h) * s).round() as i32, h);
        assert_eq!(data.len(), (w * h * 4) as usize);
        // The tip is the outline at the hotspot (0, 0).
        assert_eq!(pixel(&data, w, 0, 0), BORDER_PIXEL);
        // A filled interior pixel: logical cell (2, 5) is `.`.
        let at = |cx: f64, cy: f64| ((cx * s) as i32, (cy * s) as i32);
        let (ix, iy) = at(2.5, 5.5);
        assert_eq!(pixel(&data, w, ix, iy), INTERIOR_PIXEL);
        // Transparent inside the extent but outside the arrow: top-right of the artwork.
        let (ox, oy) = at(9.5, 0.5);
        assert_eq!(pixel(&data, w, ox, oy), [0, 0, 0, 0]);
        // Padding right/bottom is transparent.
        for x in aw..w {
            assert_eq!(pixel(&data, w, x, 0), [0, 0, 0, 0]);
        }
        for y in ah..h {
            assert_eq!(pixel(&data, w, 0, y), [0, 0, 0, 0]);
        }
        // Every arrow pixel lies inside the extent: the bitmap has opaque pixels only there.
        for y in 0..h {
            for x in 0..w {
                if x >= aw || y >= ah {
                    assert_eq!(pixel(&data, w, x, y)[3], 0);
                }
            }
        }
    }

    #[test]
    fn arrow_at_scale_one_is_the_artwork() {
        assert_eq!(arrow_extent(1.0), (ARROW_W, ARROW_H));
        let (data, phys, _) = default_cursor_bitmap(1.0);
        let (w, h) = (phys.w, phys.h);
        assert_eq!((w, h), (11, 17));
        // Tip and the end of the bottom row.
        assert_eq!(pixel(&data, w, 0, 0), BORDER_PIXEL);
        assert_eq!(pixel(&data, w, 6, 16), BORDER_PIXEL);
        assert_eq!(pixel(&data, w, 8, 16), [0, 0, 0, 0]);
        check(1.0);
    }

    #[test]
    fn arrow_rasterizes_at_physical_size_for_fractional_and_integer_scales() {
        check(170.0 / 120.0);
        check(2.0);
        check(0.5);
    }

    #[test]
    fn buffers_are_cached_per_quantized_scale_and_arrow_has_a_zero_hotspot() {
        let mut cursor = Cursor::default();
        let _ = arrow_in(&mut cursor.arrows, 1.5);
        let _ = arrow_in(&mut cursor.arrows, 1.5 + 1e-6);
        assert_eq!(cursor.arrows.len(), 1);
        let _ = arrow_in(&mut cursor.arrows, 2.0);
        assert_eq!(cursor.arrows.len(), 2);
        let b = arrow_in(&mut cursor.arrows, 1.0);
        assert_eq!(b.logical, Size::from((11, 17)));
        assert_eq!(b.hotspot, Point::default());
    }

    #[test]
    fn without_a_theme_every_icon_is_the_arrow_and_the_failure_is_cached() {
        let mut cursor = Cursor::default();
        assert_eq!(cursor.loaded(), None);
        let hotspot = cursor.buffer_for(CursorIcon::Text, 1.0).hotspot;
        assert_eq!(hotspot, Point::default());
        assert_eq!(cursor.named.len(), 1);
        assert!(cursor.named.values().all(Option::is_none));
        let _ = cursor.buffer_for(CursorIcon::Text, 1.0);
        assert_eq!(cursor.named.len(), 1);
    }

    #[test]
    fn theme_images_are_cached_with_their_hotspot() {
        use theme::fixture::{Img, load_theme, tempdir, write_icon};
        let root = tempdir();
        write_icon(
            &root,
            "T",
            "text",
            &[Img {
                size: 24,
                w: 4,
                h: 4,
                hot: (2, 3),
                pixel: [1, 2, 3, 4],
            }],
        );
        let mut cursor = Cursor {
            theme: Some(theme::ThemeState::with_theme(
                load_theme(&root, "T"),
                "T",
                24,
            )),
            ..Cursor::default()
        };
        assert_eq!(
            cursor.buffer_for(CursorIcon::Text, 1.0).hotspot,
            Point::from((2, 3))
        );
        // A shape the theme lacks (and no default) falls back to the arrow, cached as None.
        assert_eq!(
            cursor.buffer_for(CursorIcon::Wait, 1.0).hotspot,
            Point::default()
        );
        assert_eq!(cursor.named.len(), 2);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn child_env_exports_only_what_the_config_sets() {
        assert!(child_env(&CursorConfig::default()).is_empty());
        let both = CursorConfig {
            theme: Some("Adwaita".into()),
            size: Some(32),
        };
        assert_eq!(
            child_env(&both),
            vec![
                ("XCURSOR_THEME", "Adwaita".to_string()),
                ("XCURSOR_SIZE", "32".to_string())
            ]
        );
        let size_only = CursorConfig {
            theme: None,
            size: Some(48),
        };
        assert_eq!(
            child_env(&size_only),
            vec![("XCURSOR_SIZE", "48".to_string())]
        );
    }
}
