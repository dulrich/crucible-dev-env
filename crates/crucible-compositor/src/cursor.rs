//! The compositor-drawn pointer cursor.
//!
//! Focus-ring borders and stack indicators live in [`crate::chrome`]; this module owns only the
//! cursor.
//!
//! ## The pointer cursor
//! [`Cursor`] is the single owner of the compositor's cursor: the current [`CursorImageStatus`]
//! (written by the seat's `cursor_image` callback) and the cached default-cursor buffers. It draws
//! either a client-provided cursor surface (when an application sets one via `wl_pointer.set_cursor`)
//! or CDE's built-in [default cursor](default_cursor_bitmap) bitmap for the common "named/default"
//! case. anvil loads an XCursor theme here; CDE deliberately keeps a tiny self-contained bitmap so
//! the compositor has no external cursor-asset dependency. A themed cursor can replace
//! [`default_cursor_bitmap`] later without touching the render path.
//!
//! The default cursor is rasterized at the output's physical resolution (one bitmap per quantized
//! scale) and drawn texel-for-texel, so it stays crisp at fractional scales.

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

use crate::physical::{padded_dims, quantize_scale};

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

/// Side length, in logical pixels, of the built-in square default-cursor bitmap.
pub const DEFAULT_CURSOR_SIZE: i32 = 16;

/// Border colour of the default cursor, `[B, G, R, A]`.
const BORDER_PIXEL: [u8; 4] = [0x20, 0x20, 0x20, 0xff];
/// Interior colour of the default cursor, `[B, G, R, A]`.
const INTERIOR_PIXEL: [u8; 4] = [0xf0, 0xf0, 0xf0, 0xff];

/// The default cursor's physical square side `n = round(16·s)` (at least 1) and border width
/// `b = max(1, round(s))` at scale `s`.
#[must_use]
fn default_cursor_side(s: f64) -> (i32, i32) {
    let n = ((f64::from(DEFAULT_CURSOR_SIZE) * s).round() as i32).max(1);
    let b = (s.round() as i32).max(1);
    (n, b)
}

/// Builds CDE's built-in default cursor at scale `s` as a packed `Argb8888` bitmap.
///
/// Produces a small opaque white square with a dark border — enough to clearly show the pointer
/// location without shipping an XCursor theme. The square is `n = round(16·s)` physical pixels a
/// side with a `max(1, round(s))`-pixel border, padded with transparent pixels right/bottom to
/// [`padded_dims`]'s physical size so it displays 1:1. Returns `(bytes, width, height)`; the bytes
/// are 4 per pixel in the byte order expected by
/// [`Fourcc::Argb8888`](smithay::backend::allocator::Fourcc::Argb8888) on little-endian hosts
/// (`[B, G, R, A]`). At `s = 1` this is the 16×16 square with a one-pixel border.
#[must_use]
pub fn default_cursor_bitmap(s: f64) -> (Vec<u8>, i32, i32) {
    let (n, b) = default_cursor_side(s);
    let (phys, _) = padded_dims(n, n, s, s);
    let (w, h) = (phys.w, phys.h);
    let mut data = vec![0u8; (w * h * 4) as usize];
    for y in 0..n {
        for x in 0..n {
            let idx = ((y * w + x) * 4) as usize;
            let border = x < b || y < b || x >= n - b || y >= n - b;
            let px = if border { BORDER_PIXEL } else { INTERIOR_PIXEL };
            data[idx..idx + 4].copy_from_slice(&px);
        }
    }
    (data, w, h)
}

/// One scale's uploaded default-cursor bitmap and its 1:1 display sizes.
#[derive(Debug)]
struct DefaultCursorBuffer {
    /// The bitmap (buffer scale 1).
    buffer: MemoryRenderBuffer,
    /// Its physical size (the padded bitmap).
    phys: Size<i32, Physical>,
    /// The logical size it is displayed at, so `round(logical · s) == phys`.
    logical: Size<i32, Logical>,
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
    /// Default-cursor bitmaps, keyed by [`quantize_scale`] of the output's scale.
    default_buffers: HashMap<u32, DefaultCursorBuffer>,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            status: CursorImageStatus::default_named(),
            default_buffers: HashMap::new(),
        }
    }
}

