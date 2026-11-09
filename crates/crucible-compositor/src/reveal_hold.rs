//! Reveal hold (plan 38): keep an output's previous frame on screen while a just-unsuspended
//! window that dropped its content is still waiting to repaint.
//!
//! This module is the **pure** model: no Smithay types, no clock reads. Output and window keys are
//! generic, configure serials are plain `u32` (compared with `>=`; a wrap is not handled), and
//! every deadline or "now" is supplied by the caller.
//!
//! A hold on an output waits on a set of `(window, reveal serial)` entries. It ends when every
//! waited window has committed with a serial `>=` its reveal serial, when a waited window is
//! forgotten (destroyed, hidden again, closed), at its deadline, or when everything is cleared
//! (lock, snip). A second reveal on an already-held output joins the wait set but **keeps the
//! earliest deadline**, so no hold outlives its first begin plus [`REVEAL_HOLD_TIMEOUT`].

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

/// How long an output may stay held; a client that never commits is shown as-is after this.
pub const REVEAL_HOLD_TIMEOUT: Duration = Duration::from_millis(80);

/// Whether a render pass should composite an output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderAdmission {
    /// Composite and submit a frame as usual.
    Render,
    /// Held: submit nothing (the previous frame stays on screen) but still send frame callbacks.
    Skip,
}

/// Whether a committed buffer is a content-less *placeholder*. **Pure**.
///
/// Both sizes are logical (`RendererSurfaceState::buffer_size`, i.e. buffer pixels divided by the
/// buffer scale, and `surface_size`, the viewport destination when one is set). No buffer is a
/// placeholder; a buffer smaller than the surface in either dimension is too (a 1x1 buffer scaled
/// to the full tile by `wp_viewport`). A full-size buffer is not, so such clients never hold. An
/// unknown surface size cannot be judged, so a present buffer then counts as real.
#[must_use]
pub fn is_placeholder_buffer(buffer: Option<(i32, i32)>, surface: Option<(i32, i32)>) -> bool {
    match (buffer, surface) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some((bw, bh)), Some((sw, sh))) => bw < sw || bh < sh,
    }
}

#[derive(Debug)]
struct Hold<W> {
    deadline: Instant,
    waiting: Vec<(W, u32)>,
}

/// The per-output reveal holds.
#[derive(Debug)]
pub struct RevealHolds<O, W> {
    holds: HashMap<O, Hold<W>>,
}

impl<O, W> Default for RevealHolds<O, W> {
    fn default() -> Self {
        Self {
            holds: HashMap::new(),
        }
    }
}

impl<O: Hash + Eq + Clone, W: PartialEq> RevealHolds<O, W> {
    /// Begins or extends the hold on `output`, waiting for `window` to commit `serial` or newer.
    /// Returns whether this started a new hold (the caller arms a deadline timer then). An
    /// existing hold keeps its earlier deadline; a repeated window keeps its newest serial.
    pub fn begin(&mut self, output: O, window: W, serial: u32, deadline: Instant) -> bool {
        match self.holds.get_mut(&output) {
            Some(hold) => {
                hold.deadline = hold.deadline.min(deadline);
                match hold.waiting.iter_mut().find(|(w, _)| *w == window) {
                    Some(entry) => entry.1 = entry.1.max(serial),
                    None => hold.waiting.push((window, serial)),
                }
                false
            }
            None => {
                self.holds.insert(
                    output,
                    Hold {
                        deadline,
                        waiting: vec![(window, serial)],
                    },
                );
                true
            }
        }
    }

    /// Records a root commit of `window` carrying `serial`; returns the outputs released.
    pub fn commit(&mut self, window: &W, serial: u32) -> Vec<O> {
        for hold in self.holds.values_mut() {
            hold.waiting
                .retain(|(w, need)| !(w == window && serial >= *need));
        }
        self.release_where(|hold| hold.waiting.is_empty())
    }

