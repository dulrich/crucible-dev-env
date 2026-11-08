//! Keybinding engine for Crucible Dev Env (CDE).
//!
//! CDE uses a **`Super` (Mod4) leader**: the compositor grabs `Super`-prefixed chords globally and
//! resolves them to layout/workspace actions, while every non-`Super` key passes straight through to
//! the focused window. This deliberately avoids squatting shell/editor hotkeys (the chief complaint
//! against zellij's default modal "layout mode").
//!
//! This crate owns the canonical key **policy** — the [`Action`] verb set and the default
//! `Super`-leader chord table ([`Bindings`], looked up by [`chord::resolve`]) — with **no Smithay dependency**.
//! `crucible-compositor` converts its own backend
//! event types (`ModifiersState`, `Keysym`) to [`chord::Modifiers`]/[`chord::Keysym`] at the edge,
//! calls [`chord::resolve`], and executes the resulting [`Action`] through its single
//! model-mutation entry point. See `crates/crucible-compositor/src/input/` for the edge.
//!
//! - [`action`] — the [`Action`] verb set produced by chord resolution.
//! - [`bindings`] — the data-driven [`Bindings`] table (default + config overrides) and its chord/action grammar.
//! - [`chord`] — the Smithay-free [`chord::Modifiers`] type and [`chord::resolve`].

pub mod action;
pub mod bindings;
pub mod chord;

pub use action::{Action, ActionParseError, Area, NightLightArg, terminal_command};
pub use bindings::{BindError, BindTarget, Bindings, Chord};
pub use chord::{Keysym, Modifiers, resolve};
