//! Nested `winit` backend for development.
//!
//! Future home of the `winit`-hosted backend that runs CDE as a window inside an existing
//! Wayland or X11 session, for fast iteration without a spare TTY. Initializes the GL ES
//! renderer against the `winit` surface, drives the render loop, and feeds `winit` input into
//! the shared compositor state. Selected at runtime via `--winit`.
//!
//! Implemented in a later workstream (WS-2); this is a WS-1 stub.
