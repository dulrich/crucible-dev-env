//! Idle handling: auto-lock and screen blanking after inactivity, plus the `ext-idle-notify-v1`
//! and `zwp-idle-inhibit-manager-v1` glue (plan 25 WP-2).
//!
//! ## Layout
//! The decision logic is **pure** and Smithay-free (top of this module): [`IdleMachine`] is fed
//! activity events and ticks carrying an elapsed [`Duration`] (no clock, so tests never sleep) and
//! answers with an [`IdleStep`]. The compositor glue below measures time, computes which
//! inhibitors are visible, fires the locker and calls the backend power hook
//! ([`Backend::set_outputs_powered`]).
//!
//! ## Timer
//! A calloop `Timer` ticks every [`TICK`] (1 s). It runs even with idle off; an off configuration
//! makes every tick a no-op (no lock, no power hook calls).
//!
//! ## Rules
//! - Activity restarts the idle period; if blanked it requests a wake.
//! - The lock fires at most once per idle period and never while the session is locked or locking.
//! - Blank fires once per idle period (it may fire while locked) and `blank-after` may be shorter
//!   than `lock-after`.
//! - While inhibited nothing fires and idle time does not accumulate; when inhibition ends the
//!   period starts over. An inhibitor that appears while blanked wakes the outputs.
//! - A config change restarts the period; a blank timer that was removed wakes the outputs.

use std::time::{Duration, Instant};

use crucible_config::IdleConfig;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::{
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::IsAlive,
    wayland::{
        idle_inhibit::IdleInhibitHandler,
        idle_notify::{IdleNotifierHandler, IdleNotifierState},
    },
};
use tracing::{debug, info};

use crate::state::{Backend, CrucibleState, root_surface};

/// The idle timer's tick interval.
pub const TICK: Duration = Duration::from_secs(1);

// ---- pure core -------------------------------------------------------------------------------

/// What one [`IdleMachine`] step asks the compositor to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IdleStep {
    /// Spawn the locker.
    pub lock: bool,
    /// Power the outputs off.
    pub blank: bool,
    /// Power the outputs back on.
    pub wake: bool,
}

/// The idle state machine: time since the last activity, the once-per-period lock latch and the
/// blanked flag. Pure; the caller supplies elapsed time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IdleMachine {
    idle: Duration,
    lock_fired: bool,
    blanked: bool,
}

impl IdleMachine {
    /// Whether the outputs are currently blanked (as far as the machine requested).
    #[must_use]
    pub fn blanked(&self) -> bool {
        self.blanked
    }

    /// Time accumulated since the last activity.
    #[must_use]
    pub fn idle_for(&self) -> Duration {
        self.idle
    }

    fn restart(&mut self) {
        self.idle = Duration::ZERO;
        self.lock_fired = false;
    }

    /// Input happened: restarts the period; a blanked machine requests a wake (`true`).
    pub fn activity(&mut self) -> bool {
        self.restart();
        std::mem::take(&mut self.blanked)
    }

    /// Restarts the idle period (a config change); the blanked flag is kept.
    pub fn reset(&mut self) {
        self.restart();
    }

    /// Advances by `elapsed` with the current `inhibited` / `locked` state and `config`.
    pub fn tick(
        &mut self,
        elapsed: Duration,
        inhibited: bool,
        locked: bool,
        config: &IdleConfig,
    ) -> IdleStep {
        let mut step = IdleStep::default();
        if inhibited {
            self.restart();
            step.wake = std::mem::take(&mut self.blanked);
            return step;
        }
        if config.blank_after.is_none() && self.blanked {
            self.blanked = false;
            step.wake = true;
        }
        if config.lock_after.is_none() && config.blank_after.is_none() {
            self.restart();
            return step;
        }
        self.idle = self.idle.saturating_add(elapsed);
        if let Some(after) = config.blank_after
            && !self.blanked
            && self.idle >= Duration::from_secs(u64::from(after))
        {
            self.blanked = true;
            step.blank = true;
        }
        if let Some(after) = config.lock_after
            && !self.lock_fired
            && !locked
            && self.idle >= Duration::from_secs(u64::from(after))
        {
            self.lock_fired = true;
            step.lock = true;
        }
        step
    }
}

// ---- compositor glue -------------------------------------------------------------------------

