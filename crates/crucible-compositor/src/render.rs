//! Per-frame rendering: scene assembly and output presentation.
//!
//! Future home of the render path: assembles the list of render elements for an output each frame,
//! drives damage-tracked rendering through the GL ES / multi / pixman renderers, schedules frame
//! callbacks, and presents to the backend (winit surface or DRM page-flip). Backend-agnostic;
//! consumes the elements built in [`crate::element`] and [`crate::drawing`].
//!
//! Implemented in a later workstream (WS-6); this is a WS-1 stub.
