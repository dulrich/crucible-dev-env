//! The presentation gate: holds a step until a frame rendered after it was armed is on screen on
//! every output it waits on. Pure: outputs are an opaque key `K`, a continuation an opaque `C` the
//! caller runs. The gate holds several independent arms, at most one per [`ArmKind`]:
//!
//! - a [`ArmKind::Handoff`] (greetd `start_session`, the logout quit notice) waits on the outputs
//!   lit at arm time; an output that leaves the lit set leaves it for good, and a deadline
//!   releases it unconditionally (the caller logs it as unpresented);
//! - the [`ArmKind::Lock`] (the session lock's `locked` confirmation) waits on the outputs lit
//!   **now**, re-read at every evaluation: an output that appears or wakes must present too, and
//!   one that leaves forgets what it presented. It has no deadline.
//!
//! An output counts for an arm once the backend reported a frame **queued after that arm**
//! ([`note_queued`]) and then its completed flip ([`note_presented`]); a flip of a frame queued
//! before the arm does not count. A synchronous commit that is on screen when it returns reports
//! with [`note_presented_sync`]. Every report is followed by [`release`] with the current lit set
//! (enabled, powered-on outputs): it prunes and hands back every arm whose outputs all presented
//! (an empty lit set releases). Each continuation is handed out exactly once.
//!
//! [`note_queued`]: PresentGate::note_queued
//! [`note_presented`]: PresentGate::note_presented
//! [`note_presented_sync`]: PresentGate::note_presented_sync
//! [`release`]: PresentGate::release

use std::time::{Duration, Instant};

/// How long a hand-off waits for presentation before it proceeds anyway.
pub const PRESENT_DEADLINE: Duration = Duration::from_millis(250);

/// The kind of an arm: what it waits on and whether a deadline releases it (module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArmKind {
    /// A hand-off notice: the outputs lit at arm time, with [`PRESENT_DEADLINE`].
    Handoff,
    /// The session lock: every currently lit output, no deadline.
    Lock,
}

/// What [`PresentGate::arm`] did.
#[derive(Debug)]
pub enum Arm<C> {
    /// Armed with this generation (pass it to [`PresentGate::deadline`]).
    Armed(u64),
    /// An arm of this kind is pending: nothing changed; the offered continuation is handed back.
    AlreadyArmed(C),
    /// No output is lit: released at once; run the continuation.
    Released(C),
}

struct Armed<K, C> {
    kind: ArmKind,
    generation: u64,
    /// The hand-off's outputs (pruned as they leave the lit set); `None` for the live lock arm.
    snapshot: Option<Vec<K>>,
    /// Outputs that queued a frame since this arm.
    queued: Vec<K>,
    /// Outputs that presented a frame queued since this arm.
    presented: Vec<K>,
    continuation: C,
}

/// The gate (see the module docs).
pub struct PresentGate<K, C> {
    arms: Vec<Armed<K, C>>,
    generation: u64,
}

impl<K, C> Default for PresentGate<K, C> {
    fn default() -> Self {
        Self {
            arms: Vec::new(),
            generation: 0,
        }
    }
}

impl<K, C> std::fmt::Debug for PresentGate<K, C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kinds: Vec<_> = self.arms.iter().map(|a| a.kind).collect();
        f.debug_struct("PresentGate")
            .field("arms", &kinds)
            .field("generation", &self.generation)
            .finish()
    }
}

fn add<K: PartialEq + Clone>(set: &mut Vec<K>, key: &K) {
    if !set.contains(key) {
        set.push(key.clone());
    }
}

impl<K: PartialEq + Clone, C> PresentGate<K, C> {
    /// Arms a `kind` arm with `continuation`; `lit` is the enabled, powered-on outputs now. While an
    /// arm of `kind` is pending this is a no-op that hands the continuation back.
    pub fn arm(
        &mut self,
        kind: ArmKind,
        lit: impl IntoIterator<Item = K>,
        continuation: C,
    ) -> Arm<C> {
        if self.is_armed(kind) {
            return Arm::AlreadyArmed(continuation);
        }
        let lit: Vec<K> = lit.into_iter().collect();
        if lit.is_empty() {
            return Arm::Released(continuation);
        }
        self.generation += 1;
        self.arms.push(Armed {
            kind,
            generation: self.generation,
            snapshot: (kind == ArmKind::Handoff).then_some(lit),
            queued: Vec::new(),
            presented: Vec::new(),
            continuation,
        });
        Arm::Armed(self.generation)
    }