/// Idle runtime state held by [`CrucibleState`].
#[derive(Debug, Default)]
pub struct IdleRuntime {
    /// The pure state machine.
    pub machine: IdleMachine,
    /// Surfaces holding an `zwp_idle_inhibitor_v1`.
    pub inhibitors: Vec<WlSurface>,
    last_tick: Option<Instant>,
}

impl<B: Backend + 'static> IdleNotifierHandler for CrucibleState<B> {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        &mut self.idle_notifier_state
    }
}

impl<B: Backend + 'static> IdleInhibitHandler for CrucibleState<B> {
    fn inhibit(&mut self, surface: WlSurface) {
        self.idle.inhibitors.push(surface);
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        // One entry per inhibitor object: a surface holding two inhibitors stays inhibited
        // until both are destroyed.
        if let Some(i) = self.idle.inhibitors.iter().position(|s| *s == surface) {
            self.idle.inhibitors.swap_remove(i);
        }
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Marks input activity: restarts the idle period, wakes blanked outputs and tells
    /// `ext-idle-notify` clients. Called for every input event; never consumes the event.
    pub fn idle_activity(&mut self) {
        if self.idle.machine.activity() {
            info!("idle: input; waking outputs");
            B::set_outputs_powered(self, true);
        }
        self.idle_notifier_state.notify_activity(&self.seat);
    }

    /// Arms the 1 s idle timer on the event loop.
    pub fn start_idle_timer(&mut self) {
        let timer = self
            .handle
            .insert_source(Timer::from_duration(TICK), |_, _, data| {
                data.idle_tick(Instant::now());
                TimeoutAction::ToDuration(TICK)
            });
        if let Err(err) = timer {
            tracing::error!(%err, "cannot arm the idle timer; idle handling disabled");
        }
    }

    /// Whether idle is currently inhibited (see the module docs); also pushes the answer into the
    /// `ext-idle-notify` state.
    fn compute_idle_inhibited(&mut self) -> bool {
        self.idle.inhibitors.retain(|s| s.alive());
        let managed: Vec<WlSurface> = self
            .layout
            .metas()
            .filter_map(|(id, _)| self.layout.element(id))
            .filter_map(|e| e.wl_surface().map(|s| s.into_owned()))
            .collect();
        let visible = self
            .idle
            .inhibitors
            .iter()
            .filter(|surface| {
                let root = root_surface(surface);
                // A surface that is not a managed workspace window (layer, overlay, lock, popup)
                // always counts; a managed window only while mapped (on an active workspace).
                !managed.contains(&root) || self.window_for_surface(&root).is_some()
            })
            .count();
        let world = &self.layout.world;
        let fullscreen = world
            .output_ids()
            .filter(|id| world.output_fullscreen(*id).is_some())
            .count();
        // A visible inhibitor surface, or a fullscreen window on an active workspace.
        let inhibited = visible > 0 || fullscreen > 0;
        self.idle_notifier_state.set_is_inhibited(inhibited);
        inhibited
    }

    /// One timer tick at `now`: advances the machine and acts on its step.
    pub(crate) fn idle_tick(&mut self, now: Instant) {
        let elapsed = self
            .idle
            .last_tick
            .replace(now)
            .map_or(Duration::ZERO, |last| now.saturating_duration_since(last));
        let inhibited = self.compute_idle_inhibited();
        let locked = self.is_session_locked();
        let config = self.config.idle;
        let step = self.idle.machine.tick(elapsed, inhibited, locked, &config);
        debug!(?step, inhibited, locked, "idle tick");
        if step.wake {
            info!("idle: waking outputs");
            B::set_outputs_powered(self, true);
        }
        if step.blank {
            info!("idle: blanking outputs");
            B::set_outputs_powered(self, false);
            // Outputs that just went dark may complete a pending lock. Harmless on winit: no output
            // reports powered off there, so it only confirms what the render path would anyway.
            self.lock_confirm_if_presented();
        }
        if step.lock {
            info!("idle: locking the session");
            self.spawn_locker();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn cfg(lock: Option<u32>, blank: Option<u32>) -> IdleConfig {
        IdleConfig {
            lock_after: lock,
            blank_after: blank,
        }
    }

    /// Ticks `n` one-second steps; returns the steps that fired something.
    fn run(m: &mut IdleMachine, n: u32, inh: bool, locked: bool, c: &IdleConfig) -> Vec<IdleStep> {
        (0..n)
            .map(|_| m.tick(secs(1), inh, locked, c))
            .filter(|s| *s != IdleStep::default())
            .collect()
    }

    #[test]
    fn off_config_never_fires() {
        let mut m = IdleMachine::default();
        assert!(run(&mut m, 10_000, false, false, &cfg(None, None)).is_empty());
        assert!(!m.blanked());
    }

    #[test]
    fn lock_fires_once_per_period() {
        let mut m = IdleMachine::default();
        let fired = run(&mut m, 100, false, false, &cfg(Some(10), None));
        assert_eq!(fired.len(), 1);
        assert!(fired[0].lock && !fired[0].blank);
        // Activity starts a new period, which fires once more.
        assert!(!m.activity());
        assert_eq!(
            run(&mut m, 100, false, false, &cfg(Some(10), None)).len(),
            1
        );
    }

    #[test]
    fn lock_never_fires_while_locked() {
        let mut m = IdleMachine::default();
        assert!(run(&mut m, 100, false, true, &cfg(Some(10), None)).is_empty());
    }

    #[test]
    fn blank_fires_once_and_may_fire_while_locked() {
        let mut m = IdleMachine::default();
        let fired = run(&mut m, 100, false, true, &cfg(Some(10), Some(20)));
        assert_eq!(fired.len(), 1);
        assert!(fired[0].blank && !fired[0].lock);
        assert!(m.blanked());
    }

    #[test]
    fn activity_wakes_only_when_blanked() {
        let mut m = IdleMachine::default();
        assert!(!m.activity());
        run(&mut m, 30, false, false, &cfg(None, Some(20)));
        assert!(m.blanked());
        assert!(m.activity());
        assert!(!m.blanked());
        assert!(!m.activity());
    }

    #[test]
    fn blank_before_lock_then_lock_while_blanked() {
        let mut m = IdleMachine::default();
        let c = cfg(Some(30), Some(10));
        let fired = run(&mut m, 100, false, false, &c);
        assert_eq!(fired.len(), 2);
        assert!(fired[0].blank && !fired[0].lock);
        assert!(fired[1].lock && !fired[1].blank);
        assert!(m.blanked());
    }

    #[test]
    fn lock_before_blank() {
        let mut m = IdleMachine::default();
        let fired = run(&mut m, 100, false, false, &cfg(Some(10), Some(30)));
        assert_eq!(fired.len(), 2);
        assert!(fired[0].lock && fired[1].blank);
    }

    #[test]
    fn inhibition_stops_accumulation_and_restarts_the_period() {
        let mut m = IdleMachine::default();
        let c = cfg(Some(10), None);
        assert!(run(&mut m, 8, false, false, &c).is_empty());
        assert!(run(&mut m, 100, true, false, &c).is_empty());
        assert_eq!(m.idle_for(), Duration::ZERO);
        // The period starts over: 8 more seconds is still short of 10.
        assert!(run(&mut m, 8, false, false, &c).is_empty());
        assert_eq!(run(&mut m, 5, false, false, &c).len(), 1);
    }

    #[test]
    fn inhibition_while_blanked_wakes_once() {
        let mut m = IdleMachine::default();
        run(&mut m, 30, false, false, &cfg(None, Some(20)));
        let fired = run(&mut m, 5, true, false, &cfg(None, Some(20)));
        assert_eq!(fired.len(), 1);
        assert!(fired[0].wake);
    }

    #[test]
    fn reset_restarts_the_period_and_a_removed_blank_timer_wakes() {
        let mut m = IdleMachine::default();
        run(&mut m, 9, false, false, &cfg(Some(10), Some(20)));
        m.reset();
        assert!(run(&mut m, 9, false, false, &cfg(Some(10), Some(20))).is_empty());
        run(&mut m, 30, false, false, &cfg(None, Some(20)));
        assert!(m.blanked());
        let fired = run(&mut m, 1, false, false, &cfg(Some(10), None));
        assert_eq!(fired.len(), 1);
        assert!(fired[0].wake && !m.blanked());
    }

    #[test]
    fn fire_exactly_at_the_threshold() {
        let mut m = IdleMachine::default();
        let c = cfg(Some(3), None);
        assert!(m.tick(secs(2), false, false, &c) == IdleStep::default());
        assert!(m.tick(secs(1), false, false, &c).lock);
    }
}
