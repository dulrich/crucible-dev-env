//! Shared fixtures for the `World` integration tests: the gap/screen/output constants and the
//! single-output [`world`] builder. Each test binary uses a subset, hence the `dead_code` allow.

#![allow(dead_code)]

use crucible_layout::{GapConfig, Rect, World};

/// Zero gaps keep expected rects exact; a single 800x600 output keeps them readable.
pub const GAPS: GapConfig = GapConfig::new(0, 0);
pub const SCREEN: Rect = Rect::new(0, 0, 800, 600);
pub const R1: Rect = Rect::new(0, 0, 1920, 1080);
pub const R2: Rect = Rect::new(1920, 0, 1280, 1024);

/// A `World` with one output sized [`SCREEN`], so `project_stored` yields real rects.
pub fn world() -> World {
    let mut w = World::new("1");
    let output = w.focused_output();
    w.set_output_rect(output, SCREEN).unwrap();
    w
}
