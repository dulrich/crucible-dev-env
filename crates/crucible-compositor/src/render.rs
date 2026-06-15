//! Per-frame rendering: scene assembly and output presentation.
//!
//! Assembles the list of render elements for an output each frame and drives damage-tracked
//! rendering through the GL ES renderer. Backend-agnostic: it consumes the mapped windows from the
//! desktop [`Space`](smithay::desktop::Space) plus the compositor-drawn cursor element from
//! [`crate::drawing`], composites them with an [`OutputDamageTracker`], and presents to the backend.
//!
//! ## WS-3 scope
//! WS-3 renders mapped Wayland surfaces and the cursor; focus-ring borders, stack indicators,
//! window previews, and the udev/DRM presentation-feedback path are deferred. The element-collection
//! helpers ([`output_elements`] / [`render_output`]) are written generically over the renderer so
//! the udev backend (WS-6) can reuse them unchanged.

use smithay::{
    backend::renderer::{
        Color32F, ImportAll, ImportMem, Renderer,
        damage::{Error as OutputDamageTrackerError, OutputDamageTracker, RenderOutputResult},
        element::{Wrap, surface::WaylandSurfaceRenderElement},
    },
    desktop::space::{Space, SpaceRenderElements},
    output::Output,
};

use crate::{drawing::PointerRenderElement, element::WindowElement};

smithay::backend::renderer::element::render_elements! {
    /// Compositor-drawn render elements that are not part of the desktop [`Space`] — currently just
    /// the cursor. (Focus borders and stack indicators join this enum in a later phase.)
    pub CustomRenderElements<R> where R: ImportAll + ImportMem;
    /// The pointer cursor element from [`crate::drawing`].
    Pointer=PointerRenderElement<R>,
    /// A bare Wayland surface (e.g. a drag-and-drop icon) drawn outside the space.
    Surface=WaylandSurfaceRenderElement<R>,
}

impl<R: Renderer + ImportAll + ImportMem> std::fmt::Debug for CustomRenderElements<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pointer(arg0) => f.debug_tuple("Pointer").field(arg0).finish(),
            Self::Surface(arg0) => f.debug_tuple("Surface").field(arg0).finish(),
            Self::_GenericCatcher(arg0) => f.debug_tuple("_GenericCatcher").field(arg0).finish(),
        }
    }
}

smithay::backend::renderer::element::render_elements! {
    /// Every render element composited onto an output: the windows from the [`Space`] plus the
    /// compositor [`CustomRenderElements`] (cursor, …).
    pub OutputRenderElements<R, E> where R: ImportAll + ImportMem;
    /// Render elements produced by the desktop [`Space`] (mapped windows).
    Space=SpaceRenderElements<R, E>,
    /// A single window element wrapped for the damage tracker.
    Window=Wrap<E>,
    /// Compositor-drawn chrome (the cursor today).
    Custom=CustomRenderElements<R>,
}

impl<
    R: Renderer + ImportAll + ImportMem,
    E: smithay::backend::renderer::element::RenderElement<R> + std::fmt::Debug,
> std::fmt::Debug for OutputRenderElements<R, E>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Space(arg0) => f.debug_tuple("Space").field(arg0).finish(),
            Self::Window(arg0) => f.debug_tuple("Window").field(arg0).finish(),
            Self::Custom(arg0) => f.debug_tuple("Custom").field(arg0).finish(),
            Self::_GenericCatcher(arg0) => f.debug_tuple("_GenericCatcher").field(arg0).finish(),
        }
    }
}

/// Window render element produced for a [`WindowElement`] by a renderer `R`.
type WindowRenderElement<R> =
    <WindowElement as smithay::backend::renderer::element::AsRenderElements<R>>::RenderElement;

/// Collects the render elements for `output`: the compositor `custom_elements` (the cursor) on top,
/// then the mapped windows from `space` below.
///
/// Returns the element list (front-to-back) and the clear color the damage tracker uses for any
/// uncovered region.
pub fn output_elements<R>(
    output: &Output,
    space: &Space<WindowElement>,
    custom_elements: impl IntoIterator<Item = CustomRenderElements<R>>,
    renderer: &mut R,
) -> (
    Vec<OutputRenderElements<R, WindowRenderElement<R>>>,
    Color32F,
)
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + 'static,
{
    // Custom (cursor) elements render in front of the windows.
    let mut elements: Vec<OutputRenderElements<R, WindowRenderElement<R>>> = custom_elements
        .into_iter()
        .map(OutputRenderElements::from)
        .collect();

    let space_elements = smithay::desktop::space::space_render_elements::<_, WindowElement, _>(
        renderer,
        [space],
        output,
        1.0,
    )
    .expect("output without mode?");
    elements.extend(space_elements.into_iter().map(OutputRenderElements::Space));

    (elements, crate::winit::CLEAR_COLOR.into())
}

/// Composites `output` for one frame: collects the elements via [`output_elements`] and drives the
/// `damage_tracker` to render them into `framebuffer`.
#[allow(clippy::too_many_arguments)]
pub fn render_output<'d, R>(
    output: &Output,
    space: &Space<WindowElement>,
    custom_elements: impl IntoIterator<Item = CustomRenderElements<R>>,
    renderer: &mut R,
    framebuffer: &mut R::Framebuffer<'_>,
    damage_tracker: &'d mut OutputDamageTracker,
    age: usize,
) -> Result<RenderOutputResult<'d>, OutputDamageTrackerError<R::Error>>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + 'static,
{
    let (elements, clear_color) = output_elements(output, space, custom_elements, renderer);
    damage_tracker.render_output(renderer, framebuffer, age, &elements, clear_color)
}
