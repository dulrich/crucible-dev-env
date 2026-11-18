//! The login panel: a pure layout (centred on its region point, the secret field masked) and its render
//! elements (labels, then the field and panel solids, front-to-back).

use crucible_config::{Config, GreeterConfig, Milli, Palette, PanelPosition, Rgb, surface_palette};
use crucible_layout::Rect;
use smithay::{
    backend::renderer::{ImportAll, ImportMem, Renderer},
    utils::{Physical, Point, Scale},
};

use crate::{
    physical::{PhysicalMap, solid},
    render::CustomRenderElements,
    text::TextRenderer,
};

/// Panel width (logical px).
pub const PANEL_W: i32 = 420;
/// Inner padding (logical px).
pub const PAD: i32 = 24;
/// Label font size (logical px).
pub const TEXT_PX: u32 = 16;
/// Field height (logical px).
pub const FIELD_H: i32 = 36;
/// Line height of a label row (logical px).
pub const LINE_H: i32 = 22;
/// The panel border width (logical px).
pub const BORDER_W: i32 = 2;
/// One ellipsis step (see [`ellipsis`]).
pub const ELLIPSIS_STEP: std::time::Duration = std::time::Duration::from_millis(400);
/// The mask glyph for secret input.
pub const BULLET: char = '\u{2022}';

/// What the panel shows this frame (owned, so it rides in the frame inputs).
#[derive(Debug, Clone, PartialEq)]
pub struct PanelFrame {
    /// The field label.
    pub prompt: String,
    /// The field contents as drawn (already masked for secret prompts); `None` draws no field
    /// at all (neither text nor its frame), as the post-auth "Loading CDE" panel.
    pub field: Option<String>,
    /// The message line, if any.
    pub message: Option<String>,
    /// The active keyboard layout name, drawn right-aligned on the prompt row (`None` with a
    /// single layout, as swaylock shows it only with two or more).
    pub layout: Option<String>,
    /// The panel colours.
    pub palette: Palette,
    /// The panel fill alpha.
    pub alpha: Milli,
    /// Where the panel sits on the output.
    pub position: PanelPosition,
    /// The output clear colour (the profile colour, or black under an image).
    pub clear: [f32; 4],
}

/// The panel's output-local logical geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelLayout {
    /// The whole panel.
    pub panel: Rect,
    /// Top-left of the prompt label.
    pub prompt: (i32, i32),
    /// The input field box.
    pub field: Rect,
    /// Top-left of the message line.
    pub message: (i32, i32),
}

/// The field text as drawn: one bullet per character for a secret prompt, never the text.
#[must_use]
pub fn field_text(input: &str, secret: bool) -> String {
    if secret {
        std::iter::repeat_n(BULLET, input.chars().count()).collect()
    } else {
        input.to_string()
    }
}

/// The animated ellipsis after `elapsed`: `""`, `"."`, `".."`, `"..."`, one step per
/// [`ELLIPSIS_STEP`], cycling.
#[must_use]
pub(crate) fn ellipsis(elapsed: std::time::Duration) -> &'static str {
    const STEPS: [&str; 4] = ["", ".", "..", "..."];
    let i = elapsed.as_millis() / ELLIPSIS_STEP.as_millis();
    STEPS[usize::try_from(i % 4).unwrap_or(0)]
}

/// The prompt of the "Logging out" modal.
pub const LOGOUT_TEXT: &str = "Logging out";

/// The modal's palette: the `greeter` block's style and accent (alpha from `overlay { alpha }`),
/// or [`GreeterConfig::default`] without one (the tty1 default), as the published profile.
#[must_use]
pub fn logout_frame(config: &Config, elapsed: std::time::Duration) -> PanelFrame {
    let default = GreeterConfig::default();
    let g = config.greeter.as_ref().unwrap_or(&default);
    PanelFrame {
        prompt: format!("{LOGOUT_TEXT}{}", ellipsis(elapsed)),
        field: None,
        message: None,
        layout: None,
        palette: surface_palette(g.style, g.accent, config.theme.as_ref()),
        alpha: config.overlay.alpha,
        position: g.position,
        clear: [0.0, 0.0, 0.0, 1.0],
    }
}

/// The border ring just inside `r`, `w` wide: top, bottom, left, right.
#[must_use]
pub fn border_rects(r: Rect, w: i32) -> [Rect; 4] {
    let side_h = (r.height - 2 * w).max(0);
    [
        Rect::new(r.x, r.y, r.width, w),
        Rect::new(r.x, r.y + r.height - w, r.width, w),
        Rect::new(r.x, r.y + w, w, side_h),
        Rect::new(r.x + r.width - w, r.y + w, w, side_h),
    ]
}

