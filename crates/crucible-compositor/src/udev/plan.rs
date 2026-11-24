//! Pure planning of connector/output transitions for the `udev` backend.
//!
//! Everything a connector connect/disconnect changes about CDE's *output topology* is decided
//! here, on plain data: an opaque key per output plus its connector name, logical size and
//! current origin, and the desired positions looked up by connector name. No DRM, GBM, EGL, or
//! Smithay type appears in this module, so every transition the backend can undergo is
//! unit-testable without a GPU, a seat, or a `/dev/dri` node. The planner never reads config: the
//! caller passes the desired-position map in.
//!
//! **The placement rule** (recomputed on every transition). First, every output with a desired
//! position is placed at it, in ascending connector-name order; one whose rectangle would overlap
//! an output already placed (or whose arithmetic overflows) falls back to the strip and is
//! reported in [`OutputPlan::fallbacks`]. Then every other output joins the **strip**: left to
//! right in its existing strip order (current `x` order; an addition last), starting at the
//! rightmost edge of the placed outputs (`0` with none), at `y =` [`TOP_ROW_Y`]. A positioned
//! output therefore never moves because another output went away, while a strip output may
//! reflow (including when a positioned output returns). With an empty desired map this is the
//! classic top-row strip: a connect appends at the right edge and a disconnect closes the gap.
//! All arithmetic is checked; an overflow never panics.
//!
//! Outputs that share no edge with any other output are reported in [`OutputPlan::unreachable`]
//! (the pointer only crosses between outputs that touch).
//!
//! The caller ([`super::output`], [`super::device`]) turns a plan into Smithay/DRM calls; it never
//! recomputes positions itself.

use std::collections::BTreeMap;

/// Desired output origins by connector name, in logical pixels.
pub(super) type DesiredPositions = BTreeMap<String, (i32, i32)>;

/// The `y` of every strip output (outputs without a desired position). The strip is a single
/// top-aligned row; only configured positions arrange outputs vertically.
const TOP_ROW_Y: i32 = 0;

/// One output as it is mapped *right now*, from the planner's point of view.
///
/// `K` is the caller's opaque identity for the output (the backend uses
/// [`UdevOutputId`](super::output::UdevOutputId); tests use integers).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MappedOutput<K> {
    /// Opaque identity of the output.
    pub key: K,
    /// The connector name (`DP-1`), the key of the desired-position map.
    pub name: String,
    /// The output's current logical width.
    pub width: i32,
    /// The output's current logical height.
    pub height: i32,
    /// Where the output is mapped at the time the plan is made (ignored for an addition).
    pub position: (i32, i32),
}

/// Where one output ends up once a plan is applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Placement<K> {
    /// Opaque identity of the output.
    pub key: K,
    /// The output's origin after the transition.
    pub position: (i32, i32),
}

/// The steps that realize one connector transition.
///
/// `positions` is the authoritative post-transition layout and is what the backend applies;
/// `survivor_reflow` is the subset of it that actually moves, kept separate so the reason a
/// transition disturbs unrelated outputs is visible (and testable) rather than implied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct OutputPlan<K> {
    /// The output joining the layout, with the position it is mapped at.
    pub add: Vec<Placement<K>>,
    /// Outputs leaving the layout, in input order. Only outputs that are actually mapped
    /// appear here, so asking to remove an unknown connector plans nothing.
    pub remove: Vec<K>,
    /// The full layout after the transition, in strip order (an addition last).
    pub positions: Vec<Placement<K>>,
    /// The outputs in `positions` that already existed and whose origin changed.
    pub survivor_reflow: Vec<Placement<K>>,
    /// Connector names that have a desired position but were put on the strip instead
    /// (overlap with an earlier output, or overflowing arithmetic). The caller warns.
    pub fallbacks: Vec<String>,
    /// Connector names that share no edge with any other output (only with 2+ outputs). The
    /// caller warns.
    pub unreachable: Vec<String>,
    /// Outputs left unplaced because strip arithmetic overflowed; they keep their current
    /// origin (an addition gets none). The caller warns.
    pub skipped: Vec<K>,
}

impl<K> OutputPlan<K> {
    /// True when the transition changes nothing — no device, or a request against outputs that
    /// are not mapped.
    pub(super) fn is_empty(&self) -> bool {
        self.add.is_empty() && self.remove.is_empty() && self.positions.is_empty()
    }
}

/// Plans a connector *connect*: `added` joins the layout (its `position` is ignored; its size
/// matters only when it has a desired position).
pub(super) fn plan_connect<K: Copy + PartialEq>(
    current: &[MappedOutput<K>],
    added: MappedOutput<K>,
    desired: &DesiredPositions,
) -> OutputPlan<K> {
    plan(current, Some(added), &[], desired)
}

