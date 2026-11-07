//! Pure snip selection state machine.
//!
//! Everything here is plain integer geometry in **global logical coordinates**, using
//! [`crucible_layout::Rect`] (`x`, `y`, `width`, `height`, all `i32`). Points are `(i32, i32)`
//! logical. Output scale is an `f64` (fractional scales exist); only [`crop`] and
//! [`full_frame`] use it and they return physical, output-local pixel rects.
//!
//! A press arms a drag at the press point. The drag becomes real once the pointer has moved at
//! least [`DRAG_THRESHOLD_PX`] from the press point, measured **Euclidean** (as the header drag in
//! `header.rs:337` does). Unlike that precedent, which treats exactly 5 px as still a click
//! (`hypot <= 5`), this follows the plan's wording: 5 px or more is a drag.

use crucible_layout::Rect;

/// Pointer travel (Euclidean, logical px) at which a press becomes a drag.
pub const DRAG_THRESHOLD_PX: i32 = 5;

/// One output: its id, its global logical rect, its scale, and its windows topmost first.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputInfo {
    /// Plain index identifying the output; returned in [`Outcome::Output`].
    pub id: usize,
    /// Global logical rect of the output.
    pub rect: Rect,
    /// Output scale (physical pixels per logical pixel).
    pub scale: f64,
    /// Window rects (global logical) in stacking order, topmost first.
    pub windows: Vec<Rect>,
}

/// An input event for the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Pointer moved to a global logical point.
    Motion((i32, i32)),
    /// Primary button pressed at the current pointer.
    Press,
    /// Primary button released.
    Release,
    /// `Enter`: accept the hover target, else the pointer's output.
    Enter,
    /// `Esc`: cancel.
    Esc,
}

/// The result of a finished selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing is captured.
    Cancel,
    /// Capture this window rect (global logical).
    Window(Rect),
    /// Capture this region (global logical, normalised, clamped to its output, non-empty).
    Region(Rect),
    /// Capture the whole output with this id.
    Output(usize),
}

#[derive(Debug, Clone, Copy)]
struct Armed {
    origin: (i32, i32),
    output: usize,
    dragging: bool,
}

/// The selection state machine.
#[derive(Debug, Clone)]
pub struct Snip {
    outputs: Vec<OutputInfo>,
    pointer: (i32, i32),
    /// Index into `outputs` of the output the pointer is on (last known when between outputs).
    pointer_output: Option<usize>,
    armed: Option<Armed>,
}

fn contains(r: Rect, p: (i32, i32)) -> bool {
    p.0 >= r.x && p.1 >= r.y && p.0 - r.x < r.width && p.1 - r.y < r.height
}

fn intersect(a: Rect, b: Rect) -> Rect {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = (a.x + a.width).min(b.x + b.width);
    let y1 = (a.y + a.height).min(b.y + b.height);
    Rect::new(x0, y0, (x1 - x0).max(0), (y1 - y0).max(0))
}

fn normalised(a: (i32, i32), b: (i32, i32)) -> Rect {
    Rect::new(
        a.0.min(b.0),
        a.1.min(b.1),
        (a.0 - b.0).abs(),
        (a.1 - b.1).abs(),
    )
}

impl Snip {
    /// Starts a selection with the pointer at `pointer`.
    #[must_use]
    pub fn new(outputs: Vec<OutputInfo>, pointer: (i32, i32)) -> Self {
        let mut s = Self {
            outputs,
            pointer,
            pointer_output: None,
            armed: None,
        };
        s.pointer_output = s.output_at(pointer);
        s
    }

    fn output_at(&self, p: (i32, i32)) -> Option<usize> {
        self.outputs.iter().position(|o| contains(o.rect, p))
    }

    /// Current pointer position (global logical).
    #[must_use]
    pub fn pointer(&self) -> (i32, i32) {
        self.pointer
    }

    /// Topmost window rect under the pointer on the pointer's output, if any.
    #[must_use]
    pub fn hover(&self) -> Option<Rect> {
        let out = &self.outputs[self.pointer_output?];
        out.windows
            .iter()
            .copied()
            .find(|w| contains(*w, self.pointer))
    }

    /// Id of the output a real drag is pinned to (`None` when no drag is in progress).
    #[must_use]
    pub fn drag_output(&self) -> Option<usize> {
        self.armed
            .filter(|a| a.dragging)
            .map(|a| self.outputs[a.output].id)
    }

    /// The current drag rect (normalised, clamped to the press output), once a real drag began.
    #[must_use]
    pub fn drag_rect(&self) -> Option<Rect> {
        let a = self.armed.filter(|a| a.dragging)?;
        Some(intersect(
            normalised(a.origin, self.pointer),
            self.outputs[a.output].rect,
        ))
    }

