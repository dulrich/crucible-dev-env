//! The logical-to-physical conversions shared by chrome, the header and text.
//!
//! [`PhysicalMap`] (with [`solid`]) converts layout [`Rect`]s into physical Smithay rects at an
//! output's fractional scale; [`quantize_scale`] and [`padded_dims`] size rasterized text buffers
//! so their texels map 1:1 onto physical pixels.

use smithay::backend::renderer::{
    element::{Id, Kind, solid::SolidColorRenderElement},
    utils::CommitCounter,
};
use smithay::utils::{Logical, Physical, Rectangle, Scale, Size};

use crucible_layout::Rect;

/// A solid-colour render element covering the physical `rect` (in the output's frame). Build the
/// rect from a logical one with [`PhysicalMap::rect`].
#[must_use]
pub fn solid(rect: Rectangle<i32, Physical>, color: [f32; 4]) -> SolidColorRenderElement {
    SolidColorRenderElement::new(
        Id::new(),
        rect,
        CommitCounter::default(),
        color,
        Kind::Unspecified,
    )
}

/// The one logical-to-physical conversion for chrome rects (output-relative, logical) at an
/// output's fractional scale `s`.
///
/// ## Rounding rule
/// 1. **Edges, not sizes.** A rect's physical extent is `[round(x*s), round((x+w)*s))` per axis, so
///    two logical rects that share an edge share the physical edge: chrome stays seamless (no gap,
///    no overlap, area sum preserved).
/// 2. **Window edges follow smithay.** Smithay places a window at the rounded origin
///    `round(x*s)` and ends it at `origin + round(w*s)` (`space/mod.rs` rounds the location,
///    `element/surface.rs` sizes the surface relative to it), which can differ from
///    `round((x+w)*s)` by one pixel. For each registered window rect `(x, w)`, a chrome edge at
///    `x` lands on `round(x*s)` and an edge at `x + w` lands on `round(x*s) + round(w*s)`, so the
///    ring / fill bordering a window meets the client surface exactly. A window only snaps the
///    edges of rects that overlap it on the other axis (closed interval), and the first matching
///    window wins.
///
/// At scale 1 both rules reduce to the identity, so the output is unchanged.
#[derive(Debug, Clone)]
pub struct PhysicalMap {
    scale: Scale<f64>,
    windows: Vec<Rect>,
}

impl PhysicalMap {
    /// A map at `scale` whose rule 2 applies to `windows` (output-relative logical window rects).
    #[must_use]
    pub fn new(scale: Scale<f64>, windows: Vec<Rect>) -> Self {
        Self { scale, windows }
    }

    /// A map with no window edges to meet: plain edge rounding.
    #[must_use]
    pub fn plain(scale: Scale<f64>) -> Self {
        Self::new(scale, Vec::new())
    }

    /// The physical rect of the logical `r`.
    #[must_use]
    pub fn rect(&self, r: Rect) -> Rectangle<i32, Physical> {
        let near =
            |lo: i32, len: i32, w_lo: i32, w_len: i32| lo <= w_lo + w_len && w_lo <= lo + len;
        let xs: Vec<(i32, i32)> = self
            .windows
            .iter()
            .filter(|w| near(r.y, r.height, w.y, w.height))
            .map(|w| (w.x, w.width))
            .collect();
        let ys: Vec<(i32, i32)> = self
            .windows
            .iter()
            .filter(|w| near(r.x, r.width, w.x, w.width))
            .map(|w| (w.y, w.height))
            .collect();
        let (x0, x1) = (
            snap(r.x, &xs, self.scale.x),
            snap(r.x + r.width, &xs, self.scale.x),
        );
        let (y0, y1) = (
            snap(r.y, &ys, self.scale.y),
            snap(r.y + r.height, &ys, self.scale.y),
        );
        Rectangle::new((x0, y0).into(), ((x1 - x0).max(0), (y1 - y0).max(0)).into())
    }
}

/// Rounds a logical coordinate to physical, honouring window `(start, len)` spans (rule 2).
fn snap(coord: i32, spans: &[(i32, i32)], s: f64) -> i32 {
    let round = |v: i32| (f64::from(v) * s).round() as i32;
    for &(start, len) in spans {
        if coord == start {
            return round(start);
        }
        if coord == start + len {
            return round(start) + round(len);
        }
    }
    round(coord)
}

/// A fractional scale quantized to 1/120 steps (at least 1/120).
#[must_use]
pub fn quantize_scale(scale: f64) -> u32 {
    ((scale * 120.0).round() as u32).max(1)
}

/// Sizes a rasterized `w`x`h` buffer for 1:1 display at `(sx, sy)`: returns the physical buffer
/// size (the raster padded with transparent pixels right/bottom) and the integer logical size whose
/// physical size is exactly that. The renderer sizes an element as `round(logical * scale)`, so
/// padding to `round(ceil(w / sx) * sx)` makes the texels map one-to-one, never resampled. At scale
/// 1 this is the identity.
#[must_use]
pub fn padded_dims(w: i32, h: i32, sx: f64, sy: f64) -> (Size<i32, Physical>, Size<i32, Logical>) {
    let fit = |n: i32, s: f64| {
        let logical = (f64::from(n) / s).ceil() as i32;
        let phys = (f64::from(logical) * s).round() as i32;
        (phys.max(n), logical)
    };
    let ((pw, lw), (ph, lh)) = (fit(w, sx), fit(h, sy));
    ((pw, ph).into(), (lw, lh).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padded_dims_make_texels_map_one_to_one() {
        // Identity at scale 1.
        let (p, l) = padded_dims(37, 17, 1.0, 1.0);
        assert_eq!(((p.w, p.h), (l.w, l.h)), ((37, 17), (37, 17)));
        // At 170/120 the physical size is exactly round(logical * scale) and never smaller.
        let s = 170.0 / 120.0;
        for w in 1..300 {
            let (p, l) = padded_dims(w, w, s, s);
            assert!(p.w >= w && p.w - w <= 2, "w={w} padded to {}", p.w);
            assert_eq!(p.w, (f64::from(l.w) * s).round() as i32);
        }
        assert_eq!(quantize_scale(1.4167), 170);
        assert_eq!(quantize_scale(0.0), 1);
    }
}
