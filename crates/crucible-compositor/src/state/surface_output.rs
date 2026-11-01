//! Per-output surface bookkeeping: fractional scale, drag-icon frame callbacks and the three
//! per-frame surface walks (primary scan-out output, dmabuf feedback, presentation feedback).

use smithay::{
    backend::renderer::element::{
        RenderElementStates, default_primary_scanout_output_compare, utils::select_dmabuf_feedback,
    },
    desktop::{
        LayerSurface, Space,
        utils::{
            OutputPresentationFeedback, surface_presentation_feedback_flags_from_states,
            surface_primary_scanout_output, update_surface_primary_scanout_output,
            with_surfaces_surface_tree,
        },
    },
    input::pointer::CursorImageStatus,
    output::Output,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    wayland::{
        compositor::{get_parent, with_states},
        dmabuf::DmabufFeedback,
        fractional_scale::{FractionalScaleHandler, with_fractional_scale},
    },
};

use super::{Backend, CrucibleState, DndIcon};
use crate::element::WindowElement;

/// Sets the preferred fractional scale of every surface in the tree rooted at `root` to `output`'s.
///
/// `set_preferred_scale` only sends an event when the value changes, so this is cheap per frame.
fn set_tree_preferred_scale(root: &WlSurface, output: &Output) {
    let scale = output.current_scale().fractional_scale();
    with_surfaces_surface_tree(root, |_, states| {
        with_fractional_scale(states, |fs| fs.set_preferred_scale(scale));
    });
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// The Smithay output backing the model's focused output, if bound.
    pub(crate) fn focused_smithay_output(&self) -> Option<Output> {
        let focused = self.layout.world.focused_output();
        self.space
            .outputs()
            .find(|o| crate::layout::model_output_id(o) == Some(focused))
            .cloned()
    }

    /// The output a root surface lives on by role: a mapped window's first output, a layer
    /// surface's output, or a lock surface's output.
    fn output_for_root(&self, root: &WlSurface) -> Option<Output> {
        if let Some(window) = self.window_for_surface(root) {
            if let Some(output) = self.space.outputs_for_element(&window).first() {
                return Some(output.clone());
            }
        }
        self.space
            .outputs()
            .find(|output| {
                smithay::desktop::layer_map_for_output(output)
                    .layer_for_surface(root, smithay::desktop::WindowSurfaceType::ALL)
                    .is_some()
                    || self
                        .session_lock
                        .surface_for(output)
                        .is_some_and(|lock| lock.wl_surface() == root)
            })
            .cloned()
    }

    /// Per-frame fractional-scale update for everything on `output` (E1): each window and layer
    /// surface tracks its primary scan-out output, the lock surface takes the output's scale, and a
    /// client cursor surface takes the scale of the output under the pointer.
    ///
    /// Shared by both backends; call after a frame for `output` was rendered. A scale change
    /// reaches clients on the next frame through this walk.
    pub fn update_fractional_scales(&self, output: &Output) {
        let set_primary =
            |surface: &WlSurface, states: &smithay::wayland::compositor::SurfaceData| {
                if let Some(primary) = surface_primary_scanout_output(surface, states) {
                    let scale = primary.current_scale().fractional_scale();
                    with_fractional_scale(states, |fs| fs.set_preferred_scale(scale));
                }
            };
        self.space.elements().for_each(|window| {
            if self.space.outputs_for_element(window).contains(output) {
                window.0.with_surfaces(set_primary);
            }
        });
        for layer_surface in smithay::desktop::layer_map_for_output(output).layers() {
            layer_surface.with_surfaces(set_primary);
        }
        if let Some(lock) = self.session_lock.surface_for(output) {
            set_tree_preferred_scale(lock.wl_surface(), output);
        }
        if let CursorImageStatus::Surface(ref surface) = self.cursor_status {
            let location = self.pointer.current_location();
            if let Some(under) = self.space.output_under(location).next() {
                set_tree_preferred_scale(surface, under);
            }
        }
    }
}

impl<B: Backend + 'static> FractionalScaleHandler for CrucibleState<B> {
    /// Initial preferred scale (E1): the surface's (or its root's) primary scan-out output, else
    /// the output its root's role places it on, else the model's focused output.
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }
        let scanout =
            |s: &WlSurface| with_states(s, |states| surface_primary_scanout_output(s, states));
        let output = scanout(&surface)
            .or_else(|| (root != surface).then(|| scanout(&root)).flatten())
            .or_else(|| self.output_for_root(&root))
            .or_else(|| self.focused_smithay_output());
        if let Some(output) = output {
            let scale = output.current_scale().fractional_scale();
            with_states(&surface, |states| {
                with_fractional_scale(states, |fs| fs.set_preferred_scale(scale));
            });
        }
    }
}

/// Sends frame callbacks to the drag icon's surface tree, when a drag is active.
///
/// The icon is not in the `Space`, so the backends' per-window frame-callback loops miss it.
pub fn send_dnd_icon_frames(
    dnd_icon: Option<&DndIcon>,
    output: &Output,
    time: impl Into<std::time::Duration>,
    throttle: Option<std::time::Duration>,
) {
    if let Some(icon) = dnd_icon {
        smithay::desktop::utils::send_frames_surface_tree(
            &icon.surface,
            output,
            time,
            throttle,
            |_, _| Some(output.clone()),
        );
    }
}

/// Which mapped windows a surface walk visits (see [`for_each_surface_on`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowFilter {
    /// Every element in the `Space`, whichever output it is on.
    All,
    /// Only windows whose `Space` extent overlaps the output.
    OverlappingOutput,
}

