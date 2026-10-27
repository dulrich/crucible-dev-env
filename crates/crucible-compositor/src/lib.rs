//! Wayland compositor core for Crucible Dev Env (CDE).
//!
//! CDE *is* the display server. This crate holds the Smithay state and backends (`winit` for nested
//! development, `udev`/DRM for a real TTY), XWayland integration for X11 apps, rendering, and input —
//! translating input events into operations on the [`crucible_layout`] model and presenting the
//! result. It is the only crate that depends on Smithay, keeping the layout/persist/config/keys
//! crates pure and testable.
//!
//! Each module below owns one durable boundary; `plans/` records how they arrived.

/// Compositor state, Wayland globals, and calloop event-loop wiring.
pub mod config;
pub mod state;

/// Nested `winit` development backend.
pub mod winit;

/// `udev`/DRM/KMS TTY backend (multi-output, libseat session, VT switching).
pub mod udev;

/// Wayland shell protocol handlers (xdg-shell + wlr-layer-shell).
pub mod shell;

/// Input handling: keyboard/pointer I/O at the seat and execution of resolved `crucible-keys` actions.
pub mod input;

/// Focus targets and focus-following policy.
pub mod focus;

/// Bridge from the pure `crucible-layout` model to the Smithay `Space` (placement/visibility/focus).
pub mod layout;

/// XWayland integration: server lifecycle and X11 window management.
pub mod xwayland;

/// Per-frame rendering and presentation.
pub mod render;

/// Compositor-drawn chrome: cursor, borders, stack indicators.
pub mod drawing;

/// Focus-ring border and per-pane stack indicator chrome.
pub mod chrome;

/// Unified render-element type.
pub mod element;

/// The `CDE_SOCKET` IPC protocol (pure codec shared with the binary) and its socket server.
pub mod ipc;

/// Workspace persistence in the compositor: auto-save, capture, restore, picker overlay.
pub mod persist;

/// Workspace header strip: pure layout/hit-test/Alt-tap core plus render and click glue.
pub mod header;

/// Compositor-drawn text: font resolution, fontdue rasterization, memory render element.
pub mod text;

/// Screen lock (`ext-session-lock-v1`): the lock state machine and its Smithay glue.
pub mod lock;
