//! Pure planning of connector/output transitions for the `udev` backend.
//!
//! Everything a connector connect/disconnect changes about CDE's *output topology* is decided
//! here, on plain data: an opaque key per output plus its width and current origin. No DRM,
//! GBM, EGL, or Smithay type appears in this module, so every transition the backend can undergo
//! is unit-testable without a GPU, a seat, or a `/dev/dri` node.
//!
//! **The layout rule.** CDE lays outputs out **left-to-right on the top row**: an output's left
//! edge is the sum of the widths of every output to its left, and its `y` is always
//! [`TOP_ROW_Y`]. A connect appends at the right edge (so no existing output moves); a
//! disconnect closes the gap, which reflows every survivor to the right of the departed output.
//!
//! The caller ([`super::output`], [`super::device`]) turns a plan into Smithay/DRM calls; it never
//! recomputes positions itself.

/// The single row every output is aligned to. CDE has no vertical output stacking.
const TOP_ROW_Y: i32 = 0;

/// One output as it is mapped *right now*, from the planner's point of view.
///
/// `K` is the caller's opaque identity for the output (the backend uses
/// [`UdevOutputId`](super::output::UdevOutputId); tests use integers). Heights are absent on
/// purpose: the strip is top-aligned, so only widths affect placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MappedOutput<K> {
    /// Opaque identity of the output.
    pub key: K,
    /// The output's current logical width.
    pub width: i32,
    /// Where the output is mapped at the time the plan is made.
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
/// `positions` is the authoritative post-transition strip and is what the backend applies;
/// `survivor_reflow` is the subset of it that actually moves, kept separate so the reason a
/// transition disturbs unrelated outputs is visible (and testable) rather than implied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct OutputPlan<K> {
    /// Outputs joining the strip, with the position each is mapped at.
    pub add: Vec<Placement<K>>,
    /// Outputs leaving the strip, in strip order. Only outputs that are actually mapped
    /// appear here, so asking to remove an unknown connector plans nothing.
    pub remove: Vec<K>,
    /// The full left-to-right strip after the transition, including any addition.
    pub positions: Vec<Placement<K>>,
    /// The outputs in `positions` that already existed and whose origin changed.
    pub survivor_reflow: Vec<Placement<K>>,
}

impl<K> OutputPlan<K> {
    /// True when the transition changes nothing — no device, or a request against outputs that
    /// are not mapped.
    pub(super) fn is_empty(&self) -> bool {
        self.add.is_empty() && self.remove.is_empty() && self.positions.is_empty()
    }
}

/// Plans a connector *connect*: the new output joins at the right edge of the strip.
///
/// The addition's own width is irrelevant (nothing is placed to its right), so it is not asked
/// for. The returned plan always holds exactly one `add`, and `survivor_reflow` is always empty:
/// appending never disturbs an existing output.
pub(super) fn plan_connect<K: Copy + PartialEq>(
    current: &[MappedOutput<K>],
    key: K,
) -> OutputPlan<K> {
    plan(current, Some(key), &[])
}

/// Plans a connector or device *disconnect*: `removed` leaves the strip and the survivors close
/// the gap.
///
/// Keys that are not currently mapped are ignored, which is what makes a removal of an
/// already-gone connector (or of a device that never mapped one) a no-op plan.
pub(super) fn plan_disconnect<K: Copy + PartialEq>(
    current: &[MappedOutput<K>],
    removed: &[K],
) -> OutputPlan<K> {
    plan(current, None, removed)
}

/// Plans a pure re-lay of the existing strip, with no membership change.
///
/// Used after a device rescan: a mode change can resize an output under us, which moves
/// everything to its right.
pub(super) fn plan_reflow<K: Copy + PartialEq>(current: &[MappedOutput<K>]) -> OutputPlan<K> {
    plan(current, None, &[])
}

