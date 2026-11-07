//! Snip mode, compositor side: freeze every output, route modal input to the [`Snip`] model, draw
//! the overlay, and turn the outcome into a cropped [`Capture`] for
//! [`CrucibleState::deliver_screenshot`].
//!
//! The geometry decisions are pure and tested here: [`crop_capture`], [`delivery_target`],
//! [`dim_rects`], [`ring_rects`] and [`label_origin`]. The rest is thin glue.

use crucible_layout::Rect;
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            ImportAll, ImportMem, Renderer,
            element::{
                Kind,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            },
        },
    },
    output::Output,
    utils::{Logical, Physical, Point, Rectangle, Scale, Transform},
};
use tracing::{info, warn};

use super::model::{Event, Outcome, OutputInfo, Snip, crop};
use crate::capture::Capture;
use crate::cursor::Cursor;
use crate::render::{CustomRenderElements, FrameElements, OutputRenderElements};
use crate::state::{Backend, CrucibleState};
use crate::text::TextRenderer;

/// The dim drawn over everything outside the selection (black, ~40% alpha).
pub const DIM_COLOR: [f32; 4] = [0.0, 0.0, 0.0, 0.4];
/// The selection ring thickness in physical pixels.
pub const RING_PX: i32 = 2;
/// The selection ring colour: the focus-ring accent.
pub const RING_COLOR: [f32; 4] = crate::palette::BORDER_COLOR;
/// The size label's font size (logical px).
const LABEL_PX: u32 = 14;
/// The size label's text colour.
const LABEL_RGBA: [u8; 4] = [255, 255, 255, 255];
/// The size label's backdrop.
const LABEL_BG: [f32; 4] = [0.0, 0.0, 0.0, 0.75];
/// Gap between the selection's bottom-right corner and the label, and the label's padding.
const LABEL_GAP: i32 = 4;

/// One frozen output.
#[derive(Debug)]
pub struct SnipFrame {
    /// The output it was captured from.
    pub output: Output,
    /// The output's global logical rect at capture time.
    pub rect: Rect,
    /// The output's fractional scale at capture time.
    pub scale: f64,
    /// The captured pixels (cropped at the outcome).
    pub capture: Capture,
    /// The same pixels as a render buffer, built once so the damage tracker sees one unchanged
    /// element for the whole snip.
    pub buffer: MemoryRenderBuffer,
}

/// An active snip: the selection model plus one frozen frame per output (`frames[i]` is the
/// model's output index `i`).
#[derive(Debug)]
pub struct SnipSession {
    /// The selection state machine.
    pub model: Snip,
    /// The frozen frames.
    pub frames: Vec<SnipFrame>,
}

impl SnipSession {
    /// The rect currently selected (global logical): the drag rect, else the hover window.
    fn selection(&self) -> Option<Rect> {
        self.model.drag_rect().or_else(|| self.model.hover())
    }
}

#[allow(clippy::cast_sign_loss)]
fn to_u32(v: i32) -> u32 {
    v.max(0) as u32
}

/// Copies the physical `rect` (clamped to the frame) out of `capture` into a new tightly packed
/// capture. A rect outside the frame yields a zero-sized capture.
#[must_use]
pub fn crop_capture(capture: &Capture, rect: Rect) -> Capture {
    let (fw, fh) = (
        i32::try_from(capture.width).unwrap_or(i32::MAX),
        i32::try_from(capture.height).unwrap_or(i32::MAX),
    );
    let x0 = rect.x.clamp(0, fw);
    let y0 = rect.y.clamp(0, fh);
    let x1 = rect.x.saturating_add(rect.width).clamp(x0, fw);
    let y1 = rect.y.saturating_add(rect.height).clamp(y0, fh);
    let (w, h) = (to_u32(x1 - x0), to_u32(y1 - y0));
    let stride = w * 4;
    let mut rgba = Vec::with_capacity((stride * h) as usize);
    for row in to_u32(y0)..to_u32(y1) {
        let start = (row * capture.stride + to_u32(x0) * 4) as usize;
        rgba.extend_from_slice(&capture.rgba[start..start + stride as usize]);
    }
    Capture {
        width: w,
        height: h,
        stride,
        rgba,
    }
}

