//! Night light: a scheduled colour-temperature shift applied as a per-CRTC gamma ramp.
//!
//! [`model`] is the pure part (schedule, temperature to ramp, verb semantics); the DRM runtime
//! that applies it arrives alongside the `night-light` verb.

pub mod model;
