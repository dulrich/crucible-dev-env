//! Per-frame rendering: scene assembly shared by both backends.
//!
//! [`frame_elements`] assembles the front-to-back element list (and clear colour) for one output
//! each frame: while the session is locked only the output's lock surface over
//! [`LOCK_CLEAR_COLOR`](crate::lock::LOCK_CLEAR_COLOR); otherwise the cursor, the header strip, the
//! chrome (focus-ring borders, fills, stack indicators) and the mapped windows from the desktop
//! [`Space`](smithay::desktop::Space). It is generic over the renderer so the winit and udev
//! backends share it unchanged; each backend keeps only its own present/feedback logic.
//!
//! ## Scope
//! Window previews are not rendered.

use crucible_config::TilingConfig;
use crucible_layout::{OutputId, Projection, Rect};
use smithay::{
    backend::renderer::{
        Color32F, ImportAll, ImportMem, Renderer,
        element::{
            AsRenderElements, Wrap, memory::MemoryRenderBufferRenderElement,
            solid::SolidColorRenderElement, surface::WaylandSurfaceRenderElement,
        },
    },
    desktop::space::{Space, SpaceRenderElements, SurfaceTree},
    output::Output,
    utils::{IsAlive, Logical, Physical, Point, Scale},
};

use crate::{
    chrome::OverlayChrome,
    drawing::{PointerElement, PointerRenderElement},
    element::WindowElement,
    header::HeaderLayout,
    layout::LayoutState,
    lock::SessionLock,
    state::DndIcon,
    text::TextRenderer,
};

/// Solid color the output is cleared to each frame (a dark slate), as `[r, g, b, a]`.
pub const CLEAR_COLOR: [f32; 4] = [0.05, 0.05, 0.08, 1.0];

smithay::backend::renderer::element::render_elements! {
    /// Compositor-drawn render elements that are not part of the desktop [`Space`]: the cursor,
    /// plus focus-ring borders and per-pane stack indicators.
    pub CustomRenderElements<R> where R: ImportAll + ImportMem;
    /// The pointer cursor element from [`crate::drawing`].
    Pointer=PointerRenderElement<R>,
    /// A bare Wayland surface (e.g. a drag-and-drop icon) drawn outside the space.
    Surface=WaylandSurfaceRenderElement<R>,
    /// Solid-color chrome: focus-ring border strips and per-pane stack-indicator pips.
    SolidColor=SolidColorRenderElement,
    /// Compositor-drawn text: a rasterized string in a memory buffer (see [`crate::text`]).
    Text=MemoryRenderBufferRenderElement<R>,
}

impl<R: Renderer + ImportAll + ImportMem> std::fmt::Debug for CustomRenderElements<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pointer(arg0) => f.debug_tuple("Pointer").field(arg0).finish(),
            Self::Surface(arg0) => f.debug_tuple("Surface").field(arg0).finish(),
            Self::SolidColor(arg0) => f.debug_tuple("SolidColor").field(arg0).finish(),
            Self::Text(arg0) => f.debug_tuple("Text").field(arg0).finish(),
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
pub type WindowRenderElement<R> = <WindowElement as AsRenderElements<R>>::RenderElement;

/// One frame's composited element list (front-to-back) for renderer `R`.
pub type FrameElements<R> = Vec<OutputRenderElements<R, WindowRenderElement<R>>>;

/// The read-only state one output's frame is composed from: disjoint borrows of
/// [`CrucibleState`](crate::state::CrucibleState) fields, taken by each backend beside its own
/// renderer borrow (which holds the backend data).
pub struct FrameView<'a> {
    /// The output being rendered.
    pub output: &'a Output,
    /// Its fractional scale.
    pub scale: Scale<f64>,
    /// The mapped windows (the model's visible set).
    pub space: &'a Space<WindowElement>,
    /// The session lock (selects the lock branch and supplies the lock surface).
    pub session_lock: &'a SessionLock,
    /// The model, read for the chrome's stack indicators and window classes.
    pub layout: &'a LayoutState,
    /// The projection last applied to the `Space`.
    pub projection: &'a Projection,
    /// The focused empty pane's tile, if any (it gets the terminal ring).
    pub empty_focus_rect: Option<Rect>,
    /// The mapped overlay's chrome settings, if an overlay is mapped.
    pub overlay: Option<OverlayChrome>,
    /// Per-class tiling settings (rings, gaps, fill).
    pub tiling: &'a TilingConfig,
    /// Every output's header strip layout.
    pub header_layouts: &'a [HeaderLayout],
    /// The model id of `output`; `None` (an output the model has not bound) draws no header.
    pub header_output: Option<OutputId>,
    /// The header font size.
    pub header_size: u32,
    /// The active pointer drag's icon, drawn at the cursor (unlocked frames only).
    pub dnd_icon: Option<&'a DndIcon>,
}