/// Plans a connector or device *disconnect*: `removed` leaves the layout and strip outputs close
/// the gap.
///
/// Keys that are not currently mapped are ignored, which is what makes a removal of an
/// already-gone connector (or of a device that never mapped one) a no-op plan.
pub(super) fn plan_disconnect<K: Copy + PartialEq>(
    current: &[MappedOutput<K>],
    removed: &[K],
    desired: &DesiredPositions,
) -> OutputPlan<K> {
    plan(current, None, removed, desired)
}

/// Plans a pure re-lay of the existing outputs, with no membership change.
///
/// Used after a device rescan (a mode change can resize an output under us) and when the desired
/// positions change.
pub(super) fn plan_reflow<K: Copy + PartialEq>(
    current: &[MappedOutput<K>],
    desired: &DesiredPositions,
) -> OutputPlan<K> {
    plan(current, None, &[], desired)
}

/// The right and bottom edge of `output` placed at `at`, or `None` on overflow.
fn far_corner<K>(output: &MappedOutput<K>, at: (i32, i32)) -> Option<(i32, i32)> {
    Some((
        at.0.checked_add(output.width)?,
        at.1.checked_add(output.height)?,
    ))
}

/// True when the rectangles overlap with positive area.
fn overlaps(a: ((i32, i32), (i32, i32)), b: ((i32, i32), (i32, i32))) -> bool {
    a.0.0 < b.1.0 && b.0.0 < a.1.0 && a.0.1 < b.1.1 && b.0.1 < a.1.1
}

/// The one placement rule; see the module doc.
fn plan<K: Copy + PartialEq>(
    current: &[MappedOutput<K>],
    added: Option<MappedOutput<K>>,
    removed: &[K],
    desired: &DesiredPositions,
) -> OutputPlan<K> {
    let remove: Vec<K> = current
        .iter()
        .filter(|o| removed.contains(&o.key))
        .map(|o| o.key)
        .collect();
    // Surviving members in strip order (current x; the sort is stable), then the addition.
    let mut members: Vec<&MappedOutput<K>> = current
        .iter()
        .filter(|o| !removed.contains(&o.key))
        .collect();
    members.sort_by_key(|o| o.position.0);
    let survivors = members.len();
    members.extend(added.as_ref());

    // Pass 1: positioned outputs, ascending connector name.
    let mut order: Vec<usize> = (0..members.len())
        .filter(|&i| desired.contains_key(&members[i].name))
        .collect();
    order.sort_by(|&a, &b| members[a].name.cmp(&members[b].name));
    let mut slots: Vec<Option<(i32, i32)>> = vec![None; members.len()];
    let mut rects: Vec<((i32, i32), (i32, i32))> = Vec::new();
    let mut fallbacks = Vec::new();
    for i in order {
        let at = desired[&members[i].name];
        let rect = far_corner(members[i], at).map(|far| (at, far));
        match rect {
            Some(rect) if !rects.iter().any(|placed| overlaps(*placed, rect)) => {
                slots[i] = Some(at);
                rects.push(rect);
            }
            _ => fallbacks.push(members[i].name.clone()),
        }
    }

    // Pass 2: the strip, from the rightmost edge of the placed outputs.
    let mut strip_x = rects.iter().map(|r| r.1.0).max().unwrap_or(0);
    let mut skipped = Vec::new();
    let mut overflowed = false;
    for (i, member) in members.iter().enumerate() {
        if slots[i].is_some() {
            continue;
        }
        let next = strip_x.checked_add(member.width);
        match next {
            Some(next) if !overflowed => {
                slots[i] = Some((strip_x, TOP_ROW_Y));
                strip_x = next;
            }
            _ => {
                overflowed = true;
                skipped.push(member.key);
            }
        }
    }

    let mut positions = Vec::with_capacity(members.len());
    let mut survivor_reflow = Vec::new();
    let mut add = Vec::new();
    let mut laid_out: Vec<(&MappedOutput<K>, (i32, i32))> = Vec::new();
    for (i, member) in members.iter().enumerate() {
        let Some(position) = slots[i] else {
            // Skipped: a survivor stays where it is, an addition gets no placement.
            if i < survivors {
                laid_out.push((member, member.position));
            }
            continue;
        };
        let placement = Placement {
            key: member.key,
            position,
        };
        if i >= survivors {
            add.push(placement);
        } else if position != member.position {
            survivor_reflow.push(placement);
        }
        positions.push(placement);
        laid_out.push((member, position));
    }

    OutputPlan {
        add,
        remove,
        positions,
        survivor_reflow,
        fallbacks,
        unreachable: unreachable_outputs(&laid_out),
        skipped,
    }
}

