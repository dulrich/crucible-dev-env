//! Compositor-drawn chrome: cursor, focus borders, and stack indicators.
//!
//! Future home of the compositor's own drawing primitives — focus-ring borders around the active
//! pane/window and the stack indicators that show how many windows a pane (or workspace) holds.
//!
//! ## WS-3 — the pointer cursor
//! WS-3 brings up the [`PointerElement`]: the render element for the compositor's cursor. It draws
//! either a client-provided cursor surface (when an application sets one via `wl_pointer.set_cursor`)
//! or CDE's built-in [`default cursor`](DEFAULT_CURSOR) bitmap for the common "named/default" case.
//! anvil loads an XCursor theme here; CDE deliberately keeps a tiny self-contained bitmap so WS-3
//! has no external cursor-asset dependency. A themed cursor can replace [`build_default_cursor`]
//! later without touching the render path.

use smithay::{
    backend::renderer::{
        ImportAll, ImportMem, Renderer,
        element::{
            AsRenderElements, Kind,
            memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            surface::WaylandSurfaceRenderElement,
        },
    },
    input::pointer::CursorImageStatus,
    render_elements,
    utils::{Physical, Point, Scale},
};

/// Side length, in pixels, of the built-in square default-cursor bitmap.
pub const DEFAULT_CURSOR_SIZE: i32 = 16;

/// Builds CDE's built-in default cursor as a packed `Argb8888` bitmap.
///
/// Produces a small opaque white square with a one-pixel dark border — enough to clearly show the
/// pointer location in the nested `winit` backend without shipping an XCursor theme. The returned
/// bytes are `DEFAULT_CURSOR_SIZE` × `DEFAULT_CURSOR_SIZE` pixels, 4 bytes per pixel, in the byte
/// order expected by [`Fourcc::Argb8888`](smithay::backend::allocator::Fourcc::Argb8888) on
/// little-endian hosts (`[B, G, R, A]`).
fn build_default_cursor() -> Vec<u8> {
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

/// The compositor's cursor element.
///
/// Tracks the current [`CursorImageStatus`] (hidden, a named/default shape, or a client-provided
/// surface) and renders the appropriate element each frame. Built once in the render path and
/// updated from the seat's `cursor_image` callback (stored on [`crate::state::CrucibleState`]).
#[derive(Debug)]
pub struct PointerElement {
    /// Pre-uploaded bitmap for the built-in default cursor.
    default_buffer: MemoryRenderBuffer,
    /// The cursor state most recently requested by the focused client (or the default).
    status: CursorImageStatus,
}

impl Default for PointerElement {
    fn default() -> Self {
        let default_buffer = MemoryRenderBuffer::from_slice(
            &build_default_cursor(),
            smithay::backend::allocator::Fourcc::Argb8888,
            (DEFAULT_CURSOR_SIZE, DEFAULT_CURSOR_SIZE),
            1,
            smithay::utils::Transform::Normal,
            None,
        );
        PointerElement {
            default_buffer,
            status: CursorImageStatus::default_named(),
        }
    }
}

impl PointerElement {
    /// Updates the cursor state (called when a client sets, or clears, its cursor).
    pub fn set_status(&mut self, status: CursorImageStatus) {
        self.status = status;
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

impl<T, R> AsRenderElements<R> for PointerElement
where
    T: smithay::backend::renderer::Texture + Clone + Send + 'static,
    R: Renderer<TextureId = T> + ImportAll + ImportMem,
{
    type RenderElement = PointerRenderElement<R>;

    fn render_elements<E>(
        &self,
        renderer: &mut R,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Vec<E>
    where
        E: From<PointerRenderElement<R>>,
    {
        match &self.status {
            // Nothing to draw when the client explicitly hides the cursor.
            CursorImageStatus::Hidden => vec![],
            // A named shape (the common "default arrow" case): draw the built-in bitmap.
            CursorImageStatus::Named(_) => {
                match MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    location.to_f64(),
                    &self.default_buffer,
                    Some(alpha),
                    None,
                    None,
                    Kind::Cursor,
                ) {
                    Ok(element) => vec![PointerRenderElement::<R>::from(element).into()],
                    Err(_) => vec![],
                }
            }
            // A client-provided cursor surface: draw its surface tree.
            CursorImageStatus::Surface(surface) => {
                let elements: Vec<PointerRenderElement<R>> =
                    smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                        renderer,
                        surface,
                        location,
                        scale,
                        alpha,
                        Kind::Cursor,
                    );
                elements.into_iter().map(E::from).collect()
            }
        }
    }
}