/// Maps a finished selection to `(frame index, output-local physical crop)`: a window or region
/// crops its output, an output is its full capture. `frames` are each frame's `(global logical
/// rect, scale, capture size (w, h) in physical px)`. `None` for `Cancel` or when the output is
/// unknown; the crop may be zero-sized (the caller refuses it).
#[must_use]
pub fn delivery_target(
    outcome: Outcome,
    frames: &[(Rect, f64, (i32, i32))],
) -> Option<(usize, Rect)> {
    let (index, rect) = match outcome {
        Outcome::Cancel => return None,
        Outcome::Window { output, rect } | Outcome::Region { output, rect } => (output, Some(rect)),
        Outcome::Output(output) => (output, None),
    };
    let (output, scale, size) = *frames.get(index)?;
    let crop = match rect {
        Some(r) => crop(r, output, scale, size)?,
        None => Rect::new(0, 0, size.0, size.1),
    };
    Some((index, crop))
}

/// The dim strips over a `w`×`h` frame outside `selection` (output-local physical): the whole
/// frame without a selection, else up to four strips around it.
#[must_use]
pub fn dim_rects(w: i32, h: i32, selection: Option<Rect>) -> Vec<Rect> {
    let frame = Rect::new(0, 0, w, h);
    match selection.filter(|s| s.has_area()) {
        Some(s) => frame.subtract(s),
        None => vec![frame],
    }
}

/// The ring strips ([`RING_PX`] thick) just inside `selection`, so a full-output selection's ring
/// stays on screen.
#[must_use]
pub fn ring_rects(s: Rect) -> Vec<Rect> {
    s.border_strips(RING_PX)
        .into_iter()
        .filter_map(|r| r.intersection(s))
        .collect()
}

/// Where the size label (`label` = its physical `(w, h)`) goes: its top-left just below-right of
/// the selection's bottom-right corner, clamped so it stays inside the `frame` `(w, h)`.
#[must_use]
pub fn label_origin(selection: Rect, label: (i32, i32), frame: (i32, i32)) -> (i32, i32) {
    let x = selection.x + selection.width + LABEL_GAP;
    let y = selection.y + selection.height + LABEL_GAP;
    (
        x.min(frame.0 - label.0).max(0),
        y.min(frame.1 - label.1).max(0),
    )
}

fn phys(r: Rect) -> Rectangle<i32, Physical> {
    Rectangle::new((r.x, r.y).into(), (r.width, r.height).into())
}