    /// Drops the pending `kind` arm, if any, handing its continuation back unrun (a replaced lock).
    pub fn disarm(&mut self, kind: ArmKind) -> Option<C> {
        let i = self.arms.iter().position(|a| a.kind == kind)?;
        Some(self.arms.remove(i).continuation)
    }

    /// Whether an arm of `kind` is pending.
    #[must_use]
    pub fn is_armed(&self, kind: ArmKind) -> bool {
        self.arms.iter().any(|a| a.kind == kind)
    }

    /// Whether no arm is pending.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.arms.is_empty()
    }

    /// `output` queued a frame (rendered now, so after every pending arm).
    pub fn note_queued(&mut self, output: &K) {
        for a in &mut self.arms {
            add(&mut a.queued, output);
        }
    }

    /// `output`'s page flip completed: it presents for each arm it queued a frame for since.
    pub fn note_presented(&mut self, output: &K) {
        for a in &mut self.arms {
            if a.queued.contains(output) {
                add(&mut a.presented, output);
            }
        }
    }

    /// `output` showed a frame rendered now synchronously (no flip follows).
    pub fn note_presented_sync(&mut self, output: &K) {
        for a in &mut self.arms {
            add(&mut a.presented, output);
        }
    }

    /// Re-evaluates every arm against `lit` (the enabled, powered-on outputs now): outputs that
    /// left forget their reports (and leave a hand-off for good), and every arm whose outputs all
    /// presented is released. Returns the released continuations, in arming order.
    pub fn release(&mut self, lit: &[K]) -> Vec<C> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.arms.len() {
            let a = &mut self.arms[i];
            for set in [&mut a.queued, &mut a.presented] {
                set.retain(|k| lit.contains(k));
            }
            if let Some(snapshot) = &mut a.snapshot {
                snapshot.retain(|k| lit.contains(k));
            }
            let members = a.snapshot.as_deref().unwrap_or(lit);
            if members.iter().all(|k| a.presented.contains(k)) {
                out.push(self.arms.remove(i).continuation);
            } else {
                i += 1;
            }
        }
        out
    }

    /// The deadline of arm `generation` passed: releases it if still pending (a stale timer, or
    /// one after a presented release, is a no-op).
    pub fn deadline(&mut self, generation: u64) -> Option<C> {
        let i = self.arms.iter().position(|a| a.generation == generation)?;
        Some(self.arms.remove(i).continuation)
    }
}

// --- Compositor glue (the only part that knows outputs and the state) ---

/// A continuation run on release, with the compositor state.
pub type GateContinuation<B> = Box<dyn FnOnce(&mut crate::state::CrucibleState<B>)>;
/// The gate as the state holds it.
pub type StateGate<B> = PresentGate<smithay::output::Output, GateContinuation<B>>;