/// True when the half-open spans `[a0, a1)` and `[b0, b1)` share positive length.
fn spans_overlap(a0: i64, a1: i64, b0: i64, b1: i64) -> bool {
    a0 < b1 && b0 < a1
}

/// Names of outputs that share no edge (touching, with overlapping span) with any other output.
/// Empty with fewer than two outputs.
fn unreachable_outputs<K>(laid_out: &[(&MappedOutput<K>, (i32, i32))]) -> Vec<String> {
    if laid_out.len() < 2 {
        return Vec::new();
    }
    let rect = |(o, at): &(&MappedOutput<K>, (i32, i32))| {
        let (x, y) = (i64::from(at.0), i64::from(at.1));
        (x, y, x + i64::from(o.width), y + i64::from(o.height))
    };
    let adjacent = |a: (i64, i64, i64, i64), b: (i64, i64, i64, i64)| {
        ((a.2 == b.0 || b.2 == a.0) && spans_overlap(a.1, a.3, b.1, b.3))
            || ((a.3 == b.1 || b.3 == a.1) && spans_overlap(a.0, a.2, b.0, b.2))
    };
    laid_out
        .iter()
        .enumerate()
        .filter(|(i, a)| {
            !laid_out
                .iter()
                .enumerate()
                .any(|(j, b)| *i != j && adjacent(rect(a), rect(b)))
        })
        .map(|(_, (o, _))| o.name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test outputs are identified by plain integers: the planner is generic over identity and
    /// never touches a DRM node, so no hardware (and no Smithay `Output`) is involved.
    fn mapped(key: u32, width: i32, x: i32) -> MappedOutput<u32> {
        MappedOutput {
            key,
            name: format!("o{key}"),
            width,
            height: 1080,
            position: (x, 0),
        }
    }

    /// An output as `mapped` makes it but named and positioned explicitly.
    fn named(key: u32, name: &str, width: i32, at: (i32, i32)) -> MappedOutput<u32> {
        MappedOutput {
            name: name.to_string(),
            position: at,
            ..mapped(key, width, at.0)
        }
    }

    fn none() -> DesiredPositions {
        DesiredPositions::new()
    }

    fn want(entries: &[(&str, (i32, i32))]) -> DesiredPositions {
        entries.iter().map(|(n, p)| (n.to_string(), *p)).collect()
    }

    fn placed(key: u32, x: i32) -> Placement<u32> {
        Placement {
            key,
            position: (x, 0),
        }
    }

    #[test]
    fn first_output_starts_at_origin() {
        // No existing outputs -> the first output is placed at (0, 0).
        let plan = plan_connect(&[], mapped(1, 1920, 0), &none());
        assert_eq!(plan.add, vec![placed(1, 0)]);
        assert_eq!(plan.positions, vec![placed(1, 0)]);
        assert!(plan.remove.is_empty());
    }

    #[test]
    fn outputs_lay_out_left_to_right() {
        // A second output goes immediately to the right of a 1920-wide first output.
        let plan = plan_connect(&[mapped(1, 1920, 0)], mapped(2, 1280, 0), &none());
        assert_eq!(plan.add, vec![placed(2, 1920)]);
        // A third output's x is the sum of the two to its left.
        let plan = plan_connect(
            &[mapped(1, 1920, 0), mapped(2, 1280, 1920)],
            mapped(3, 100, 0),
            &none(),
        );
        assert_eq!(plan.add, vec![placed(3, 3200)]);
        assert_eq!(
            plan.positions,
            vec![placed(1, 0), placed(2, 1920), placed(3, 3200)]
        );
    }

    #[test]
    fn always_aligns_to_top_row() {
        // Outputs are stacked horizontally at y = 0 regardless of widths.
        let strip = [
            mapped(1, 800, 0),
            mapped(2, 600, 800),
            mapped(3, 1024, 1400),
        ];
        let plan = plan_connect(&strip, mapped(4, 100, 0), &none());
        assert!(plan.positions.iter().all(|p| p.position.1 == 0));
        assert_eq!(plan.add[0].position, (2424, 0));
    }

    #[test]
    fn connecting_never_moves_an_existing_output() {
        // Appending at the right edge is the whole point of the rule: no survivor reflows.
        let strip = [mapped(1, 1920, 0), mapped(2, 1280, 1920)];
        let plan = plan_connect(&strip, mapped(3, 100, 0), &none());
        assert!(plan.survivor_reflow.is_empty());
        assert!(plan.remove.is_empty());
        assert_eq!(plan.positions[..2], [placed(1, 0), placed(2, 1920)]);
    }

    #[test]
    fn disconnecting_the_last_output_of_a_strip_leaves_the_rest_in_place() {
        // Removing the right-most output cannot move anything: nothing is to its right.
        let strip = [mapped(1, 1920, 0), mapped(2, 1280, 1920)];
        let plan = plan_disconnect(&strip, &[2], &none());
        assert_eq!(plan.remove, vec![2]);
        assert_eq!(plan.positions, vec![placed(1, 0)]);
        assert!(plan.survivor_reflow.is_empty());
        assert!(plan.add.is_empty());
    }

    #[test]
    fn removing_a_middle_output_reflows_only_the_survivors_to_its_right() {
        // 1920 | 1280 | 2560 -> the middle output goes away and the third slides left onto it.
        let strip = [
            mapped(1, 1920, 0),
            mapped(2, 1280, 1920),
            mapped(3, 2560, 3200),
        ];
        let plan = plan_disconnect(&strip, &[2], &none());
        assert_eq!(plan.remove, vec![2]);
        assert_eq!(plan.positions, vec![placed(1, 0), placed(3, 1920)]);
        // Only the output that actually moved is reported as reflowed.
        assert_eq!(plan.survivor_reflow, vec![placed(3, 1920)]);
    }

    #[test]
    fn removing_every_output_of_the_last_device_empties_the_strip() {
        // Both connectors live on the one DRM device that just went away.
        let strip = [mapped(1, 1920, 0), mapped(2, 1280, 1920)];
        let plan = plan_disconnect(&strip, &[1, 2], &none());
        assert_eq!(plan.remove, vec![1, 2]);
        assert!(plan.positions.is_empty());
        assert!(plan.survivor_reflow.is_empty());
        assert!(!plan.is_empty(), "a removal is still a transition");
    }

    #[test]
    fn no_device_plans_nothing() {
        // Nothing is mapped: a rescan, and a removal of a connector that was never mapped,
        // both plan zero steps.
        assert!(plan_reflow::<u32>(&[], &none()).is_empty());
        assert!(plan_disconnect(&[], &[7], &none()).is_empty());
        // Same for an unknown key against a live strip: only mapped outputs are planned for.
        let plan = plan_disconnect(&[mapped(1, 1920, 0)], &[7], &none());
        assert!(plan.remove.is_empty());
        assert_eq!(plan.positions, vec![placed(1, 0)]);
        assert!(plan.survivor_reflow.is_empty());
    }

    #[test]
    fn reflow_absorbs_a_mode_change_that_resized_an_output() {
        // The first output came back at 2560 wide; everything to its right must shift.
        let strip = [
            mapped(1, 2560, 0),
            mapped(2, 1280, 1920),
            mapped(3, 1024, 3200),
        ];
        let plan = plan_reflow(&strip, &none());
        assert!(plan.add.is_empty() && plan.remove.is_empty());
        assert_eq!(
            plan.positions,
            vec![placed(1, 0), placed(2, 2560), placed(3, 3840)]
        );
        assert_eq!(
            plan.survivor_reflow,
            vec![placed(2, 2560), placed(3, 3840)],
            "the resized output stays put; the two to its right move"
        );
    }

    #[test]
    fn a_settled_strip_reflows_nothing() {
        // Re-planning an already-correct strip is a no-op, which is what makes the backend's
        // post-rescan re-assert safe to run on every device change.
        let strip = [mapped(1, 1920, 0), mapped(2, 1280, 1920)];
        let plan = plan_reflow(&strip, &none());
        assert!(plan.survivor_reflow.is_empty());
        assert_eq!(plan.positions, vec![placed(1, 0), placed(2, 1920)]);
    }

    #[test]
    fn a_configured_output_returns_to_its_slot_after_a_replug() {
        // The user's report: A (left) and B (right) are configured; A disconnects, then returns.
        let desired = want(&[("A", (0, 0)), ("B", (1920, 0))]);
        let a = named(1, "A", 1920, (0, 0));
        let b = named(2, "B", 1920, (1920, 0));
        // B stays at its configured x when A goes away.
        let plan = plan_disconnect(&[a.clone(), b.clone()], &[1], &desired);
        assert_eq!(plan.positions, vec![placed(2, 1920)]);
        assert!(plan.survivor_reflow.is_empty());
        // A reconnects: it lands at x = 0, not appended at the right edge.
        let plan = plan_connect(&[b], named(1, "A", 1920, (0, 0)), &desired);
        assert_eq!(plan.add, vec![placed(1, 0)]);
        assert!(plan.survivor_reflow.is_empty());
        assert!(plan.fallbacks.is_empty() && plan.unreachable.is_empty());
    }

    #[test]
    fn a_configured_output_returning_next_to_an_unpositioned_one_pushes_it_onto_the_strip() {
        // U (unpositioned) was alone at 0; A is configured at 0 and returns.
        let desired = want(&[("A", (0, 0))]);
        let u = named(2, "U", 1280, (0, 0));
        let plan = plan_connect(&[u], named(1, "A", 1920, (0, 0)), &desired);
        assert_eq!(plan.add, vec![placed(1, 0)]);
        assert_eq!(plan.survivor_reflow, vec![placed(2, 1920)]);
        // The transition's final positions are consistent: one rect after the other, no overlap.
        assert_eq!(plan.positions, vec![placed(2, 1920), placed(1, 0)]);
        assert!(plan.unreachable.is_empty());
        // Replanning from the result is a fixed point.
        let settled = [named(2, "U", 1280, (1920, 0)), named(1, "A", 1920, (0, 0))];
        let again = plan_reflow(&settled, &desired);
        assert!(again.survivor_reflow.is_empty());
    }

    #[test]
    fn overlapping_configured_positions_resolve_by_connector_name() {
        let desired = want(&[("A", (0, 0)), ("B", (960, 0))]);
        // Listed in reverse name order on purpose: name order, not input order, decides.
        let current = [named(2, "B", 1920, (960, 0))];
        let plan = plan_connect(&current, named(1, "A", 1920, (0, 0)), &desired);
        assert_eq!(plan.fallbacks, vec!["B".to_string()]);
        assert_eq!(plan.add, vec![placed(1, 0)]);
        // B joins the strip at A's right edge.
        assert_eq!(plan.survivor_reflow, vec![placed(2, 1920)]);
    }

    #[test]
    fn a_strip_output_never_overlaps_a_positioned_one() {
        // A is configured far to the right and tall; strip outputs start after its right edge.
        let desired = want(&[("A", (500, 0))]);
        let current = [named(2, "U", 800, (0, 0)), named(3, "V", 800, (800, 0))];
        let plan = plan_connect(&current, named(1, "A", 1000, (0, 0)), &desired);
        assert_eq!(
            plan.positions,
            vec![placed(2, 1500), placed(3, 2300), placed(1, 500)]
        );
        // Unconfigured outputs are never moved onto the placed rectangle.
        for p in plan.positions.iter().filter(|p| p.key != 1) {
            assert!(p.position.0 >= 1500);
        }
    }

    #[test]
    fn overflowing_arithmetic_does_not_panic() {
        let desired = want(&[("A", (i32::MAX, 0))]);
        let current = [named(2, "U", i32::MAX, (0, 0))];
        // A's rect overflows: it falls back to the strip, and the strip then overflows too.
        let plan = plan_connect(&current, named(1, "A", 10, (0, 0)), &desired);
        assert_eq!(plan.fallbacks, vec!["A".to_string()]);
        assert_eq!(plan.skipped, vec![1]);
        assert!(plan.add.is_empty());
        assert_eq!(plan.positions, vec![placed(2, 0)]);
    }

    #[test]
    fn outputs_sharing_no_edge_are_reported_unreachable() {
        let desired = want(&[("A", (0, 0)), ("B", (5000, 0)), ("C", (1920, 0))]);
        let current = [named(1, "A", 1920, (0, 0)), named(3, "C", 1920, (1920, 0))];
        let plan = plan_connect(&current, named(2, "B", 1920, (0, 0)), &desired);
        assert_eq!(plan.unreachable, vec!["B".to_string()]);
        // A vertical neighbour sharing an edge span is reachable; a corner touch is not.
        let current = [named(1, "A", 1920, (0, 0))];
        let below = want(&[("A", (0, 0)), ("B", (0, 1080))]);
        let ok = plan_connect(&current, named(2, "B", 1920, (0, 0)), &below);
        assert!(ok.unreachable.is_empty());
        let diagonal = want(&[("A", (0, 0)), ("B", (1920, 1080))]);
        let corner = plan_connect(&current, named(2, "B", 1920, (0, 0)), &diagonal);
        assert_eq!(corner.unreachable, vec!["A".to_string(), "B".to_string()]);
        // A lone output is never unreachable.
        assert!(
            plan_connect(&[], mapped(1, 100, 0), &none())
                .unreachable
                .is_empty()
        );
    }
}
