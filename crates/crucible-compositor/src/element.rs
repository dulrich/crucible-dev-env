//! Unified render-element type spanning all window and chrome kinds.
//!
//! Future home of the `CrucibleRenderElement` enum that unifies the renderable kinds — Wayland
//! surface trees, XWayland windows, and the compositor-drawn chrome from [`crate::drawing`] — into
//! a single damage-tracked element type the renderer in [`crate::render`] can iterate per output.
//!
//! For WS-3 [`WindowElement`] remains a thin alias to Smithay's
//! [`Window`](smithay::desktop::Window): a plain Wayland window already implements every trait the
//! compositor relies on here — [`SpaceElement`](smithay::desktop::space::SpaceElement) (so it can
//! live in the [`Space`](smithay::desktop::Space)),
//! [`WaylandFocus`](smithay::wayland::seat::WaylandFocus) (surface lookup),
//! [`IsAlive`](smithay::utils::IsAlive), and
//! [`AsRenderElements`](smithay::backend::renderer::element::AsRenderElements) (so the render path
//! can collect its surface tree).
//!
//! ## WS-5 seam (XWayland)
//! anvil wraps the window in a `WindowElement(Window)` newtype so it can carry an additional
//! `X11Surface` arm and server-side-decoration state. CDE deliberately defers that: when WS-5 adds
//! XWayland, replace this alias with such a newtype (or enum) and forward the trait impls to the
//! inner kind. Keeping the alias now means the WS-3 shell/render code is already written against the
//! `WindowElement` name, so widening it later is a localized change.

/// The element type stored in the compositor's desktop [`Space`](smithay::desktop::Space).
///
/// A WS-3 alias to Smithay's [`Window`](smithay::desktop::Window) (Wayland toplevels only). Widened
/// to a surface-kind-spanning wrapper when XWayland lands in WS-5 — see the module docs for the
/// seam.
pub type WindowElement = smithay::desktop::Window;