    /// Forgets `window` (destroyed, hidden again, closed); returns the outputs released.
    pub fn forget(&mut self, window: &W) -> Vec<O> {
        for hold in self.holds.values_mut() {
            hold.waiting.retain(|(w, _)| w != window);
        }
        self.release_where(|hold| hold.waiting.is_empty())
    }

    /// Releases every hold whose deadline is at or before `now`.
    pub fn expire(&mut self, now: Instant) -> Vec<O> {
        self.release_where(|hold| now >= hold.deadline)
    }

    /// Releases every hold (lock, snip).
    pub fn clear_all(&mut self) -> Vec<O> {
        self.release_where(|_| true)
    }

    /// Whether `output` is currently held.
    #[must_use]
    pub fn is_held(&self, output: &O) -> bool {
        self.holds.contains_key(output)
    }

    /// The deadline of `output`'s hold, if held.
    #[must_use]
    pub fn deadline(&self, output: &O) -> Option<Instant> {
        self.holds.get(output).map(|hold| hold.deadline)
    }

    /// Whether any hold is waiting on a commit (a cheap pre-check for the commit handler).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.holds.is_empty()
    }

    /// The render admission for `output`: held outputs are skipped.
    #[must_use]
    pub fn admission(&self, output: &O) -> RenderAdmission {
        if self.is_held(output) {
            RenderAdmission::Skip
        } else {
            RenderAdmission::Render
        }
    }

    fn release_where(&mut self, mut released: impl FnMut(&Hold<W>) -> bool) -> Vec<O> {
        let outputs: Vec<O> = self
            .holds
            .iter()
            .filter(|(_, hold)| released(hold))
            .map(|(output, _)| output.clone())
            .collect();
        for output in &outputs {
            self.holds.remove(output);
        }
        outputs
    }
}

/// A logical rect as `(x, y, w, h)`.
pub type RectXywh = (i32, i32, i32, i32);

