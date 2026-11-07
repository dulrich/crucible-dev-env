//! Snip mode: the interactive region/window/output selection used by the `snip` action.
//!
//! [`model`] is the pure selection state machine (no Smithay types); the compositor side
//! (freeze, modal input, overlay drawing) is added alongside it.

pub mod model;

pub use model::{Event, Outcome, OutputInfo, Snip, crop, full_frame};