    /// Feeds one event; returns the outcome if the selection finished.
    pub fn step(&mut self, event: Event) -> Option<Outcome> {
        match event {
            Event::Motion(p) => {
                self.pointer = p;
                if let Some(i) = self.output_at(p) {
                    self.pointer_output = Some(i);
                }
                if let Some(a) = self.armed.as_mut() {
                    let dx = i64::from(p.0 - a.origin.0);
                    let dy = i64::from(p.1 - a.origin.1);
                    let t = i64::from(DRAG_THRESHOLD_PX);
                    if dx * dx + dy * dy >= t * t {
                        a.dragging = true;
                    }
                }
                None
            }
            Event::Press => {
                self.armed = self.pointer_output.map(|output| Armed {
                    origin: self.pointer,
                    output,
                    dragging: false,
                });
                None
            }
            Event::Release => {
                let a = self.armed.take()?;
                if a.dragging {
                    let r = intersect(
                        normalised(a.origin, self.pointer),
                        self.outputs[a.output].rect,
                    );
                    if r.width == 0 || r.height == 0 {
                        Some(Outcome::Cancel)
                    } else {
                        Some(Outcome::Region(r))
                    }
                } else {
                    self.accept()
                }
            }
            Event::Enter => self.accept(),
            Event::Esc => Some(Outcome::Cancel),
        }
    }

    /// `Window(hover)` if hovering, else `Output` of the pointer's output.
    fn accept(&self) -> Option<Outcome> {
        if let Some(w) = self.hover() {
            return Some(Outcome::Window(w));
        }
        self.pointer_output
            .map(|i| Outcome::Output(self.outputs[i].id))
    }
}

fn valid_scale(scale: f64) -> bool {
    scale.is_finite() && scale > 0.0
}

#[allow(clippy::cast_possible_truncation)]
fn to_i32(v: f64) -> i32 {
    v as i32
}

/// Physical frame size of an output: `ceil(logical * scale)` per axis.
fn frame_size(output: Rect, scale: f64) -> (i32, i32) {
    (
        to_i32((f64::from(output.width) * scale).ceil()),
        to_i32((f64::from(output.height) * scale).ceil()),
    )
}

/// The whole output frame in physical pixels (the crop for [`Outcome::Output`]).
/// `None` for a non-positive or non-finite `scale`.
#[must_use]
pub fn full_frame(output: Rect, scale: f64) -> Option<Rect> {
    if !valid_scale(scale) {
        return None;
    }
    let (w, h) = frame_size(output, scale);
    Some(Rect::new(0, 0, w, h))
}