/// The layout indicator text: the name of the active layout when two or more are configured.
#[must_use]
pub fn layout_label(layouts: &[String], active: usize) -> Option<String> {
    (layouts.len() >= 2)
        .then(|| layouts.get(active))
        .flatten()
        .cloned()
}

/// Centres `size` on `frac` of `extent`, clamped so `0..=extent - size` holds.
fn centre_on(extent: i32, size: i32, frac: f64) -> i32 {
    #[allow(clippy::cast_possible_truncation)]
    let at = (f64::from(extent) * frac).round() as i32;
    (at - size / 2).clamp(0, (extent - size).max(0))
}

/// Lays the panel out centred on the `position` region point of an output of logical size
/// `w`x`h`, clamped fully inside it (narrow outputs shrink it).
#[must_use]
pub fn layout(w: i32, h: i32, position: PanelPosition) -> PanelLayout {
    let pw = PANEL_W.min((w - 2 * PAD).max(PAD * 4));
    let ph = PAD + LINE_H + 8 + FIELD_H + 12 + LINE_H + PAD;
    let (fx, fy) = position.fractions();
    let (x, y) = (centre_on(w, pw, fx), centre_on(h, ph, fy));
    let prompt = (x + PAD, y + PAD);
    let field = Rect::new(x + PAD, prompt.1 + LINE_H + 8, pw - 2 * PAD, FIELD_H);
    let message = (x + PAD, field.y + FIELD_H + 12);
    PanelLayout {
        panel: Rect::new(x, y, pw, ph),
        prompt,
        field,
        message,
    }
}

fn rgba_u8(c: Rgb) -> [u8; 4] {
    [c.0, c.1, c.2, 255]
}

/// `c` at `alpha`, premultiplied (the solid element's colour space).
fn with_alpha(c: Rgb, alpha: Milli) -> [f32; 4] {
    #[allow(clippy::cast_precision_loss)]
    let a = (alpha.0.min(1000) as f32) / 1000.0;
    let [r, g, b, _] = c.to_rgba_f32();
    [r * a, g * a, b * a, a]
}

