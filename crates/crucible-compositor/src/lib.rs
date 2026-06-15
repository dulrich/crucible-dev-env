//! Wayland compositor core for Crucible Dev Env (CDE).
//!
//! CDE *is* the display server. This crate holds the Smithay state and backends (`winit` for nested
//! development, `udev`/DRM for a real TTY), XWayland integration for X11 apps, rendering, and input —
//! translating input events into operations on the [`crucible_layout`] model and presenting the
//! result. It is the only crate that depends on Smithay, keeping the layout/persist/config/keys
//! crates pure and testable.
//!
//! The compositor is brought up in Phase 1; see `plans/`.