/// The one placement rule: walk the surviving strip left to right, then append any addition.
fn plan<K: Copy + PartialEq>(
    current: &[MappedOutput<K>],
    added: Option<K>,
    removed: &[K],
) -> OutputPlan<K> {
    let mut offset_x = 0;
    let mut remove = Vec::new();
    let mut positions = Vec::with_capacity(current.len() + 1);
    let mut survivor_reflow = Vec::new();

    for output in current {
        if removed.contains(&output.key) {
            remove.push(output.key);
            continue;
        }
        let placement = Placement {
            key: output.key,
            position: (offset_x, TOP_ROW_Y),
        };
        if placement.position != output.position {
            survivor_reflow.push(placement);
        }
        positions.push(placement);
        offset_x += output.width;
    }

    let add: Vec<Placement<K>> = added
        .map(|key| Placement {
            key,
            position: (offset_x, TOP_ROW_Y),
        })
        .into_iter()
        .collect();
    positions.extend(add.iter().copied());

    OutputPlan {
        add,
        remove,
        positions,
        survivor_reflow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test outputs are identified by plain integers: the planner is generic over identity and
    /// never touches a DRM node, so no hardware (and no Smithay `Output`) is involved.
    fn mapped(key: u32, width: i32, x: i32) -> MappedOutput<u32> {
        MappedOutput {
            key,
            width,
            position: (x, 0),
        }
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
        let plan = plan_connect(&[], 1);
        assert_eq!(plan.add, vec![placed(1, 0)]);
        assert_eq!(plan.positions, vec![placed(1, 0)]);
        assert!(plan.remove.is_empty());
    }

    #[test]
    fn outputs_lay_out_left_to_right() {
        // A second output goes immediately to the right of a 1920-wide first output.
        let plan = plan_connect(&[mapped(1, 1920, 0)], 2);
        assert_eq!(plan.add, vec![placed(2, 1920)]);
        // A third output's x is the sum of the two to its left.
        let plan = plan_connect(&[mapped(1, 1920, 0), mapped(2, 1280, 1920)], 3);
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
        let plan = plan_connect(&strip, 4);
        assert!(plan.positions.iter().all(|p| p.position.1 == 0));
        assert_eq!(plan.add[0].position, (2424, 0));
    }

    #[test]
    fn connecting_never_moves_an_existing_output() {
        // Appending at the right edge is the whole point of the rule: no survivor reflows.
        let strip = [mapped(1, 1920, 0), mapped(2, 1280, 1920)];
        let plan = plan_connect(&strip, 3);
        assert!(plan.survivor_reflow.is_empty());
        assert!(plan.remove.is_empty());
        assert_eq!(plan.positions[..2], [placed(1, 0), placed(2, 1920)]);
    }

    #[test]
    fn disconnecting_the_last_output_of_a_strip_leaves_the_rest_in_place() {
        // Removing the right-most output cannot move anything: nothing is to its right.
        let strip = [mapped(1, 1920, 0), mapped(2, 1280, 1920)];
        let plan = plan_disconnect(&strip, &[2]);
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
        let plan = plan_disconnect(&strip, &[2]);
        assert_eq!(plan.remove, vec![2]);
        assert_eq!(plan.positions, vec![placed(1, 0), placed(3, 1920)]);
        // Only the output that actually moved is reported as reflowed.
        assert_eq!(plan.survivor_reflow, vec![placed(3, 1920)]);
    }

    #[test]
    fn removing_every_output_of_the_last_device_empties_the_strip() {
        // Both connectors live on the one DRM device that just went away.
        let strip = [mapped(1, 1920, 0), mapped(2, 1280, 1920)];
        let plan = plan_disconnect(&strip, &[1, 2]);
        assert_eq!(plan.remove, vec![1, 2]);
        assert!(plan.positions.is_empty());
        assert!(plan.survivor_reflow.is_empty());
        assert!(!plan.is_empty(), "a removal is still a transition");
    }

    #[test]
    fn no_device_plans_nothing() {
        // Nothing is mapped: a rescan, and a removal of a connector that was never mapped,
        // both plan zero steps.
        assert!(plan_reflow::<u32>(&[]).is_empty());
        assert!(plan_disconnect(&[], &[7]).is_empty());
        // Same for an unknown key against a live strip: only mapped outputs are planned for.
        let plan = plan_disconnect(&[mapped(1, 1920, 0)], &[7]);
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
        let plan = plan_reflow(&strip);
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
        let plan = plan_reflow(&strip);
        assert!(plan.survivor_reflow.is_empty());
        assert_eq!(plan.positions, vec![placed(1, 0), placed(2, 1920)]);
    }
}
