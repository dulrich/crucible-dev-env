//! Wayland compositor core for Crucible Dev Env (CDE).
//!
//! CDE *is* the display server. This crate holds the Smithay state and backends (`winit` for nested
//! development, `udev`/DRM for a real TTY), XWayland integration for X11 apps, rendering, and input —
//! translating input events into operations on the [`crucible_layout`] model and presenting the
//! result. It is the only crate that depends on Smithay, keeping the layout/persist/config/keys
//! crates pure and testable.
//!
//! The compositor is brought up in Phase 1; see `plans/01-compositor-up.md`. WS-1 establishes the
//! module layout and links Smithay; each module below is a documented stub filled in by a later
//! workstream.

/// Compositor state and calloop event-loop wiring (WS-1 stub).
pub mod state;

/// Nested `winit` development backend (WS-1 stub).
pub mod winit;

/// `udev`/DRM/KMS TTY backend (WS-1 stub).
pub mod udev;

/// Wayland shell protocol handlers (WS-1 stub).
pub mod shell;

/// Input handling and Super-leader dispatch (WS-1 stub).
pub mod input;

/// Focus targets and focus-following policy (WS-1 stub).
pub mod focus;

/// XWayland integration (WS-1 stub).
pub mod xwayland;

/// Per-frame rendering and presentation (WS-1 stub).
pub mod render;

/// Compositor-drawn chrome: cursor, borders, stack indicators (WS-1 stub).
pub mod drawing;

/// Unified render-element type (WS-1 stub).
pub mod element;