/// The output a just-placed window lands on: the one its rect overlaps most. **Pure**. Rects are
/// `(x, y, w, h)` in global logical space. `None` when it overlaps no output.
#[must_use]
pub fn output_for_rect<O: Clone>(outputs: &[(O, RectXywh)], rect: RectXywh) -> Option<O> {
    let overlap = |(ox, oy, ow, oh): RectXywh| {
        let (rx, ry, rw, rh) = rect;
        let w = (ox + ow).min(rx + rw) - ox.max(rx);
        let h = (oy + oh).min(ry + rh) - oy.max(ry);
        if w > 0 && h > 0 {
            i64::from(w) * i64::from(h)
        } else {
            0
        }
    };
    outputs
        .iter()
        .map(|(o, g)| (o, overlap(*g)))
        .filter(|(_, area)| *area > 0)
        .max_by_key(|(_, area)| *area)
        .map(|(o, _)| o.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    type Holds = RevealHolds<&'static str, u32>;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn placeholder_predicate() {
        // No buffer.
        assert!(is_placeholder_buffer(None, Some((800, 600))));
        // cterm: a 1x1 buffer scaled to the tile by wp_viewport.
        assert!(is_placeholder_buffer(Some((1, 1)), Some((800, 600))));
        // foot: a full-size buffer (also a HiDPI one: size is already divided by the scale).
        assert!(!is_placeholder_buffer(Some((800, 600)), Some((800, 600))));
        // Smaller in one dimension only.
        assert!(is_placeholder_buffer(Some((800, 100)), Some((800, 600))));
        // Unknown surface size with a buffer: real.
        assert!(!is_placeholder_buffer(Some((1, 1)), None));
    }

    #[test]
    fn releases_on_the_last_matching_commit() {
        let now = t0();
        let mut h = Holds::default();
        assert!(h.begin("A", 1, 10, now + REVEAL_HOLD_TIMEOUT));
        assert!(!h.begin("A", 2, 11, now + REVEAL_HOLD_TIMEOUT));
        assert!(h.commit(&1, 10).is_empty());
        assert!(h.is_held(&"A"));
        assert_eq!(h.commit(&2, 12), vec!["A"]);
        assert!(!h.is_held(&"A"));
    }

    #[test]
    fn older_serial_is_ignored() {
        let now = t0();
        let mut h = Holds::default();
        h.begin("A", 1, 10, now + REVEAL_HOLD_TIMEOUT);
        assert!(h.commit(&1, 9).is_empty());
        assert!(h.is_held(&"A"));
        assert_eq!(h.commit(&1, 10), vec!["A"]);
    }

    #[test]
    fn forget_releases() {
        let now = t0();
        let mut h = Holds::default();
        h.begin("A", 1, 10, now + REVEAL_HOLD_TIMEOUT);
        assert!(h.forget(&7).is_empty());
        assert_eq!(h.forget(&1), vec!["A"]);
    }

    #[test]
    fn deadline_releases() {
        let now = t0();
        let mut h = Holds::default();
        h.begin("A", 1, 10, now + REVEAL_HOLD_TIMEOUT);
        assert!(
            h.expire(now + REVEAL_HOLD_TIMEOUT - Duration::from_millis(1))
                .is_empty()
        );
        assert_eq!(h.expire(now + REVEAL_HOLD_TIMEOUT), vec!["A"]);
        assert!(h.is_empty());
    }

    #[test]
    fn second_reveal_adds_and_keeps_the_earliest_deadline() {
        let now = t0();
        let first = now + REVEAL_HOLD_TIMEOUT;
        let mut h = Holds::default();
        h.begin("A", 1, 10, first);
        h.begin("A", 2, 11, first + Duration::from_millis(50));
        assert_eq!(h.deadline(&"A"), Some(first));
        // Both windows are now waited on.
        assert!(h.commit(&1, 10).is_empty());
        assert_eq!(h.commit(&2, 11), vec!["A"]);
    }

    #[test]
    fn outputs_are_independent() {
        let now = t0();
        let mut h = Holds::default();
        h.begin("A", 1, 10, now + REVEAL_HOLD_TIMEOUT);
        h.begin("B", 2, 20, now + REVEAL_HOLD_TIMEOUT);
        assert_eq!(h.commit(&1, 10), vec!["A"]);
        assert!(h.is_held(&"B"));
        assert!(!h.is_held(&"A"));
    }

    #[test]
    fn clear_all_releases_everything() {
        let now = t0();
        let mut h = Holds::default();
        h.begin("A", 1, 10, now + REVEAL_HOLD_TIMEOUT);
        h.begin("B", 2, 20, now + REVEAL_HOLD_TIMEOUT);
        let mut out = h.clear_all();
        out.sort_unstable();
        assert_eq!(out, vec!["A", "B"]);
        assert!(h.is_empty());
    }

    #[test]
    fn held_output_skips_render_until_released() {
        let now = t0();
        let mut h = Holds::default();
        assert_eq!(h.admission(&"A"), RenderAdmission::Render);
        h.begin("A", 1, 10, now + REVEAL_HOLD_TIMEOUT);
        assert_eq!(h.admission(&"A"), RenderAdmission::Skip);
        assert_eq!(h.admission(&"B"), RenderAdmission::Render);
        h.expire(now + REVEAL_HOLD_TIMEOUT);
        assert_eq!(h.admission(&"A"), RenderAdmission::Render);
    }

    #[test]
    fn output_for_rect_picks_the_most_overlapped_output() {
        let outs = [("A", (0, 0, 100, 100)), ("B", (100, 0, 100, 100))];
        assert_eq!(output_for_rect(&outs, (10, 10, 50, 50)), Some("A"));
        assert_eq!(output_for_rect(&outs, (80, 0, 100, 100)), Some("B"));
        assert_eq!(output_for_rect(&outs, (500, 500, 10, 10)), None);
    }
}
