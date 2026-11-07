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

use std::time::Duration;

use crucible_config::TilingConfig;
use crucible_layout::{Projection, Rect};
use smithay::{
    backend::renderer::{
        Color32F, ImportAll, ImportMem, Renderer,
        element::{
            AsRenderElements, Wrap, memory::MemoryRenderBufferRenderElement,
            solid::SolidColorRenderElement, surface::WaylandSurfaceRenderElement,
        },
    },
    desktop::space::{Space, SpaceRenderElements, SurfaceTree},
    input::pointer::CursorImageStatus,
    output::Output,
    utils::{IsAlive, Logical, Physical, Point, Rectangle, Scale},
};

use crate::{
    chrome::OverlayChrome,
    cursor::{Cursor, PointerRenderElement},
    element::WindowElement,
    header::HeaderLayout,
    layout::LayoutState,
    lock::SessionLock,
    state::{Backend, CrucibleState, DndIcon},
    text::TextRenderer,
};

/// Solid color the output is cleared to each frame (a dark slate), as `[r, g, b, a]`.
pub const CLEAR_COLOR: [f32; 4] = [0.05, 0.05, 0.08, 1.0];

smithay::backend::renderer::element::render_elements! {
    /// Compositor-drawn render elements that are not part of the desktop [`Space`]: the cursor,
    /// plus focus-ring borders and per-pane stack indicators.
    pub CustomRenderElements<R> where R: ImportAll + ImportMem;
    /// The pointer cursor element from [`crate::cursor`].
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

/// The owned per-output inputs of one frame, built by [`CrucibleState::begin_frame`] before the
/// backend takes its renderer borrow.
#[derive(Debug, Clone)]
pub struct FrameInputs {
    /// The output's fractional scale.
    pub scale: Scale<f64>,
    /// Where the cursor is drawn (output-local, physical); `None` when the pointer is not over
    /// this output.
    pub cursor: Option<Point<i32, Physical>>,
    /// The focused empty pane's tile, if any (it gets the terminal ring).
    pub empty_focus_rect: Option<Rect>,
    /// The mapped overlay's chrome settings, if an overlay is mapped.
    pub overlay: Option<OverlayChrome>,
    /// This output's header strip layout; `None` while the strip is hidden or for an output the
    /// model has not bound.
    pub header: Option<HeaderLayout>,
    /// The header font size.
    pub header_size: u32,
}

/// The read-only state one output's frame is composed from: disjoint borrows of
/// [`CrucibleState`] fields, taken by each backend beside its own renderer borrow (which holds the
/// backend data), plus the frame's [`FrameInputs`].
pub struct FrameView<'a> {
    /// The output being rendered.
    pub output: &'a Output,
    /// The mapped windows (the model's visible set).
    pub space: &'a Space<WindowElement>,
    /// The session lock (selects the lock branch and supplies the lock surface).
    pub session_lock: &'a SessionLock,
    /// The model, read for the chrome's stack indicators and window classes.
    pub layout: &'a LayoutState,
    /// The projection last applied to the `Space`.
    pub projection: &'a Projection,
    /// Per-class tiling settings (rings, gaps, fill).
    pub tiling: &'a TilingConfig,
    /// The active pointer drag's icon, drawn at the cursor (unlocked frames only).
    pub dnd_icon: Option<&'a DndIcon>,
    /// The active snip session: when set, an output it froze shows only the snip overlay over
    /// its frozen frame (after the lock, which always wins).
    pub snip: Option<&'a crate::snip::SnipSession>,
    /// The per-output inputs from [`CrucibleState::begin_frame`].
    pub inputs: &'a FrameInputs,
}