/// Where a drag icon's surface origin is drawn: the output-local physical `cursor` plus the icon's
/// logical hotspot `offset` scaled to physical pixels.
#[must_use]
pub fn icon_location(
    cursor: Point<i32, Physical>,
    offset: Point<i32, Logical>,
    scale: Scale<f64>,
) -> Point<i32, Physical> {
    cursor + offset.to_f64().to_physical(scale).to_i32_round()
}

/// Composes one output's frame: the element list (front-to-back) and the clear colour.
///
/// - **Locked** (or locking): only `output`'s lock surface, over
///   [`LOCK_CLEAR_COLOR`](crate::lock::LOCK_CLEAR_COLOR). Chrome, header, cursor and windows are
///   not built.
/// - **Unlocked**: cursor (only when `cursor` is `Some`, at that output-local physical location)
///   → header strip → chrome → the mapped windows from the `space`, over
///   [`CLEAR_COLOR`].
///
/// Chrome and header are positioned in the output's frame: both take the output's global origin
/// from `space.output_geometry(output)`. Whether and where the cursor is drawn is the backend's
/// rule (udev: only on the output containing the pointer; winit: always), so it is passed in.
///
/// ## Visibility (decision #4)
/// The `space` is already the model's **visible** set: [`CrucibleState::apply_layout`]
/// (`crate::layout`) maps only the active workspace's focused-per-pane windows into the `Space` and
/// `unmap_elem`s everything else (inactive workspaces, stacked-but-not-focused windows). So
/// collecting from `space` here renders exactly the visible windows — no extra per-element filtering
/// is needed, and we deliberately avoid a separate `Space` per workspace.
///
/// [`CrucibleState::apply_layout`]: crate::state::CrucibleState
pub fn frame_elements<R>(
    view: FrameView<'_>,
    header_text: &mut TextRenderer,
    pointer: &PointerElement,
    cursor: Option<Point<i32, Physical>>,
    renderer: &mut R,
) -> (FrameElements<R>, Color32F)
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Send + Clone + 'static,
{
    if view.session_lock.is_locked() {
        let elements =
            crate::lock::lock_render_elements(view.session_lock, view.output, renderer, view.scale)
                .into_iter()
                .map(OutputRenderElements::Custom)
                .collect();
        return (elements, crate::lock::LOCK_CLEAR_COLOR.into());
    }

    let origin = view
        .space
        .output_geometry(view.output)
        .map_or((0, 0), |g| (g.loc.x, g.loc.y));

    // Front-to-back: cursor → drag icon → header → chrome → windows.
    let mut custom: Vec<CustomRenderElements<R>> = match cursor {
        Some(location) => pointer.render_elements(renderer, location, view.scale, 1.0),
        None => Vec::new(),
    };
    if let (Some(icon), Some(cursor)) = (view.dnd_icon, cursor)
        && icon.surface.alive()
    {
        let location = icon_location(cursor, icon.offset, view.scale);
        custom.extend(
            AsRenderElements::<R>::render_elements(
                &SurfaceTree::from_surface(&icon.surface),
                renderer,
                location,
                view.scale,
                1.0,
            )
            .into_iter()
            .map(CustomRenderElements::Surface),
        );
    }
    if let Some(output_id) = view.header_output {
        custom.extend(crate::header::build_header_elements(
            view.header_layouts,
            output_id,
            origin,
            view.header_size,
            header_text,
            renderer,
            view.scale,
        ));
    }
    custom.extend(
        crate::chrome::build_chrome_elements(
            view.layout,
            view.projection,
            view.empty_focus_rect,
            view.overlay,
            origin,
            view.tiling,
            view.scale,
        )
        .into_iter()
        .map(CustomRenderElements::SolidColor),
    );

    let mut elements: FrameElements<R> =
        custom.into_iter().map(OutputRenderElements::from).collect();
    let space_elements = smithay::desktop::space::space_render_elements::<_, WindowElement, _>(
        renderer,
        [view.space],
        view.output,
        1.0,
    )
    .expect("output without mode?");
    elements.extend(space_elements.into_iter().map(OutputRenderElements::Space));

    (elements, CLEAR_COLOR.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_sits_at_cursor_plus_offset() {
        let at = icon_location((100, 50).into(), (0, 0).into(), Scale::from(1.0));
        assert_eq!(at, Point::<i32, Physical>::from((100, 50)));
        let at = icon_location((100, 50).into(), (-8, 4).into(), Scale::from(1.0));
        assert_eq!(at, Point::<i32, Physical>::from((92, 54)));
    }

    #[test]
    fn icon_offset_scales_to_physical_pixels() {
        let at = icon_location((100, 50).into(), (-8, 4).into(), Scale::from(1.5));
        assert_eq!(at, Point::<i32, Physical>::from((88, 56)));
    }
}
