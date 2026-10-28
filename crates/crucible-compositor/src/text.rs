//! Compositor-drawn text: font resolution, `fontdue` rasterization, and a cached Smithay
//! [`MemoryRenderBuffer`] render element (consumed by the [`crate::header`] row).
//!
//! ## Font resolution
//! [`resolve_font_path`]: an explicit override path → else `fc-match -f '%{file}' <family>` (the
//! fontconfig **CLI**; fontconfig is not linked) → else [`FALLBACK_FONT`]. An unreadable or absent
//! font disables text: [`TextRenderer::load`] logs one `warn!` and every later call returns `None`
//! without further warnings.
//!
//! ## Pixel format
//! Rasterized buffers are [`Fourcc::Argb8888`] — little-endian, so each pixel is stored as the
//! bytes `[B, G, R, A]` — with **premultiplied** alpha, which is what Smithay's GLES/pixman
//! renderers blend with.
//!
//! ## Element ordering contract
//! Text elements (the header labels, as
//! [`CustomRenderElements::Text`](crate::render::CustomRenderElements)) draw **above chrome** and
//! below the cursor. Both backends get that order from the one shared composition,
//! [`crate::render::frame_elements`]: cursor → header (labels, then its solids) → chrome →
//! windows, front-to-back.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use smithay::backend::{
    allocator::Fourcc,
    renderer::{
        ImportMem, Renderer,
        element::{
            Kind,
            memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
        },
    },
};
use smithay::utils::{Physical, Point, Transform};

/// Last-resort font file when neither the override nor `fc-match` yields a readable path.
pub const FALLBACK_FONT: &str = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf";

/// Upper bound on cached buffers; the cache is cleared wholesale when exceeded.
const CACHE_CAP: usize = 256;

/// Resolves the font file to load. `fc_match` runs the fontconfig CLI for `family` and `exists`
/// checks a candidate path; both are injected so the fallback order is unit-testable.
pub fn resolve_font_path(
    override_path: Option<&Path>,
    family: &str,
    fc_match: impl Fn(&str) -> Option<String>,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if let Some(p) = override_path
        && exists(p)
    {
        return Some(p.to_path_buf());
    }
    if let Some(s) = fc_match(family) {
        let p = PathBuf::from(s.trim());
        if !p.as_os_str().is_empty() && exists(&p) {
            return Some(p);
        }
    }
    let fallback = Path::new(FALLBACK_FONT);
    exists(fallback).then(|| fallback.to_path_buf())
}

/// Runs `fc-match -f '%{file}' <family>`; `None` if the CLI is absent or fails.
pub fn system_fc_match(family: &str) -> Option<String> {
    let out = std::process::Command::new("fc-match")
        .args(["-f", "%{file}", family])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// A rasterized string: premultiplied `Argb8888` pixels (`[B, G, R, A]` bytes), row stride
/// `width * 4`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RasterText {
    /// Width in physical pixels.
    pub width: i32,
    /// Height in physical pixels (the layout's line height).
    pub height: i32,
    /// Pixel bytes, `width * height * 4`.
    pub data: Vec<u8>,
}

/// Lays out `text` horizontally at `px` pixels and rasterizes it in `rgba` (straight alpha in,
/// premultiplied `Argb8888` out). Empty text yields a 0×0 raster.
pub fn rasterize(font: &fontdue::Font, text: &str, px: f32, rgba: [u8; 4]) -> RasterText {
    use fontdue::layout::{CoordinateSystem, Layout, LayoutSettings, TextStyle};
    let mut layout = Layout::new(CoordinateSystem::PositiveYDown);
    layout.reset(&LayoutSettings::default());
    layout.append(&[font], &TextStyle::new(text, px, 0));

    let glyphs = layout.glyphs();
    let width = glyphs
        .iter()
        .map(|g| (g.x + g.width as f32).ceil() as i32)
        .max()
        .unwrap_or(0)
        .max(0);
    let height = if glyphs.is_empty() {
        0
    } else {
        layout.height().ceil() as i32
    };
    let mut cov = vec![0u8; (width * height) as usize];
    for g in glyphs {
        if g.width == 0 || g.height == 0 {
            continue;
        }
        let (_, bitmap) = font.rasterize_config(g.key);
        let (gx, gy) = (g.x.round() as i32, g.y.round() as i32);
        for row in 0..g.height as i32 {
            for col in 0..g.width as i32 {
                let (x, y) = (gx + col, gy + row);
                if x < 0 || y < 0 || x >= width || y >= height {
                    continue;
                }
                let c = bitmap[(row * g.width as i32 + col) as usize];
                let dst = &mut cov[(y * width + x) as usize];
                *dst = (*dst).max(c);
            }
        }
    }

    let [r, g, b, a] = rgba;
    let mul = |v: u8, f: u32| ((v as u32 * f + 127) / 255) as u8;
    let mut data = Vec::with_capacity(cov.len() * 4);
    for c in cov {
        let alpha = (c as u32 * a as u32 + 127) / 255;
        data.extend_from_slice(&[mul(b, alpha), mul(g, alpha), mul(r, alpha), alpha as u8]);
    }
    RasterText {
        width,
        height,
        data,
    }
}