/// The output-local physical cursor location when `pointer` (global, logical) lies inside
/// `output_geo`; `None` otherwise. Both backends draw the cursor only on the output under it.
#[must_use]
pub fn cursor_on_output(
    output_geo: Rectangle<i32, Logical>,
    pointer: Point<f64, Logical>,
    scale: Scale<f64>,
) -> Option<Point<i32, Physical>> {
    output_geo.to_f64().contains(pointer).then(|| {
        (pointer - output_geo.loc.to_f64())
            .to_physical(scale)
            .to_i32_round()
    })
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Starts `output`'s frame: resets a dead client cursor surface to the default, then builds
    /// the frame's owned [`FrameInputs`] (only this output's header is laid out).
    pub(crate) fn begin_frame(&mut self, output: &Output) -> FrameInputs {
        if let CursorImageStatus::Surface(ref surface) = self.cursor.status
            && !surface.alive()
        {
            self.cursor.status = CursorImageStatus::default_named();
        }
        let scale = Scale::from(output.current_scale().fractional_scale());
        let output_geo = self.space.output_geometry(output);
        let pointer = self.pointer.current_location();
        let cursor = output_geo.and_then(|geo| cursor_on_output(geo, pointer, scale));
        let preview = self.header_drag_preview();
        let header = crate::layout::model_output_id(output)
            .zip(output_geo)
            .and_then(|(id, geo)| {
                self.header_layout_ordered(
                    id,
                    Rect::new(geo.loc.x, geo.loc.y, geo.size.w, geo.size.h),
                    preview.as_ref(),
                )
            });
        FrameInputs {
            scale,
            cursor,
            empty_focus_rect: self.focused_empty_pane_rect(),
            overlay: self.overlay_chrome(),
            header,
            header_size: self.config.header.size,
        }
    }

    /// Sends `output`'s post-render frame callbacks at `time` with `throttle`: the lock surface's
    /// while locked, every mapped window's, and the drag icon's.
    pub(crate) fn finish_frame(&self, output: &Output, time: Duration, throttle: Option<Duration>) {
        if self.session_lock.is_locked() {
            crate::lock::send_lock_frames(&self.session_lock, output, time, throttle);
        }
        self.space.elements().for_each(|window| {
            window.send_frame(output, time, throttle, |_, _| Some(output.clone()));
        });
        crate::state::send_dnd_icon_frames(self.dnd_icon.as_ref(), output, time, throttle);
    }
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
/// - **Snipping** (an output the snip froze): cursor → size label → ring → dim → the frozen
///   frame ([`crate::snip::snip_elements`]); no header, chrome or live windows.
/// - **Unlocked**: cursor (only when `inputs.cursor` is `Some`, at that output-local physical location)
///   → header strip → chrome → the mapped windows from the `space`, over
///   [`CLEAR_COLOR`].
///
/// Chrome and header are positioned in the output's frame: both take the output's global origin
/// from `space.output_geometry(output)`. Both backends draw the cursor only on the output under the
/// pointer ([`cursor_on_output`], applied by [`CrucibleState::begin_frame`]).
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
    cursor: &mut Cursor,
    renderer: &mut R,
) -> (FrameElements<R>, Color32F)
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Send + Clone + 'static,
{
    let inputs = view.inputs;
    let scale = inputs.scale;
    if view.session_lock.is_locked() {
        let elements =
            crate::lock::lock_render_elements(view.session_lock, view.output, renderer, scale)
                .into_iter()
                .map(OutputRenderElements::Custom)
                .collect();
        return (elements, crate::lock::LOCK_CLEAR_COLOR.into());
    }
    if let Some(snip) = view.snip
        && let Some(elements) = crate::snip::snip_elements(
            snip,
            view.output,
            inputs.cursor,
            header_text,
            cursor,
            renderer,
            scale,
        )
    {
        return (elements, CLEAR_COLOR.into());
    }

    let origin = view
        .space
        .output_geometry(view.output)
        .map_or((0, 0), |g| (g.loc.x, g.loc.y));

    // Front-to-back: cursor → drag icon → header → chrome → windows.
    let mut custom: Vec<CustomRenderElements<R>> = match inputs.cursor {
        Some(location) => cursor
            .render_elements(renderer, location, scale, 1.0)
            .into_iter()
            .map(CustomRenderElements::Pointer)
            .collect(),
        None => Vec::new(),
    };
    if let (Some(icon), Some(cursor)) = (view.dnd_icon, inputs.cursor)
        && icon.surface.alive()
    {
        let location = icon_location(cursor, icon.offset, scale);
        custom.extend(
            AsRenderElements::<R>::render_elements(
                &SurfaceTree::from_surface(&icon.surface),
                renderer,
                location,
                scale,
                1.0,
            )
            .into_iter()
            .map(CustomRenderElements::Surface),
        );
    }
    if let Some(header) = &inputs.header {
        custom.extend(crate::header::build_header_elements(
            header,
            origin,
            inputs.header_size,
            header_text,
            renderer,
            scale,
        ));
    }
    custom.extend(
        crate::chrome::build_chrome_elements(
            view.layout,
            view.projection,
            inputs.empty_focus_rect,
            inputs.overlay,
            origin,
            view.tiling,
            scale,
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

    #[test]
    fn cursor_is_drawn_only_on_the_output_under_the_pointer() {
        let geo = Rectangle::<i32, Logical>::new((1000, 0).into(), (800, 600).into());
        let s = Scale::from(1.5);
        assert_eq!(cursor_on_output(geo, (999.0, 10.0).into(), s), None);
        assert_eq!(cursor_on_output(geo, (1800.0, 10.0).into(), s), None);
        assert_eq!(
            cursor_on_output(geo, (1000.0, 0.0).into(), s),
            Some(Point::<i32, Physical>::from((0, 0)))
        );
        assert_eq!(
            cursor_on_output(geo, (1100.0, 20.0).into(), s),
            Some(Point::<i32, Physical>::from((150, 30)))
        );
    }
}
