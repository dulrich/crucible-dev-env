//! Unified render-element type spanning all window and chrome kinds.
//!
//! Future home of the `CrucibleRenderElement` enum that unifies the renderable kinds — Wayland
//! surface trees, XWayland windows, and the compositor-drawn chrome from [`crate::drawing`] — into
//! a single damage-tracked element type the renderer in [`crate::render`] can iterate per output.
//!
//! Implemented in a later workstream (WS-6); the richer `CrucibleRenderElement` enum lands then.
//!
//! For WS-2 this is a minimal alias: the desktop [`Space`](smithay::desktop::Space) in
//! [`crate::state::CrucibleState`] needs a concrete element type, and a plain Wayland
//! [`Window`](smithay::desktop::Window) already implements
//! [`SpaceElement`](smithay::desktop::space::SpaceElement). WS-3 replaces this with a wrapper that
//! also carries XWayland surfaces.

/// The element type stored in the compositor's desktop space.
///
/// WS-2 placeholder aliased to Smithay's [`Window`](smithay::desktop::Window); widened to a
/// surface-kind-spanning wrapper in WS-3.
pub type WindowElement = smithay::desktop::Window;
