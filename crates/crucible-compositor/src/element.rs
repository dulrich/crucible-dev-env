//! Unified render-element type spanning all window and chrome kinds.
//!
//! Future home of the `CrucibleRenderElement` enum that unifies the renderable kinds — Wayland
//! surface trees, XWayland windows, and the compositor-drawn chrome from [`crate::drawing`] — into
//! a single damage-tracked element type the renderer in [`crate::render`] can iterate per output.
//!
//! Implemented in a later workstream (WS-6); this is a WS-1 stub.
