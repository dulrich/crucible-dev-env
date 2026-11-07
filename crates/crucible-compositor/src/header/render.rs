//! Header strip rendering: the strip's render elements for one output's [`HeaderLayout`].

use crucible_layout::Rect;
use smithay::{
    backend::renderer::{ImportAll, ImportMem, Renderer},
    utils::{Physical, Point, Scale},
};

use super::{HeaderLayout, LABEL_ACTIVE_RGBA, LABEL_RGBA, PAD_X};
use crate::{
    palette::{BORDER_COLOR, STRIP_BG},
    physical::{PhysicalMap, solid},
    render::CustomRenderElements,
    text::TextRenderer,
};

/// Builds the strip's render elements (front-to-back: labels, then highlight and background
/// solids) for one output's `layout`, positioned in the output's frame at
/// `origin`. Without a loaded font only the backgrounds and highlight are produced.
pub fn build_header_elements<R>(
    layout: &HeaderLayout,
    origin: (i32, i32),
    size: u32,
    text: &mut TextRenderer,
    renderer: &mut R,
    scale: Scale<f64>,
) -> Vec<CustomRenderElements<R>>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Send + Clone + 'static,
{
    let map = PhysicalMap::plain(scale);
    let to_origin = |r: Rect| r.translate(-origin.0, -origin.1);
    let mut labels = Vec::new();
    let mut solids = Vec::new();
    let l = layout;
    let strip = map.rect(to_origin(l.strip));
    let line = (f64::from(size) * 1.2 * scale.y).round() as i32;
    let text_y = strip.loc.y + (strip.size.h - line) / 2;
    let pad_x = (f64::from(PAD_X) * scale.x).round() as i32;
    for seg in &l.segments {
        let active = seg.index == l.active;
        let seg_rect = map.rect(to_origin(seg.rect));
        if active {
            solids.push(solid(seg_rect, BORDER_COLOR));
        }
        let rgba = if active {
            LABEL_ACTIVE_RGBA
        } else {
            LABEL_RGBA
        };
        let loc =
            Point::<f64, Physical>::from((f64::from(seg_rect.loc.x + pad_x), f64::from(text_y)));
        if let Some(el) = text.element(renderer, loc, &l.names[seg.index], size, rgba, scale) {
            labels.push(CustomRenderElements::Text(el));
        }
    }
    let divider = l.divider_color.to_rgba_f32();
    for div in &l.dividers {
        solids.push(solid(map.rect(to_origin(*div)), divider));
    }
    solids.push(solid(strip, STRIP_BG));
    labels.extend(solids.into_iter().map(CustomRenderElements::SolidColor));
    labels
}

#[cfg(test)]
mod scale_tests {
    use super::super::segment_layout;
    use super::*;

    #[test]
    fn scaled_strip_and_segments_stay_contiguous() {
        let s = Scale::from(170.0 / 120.0);
        let map = PhysicalMap::plain(s);
        let strip = Rect::new(100, 0, 300, 24);
        let segs = segment_layout(&[20, 30], strip, 0).0;
        let (ps, p0, p1) = (
            map.rect(strip),
            map.rect(segs[0].rect),
            map.rect(segs[1].rect),
        );
        // Strip: round(100*s)=142 .. round(400*s)=567, height round(24*s)=34.
        assert_eq!(
            (ps.loc.x, ps.size.w, ps.loc.y, ps.size.h),
            (142, 425, 0, 34)
        );
        // Segments abut (shared edge) and start at the strip's left edge, full strip height.
        assert_eq!(p0.loc.x, ps.loc.x);
        assert_eq!(p0.loc.x + p0.size.w, p1.loc.x);
        assert_eq!((p0.loc.y, p0.size.h), (ps.loc.y, ps.size.h));
        assert!(p1.loc.x + p1.size.w <= ps.loc.x + ps.size.w);
        // At scale 1 the strip is unchanged.
        let one = PhysicalMap::plain(Scale::from(1.0)).rect(strip);
        assert_eq!((one.loc.x, one.size.w, one.size.h), (100, 300, 24));
    }

    #[test]
    fn half_scale_divider_never_overlaps_segments() {
        let map = PhysicalMap::plain(Scale::from(0.5));
        // Odd divider origin: segment 0 = 101..141, divider 141..142, segment 1 = 142..192.
        let strip = Rect::new(101, 0, 300, 24);
        let (segs, divs) = segment_layout(&[20, 30], strip, 1);
        let (p0, pd, p1) = (
            map.rect(segs[0].rect),
            map.rect(divs[0]),
            map.rect(segs[1].rect),
        );
        let end = |r: smithay::utils::Rectangle<i32, Physical>| r.loc.x + r.size.w;
        assert!(end(p0) <= pd.loc.x);
        assert!(end(pd) <= p1.loc.x);
        assert!(pd.size.w >= 0);
    }
}