/// Builds the panel's elements for an output of logical size `size`.
pub fn build_panel_elements<R>(
    frame: &PanelFrame,
    size: (i32, i32),
    text: &mut TextRenderer,
    renderer: &mut R,
    scale: Scale<f64>,
) -> Vec<CustomRenderElements<R>>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Send + Clone + 'static,
{
    let l = layout(size.0, size.1, frame.position);
    let map = PhysicalMap::plain(scale);
    let pal = frame.palette;
    let mut out = Vec::new();
    let layout_w = frame
        .layout
        .as_deref()
        .map_or(0, |name| text.measure(name, TEXT_PX).unwrap_or(0));
    let mut label = |s: &str, at: (i32, i32), rgba: [u8; 4], out: &mut Vec<_>| {
        if s.is_empty() {
            return;
        }
        let p = map.rect(Rect::new(at.0, at.1, 1, 1)).loc;
        let loc = Point::<f64, Physical>::from((f64::from(p.x), f64::from(p.y)));
        if let Some(el) = text.element(renderer, loc, s, TEXT_PX, rgba, scale) {
            out.push(CustomRenderElements::Text(el));
        }
    };
    label(&frame.prompt, l.prompt, rgba_u8(pal.text), &mut out);
    let field_at = (l.field.x + 8, l.field.y + (FIELD_H - LINE_H) / 2 + 2);
    if let Some(field) = &frame.field {
        label(field, field_at, rgba_u8(pal.text), &mut out);
    }
    if let Some(msg) = &frame.message {
        label(msg, l.message, rgba_u8(pal.accent), &mut out);
    }
    if let Some(name) = &frame.layout {
        let at = (l.panel.x + l.panel.width - PAD - layout_w, l.prompt.1);
        label(name, at, rgba_u8(pal.accent), &mut out);
    }
    if frame.field.is_some() {
        let f = l.field;
        let inner = Rect::new(f.x + 1, f.y + 1, f.width - 2, f.height - 2);
        out.push(CustomRenderElements::SolidColor(solid(
            map.rect(inner),
            pal.bg.to_rgba_f32(),
        )));
        out.push(CustomRenderElements::SolidColor(solid(
            map.rect(f),
            pal.accent.to_rgba_f32(),
        )));
    }
    // The border ring sits on the panel edge, in front of the panel fill (front-to-back order).
    for r in border_rects(l.panel, BORDER_W) {
        out.push(CustomRenderElements::SolidColor(solid(
            map.rect(r),
            crate::palette::PANEL_BORDER.to_rgba_f32(),
        )));
    }
    out.push(CustomRenderElements::SolidColor(solid(
        map.rect(l.panel),
        with_alpha(pal.bg, frame.alpha),
    )));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_position_centres_on_its_region_point() {
        for (pos, fx, fy) in [
            (PanelPosition::Center, 0.5, 0.5),
            (PanelPosition::Upper, 0.5, 0.25),
            (PanelPosition::Lower, 0.5, 0.75),
            (PanelPosition::UpperLeft, 0.25, 0.25),
            (PanelPosition::UpperRight, 0.75, 0.25),
            (PanelPosition::LowerLeft, 0.25, 0.75),
            (PanelPosition::LowerRight, 0.75, 0.75),
        ] {
            let l = layout(1920, 1080, pos);
            let p = l.panel;
            assert!(p.x >= 0 && p.x + p.width <= 1920 && p.y >= 0 && p.y + p.height <= 1080);
            assert!(
                (p.x * 2 + p.width - (1920.0 * fx * 2.0) as i32).abs() <= 1,
                "{pos:?}"
            );
            assert!(
                (p.y * 2 + p.height - (1080.0 * fy * 2.0) as i32).abs() <= 1,
                "{pos:?}"
            );
            assert!(
                l.field.x >= p.x && l.field.x + l.field.width <= p.x + p.width,
                "{pos:?}"
            );
        }
    }

    #[test]
    fn a_small_output_clamps_the_panel_and_secret_input_is_masked() {
        // A narrow output shrinks the panel and clamps it inside at every position.
        for pos in [PanelPosition::UpperLeft, PanelPosition::LowerRight] {
            let n = layout(300, 400, pos);
            let p = n.panel;
            assert!(p.x >= 0 && p.x + p.width <= 300, "{pos:?}");
            assert!(p.y >= 0 && p.y + p.height <= 400, "{pos:?}");
        }
        let n = layout(300, 400, PanelPosition::LowerRight);
        assert_eq!(n.panel.x, 300 - n.panel.width, "pinned to the right edge");
        assert_eq!(field_text("hunter2", true), "\u{2022}".repeat(7));
        assert_eq!(field_text("dulrich", false), "dulrich");
        assert_eq!(field_text("ñé", true).chars().count(), 2);
    }

    #[test]
    fn layout_label_names_the_active_layout_only_with_two_or_more() {
        let two = vec!["us".to_string(), "es".to_string()];
        assert_eq!(layout_label(&two, 0).as_deref(), Some("us"));
        assert_eq!(layout_label(&two, 1).as_deref(), Some("es"));
        assert_eq!(
            layout_label(&two, 2),
            None,
            "an out-of-range group draws nothing"
        );
        assert_eq!(layout_label(&["us".to_string()], 0), None);
        assert_eq!(layout_label(&[], 0), None);
    }
    #[test]
    fn ellipsis_steps_every_400ms_and_cycles() {
        let at = |ms| ellipsis(std::time::Duration::from_millis(ms));
        assert_eq!(
            [at(0), at(399), at(400), at(800), at(1200), at(1600)],
            ["", "", ".", "..", "...", ""]
        );
    }

    #[test]
    fn the_border_rings_the_panel_edge() {
        let p = Rect::new(10, 20, 100, 50);
        let [t, b, l, r] = border_rects(p, BORDER_W);
        assert_eq!(t, Rect::new(10, 20, 100, 2));
        assert_eq!(b, Rect::new(10, 68, 100, 2));
        assert_eq!(l, Rect::new(10, 22, 2, 46));
        assert_eq!(r, Rect::new(108, 22, 2, 46));
        assert_eq!(
            crate::palette::PANEL_BORDER,
            crucible_config::Rgb(0x4a, 0x4b, 0x4f)
        );
    }

    #[test]
    fn logout_frame_is_a_fieldless_prompt_with_the_animated_ellipsis() {
        use std::time::Duration;
        let f = logout_frame(&Config::default(), Duration::ZERO);
        assert_eq!(f.prompt, "Logging out");
        assert_eq!((f.field, f.message, f.layout), (None, None, None));
        let f = logout_frame(&Config::default(), ELLIPSIS_STEP * 3);
        assert_eq!(f.prompt, "Logging out...");
    }

    #[test]
    fn logout_palette_follows_the_greeter_block_else_the_default() {
        use crucible_config::{SurfaceAccent, SurfaceStyle};
        let none = Config::default();
        assert!(none.greeter.is_none());
        let d = GreeterConfig::default();
        let want = surface_palette(d.style, d.accent, None);
        let f = logout_frame(&none, std::time::Duration::ZERO);
        assert_eq!((f.palette, f.position), (want, d.position));
        let block = GreeterConfig {
            style: SurfaceStyle::N1Light,
            accent: SurfaceAccent::Style,
            ..GreeterConfig::default()
        };
        let cfg = Config {
            greeter: Some(block.clone()),
            ..Config::default()
        };
        let f = logout_frame(&cfg, std::time::Duration::ZERO);
        assert_eq!(f.palette, surface_palette(block.style, block.accent, None));
        assert_ne!(f.palette, want, "the block, not the default");
    }
}
