//! The window kind stored in the desktop [`Space`](smithay::desktop::Space).
//!
//! [`WindowElement`] is the unified window type CDE maps, places, focuses, and renders. It spans
//! both window kinds CDE supports:
//! - **Wayland** toplevels (xdg-shell), and
//! - **XWayland** X11 windows (an [`X11Surface`](smithay::xwayland::X11Surface)).
//!
//! ## Why a newtype over [`Window`](smithay::desktop::Window) rather than a hand-rolled enum
//! Smithay's [`Window`] *already* unifies the two kinds internally via
//! [`WindowSurface`](smithay::desktop::WindowSurface): it is constructed with either
//! [`Window::new_wayland_window`] or [`Window::new_x11_window`], and its
//! [`SpaceElement`]/[`WaylandFocus`]/[`IsAlive`]/[`AsRenderElements`] impls already dispatch over
//! both. Wrapping `Window` in a one-field newtype (exactly as Smithay's `anvil` reference compositor
//! does) is therefore the DRY way to "make `WindowElement` span a Wayland window and an
//! `X11Surface`": the X11 arm is the `Window` whose underlying surface is
//! [`WindowSurface::X11`]. CDE keeps the newtype (instead of a bare alias) so it owns the type and
//! can attach CDE-specific behaviour later without touching call sites.
//!
//! The newtype's trait impls forward to the inner `Window`, so shell/render code only ever sees
//! `WindowElement`.

use std::borrow::Cow;

use crucible_layout::WindowId;
use smithay::{
    backend::renderer::{
        ImportAll, ImportMem, Renderer, Texture,
        element::{AsRenderElements, surface::WaylandSurfaceRenderElement},
    },
    desktop::{Window, WindowSurface, WindowSurfaceType, space::SpaceElement},
    output::Output,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    render_elements,
    utils::{IsAlive, Logical, Physical, Point, Rectangle, Scale, user_data::UserDataMap},
    wayland::seat::WaylandFocus,
};

use crate::focus::PointerFocusTarget;

/// The element type stored in the compositor's desktop [`Space`](smithay::desktop::Space).
///
/// A newtype wrapper over Smithay's [`Window`], which itself spans a Wayland toplevel
/// ([`Window::new_wayland_window`]) or an XWayland window ([`Window::new_x11_window`]). See the
/// module docs for why this is a newtype-over-`Window` rather than a hand-rolled enum.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowElement(pub Window);

/// The model [`WindowId`] this element is bound to, stored in the window's [`UserDataMap`].
///
/// Wrapped in a newtype so it is keyed distinctly in the type-indexed user-data map. The id is set
/// once, when the window is mapped through `World::open_window` (see [`crate::layout`]), and read
/// back to drive geometry/visibility/focus from the model.
#[derive(Debug, Clone, Copy)]
struct WindowIdMarker(WindowId);

impl WindowElement {
    /// Binds this element to its model [`WindowId`].
    ///
    /// Called exactly once on map. Idempotent for the same id; replacing an existing id is a bug, so
    /// later writes are ignored (the first binding wins) — [`Self::window_id`] always reflects the
    /// id assigned at map time.
    pub fn set_window_id(&self, id: WindowId) {
        self.0.user_data().insert_if_missing(|| WindowIdMarker(id));
    }

    /// Returns the model [`WindowId`] bound to this element, if any.
    ///
    /// `None` only for elements not (yet) routed through the model — e.g. override-redirect X11
    /// surfaces that place themselves and are never inserted into the [`World`](crucible_layout::World).
    pub fn window_id(&self) -> Option<WindowId> {
        self.0
            .user_data()
            .get::<WindowIdMarker>()
            .map(|marker| marker.0)
    }
    /// Wraps a Wayland toplevel surface into a [`WindowElement`].
    pub fn new_wayland_window(toplevel: smithay::wayland::shell::xdg::ToplevelSurface) -> Self {
        WindowElement(Window::new_wayland_window(toplevel))
    }

    /// Wraps an XWayland [`X11Surface`](smithay::xwayland::X11Surface) into a [`WindowElement`].
    pub fn new_x11_window(surface: smithay::xwayland::X11Surface) -> Self {
        WindowElement(Window::new_x11_window(surface))
    }

    /// Returns the underlying xdg toplevel handle, if this is a Wayland window.
    pub fn toplevel(&self) -> Option<&smithay::wayland::shell::xdg::ToplevelSurface> {
        self.0.toplevel()
    }

    /// Returns the underlying [`X11Surface`](smithay::xwayland::X11Surface), if this is an X11
    /// window.
    pub fn x11_surface(&self) -> Option<&smithay::xwayland::X11Surface> {
        self.0.x11_surface()
    }

    /// Whether this element is backed by an XWayland X11 surface.
    pub fn is_x11(&self) -> bool {
        self.0.is_x11()
    }