/// Cache key: `(text, px size, rgba colour, output scale)`.
type CacheKey = (String, u32, [u8; 4], i32);

/// Loaded font plus a bounded buffer cache. `font == None` means text is disabled.
pub struct TextRenderer {
    font: Option<fontdue::Font>,
    cache: HashMap<CacheKey, MemoryRenderBuffer>,
    /// Number of rasterizations performed (cache misses); exposed for tests/diagnostics.
    rasterized: usize,
}

impl std::fmt::Debug for TextRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextRenderer")
            .field("enabled", &self.font.is_some())
            .field("cached", &self.cache.len())
            .finish()
    }
}

impl TextRenderer {
    /// Resolves and loads the font (see [`resolve_font_path`]) using the real `fc-match` CLI.
    /// On failure, logs one `warn!` and returns a disabled renderer.
    pub fn load(override_path: Option<&Path>, family: &str) -> Self {
        let path = resolve_font_path(override_path, family, system_fc_match, |p| p.is_file());
        let font = match path {
            None => {
                tracing::warn!(family, "no font file found; compositor text disabled");
                None
            }
            Some(p) => match std::fs::read(&p)
                .map_err(|e| e.to_string())
                .and_then(|bytes| {
                    fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default())
                        .map_err(|e| e.to_string())
                }) {
                Ok(f) => Some(f),
                Err(e) => {
                    tracing::warn!(path = %p.display(), error = %e, "font unreadable; compositor text disabled");
                    None
                }
            },
        };
        Self::from_font(font)
    }

    /// Wraps an already-loaded font (`None` = disabled).
    pub fn from_font(font: Option<fontdue::Font>) -> Self {
        Self {
            font,
            cache: HashMap::new(),
            rasterized: 0,
        }
    }

    /// Whether a font is loaded.
    pub fn enabled(&self) -> bool {
        self.font.is_some()
    }

    /// Whether the loaded font carries a glyph for `c` (`false` when text is disabled).
    pub fn has_glyph(&self, c: char) -> bool {
        self.font
            .as_ref()
            .is_some_and(|f| f.lookup_glyph_index(c) != 0)
    }

    /// Width in pixels `text` lays out to at `px` (no rasterization); `None` when text is
    /// disabled. Used by the header to size segments and hit-test clicks.
    pub fn measure(&self, text: &str, px: u32) -> Option<i32> {
        use fontdue::layout::{CoordinateSystem, Layout, LayoutSettings, TextStyle};
        let font = self.font.as_ref()?;
        let mut layout = Layout::new(CoordinateSystem::PositiveYDown);
        layout.reset(&LayoutSettings::default());
        layout.append(&[font], &TextStyle::new(text, px as f32, 0));
        Some(
            layout
                .glyphs()
                .iter()
                .map(|g| (g.x + g.width as f32).ceil() as i32)
                .max()
                .unwrap_or(0)
                .max(0),
        )
    }

    /// Number of rasterizations (cache misses) so far.
    pub fn rasterized_count(&self) -> usize {
        self.rasterized
    }

    /// Drops every cached buffer.
    pub fn clear_cache(&mut self) {
        self.cache.clear();
    }

    /// Returns the (cached) buffer for `text` at logical `px` size in `rgba`, rasterized at
    /// `px * scale` physical pixels with buffer scale `scale`. `None` if text is disabled or the
    /// string renders to nothing.
    pub fn buffer(
        &mut self,
        text: &str,
        px: u32,
        rgba: [u8; 4],
        scale: i32,
    ) -> Option<MemoryRenderBuffer> {
        let font = self.font.as_ref()?;
        let scale = scale.max(1);
        let key = (text.to_owned(), px, rgba, scale);
        if let Some(buf) = self.cache.get(&key) {
            return Some(buf.clone());
        }
        let raster = rasterize(font, text, (px as i32 * scale) as f32, rgba);
        self.rasterized += 1;
        if raster.width == 0 || raster.height == 0 {
            return None;
        }
        let buf = MemoryRenderBuffer::from_slice(
            &raster.data,
            Fourcc::Argb8888,
            (raster.width, raster.height),
            scale,
            Transform::Normal,
            None,
        );
        if self.cache.len() >= CACHE_CAP {
            self.cache.clear();
        }
        self.cache.insert(key, buf.clone());
        Some(buf)
    }

    /// Builds a render element for `text` with its top-left at physical `location`.
    pub fn element<R>(
        &mut self,
        renderer: &mut R,
        location: Point<f64, Physical>,
        text: &str,
        px: u32,
        rgba: [u8; 4],
        scale: i32,
    ) -> Option<MemoryRenderBufferRenderElement<R>>
    where
        R: Renderer + ImportMem,
        R::TextureId: Send + Clone + 'static,
    {
        let buf = self.buffer(text, px, rgba, scale)?;
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location,
            &buf,
            None,
            None,
            None,
            Kind::Unspecified,
        )
        .map_err(|e| tracing::debug!(?e, "text texture import failed"))
        .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dejavu() -> Option<fontdue::Font> {
        match std::fs::read(FALLBACK_FONT) {
            Ok(b) => Some(fontdue::Font::from_bytes(b, fontdue::FontSettings::default()).unwrap()),
            Err(_) => {
                eprintln!("SKIP: {FALLBACK_FONT} absent on this host");
                None
            }
        }
    }

    #[test]
    fn raster_dimensions_for_known_string() {
        let Some(font) = dejavu() else { return };
        let r = rasterize(&font, "Hello", 16.0, [255, 255, 255, 255]);
        assert!(r.width > 20 && r.width < 60, "width {}", r.width);
        assert!(r.height >= 16 && r.height < 30, "height {}", r.height);
        assert_eq!(r.data.len(), (r.width * r.height * 4) as usize);
        assert_eq!(rasterize(&font, "", 16.0, [255; 4]).width, 0);
    }

    #[test]
    fn known_glyph_has_premultiplied_coverage() {
        let Some(font) = dejavu() else { return };
        let r = rasterize(&font, "M", 24.0, [255, 0, 0, 255]);
        let px: Vec<&[u8]> = r.data.chunks(4).collect();
        assert!(px.iter().any(|p| p[3] == 255), "no fully covered pixel");
        // Premultiplied red in [B, G, R, A]: R == A, B == G == 0.
        assert!(px.iter().all(|p| p[0] == 0 && p[1] == 0 && p[2] == p[3]));
    }

    #[test]
    fn cache_hit_skips_rasterization() {
        let Some(font) = dejavu() else { return };
        let mut t = TextRenderer::from_font(Some(font));
        let a = t.buffer("ws-1", 14, [255; 4], 1).unwrap();
        let b = t.buffer("ws-1", 14, [255; 4], 1).unwrap();
        assert_eq!(t.rasterized_count(), 1);
        // Same underlying buffer (shared id + contents); `MemoryRenderBuffer` has no `PartialEq`.
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
        t.buffer("ws-1", 14, [255; 4], 2).unwrap();
        assert_eq!(t.rasterized_count(), 2);
        t.clear_cache();
        t.buffer("ws-1", 14, [255; 4], 1).unwrap();
        assert_eq!(t.rasterized_count(), 3);
    }

    #[test]
    fn disabled_renderer_returns_none() {
        let mut t = TextRenderer::from_font(None);
        assert!(!t.enabled());
        assert!(t.buffer("x", 14, [255; 4], 1).is_none());
    }

    #[test]
    fn resolver_fallback_order() {
        let ov = Path::new("/o/override.ttf");
        let fc = || |_: &str| Some("/fc/match.ttf\n".to_string());
        let all = |_: &Path| true;
        assert_eq!(
            resolve_font_path(Some(ov), "Sans", fc(), all),
            Some(ov.into())
        );
        // Override missing → fc-match.
        let not_ov = |p: &Path| p != ov;
        assert_eq!(
            resolve_font_path(Some(ov), "Sans", fc(), not_ov),
            Some("/fc/match.ttf".into())
        );
        // fc-match absent → fallback.
        assert_eq!(
            resolve_font_path(None, "Sans", |_| None, all),
            Some(FALLBACK_FONT.into())
        );
        // Nothing exists → None.
        assert_eq!(resolve_font_path(Some(ov), "Sans", fc(), |_| false), None);
    }
}