/// Converts a global logical `rect` to an output-local physical rect: origin floored, far edge
/// ceiled (outward rounding), then clamped to the output's frame. `None` for an invalid `scale`.
/// A rect entirely outside the frame yields a zero-sized rect.
#[must_use]
pub fn crop(rect: Rect, output: Rect, scale: f64) -> Option<Rect> {
    if !valid_scale(scale) {
        return None;
    }
    let (fw, fh) = frame_size(output, scale);
    let lx0 = f64::from(rect.x - output.x) * scale;
    let ly0 = f64::from(rect.y - output.y) * scale;
    let lx1 = f64::from(rect.x - output.x + rect.width) * scale;
    let ly1 = f64::from(rect.y - output.y + rect.height) * scale;
    let x0 = to_i32(lx0.floor()).clamp(0, fw);
    let y0 = to_i32(ly0.floor()).clamp(0, fh);
    let x1 = to_i32(lx1.ceil()).clamp(x0, fw);
    let y1 = to_i32(ly1.ceil()).clamp(y0, fh);
    Some(Rect::new(x0, y0, x1 - x0, y1 - y0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(id: usize, rect: Rect, windows: Vec<Rect>) -> OutputInfo {
        OutputInfo {
            id,
            rect,
            scale: 1.0,
            windows,
        }
    }

    fn two() -> Snip {
        let o0 = Rect::new(0, 0, 1000, 800);
        let o1 = Rect::new(1000, 0, 800, 600);
        Snip::new(
            vec![
                out(
                    0,
                    o0,
                    vec![Rect::new(100, 100, 300, 300), Rect::new(0, 0, 500, 500)],
                ),
                out(1, o1, vec![Rect::new(1100, 50, 200, 200)]),
            ],
            (900, 700),
        )
    }

    fn mv(s: &mut Snip, x: i32, y: i32) {
        assert_eq!(s.step(Event::Motion((x, y))), None);
    }

    #[test]
    fn hover_topmost_and_none_on_background() {
        let mut s = two();
        assert_eq!(s.hover(), None);
        mv(&mut s, 150, 150);
        assert_eq!(s.hover(), Some(Rect::new(100, 100, 300, 300)));
        mv(&mut s, 450, 450);
        assert_eq!(s.hover(), Some(Rect::new(0, 0, 500, 500)));
        mv(&mut s, 1150, 100);
        assert_eq!(s.hover(), Some(Rect::new(1100, 50, 200, 200)));
        assert_eq!(s.pointer(), (1150, 100));
    }

    #[test]
    fn threshold_four_is_click_five_is_drag() {
        let mut s = two();
        mv(&mut s, 600, 600);
        s.step(Event::Press);
        mv(&mut s, 604, 600);
        assert_eq!(s.drag_rect(), None);
        assert_eq!(s.step(Event::Release), Some(Outcome::Output(0)));
        mv(&mut s, 600, 600);
        s.step(Event::Press);
        mv(&mut s, 605, 600);
        assert_eq!(s.drag_rect(), Some(Rect::new(600, 600, 5, 0)));
        assert_eq!(s.drag_output(), Some(0));
        mv(&mut s, 605, 603);
        assert_eq!(
            s.step(Event::Release),
            Some(Outcome::Region(Rect::new(600, 600, 5, 3)))
        );
    }

    #[test]
    fn threshold_is_euclidean() {
        let mut s = two();
        mv(&mut s, 600, 600);
        s.step(Event::Press);
        mv(&mut s, 603, 604);
        assert!(s.drag_rect().is_some());
    }

    #[test]
    fn drag_clamps_to_press_output() {
        let mut s = two();
        mv(&mut s, 900, 700);
        s.step(Event::Press);
        mv(&mut s, 1200, 900);
        assert_eq!(s.drag_rect(), Some(Rect::new(900, 700, 100, 100)));
        assert_eq!(s.drag_output(), Some(0));
        assert_eq!(
            s.step(Event::Release),
            Some(Outcome::Region(Rect::new(900, 700, 100, 100)))
        );
    }

    #[test]
    fn drag_normalised_any_corner() {
        for (end, want) in [
            ((300, 300), Rect::new(300, 300, 100, 100)),
            ((500, 300), Rect::new(400, 300, 100, 100)),
            ((300, 500), Rect::new(300, 400, 100, 100)),
            ((500, 500), Rect::new(400, 400, 100, 100)),
        ] {
            let mut s = two();
            mv(&mut s, 400, 400);
            s.step(Event::Press);
            mv(&mut s, end.0, end.1);
            assert_eq!(s.drag_rect(), Some(want));
            assert_eq!(s.step(Event::Release), Some(Outcome::Region(want)));
        }
    }

    #[test]
    fn outcomes() {
        let mut s = two();
        assert_eq!(s.step(Event::Esc), Some(Outcome::Cancel));
        // Window via click.
        mv(&mut s, 150, 150);
        s.step(Event::Press);
        assert_eq!(
            s.step(Event::Release),
            Some(Outcome::Window(Rect::new(100, 100, 300, 300)))
        );
        // Window via Enter.
        assert_eq!(
            s.step(Event::Enter),
            Some(Outcome::Window(Rect::new(100, 100, 300, 300)))
        );
        // Output via Enter on background of output 1.
        mv(&mut s, 1700, 500);
        assert_eq!(s.step(Event::Enter), Some(Outcome::Output(1)));
        // Output via background click.
        s.step(Event::Press);
        assert_eq!(s.step(Event::Release), Some(Outcome::Output(1)));
        // Release without press does nothing.
        assert_eq!(s.step(Event::Release), None);
    }

    #[test]
    fn zero_area_region_cancels() {
        let mut s = two();
        mv(&mut s, 600, 600);
        s.step(Event::Press);
        mv(&mut s, 700, 600);
        assert_eq!(s.step(Event::Release), Some(Outcome::Cancel));
    }

    #[test]
    fn crop_scale_one_and_two() {
        let o = Rect::new(0, 0, 100, 50);
        assert_eq!(
            crop(Rect::new(10, 5, 20, 10), o, 1.0),
            Some(Rect::new(10, 5, 20, 10))
        );
        assert_eq!(
            crop(Rect::new(10, 5, 20, 10), o, 2.0),
            Some(Rect::new(20, 10, 40, 20))
        );
        assert_eq!(
            crop(Rect::new(11, 7, 3, 3), o, 2.0),
            Some(Rect::new(22, 14, 6, 6))
        );
        assert_eq!(full_frame(o, 2.0), Some(Rect::new(0, 0, 200, 100)));
    }

    #[test]
    fn crop_rounds_outward_fractional() {
        let o = Rect::new(0, 0, 100, 100);
        // 1.5x: x 1..2 -> 1.5..3.0 -> floor 1, ceil 3.
        assert_eq!(
            crop(Rect::new(1, 1, 1, 1), o, 1.5),
            Some(Rect::new(1, 1, 2, 2))
        );
    }

    #[test]
    fn crop_clamps_far_edge() {
        let o = Rect::new(0, 0, 100, 50);
        assert_eq!(
            crop(Rect::new(90, 40, 50, 50), o, 2.0),
            Some(Rect::new(180, 80, 20, 20))
        );
        assert_eq!(
            crop(Rect::new(-10, -10, 20, 20), o, 1.0),
            Some(Rect::new(0, 0, 10, 10))
        );
    }

    #[test]
    fn crop_multi_output_is_local() {
        let o1 = Rect::new(1000, 200, 800, 600);
        assert_eq!(
            crop(Rect::new(1101, 251, 10, 10), o1, 2.0),
            Some(Rect::new(202, 102, 20, 20))
        );
        assert_eq!(full_frame(o1, 1.0), Some(Rect::new(0, 0, 800, 600)));
    }

    #[test]
    fn crop_rejects_bad_scale() {
        let o = Rect::new(0, 0, 10, 10);
        for s in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(crop(o, o, s), None);
            assert_eq!(full_frame(o, s), None);
        }
    }
}