    /// Whether this element is backed by a native Wayland surface.
    pub fn is_wayland(&self) -> bool {
        self.0.is_wayland()
    }

    /// Returns the root [`WlSurface`] of this window, if it currently has one.
    ///
    /// X11 windows only have a backing `wl_surface` once XWayland has associated one; before that
    /// this is `None`.
    pub fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        self.0.wl_surface()
    }

    /// The per-window [`UserDataMap`], used to attach compositor-specific state to a window.
    pub fn user_data(&self) -> &UserDataMap {
        self.0.user_data()
    }

    /// The window's logical geometry (size + offset of its content within its bounding box).
    ///
    /// Convenience accessor so call sites need not bring [`SpaceElement`] into scope; forwards to
    /// the inner [`Window`]'s [`SpaceElement::geometry`].
    pub fn geometry(&self) -> Rectangle<i32, Logical> {
        SpaceElement::geometry(&self.0)
    }

    /// Forwards a surface commit to the inner [`Window`] so its cached geometry refreshes.
    pub fn on_commit(&self) {
        self.0.on_commit();
    }

    /// Resolves the [`PointerFocusTarget`] (and its top-left, in window-local logical coords) under
    /// `location`, dispatching over the window's kind.
    ///
    /// Wayland windows resolve to a [`PointerFocusTarget::WlSurface`]; X11 windows resolve to a
    /// [`PointerFocusTarget::X11Surface`] so pointer events route through the XWayland surface.
    pub fn surface_under(
        &self,
        location: Point<f64, Logical>,
        window_type: WindowSurfaceType,
    ) -> Option<(PointerFocusTarget, Point<i32, Logical>)> {
        let surface_under = self.0.surface_under(location, window_type);
        match self.0.underlying_surface() {
            WindowSurface::Wayland(_) => {
                surface_under.map(|(surface, loc)| (PointerFocusTarget::WlSurface(surface), loc))
            }
            WindowSurface::X11(s) => {
                surface_under.map(|(_, loc)| (PointerFocusTarget::X11Surface(s.clone()), loc))
            }
        }
    }

    /// Sends frame callbacks for this window's surface tree. Forwards to the inner [`Window`].
    pub fn send_frame<T, F>(
        &self,
        output: &Output,
        time: T,
        throttle: Option<std::time::Duration>,
        primary_scan_out_output: F,
    ) where
        T: Into<std::time::Duration>,
        F: FnMut(&WlSurface, &smithay::wayland::compositor::SurfaceData) -> Option<Output> + Copy,
    {
        self.0
            .send_frame(output, time, throttle, primary_scan_out_output);
    }
}

impl IsAlive for WindowElement {
    #[inline]
    fn alive(&self) -> bool {
        self.0.alive()
    }
}

impl WaylandFocus for WindowElement {
    #[inline]
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        self.0.wl_surface()
    }
}

impl SpaceElement for WindowElement {
    fn geometry(&self) -> Rectangle<i32, Logical> {
        SpaceElement::geometry(&self.0)
    }
    fn bbox(&self) -> Rectangle<i32, Logical> {
        SpaceElement::bbox(&self.0)
    }
    fn is_in_input_region(&self, point: &Point<f64, Logical>) -> bool {
        SpaceElement::is_in_input_region(&self.0, point)
    }
    fn z_index(&self) -> u8 {
        SpaceElement::z_index(&self.0)
    }
    fn set_activate(&self, activated: bool) {
        SpaceElement::set_activate(&self.0, activated);
    }
    fn output_enter(&self, output: &Output, overlap: Rectangle<i32, Logical>) {
        SpaceElement::output_enter(&self.0, output, overlap);
    }
    fn output_leave(&self, output: &Output) {
        SpaceElement::output_leave(&self.0, output);
    }
    fn refresh(&self) {
        SpaceElement::refresh(&self.0);
    }
}

render_elements!(
    /// Render element for a [`WindowElement`]: a single Wayland surface tree (both Wayland and
    /// XWayland windows present their content as a `wl_surface` tree to the renderer).
    pub WindowRenderElement<R> where R: ImportAll + ImportMem;
    /// The window's Wayland surface tree.
    Window=WaylandSurfaceRenderElement<R>,
);

impl<R: Renderer> std::fmt::Debug for WindowRenderElement<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Window(arg0) => f.debug_tuple("Window").field(arg0).finish(),
            Self::_GenericCatcher(arg0) => f.debug_tuple("_GenericCatcher").field(arg0).finish(),
        }
    }
}

impl<R> AsRenderElements<R> for WindowElement
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + Texture + 'static,
{
    type RenderElement = WindowRenderElement<R>;

    fn render_elements<C: From<Self::RenderElement>>(
        &self,
        renderer: &mut R,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Vec<C> {
        AsRenderElements::render_elements(&self.0, renderer, location, scale, alpha)
            .into_iter()
            .map(C::from)
            .collect()
    }
}