impl Cursor {
    /// The default-cursor buffer for scale `s`, rasterized on first use per quantized scale.
    fn default_buffer(&mut self, s: f64) -> &DefaultCursorBuffer {
        self.default_buffers
            .entry(quantize_scale(s))
            .or_insert_with(|| {
                let (bytes, w, h) = default_cursor_bitmap(s);
                let n = default_cursor_side(s).0;
                let (phys, logical) = padded_dims(n, n, s, s);
                debug_assert_eq!((phys.w, phys.h), (w, h));
                DefaultCursorBuffer {
                    buffer: MemoryRenderBuffer::from_slice(
                        &bytes,
                        smithay::backend::allocator::Fourcc::Argb8888,
                        (w, h),
                        1,
                        smithay::utils::Transform::Normal,
                        None,
                    ),
                    phys,
                    logical,
                }
            })
    }

    /// Renders the cursor with its top-left (or, for a client surface, its hotspot) at the
    /// output-local physical `location`.
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
            // A named shape (the common "default arrow" case): draw the built-in bitmap 1:1.
            CursorImageStatus::Named(_) => {
                let buf = self.default_buffer(scale.x);
                match MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    location.to_f64(),
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

    /// The pre-WP-R6 16×16 bitmap, kept as the `s = 1` oracle.
    fn old_build_default_cursor() -> Vec<u8> {
        let n = DEFAULT_CURSOR_SIZE;
        let mut data = vec![0u8; (n * n * 4) as usize];
        for y in 0..n {
            for x in 0..n {
                let idx = ((y * n + x) * 4) as usize;
                let border = x == 0 || y == 0 || x == n - 1 || y == n - 1;
                let (b, g, r, a) = if border {
                    (0x20, 0x20, 0x20, 0xff)
                } else {
                    (0xf0, 0xf0, 0xf0, 0xff)
                };
                data[idx] = b;
                data[idx + 1] = g;
                data[idx + 2] = r;
                data[idx + 3] = a;
            }
        }
        data
    }

    fn pixel(data: &[u8], w: i32, x: i32, y: i32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        data[i..i + 4].try_into().unwrap()
    }

    /// Checks the square of side `n` with border `b` and the transparent padding, plus the 1:1 rule.
    fn check(s: f64, n: i32, b: i32) {
        assert_eq!(default_cursor_side(s), (n, b));
        let (data, w, h) = default_cursor_bitmap(s);
        let (phys, logical) = padded_dims(n, n, s, s);
        assert_eq!((w, h), (phys.w, phys.h));
        assert!(w >= n && h >= n);
        assert_eq!((f64::from(logical.w) * s).round() as i32, w);
        assert_eq!((f64::from(logical.h) * s).round() as i32, h);
        assert_eq!(data.len(), (w * h * 4) as usize);
        assert_eq!(pixel(&data, w, 0, 0), BORDER_PIXEL);
        assert_eq!(pixel(&data, w, b - 1, n / 2), BORDER_PIXEL);
        assert_eq!(pixel(&data, w, n - b, n / 2), BORDER_PIXEL);
        assert_eq!(pixel(&data, w, n / 2, n - 1), BORDER_PIXEL);
        assert_eq!(pixel(&data, w, b, b), INTERIOR_PIXEL);
        assert_eq!(pixel(&data, w, n - b - 1, n - b - 1), INTERIOR_PIXEL);
        for x in n..w {
            assert_eq!(pixel(&data, w, x, 0), [0, 0, 0, 0]);
        }
        for y in n..h {
            assert_eq!(pixel(&data, w, 0, y), [0, 0, 0, 0]);
        }
    }

    #[test]
    fn scale_one_matches_the_old_bitmap() {
        let (data, w, h) = default_cursor_bitmap(1.0);
        assert_eq!((w, h), (16, 16));
        assert_eq!(data, old_build_default_cursor());
        check(1.0, 16, 1);
    }

    #[test]
    fn fractional_scale_rasterizes_at_physical_size() {
        let s = 170.0 / 120.0;
        check(s, 23, 1);
        let (_, w, h) = default_cursor_bitmap(s);
        assert_eq!((w, h), (24, 24));
    }

    #[test]
    fn scale_two_doubles_side_and_border() {
        check(2.0, 32, 2);
        let (_, w, h) = default_cursor_bitmap(2.0);
        assert_eq!((w, h), (32, 32));
    }

    #[test]
    fn buffers_are_cached_per_quantized_scale() {
        let mut cursor = Cursor::default();
        let _ = cursor.default_buffer(1.5);
        let _ = cursor.default_buffer(1.5 + 1e-6);
        assert_eq!(cursor.default_buffers.len(), 1);
        let _ = cursor.default_buffer(2.0);
        assert_eq!(cursor.default_buffers.len(), 2);
        assert_eq!(cursor.default_buffer(2.0).logical, Size::from((16, 16)));
    }
}
