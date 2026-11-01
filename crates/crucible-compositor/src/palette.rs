//! The fixed chrome and header colours, as `[R, G, B, A]` premultiplied floats.
//!
//! Configured colours (`tiling` fills and frames, the header divider) come from the config as
//! [`Rgb`](crucible_config::Rgb) and convert with [`Rgb::to_rgba_f32`](crucible_config::Rgb::to_rgba_f32).

/// Colour of the mark ring and of the stack pip of a marked, non-visible window. Amber, distinct
/// from the teal [`BORDER_COLOR`].
pub const MARK_COLOR: [f32; 4] = [0.95, 0.65, 0.15, 1.0];

/// Focus-ring colour as `[R, G, B, A]` premultiplied floats.  Bright teal accent.
pub const BORDER_COLOR: [f32; 4] = [0.20, 0.80, 0.80, 1.0];

/// Colour of the **active** (focused) pip: bright accent matches the border.
pub const PIP_ACTIVE_COLOR: [f32; 4] = [0.20, 0.80, 0.80, 1.0];

/// Colour of **inactive** pips: muted grey so the position is still legible.
pub const PIP_INACTIVE_COLOR: [f32; 4] = [0.45, 0.45, 0.45, 0.85];

/// The N1 accent for the `n1-dark` and `n1-med` overlay styles (`#ff8a3d`).
pub const N1_ACCENT: [f32; 4] = [1.0, 138.0 / 255.0, 61.0 / 255.0, 1.0];

/// The N1 accent for the `n1-light` overlay style (`#f07714`).
pub const N1_LIGHT_ACCENT: [f32; 4] = [240.0 / 255.0, 119.0 / 255.0, 20.0 / 255.0, 1.0];

/// Header strip background (a dark neutral, distinct from the ring accent).
pub const STRIP_BG: [f32; 4] = [0.09, 0.10, 0.12, 1.0];
