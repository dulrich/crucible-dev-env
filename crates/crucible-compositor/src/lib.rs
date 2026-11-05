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

/// The clipboard bridge, primary selection and pointer drag-and-drop.
pub mod selection;

/// Once-per-process `autostart` commands.
pub mod autostart;

/// Per-frame rendering and presentation.
pub mod render;

/// `zwp_linux_dmabuf_v1` global creation and import notification shared by both backends.
pub mod dmabuf;

/// The compositor-drawn pointer cursor (the single cursor owner).
pub mod cursor;

/// Focus-ring border and per-pane stack indicator chrome.
pub mod chrome;

/// Logical-to-physical conversions shared by chrome, the header and text.
pub mod physical;

/// The fixed chrome and header colours.
pub mod palette;

/// Unified render-element type.
pub mod element;

/// The `CDE_SOCKET` IPC protocol (pure codec shared with the binary) and its socket server.
pub mod ipc;

/// Workspace persistence in the compositor: auto-save, capture, restore, picker overlay.
pub mod persist;

/// Workspace header strip: pure layout/hit-test core plus render and click glue.
pub mod header;

/// Compositor-drawn text: font resolution, fontdue rasterization, memory render element.
pub mod text;

/// Screen lock (`ext-session-lock-v1`): the lock state machine and its Smithay glue.
pub mod lock;

/// Idle lock/blank timers and the idle-notify / idle-inhibit protocols.
pub mod idle;

/// Relative pointer and pointer constraints (pointer lock for games).
pub mod pointer_lock;

/// The `crucible-window-identity-v1` protocol server (authenticated per-window identity).
pub mod identity_protocol;
