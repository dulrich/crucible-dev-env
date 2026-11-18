//! The presentation gate: holds a hand-off step (greetd `start_session`, the logout quit notice)
//! until a frame rendered after [`PresentGate::arm`] is on screen on every armed output, or until a
//! deadline. Pure: outputs are an opaque key `K`, the continuation an opaque `C` the caller runs.
//!
//! An output counts once the backend reported a frame **queued after arm** ([`note_queued`]) and
//! then its completed flip ([`note_presented`]); a flip of a frame queued before `arm` does not
//! count. A synchronous commit that is on screen when it returns reports with
//! [`note_presented_sync`]. Removed or powered-off outputs leave the armed set ([`retain`]); an
//! empty set releases. [`deadline`] releases unconditionally (the caller logs it as unpresented).
//! The continuation is handed out exactly once.
//!
//! [`note_queued`]: PresentGate::note_queued
//! [`note_presented`]: PresentGate::note_presented
//! [`note_presented_sync`]: PresentGate::note_presented_sync
//! [`retain`]: PresentGate::retain
//! [`deadline`]: PresentGate::deadline

use std::time::Duration;

/// How long a hand-off waits for presentation before it proceeds anyway.
pub const PRESENT_DEADLINE: Duration = Duration::from_millis(250);

/// What [`PresentGate::arm`] did.
#[derive(Debug)]
pub enum Arm<C> {
    /// Armed with this generation (pass it to [`PresentGate::deadline`]).
    Armed(u64),
    /// Already armed: nothing changed; the offered continuation is handed back.
    AlreadyArmed(C),
    /// The armed set was empty: released at once; run the continuation.
    Released(C),
}

/// Why the gate released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Release {
    /// Every armed output presented a post-arm frame, or left the set.
    Presented,
    /// The deadline passed first.
    Deadline,
}

struct Pending<K> {
    key: K,
    queued: bool,
}

struct Armed<K, C> {
    generation: u64,
    pending: Vec<Pending<K>>,
    continuation: C,
}

/// The gate (see the module docs).
pub struct PresentGate<K, C> {
    armed: Option<Armed<K, C>>,
    generation: u64,
}

impl<K, C> Default for PresentGate<K, C> {
    fn default() -> Self {
        Self {
            armed: None,
            generation: 0,
        }
    }
}

impl<K, C> std::fmt::Debug for PresentGate<K, C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PresentGate")
            .field("armed", &self.armed.is_some())
            .field("generation", &self.generation)
            .finish()
    }
}

impl<K: PartialEq, C> PresentGate<K, C> {
    /// Arms on `outputs` (the enabled, powered-on outputs) with `continuation`. Re-arming while
    /// armed is a no-op that hands the continuation back.
    pub fn arm(&mut self, outputs: impl IntoIterator<Item = K>, continuation: C) -> Arm<C> {
        if self.armed.is_some() {
            return Arm::AlreadyArmed(continuation);
        }
        let pending: Vec<_> = outputs
            .into_iter()
            .map(|key| Pending { key, queued: false })
            .collect();
        if pending.is_empty() {
            return Arm::Released(continuation);
        }
        self.generation += 1;
        self.armed = Some(Armed {
            generation: self.generation,
            pending,
            continuation,
        });
        Arm::Armed(self.generation)
    }

    /// Whether the gate is armed.
    #[must_use]
    pub fn is_armed(&self) -> bool {
        self.armed.is_some()
    }

    /// `output` queued a frame (rendered now, so after arm).
    pub fn note_queued(&mut self, output: &K) {
        if let Some(a) = &mut self.armed
            && let Some(p) = a.pending.iter_mut().find(|p| p.key == *output)
        {
            p.queued = true;
        }
    }

    /// `output`'s page flip completed: it counts iff a post-arm frame was queued.
    pub fn note_presented(&mut self, output: &K) -> Option<C> {
        let a = self.armed.as_mut()?;
        a.pending.retain(|p| !(p.key == *output && p.queued));
        self.release_if_empty()
    }

    /// `output` showed a post-arm frame synchronously (no flip follows).
    pub fn note_presented_sync(&mut self, output: &K) -> Option<C> {
        self.retain(|k| k != output)
    }

    /// Keeps only the armed outputs for which `keep` holds (drop removed / powered-off ones).
    pub fn retain(&mut self, mut keep: impl FnMut(&K) -> bool) -> Option<C> {
        let a = self.armed.as_mut()?;
        a.pending.retain(|p| keep(&p.key));
        self.release_if_empty()
    }

    /// The deadline of arm `generation` passed: releases if that arm is still pending (a stale
    /// timer, or one after a presented release, is a no-op).
    pub fn deadline(&mut self, generation: u64) -> Option<C> {
        if self.armed.as_ref()?.generation != generation {
            return None;
        }
        self.armed.take().map(|a| a.continuation)
    }

    fn release_if_empty(&mut self) -> Option<C> {
        if self.armed.as_ref()?.pending.is_empty() {
            self.armed.take().map(|a| a.continuation)
        } else {
            None
        }
    }
}

// --- Compositor glue (the only part that knows outputs and the state) ---

/// A continuation run on release, with the compositor state.
pub type GateContinuation<B> = Box<dyn FnOnce(&mut crate::state::CrucibleState<B>)>;
/// The gate as the state holds it.
pub type StateGate<B> = PresentGate<smithay::output::Output, GateContinuation<B>>;