impl<B: crate::state::Backend + 'static> crate::state::CrucibleState<B> {
    /// The enabled, powered-on outputs.
    fn lit_outputs(&self) -> Vec<smithay::output::Output> {
        self.space
            .outputs()
            .filter(|o| !self.backend_data.output_powered_off(o))
            .cloned()
            .collect()
    }

    /// Arms a `kind` arm with `then` on the lit outputs; `what` names it in the logs. A hand-off
    /// also redraws its outputs and starts the [`PRESENT_DEADLINE`] timer (the lock redraws
    /// itself and has no deadline). Returns `false` (and drops `then`) when a `kind` arm is
    /// already pending; with no lit output `then` runs at once.
    pub(crate) fn arm_present_gate(
        &mut self,
        kind: ArmKind,
        what: &'static str,
        then: GateContinuation<B>,
    ) -> bool {
        let lit = self.lit_outputs();
        let generation = match self.present_gate.arm(kind, lit.iter().cloned(), then) {
            Arm::AlreadyArmed(_) => return false,
            Arm::Released(then) => {
                tracing::info!(what, "present gate: no lit output; proceeding");
                then(self);
                return true;
            }
            Arm::Armed(generation) => generation,
        };
        tracing::info!(what, outputs = lit.len(), "present gate: armed");
        if kind == ArmKind::Lock {
            return true;
        }
        for o in &lit {
            B::schedule_render(self, o);
        }
        let timer = smithay::reexports::calloop::timer::Timer::from_duration(PRESENT_DEADLINE);
        let armed = self.handle.insert_source(timer, move |_, _, data| {
            if let Some(then) = data.present_gate.deadline(generation) {
                tracing::warn!(what, "present gate: deadline; proceeding unpresented");
                then(data);
            }
            smithay::reexports::calloop::timer::TimeoutAction::Drop
        });
        if let Err(err) = armed {
            tracing::error!(%err, what, "present gate: no deadline timer; proceeding");
            if let Some(then) = self.present_gate.deadline(generation) {
                then(self);
            }
        }
        true
    }

    /// Re-evaluates every arm against the lit outputs and runs the released continuations.
    fn present_gate_release(&mut self) {
        if self.present_gate.is_idle() {
            return;
        }
        let lit = self.lit_outputs();
        for then in self.present_gate.release(&lit) {
            tracing::info!("present gate: released (presented)");
            then(self);
        }
    }

    /// Backend report: `output` queued a frame (udev).
    pub(crate) fn present_gate_queued(&mut self, output: &smithay::output::Output) {
        self.present_gate.note_queued(output);
    }

    /// Backend report: `output`'s page flip completed (udev `frame_finish`).
    pub(crate) fn present_gate_flipped(&mut self, output: &smithay::output::Output) {
        self.present_gate.note_presented(output);
        self.present_gate_release();
    }

    /// Backend report: `output` showed a frame rendered now without a flip (the udev deferred
    /// adoption commit, a winit submit).
    pub(crate) fn present_gate_presented_now(&mut self, output: &smithay::output::Output) {
        self.present_gate.note_presented_sync(output);
        self.present_gate_release();
    }

    /// Backend report: the lit set shrank (output removal, a failed adoption, idle blanking, the
    /// flip of a powered-off output). Re-evaluates every arm, the lock's included.
    pub(crate) fn present_gate_prune(&mut self) {
        self.present_gate_release();
    }

    /// Shows the hand-off notice `text` ("Loading CDE", "Logging out"): holds idle blanking off,
    /// animates its ellipsis every [`ELLIPSIS_STEP`] until the notice clears or is replaced (the
    /// timer drops itself at its next tick), and
    /// arms a hand-off gate arm (`what` in the logs) with `then`. Returns whether the arm was
    /// taken (`false`: one was pending and `then` was dropped).
    ///
    /// [`ELLIPSIS_STEP`]: crate::greeter::panel::ELLIPSIS_STEP
    pub(crate) fn show_handoff_notice(
        &mut self,
        text: &'static str,
        what: &'static str,
        then: GateContinuation<B>,
    ) -> bool {
        use crate::greeter::panel::ELLIPSIS_STEP;
        use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
        let since = Instant::now();
        self.handoff_notice = Some(crate::greeter::panel::HandoffNotice { text, since });
        let timer =
            self.handle
                .insert_source(Timer::from_duration(ELLIPSIS_STEP), move |_, _, state| {
                    if state
                        .handoff_notice
                        .as_ref()
                        .is_some_and(|n| n.since == since)
                    {
                        state.redraw_all_outputs();
                        TimeoutAction::ToDuration(ELLIPSIS_STEP)
                    } else {
                        TimeoutAction::Drop
                    }
                });
        if let Err(err) = timer {
            tracing::warn!(%err, text, "no ellipsis timer; the hand-off notice is static");
        }
        let armed = self.arm_present_gate(ArmKind::Handoff, what, then);
        self.redraw_all_outputs();
        armed
    }

    /// Whether the "Logging out" notice shows (never true for "Loading CDE").
    #[must_use]
    pub fn logging_out(&self) -> bool {
        self.handoff_notice
            .as_ref()
            .is_some_and(|n| n.text == crate::greeter::panel::LOGOUT_TEXT)
    }

    /// Schedules a render on every mapped output.
    pub(crate) fn redraw_all_outputs(&mut self) {
        let outputs: Vec<_> = self.space.outputs().cloned().collect();
        for o in &outputs {
            B::schedule_render(self, o);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIT: &[u8] = &[1, 2];

    fn armed(outputs: &[u8]) -> (PresentGate<u8, &'static str>, u64) {
        let mut g = PresentGate::default();
        let Arm::Armed(generation) = g.arm(ArmKind::Handoff, outputs.iter().copied(), "go") else {
            panic!("armed");
        };
        (g, generation)
    }

    /// Queues and flips `output`, then evaluates against `lit`.
    fn flip(g: &mut PresentGate<u8, &'static str>, output: u8, lit: &[u8]) -> Vec<&'static str> {
        g.note_queued(&output);
        g.note_presented(&output);
        g.release(lit)
    }

    #[test]
    fn releases_once_every_output_presented_a_post_arm_frame() {
        let (mut g, _) = armed(LIT);
        assert!(flip(&mut g, 1, LIT).is_empty(), "output 2 still pending");
        assert!(g.is_armed(ArmKind::Handoff));
        assert_eq!(flip(&mut g, 2, LIT), ["go"]);
        assert!(g.is_idle());
        assert!(flip(&mut g, 2, LIT).is_empty(), "never releases twice");
    }

    #[test]
    fn a_flip_of_a_pre_arm_frame_does_not_count() {
        let (mut g, _) = armed(&[1]);
        // The flip completes for a frame queued before arm: no `note_queued` since.
        g.note_presented(&1);
        assert!(g.release(&[1]).is_empty());
        g.note_queued(&9); // an unarmed output is ignored
        g.note_presented(&1);
        assert!(g.release(&[1, 9]).is_empty());
        assert_eq!(flip(&mut g, 1, &[1, 9]), ["go"]);
    }

    #[test]
    fn removal_or_power_off_leaves_the_set_and_empty_releases() {
        let (mut g, _) = armed(LIT);
        assert!(g.release(&[1]).is_empty(), "output 2 removed");
        assert_eq!(g.release(&[]), ["go"], "output 1 powered off");
        let mut g: PresentGate<u8, _> = PresentGate::default();
        assert!(matches!(
            g.arm(ArmKind::Handoff, [], "go"),
            Arm::Released("go")
        ));
        assert!(g.is_idle());
    }

    #[test]
    fn a_handoff_keeps_its_snapshot_a_woken_output_does_not_join() {
        let (mut g, _) = armed(&[1]);
        // Output 2 appears while armed: the hand-off waits only on its snapshot.
        assert_eq!(flip(&mut g, 1, &[1, 2]), ["go"]);
        // An output pruned once does not rejoin when it wakes.
        let (mut g, _) = armed(LIT);
        assert!(g.release(&[1]).is_empty());
        assert_eq!(flip(&mut g, 1, LIT), ["go"]);
    }

    #[test]
    fn sync_presentation_counts_without_a_flip() {
        let (mut g, _) = armed(&[1]);
        g.note_presented_sync(&1);
        assert_eq!(g.release(&[1]), ["go"]);
    }

    #[test]
    fn deadline_releases_and_is_a_no_op_after_release() {
        let (mut g, generation) = armed(&[1]);
        assert_eq!(g.deadline(generation + 1), None, "a stale timer");
        assert_eq!(g.deadline(generation), Some("go"));
        assert_eq!(g.deadline(generation), None);
        let (mut g, generation) = armed(&[1]);
        assert_eq!(flip(&mut g, 1, &[1]), ["go"]);
        assert_eq!(g.deadline(generation), None, "deadline after presented");
    }

    #[test]
    fn re_arming_a_kind_while_armed_is_a_no_op() {
        let (mut g, generation) = armed(&[1]);
        assert!(matches!(
            g.arm(ArmKind::Handoff, [2], "again"),
            Arm::AlreadyArmed("again")
        ));
        assert!(flip(&mut g, 2, &[1, 2]).is_empty(), "the set is unchanged");
        assert_eq!(g.deadline(generation), Some("go"), "the first continuation");
    }

    // --- The lock arm ---

    fn lock(lit: &[u8]) -> PresentGate<u8, &'static str> {
        let mut g = PresentGate::default();
        assert!(matches!(
            g.arm(ArmKind::Lock, lit.iter().copied(), "lock"),
            Arm::Armed(_)
        ));
        g
    }

    #[test]
    fn a_handoff_and_the_lock_are_independent_arms() {
        let (mut g, generation) = armed(LIT);
        let Arm::Armed(lock_generation) = g.arm(ArmKind::Lock, LIT.iter().copied(), "lock") else {
            panic!("the lock arms beside the hand-off");
        };
        assert!(matches!(
            g.arm(ArmKind::Lock, [1], "again"),
            Arm::AlreadyArmed("again")
        ));
        // Both arms see every report; each releases on its own condition.
        assert!(flip(&mut g, 1, LIT).is_empty());
        assert_eq!(
            g.deadline(generation),
            Some("go"),
            "the hand-off's deadline"
        );
        assert!(g.is_armed(ArmKind::Lock), "the lock arm has no deadline");
        assert_eq!(g.deadline(lock_generation + 1), None);
        assert_eq!(flip(&mut g, 2, LIT), ["lock"]);
        assert!(g.is_idle());
    }

    #[test]
    fn concurrent_arms_release_on_their_own_frames() {
        // The hand-off armed first; output 1 queued; then the lock arms: output 1's flip counts
        // for the hand-off only (its frame was queued before the lock).
        let (mut g, _) = armed(&[1]);
        g.note_queued(&1);
        assert!(matches!(g.arm(ArmKind::Lock, [1], "lock"), Arm::Armed(_)));
        g.note_presented(&1);
        assert_eq!(g.release(&[1]), ["go"]);
        assert!(g.is_armed(ArmKind::Lock));
        assert_eq!(flip(&mut g, 1, &[1]), ["lock"]);
    }

    #[test]
    fn the_lock_waits_on_an_output_that_appears_or_wakes() {
        let mut g = lock(&[1]);
        g.note_queued(&1);
        // Output 2 appears (or wakes) before output 1's flip: it must present too.
        g.note_presented(&1);
        assert!(g.release(&[1, 2]).is_empty(), "output 2 has not presented");
        // Output 1 powers off and wakes: it forgets its presentation and must present again.
        assert!(g.release(&[2]).is_empty());
        g.note_presented_sync(&2);
        assert!(
            g.release(&[1, 2]).is_empty(),
            "woken output 1 presents again"
        );
        assert_eq!(flip(&mut g, 1, &[1, 2]), ["lock"]);
    }

    #[test]
    fn removing_the_last_unpresented_output_confirms_the_lock() {
        // B1: the pending lock waited on output 2; unplugging it re-evaluates and confirms.
        let mut g = lock(LIT);
        assert!(flip(&mut g, 1, LIT).is_empty());
        assert_eq!(g.release(&[1]), ["lock"]);
    }

    #[test]
    fn the_lock_confirms_when_every_lit_output_presented_and_off_ones_are_exempt() {
        // 1 presented, 2 powered off (not lit), 3 lit and not presented: pending.
        let mut g = lock(&[1, 2, 3]);
        assert!(flip(&mut g, 1, &[1, 3]).is_empty());
        // 3 also off: confirmed.
        assert_eq!(g.release(&[1]), ["lock"]);
        // An output whose power-off failed is still lit, so it must still present.
        let mut g = lock(LIT);
        assert!(flip(&mut g, 1, LIT).is_empty());
        assert!(g.release(LIT).is_empty());
        // No lit output: released at once.
        let mut g: PresentGate<u8, _> = PresentGate::default();
        assert!(matches!(
            g.arm(ArmKind::Lock, [], "lock"),
            Arm::Released("lock")
        ));
    }

    #[test]
    fn a_replaced_lock_arm_never_confirms_its_successor() {
        let mut g = lock(&[1]);
        g.note_queued(&1);
        // The pending locker died; a new one is accepted: the old arm is dropped unrun.
        assert_eq!(g.disarm(ArmKind::Lock), Some("lock"));
        assert!(matches!(
            g.arm(ArmKind::Lock, [1], "successor"),
            Arm::Armed(_)
        ));
        // The flip of the frame queued for the old arm does not count for the new one.
        g.note_presented(&1);
        assert!(g.release(&[1]).is_empty());
        assert_eq!(flip(&mut g, 1, &[1]), ["successor"]);
    }

    #[test]
    fn a_synchronous_adoption_commit_presents_the_lock() {
        // Output 1 presented by flip; output 2's deferred adoption commit (no VBlank) reports
        // synchronously and confirms.
        let mut g = lock(LIT);
        assert!(
            flip(&mut g, 1, LIT).is_empty(),
            "output 2 has shown nothing locked yet"
        );
        g.note_presented_sync(&2);
        assert_eq!(g.release(LIT), ["lock"]);
    }
}
