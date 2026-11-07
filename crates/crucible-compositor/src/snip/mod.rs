//! Snip mode: the interactive region/window/output selection used by the `snip` action.
//!
//! [`model`] is the pure selection state machine (no Smithay types); [`mode`] is the compositor
//! side (freeze every output, modal input, overlay drawing, crop and delivery).

pub mod mode;
pub mod model;

pub use mode::{SnipSession, crop_capture, snip_elements};
pub use model::{Event, Outcome, OutputInfo, Snip, crop, full_frame};