impl<B: crate::state::Backend + 'static> crate::state::CrucibleState<B> {
    /// Arms the gate on every enabled, powered-on output with `then`, redraws them and starts the
    /// [`PRESENT_DEADLINE`] timer. `what` names the hand-off in the logs. Returns `false` (and
    /// drops `then`) when the gate is already armed; with no lit output `then` runs at once.
    pub(crate) fn arm_present_gate(
        &mut self,
        what: &'static str,
        then: GateContinuation<B>,
    ) -> bool {
        let outputs: Vec<_> = self
            .space
            .outputs()
            .filter(|o| !self.backend_data.output_powered_off(o))
            .cloned()
            .collect();
        match self.present_gate.arm(outputs.iter().cloned(), then) {
            Arm::AlreadyArmed(_) => false,
            Arm::Released(then) => {
                tracing::info!(what, "present gate: no lit output; proceeding");
                then(self);
                true
            }
            Arm::Armed(generation) => {
                tracing::info!(what, outputs = outputs.len(), "present gate: armed");
                for o in &outputs {
                    B::schedule_render(self, o);
                }
                let timer =
                    smithay::reexports::calloop::timer::Timer::from_duration(PRESENT_DEADLINE);
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
        }
    }

    fn present_gate_run(&mut self, then: Option<GateContinuation<B>>) {
        if let Some(then) = then {
            tracing::info!("present gate: released (presented)");
            then(self);
        }
    }

    /// Backend report: `output` queued a frame (udev, beside the lock's `note_queued`).
    pub(crate) fn present_gate_queued(&mut self, output: &smithay::output::Output) {
        self.present_gate.note_queued(output);
    }

    /// Backend report: `output`'s page flip completed (udev `frame_finish`).
    pub(crate) fn present_gate_flipped(&mut self, output: &smithay::output::Output) {
        let then = self.present_gate.note_presented(output);
        self.present_gate_run(then);
    }

    /// Backend report: `output` showed a post-arm frame without a flip (the udev adoption commit,
    /// a winit submit).
    pub(crate) fn present_gate_presented_now(&mut self, output: &smithay::output::Output) {
        let then = self.present_gate.note_presented_sync(output);
        self.present_gate_run(then);
    }

    /// Drops removed and powered-off outputs from the armed set (after output removal and idle
    /// blanking).
    pub(crate) fn present_gate_prune(&mut self) {
        if !self.present_gate.is_armed() {
            return;
        }
        let mapped: Vec<_> = self.space.outputs().cloned().collect();
        let then = self
            .present_gate
            .retain(|o| mapped.contains(o) && !self.backend_data.output_powered_off(o));
        self.present_gate_run(then);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn armed(outputs: &[u8]) -> (PresentGate<u8, &'static str>, u64) {
        let mut g = PresentGate::default();
        let Arm::Armed(generation) = g.arm(outputs.iter().copied(), "go") else {
            panic!("armed");
        };
        (g, generation)
    }

    #[test]
    fn releases_once_every_output_presented_a_post_arm_frame() {
        let (mut g, _) = armed(&[1, 2]);
        g.note_queued(&1);
        assert_eq!(g.note_presented(&1), None, "output 2 still pending");
        assert!(g.is_armed());
        g.note_queued(&2);
        assert_eq!(g.note_presented(&2), Some("go"));
        assert!(!g.is_armed());
        assert_eq!(g.note_presented(&2), None, "never releases twice");
    }

    #[test]
    fn a_flip_of_a_pre_arm_frame_does_not_count() {
        let (mut g, _) = armed(&[1]);
        // The flip completes for a frame queued before arm: no `note_queued` since.
        assert_eq!(g.note_presented(&1), None);
        g.note_queued(&9); // an unarmed output is ignored
        assert_eq!(g.note_presented(&1), None);
        g.note_queued(&1);
        assert_eq!(g.note_presented(&1), Some("go"));
    }

    #[test]
    fn removal_or_power_off_leaves_the_set_and_empty_releases() {
        let (mut g, _) = armed(&[1, 2]);
        assert_eq!(g.retain(|k| *k != 2), None, "output 2 removed");
        assert_eq!(g.retain(|_| false), Some("go"), "output 1 powered off");
        let mut g: PresentGate<u8, _> = PresentGate::default();
        assert!(matches!(g.arm([], "go"), Arm::Released("go")));
        assert!(!g.is_armed());
    }

    #[test]
    fn sync_presentation_counts_without_a_flip() {
        let (mut g, _) = armed(&[1]);
        assert_eq!(g.note_presented_sync(&1), Some("go"));
    }

    #[test]
    fn deadline_releases_and_is_a_no_op_after_release() {
        let (mut g, generation) = armed(&[1]);
        assert_eq!(g.deadline(generation + 1), None, "a stale timer");
        assert_eq!(g.deadline(generation), Some("go"));
        assert_eq!(g.deadline(generation), None);
        let (mut g, generation) = armed(&[1]);
        g.note_queued(&1);
        assert_eq!(g.note_presented(&1), Some("go"));
        assert_eq!(g.deadline(generation), None, "deadline after presented");
    }

    #[test]
    fn re_arming_while_armed_is_a_no_op() {
        let (mut g, generation) = armed(&[1]);
        assert!(matches!(g.arm([2], "again"), Arm::AlreadyArmed("again")));
        g.note_queued(&2);
        assert_eq!(g.note_presented(&2), None, "the set is unchanged");
        assert_eq!(g.deadline(generation), Some("go"), "the first continuation");
    }
}