/// Whether a window passes `filter`; `overlaps` is only evaluated for
/// [`WindowFilter::OverlappingOutput`].
fn include_window(filter: WindowFilter, overlaps: impl FnOnce() -> bool) -> bool {
    match filter {
        WindowFilter::All => true,
        WindowFilter::OverlappingOutput => overlaps(),
    }
}

/// The owner of a surface tree visited by [`for_each_surface_on`].
enum SurfaceOwner<'a> {
    /// A mapped window (Wayland or X11).
    Window(&'a WindowElement),
    /// One of the output's layer surfaces.
    Layer(&'a LayerSurface),
}

/// Visits the windows passing `filter` (in `Space` order), then `output`'s layer surfaces.
///
/// The one element-selection shared by the scan-out, dmabuf-feedback and presentation-feedback
/// walks; the per-surface operation is the callback's. The drag icon is not part of this set.
fn for_each_surface_on(
    space: &Space<WindowElement>,
    output: &Output,
    filter: WindowFilter,
    mut f: impl FnMut(SurfaceOwner<'_>),
) {
    space.elements().for_each(|window| {
        if include_window(filter, || {
            space.outputs_for_element(window).contains(output)
        }) {
            f(SurfaceOwner::Window(window));
        }
    });
    let map = smithay::desktop::layer_map_for_output(output);
    for layer_surface in map.layers() {
        f(SurfaceOwner::Layer(layer_surface));
    }
}

/// Records, for every surface on `output`, which output actually scanned it out this frame.
///
/// Smithay needs this bookkeeping to deliver correct `wl_surface.frame` throttling and presentation
/// feedback when a surface spans multiple outputs. Used by the udev/DRM render path after a frame is
/// rendered (the nested `winit` path has a single output and does not need it). Mirrors anvil's
/// helper, trimmed to CDE's window set plus the DnD icon (no cursor-surface scanout tracking yet).
/// Visits the drag icon, then **every** `Space` element (no output filter), then the layer surfaces.
pub fn update_primary_scanout_output(
    space: &Space<WindowElement>,
    output: &Output,
    dnd_icon: Option<&DndIcon>,
    render_element_states: &RenderElementStates,
) {
    let update = |surface: &WlSurface, states: &smithay::wayland::compositor::SurfaceData| {
        update_surface_primary_scanout_output(
            surface,
            output,
            states,
            None,
            render_element_states,
            default_primary_scanout_output_compare,
        );
    };
    if let Some(icon) = dnd_icon {
        with_surfaces_surface_tree(&icon.surface, update);
    }
    for_each_surface_on(space, output, WindowFilter::All, |owner| match owner {
        SurfaceOwner::Window(window) => window.0.with_surfaces(update),
        SurfaceOwner::Layer(layer) => layer.with_surfaces(update),
    });
}

/// Sends each surface on `output` (windows overlapping it, and its layer surfaces) the dmabuf
/// feedback matching how it was just presented, so clients can allocate scanout-capable buffers.
///
/// Walks the windows overlapping `output` and its layer surfaces (no drag icon); mirrors anvil's
/// `post_repaint`.
pub fn send_dmabuf_feedback(
    space: &Space<WindowElement>,
    output: &Output,
    render_element_states: &RenderElementStates,
    render_feedback: &DmabufFeedback,
    scanout_feedback: &DmabufFeedback,
) {
    let select = |surface: &WlSurface, _: &smithay::wayland::compositor::SurfaceData| {
        select_dmabuf_feedback(
            surface,
            render_element_states,
            render_feedback,
            scanout_feedback,
        )
    };
    for_each_surface_on(
        space,
        output,
        WindowFilter::OverlappingOutput,
        |owner| match owner {
            SurfaceOwner::Window(window) => {
                window
                    .0
                    .send_dmabuf_feedback(output, surface_primary_scanout_output, select);
            }
            SurfaceOwner::Layer(layer) => {
                layer.send_dmabuf_feedback(output, surface_primary_scanout_output, select);
            }
        },
    );
}

/// Collects the [`OutputPresentationFeedback`] for everything visible on `output`.
///
/// Walks the mapped windows overlapping `output` (and its layer surfaces) and gathers their
/// presentation feedback so the DRM backend can call `feedback.presented(..)` after the page-flip
/// completes, telling clients exactly when their frame hit the screen. Mirrors anvil's helper.
pub fn take_presentation_feedback(
    output: &Output,
    space: &Space<WindowElement>,
    render_element_states: &RenderElementStates,
) -> OutputPresentationFeedback {
    let mut output_presentation_feedback = OutputPresentationFeedback::new(output);
    let flags = |surface: &WlSurface, _: &smithay::wayland::compositor::SurfaceData| {
        surface_presentation_feedback_flags_from_states(surface, None, render_element_states)
    };
    for_each_surface_on(
        space,
        output,
        WindowFilter::OverlappingOutput,
        |owner| match owner {
            SurfaceOwner::Window(window) => window.0.take_presentation_feedback(
                &mut output_presentation_feedback,
                surface_primary_scanout_output,
                flags,
            ),
            SurfaceOwner::Layer(layer) => layer.take_presentation_feedback(
                &mut output_presentation_feedback,
                surface_primary_scanout_output,
                flags,
            ),
        },
    );

    output_presentation_feedback
}

#[cfg(test)]
mod walk_tests {
    use super::{WindowFilter, include_window};

    #[test]
    fn all_includes_every_window_without_asking_for_overlap() {
        assert!(include_window(WindowFilter::All, || unreachable!()));
    }

    #[test]
    fn overlapping_output_follows_the_overlap() {
        assert!(include_window(WindowFilter::OverlappingOutput, || true));
        assert!(!include_window(WindowFilter::OverlappingOutput, || false));
    }
}
