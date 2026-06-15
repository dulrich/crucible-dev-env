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

/// Compositor state, Wayland globals, and calloop event-loop wiring.
pub mod state;

/// Nested `winit` development backend.
pub mod winit;

/// `udev`/DRM/KMS TTY backend (multi-output, libseat session, VT switching).
pub mod udev;

/// Wayland shell protocol handlers (xdg-shell + wlr-layer-shell).
pub mod shell;

/// Input handling and Super-leader dispatch (WS-1 stub; implemented in WS-4).
pub mod input;

/// Focus targets and focus-following policy.
pub mod focus;

/// Bridge from the pure `crucible-layout` model to the Smithay `Space` (placement/visibility/focus).
pub mod layout;

/// XWayland integration (WS-1 stub; implemented in WS-5).
pub mod xwayland;

/// Per-frame rendering and presentation.
pub mod render;

/// Compositor-drawn chrome: cursor, borders, stack indicators.
pub mod drawing;

/// Unified render-element type.
pub mod element;
