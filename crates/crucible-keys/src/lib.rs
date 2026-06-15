//! Keybinding engine for Crucible Dev Env (CDE).
//!
//! CDE uses a **`Super` (Mod4) leader**: the compositor grabs `Super`-prefixed chords globally and
//! resolves them to layout/workspace actions, while every non-`Super` key passes straight through to
//! the focused window. This deliberately avoids squatting shell/editor hotkeys (the chief complaint
//! against zellij's default modal "layout mode").
//!
//! This crate maps configured chords to actions over the [`crucible_layout`] model. Implementation
//! lands in Phase 2; see `plans/`.