/// The snip overlay for `output`, front-to-back: cursor → size label → ring → dim → the frozen
/// frame. `None` when `output` was not frozen by this snip (it renders normally).
#[allow(clippy::too_many_arguments)]
pub fn snip_elements<R>(
    session: &SnipSession,
    output: &Output,
    cursor_at: Option<Point<i32, Physical>>,
    text: &mut TextRenderer,
    cursor: &mut Cursor,
    renderer: &mut R,
    scale: Scale<f64>,
) -> Option<FrameElements<R>>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Send + Clone + 'static,
{
    let frame = session.frames.iter().find(|f| &f.output == output)?;
    let (fw, fh) = (
        i32::try_from(frame.capture.width).unwrap_or(i32::MAX),
        i32::try_from(frame.capture.height).unwrap_or(i32::MAX),
    );
    let selection = session
        .selection()
        .and_then(|r| crop(r, frame.rect, frame.scale, (fw, fh)))
        .filter(|r| r.has_area());

    let mut custom: Vec<CustomRenderElements<R>> = match cursor_at {
        Some(location) => cursor
            .render_elements(renderer, location, scale, 1.0)
            .into_iter()
            .map(CustomRenderElements::Pointer)
            .collect(),
        None => Vec::new(),
    };
    if let Some(sel) = selection {
        let sep = if text.has_glyph('\u{d7}') {
            '\u{d7}'
        } else {
            'x'
        };
        let label = format!("{}{sep}{}", sel.width, sel.height);
        if let Some(buf) = text.buffer(&label, LABEL_PX, LABEL_RGBA, scale) {
            let size = (buf.phys.w, buf.phys.h);
            let padded = (size.0 + 2 * LABEL_GAP, size.1 + 2 * LABEL_GAP);
            let (x, y) = label_origin(sel, padded, (fw, fh));
            let at =
                Point::<f64, Physical>::from((f64::from(x + LABEL_GAP), f64::from(y + LABEL_GAP)));
            if let Some(el) = text.element(renderer, at, &label, LABEL_PX, LABEL_RGBA, scale) {
                custom.push(CustomRenderElements::Text(el));
                custom.push(CustomRenderElements::SolidColor(crate::physical::solid(
                    phys(Rect::new(x, y, padded.0, padded.1)),
                    LABEL_BG,
                )));
            }
        }
        custom.extend(ring_rects(sel).into_iter().map(|r| {
            CustomRenderElements::SolidColor(crate::physical::solid(phys(r), RING_COLOR))
        }));
    }
    custom.extend(
        dim_rects(fw, fh, selection)
            .into_iter()
            .map(|r| CustomRenderElements::SolidColor(crate::physical::solid(phys(r), DIM_COLOR))),
    );
    let logical: smithay::utils::Size<i32, Logical> = (frame.rect.width, frame.rect.height).into();
    match MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        (0.0, 0.0),
        &frame.buffer,
        None,
        None,
        Some(logical),
        Kind::Unspecified,
    ) {
        Ok(el) => custom.push(CustomRenderElements::Text(el)),
        Err(err) => warn!(?err, "snip: frozen frame import failed"),
    }
    Some(
        custom
            .into_iter()
            .map(OutputRenderElements::Custom)
            .collect(),
    )
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// `snip`: starts snip mode, or cancels it when already snipping.
    pub(crate) fn toggle_snip(&mut self) {
        if self.snip.is_some() {
            self.end_snip();
            info!(reason = "toggled off", "snip: ended");
        } else {
            self.start_snip();
        }
    }

    /// Freezes every output and enters snip mode; any capture failure logs and does not enter.
    fn start_snip(&mut self) {
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        if outputs.is_empty() {
            warn!("snip: not started (no output)");
            return;
        }
        let mut frames = Vec::with_capacity(outputs.len());
        let mut infos = Vec::with_capacity(outputs.len());
        for output in outputs {
            let Some(geo) = self.space.output_geometry(&output) else {
                warn!(output = %output.name(), "snip: not started (output has no geometry)");
                return;
            };
            let capture = match self.capture_output(&output) {
                Ok(c) => c,
                Err(err) => {
                    warn!(output = %output.name(), %err, "snip: not started (capture failed)");
                    return;
                }
            };
            let rect = Rect::new(geo.loc.x, geo.loc.y, geo.size.w, geo.size.h);
            let scale = output.current_scale().fractional_scale();
            // `Space::elements()` yields bottom-to-top (`raise_element` appends; `element_under`
            // walks it reversed), so reverse it for topmost first.
            let windows = self
                .space
                .elements()
                .rev()
                .filter_map(|w| self.space.element_geometry(w))
                .map(|g| Rect::new(g.loc.x, g.loc.y, g.size.w, g.size.h))
                .filter(|r| r.intersects(rect))
                .collect();
            let (w, h) = (
                i32::try_from(capture.width).unwrap_or(i32::MAX),
                i32::try_from(capture.height).unwrap_or(i32::MAX),
            );
            let buffer = MemoryRenderBuffer::from_slice(
                &capture.rgba,
                Fourcc::Abgr8888,
                (w, h),
                1,
                Transform::Normal,
                Some(vec![Rectangle::from_size((w, h).into())]),
            );
            infos.push(OutputInfo {
                rect,
                scale,
                windows,
            });
            frames.push(SnipFrame {
                output,
                rect,
                scale,
                capture,
                buffer,
            });
        }
        let at = self.pointer.current_location();
        #[allow(clippy::cast_possible_truncation)]
        let pointer = (at.x.round() as i32, at.y.round() as i32);
        info!(outputs = frames.len(), "snip: started");
        self.snip = Some(SnipSession {
            model: Snip::new(infos, pointer),
            frames,
        });
        self.clear_pointer_focus();
        self.snip_redraw_all();
    }

    /// Feeds a pointer location (global logical) to the selection.
    pub(crate) fn snip_motion(&mut self, at: Point<f64, Logical>) {
        #[allow(clippy::cast_possible_truncation)]
        let p = (at.x.round() as i32, at.y.round() as i32);
        self.snip_event(Event::Motion(p));
    }

    /// Feeds one event to the selection; a finished selection ends the mode and delivers.
    pub(crate) fn snip_event(&mut self, event: Event) {
        let Some(session) = self.snip.as_mut() else {
            return;
        };
        let Some(outcome) = session.model.step(event) else {
            return;
        };
        let Some(session) = self.snip.take() else {
            return;
        };
        self.after_snip();
        let sizes: Vec<(Rect, f64, (i32, i32))> = session
            .frames
            .iter()
            .map(|f| {
                (
                    f.rect,
                    f.scale,
                    (
                        i32::try_from(f.capture.width).unwrap_or(i32::MAX),
                        i32::try_from(f.capture.height).unwrap_or(i32::MAX),
                    ),
                )
            })
            .collect();
        match delivery_target(outcome, &sizes) {
            None => info!(?outcome, "snip: ended"),
            Some((_, r)) if !r.has_area() => {
                warn!(?outcome, "snip: ended (empty crop, nothing captured)");
            }
            Some((index, r)) => {
                let capture = crop_capture(&session.frames[index].capture, r);
                info!(
                    ?outcome,
                    width = capture.width,
                    height = capture.height,
                    "snip: ended"
                );
                drop(session);
                self.deliver_screenshot(capture);
            }
        }
    }

    /// Ends snip mode with nothing captured.
    fn end_snip(&mut self) {
        if self.snip.take().is_some() {
            self.after_snip();
        }
    }

    /// Drops snip state without the focus/redraw follow-up (lock acceptance owns both).
    pub(crate) fn discard_snip(&mut self) {
        if self.snip.take().is_some() {
            info!(reason = "session locked", "snip: ended");
        }
    }

    /// The end-of-mode follow-up: pointer focus back to the surface under the pointer, redraw.
    fn after_snip(&mut self) {
        self.refresh_pointer_focus();
        self.snip_redraw_all();
    }

    fn snip_redraw_all(&mut self) {
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        for output in &outputs {
            self.backend_data.reset_buffers(output);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `w`×`h` capture whose pixel `(x, y)` is `[x, y, 7, 255]`.
    fn gradient(w: u32, h: u32) -> Capture {
        let mut rgba = Vec::new();
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&[x as u8, y as u8, 7, 255]);
            }
        }
        Capture {
            width: w,
            height: h,
            stride: w * 4,
            rgba,
        }
    }

    #[test]
    fn crop_capture_copies_rows_and_columns_at_an_offset() {
        let c = crop_capture(&gradient(10, 8), Rect::new(3, 2, 4, 3));
        assert_eq!((c.width, c.height, c.stride), (4, 3, 16));
        assert_eq!(c.rgba.len(), 48);
        assert_eq!(&c.rgba[0..4], &[3, 2, 7, 255]);
        assert_eq!(&c.rgba[12..16], &[6, 2, 7, 255]);
        assert_eq!(&c.rgba[16..20], &[3, 3, 7, 255]);
        assert_eq!(&c.rgba[44..48], &[6, 4, 7, 255]);
    }

    #[test]
    fn crop_capture_full_frame_is_identity_and_clamps() {
        let g = gradient(5, 4);
        assert_eq!(crop_capture(&g, Rect::new(0, 0, 5, 4)), g);
        assert_eq!(crop_capture(&g, Rect::new(-3, -3, 50, 50)), g);
        let empty = crop_capture(&g, Rect::new(9, 9, 2, 2));
        assert_eq!((empty.width, empty.height, empty.rgba.len()), (0, 0, 0));
    }

    fn frames() -> Vec<(Rect, f64, (i32, i32))> {
        vec![
            (Rect::new(0, 0, 100, 80), 1.0, (100, 80)),
            (Rect::new(100, 0, 200, 100), 2.0, (400, 200)),
        ]
    }

    #[test]
    fn delivery_target_for_each_outcome() {
        let f = frames();
        assert_eq!(delivery_target(Outcome::Cancel, &f), None);
        // A window on its output (scale 2, offset by the output origin).
        assert_eq!(
            delivery_target(
                Outcome::Window {
                    output: 1,
                    rect: Rect::new(110, 10, 20, 30)
                },
                &f
            ),
            Some((1, Rect::new(20, 20, 40, 60)))
        );
        // A region on the press output.
        assert_eq!(
            delivery_target(
                Outcome::Region {
                    output: 0,
                    rect: Rect::new(10, 10, 5, 5)
                },
                &f
            ),
            Some((0, Rect::new(10, 10, 5, 5)))
        );
        // A whole output is its full capture.
        assert_eq!(
            delivery_target(Outcome::Output(1), &f),
            Some((1, Rect::new(0, 0, 400, 200)))
        );
        // Unknown output.
        assert_eq!(delivery_target(Outcome::Output(5), &f), None);
        assert_eq!(
            delivery_target(
                Outcome::Window {
                    output: 9,
                    rect: Rect::new(0, 0, 1, 1)
                },
                &f
            ),
            None
        );
    }

    #[test]
    fn delivery_crops_against_the_real_capture_size() {
        // The capture is smaller than ceil(logical * scale): crops clamp to it, output is it.
        let f = vec![(Rect::new(0, 0, 100, 80), 1.0, (90, 70))];
        assert_eq!(
            delivery_target(
                Outcome::Region {
                    output: 0,
                    rect: Rect::new(50, 50, 50, 30)
                },
                &f
            ),
            Some((0, Rect::new(50, 50, 40, 20)))
        );
        assert_eq!(
            delivery_target(Outcome::Output(0), &f),
            Some((0, Rect::new(0, 0, 90, 70)))
        );
    }

    #[test]
    fn dim_covers_everything_but_the_selection() {
        assert_eq!(dim_rects(100, 50, None), vec![Rect::new(0, 0, 100, 50)]);
        let s = Rect::new(10, 5, 20, 10);
        let d = dim_rects(100, 50, Some(s));
        let area: i32 = d.iter().map(|r| r.width * r.height).sum();
        assert_eq!(area, 100 * 50 - 20 * 10);
        assert!(d.iter().all(|r| !r.intersects(s)));
        // A full-frame selection dims nothing.
        assert!(dim_rects(100, 50, Some(Rect::new(0, 0, 100, 50))).is_empty());
    }

    #[test]
    fn ring_sits_inside_the_selection() {
        let s = Rect::new(10, 10, 20, 10);
        let r = ring_rects(s);
        assert_eq!(r.len(), 4);
        let area: i32 = r.iter().map(|r| r.width * r.height).sum();
        assert_eq!(area, 20 * 10 - 16 * 6);
        assert!(r.iter().all(|r| r.intersection(s) == Some(*r)));
        assert!(ring_rects(Rect::new(0, 0, 0, 5)).is_empty());
    }

    #[test]
    fn label_goes_below_right_and_stays_on_screen() {
        let s = Rect::new(10, 10, 20, 20);
        assert_eq!(label_origin(s, (30, 12), (200, 100)), (34, 34));
        // Clamped at the bottom-right edge.
        assert_eq!(
            label_origin(Rect::new(0, 0, 200, 100), (30, 12), (200, 100)),
            (170, 88)
        );
    }
}
