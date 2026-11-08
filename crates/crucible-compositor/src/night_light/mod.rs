//! Night light: a scheduled colour-temperature shift applied as a per-CRTC gamma ramp.
//!
//! [`model`] is the pure part (schedule, temperature to ramp, verb semantics). This module adds
//! the runtime: the verb override, [`ApplyCache`] (the pure per-output "what did we last write"
//! state machine, generic over the output key so it carries no Smithay types) and the
//! [`CrucibleState::evaluate_night_light`] trigger. The DRM writes themselves live in the udev
//! backend ([`crate::state::Backend::apply_night_light`]).

pub mod model;

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::time::Duration;

use crucible_keys::NightLightArg;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::{backend::drm::DrmNode, reexports::drm::control::crtc};
use tracing::{error, warn};

use crate::state::{Backend, CrucibleState};

/// How often the schedule is re-evaluated.
const TICK: Duration = Duration::from_secs(30);

use model::{IDENTITY_KELVIN, Override, Verb, apply_verb, effective_kelvin};

/// What the cache knows about one output's gamma ramp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    /// This temperature was last written successfully.
    Applied(u32),
    /// A ramp CDE wrote earlier may no longer be in force (session resume, device reset): the next
    /// evaluation must write, even the identity.
    Stale,
}

/// The decision for one output at one evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Nothing to do.
    Skip,
    /// Write the ramp for the desired temperature.
    Write,
}

/// Per-output record of the last successfully written temperature. Pure: no Smithay, no I/O.
#[derive(Debug)]
pub struct ApplyCache<K> {
    entries: HashMap<K, Entry>,
    /// Outputs whose last write failed and was already reported (one `warn` per failure streak).
    failing: HashSet<K>,
}

impl<K> Default for ApplyCache<K> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            failing: HashSet::new(),
        }
    }
}

impl<K: Hash + Eq> ApplyCache<K> {
    /// Whether to write `kelvin` to `key`. An inactive (paused) device is skipped and left as it
    /// is, so the write happens once it is active again. An output never written is not touched
    /// for an untinted `kelvin` unless night light is `enabled` (a `night-light` block or a verb
    /// override): then it is written once, which heals a tint left by an earlier run or a replug.
    /// With night light never enabled CDE issues no gamma call at all.
    #[must_use]
    pub fn decide(&self, key: &K, kelvin: u32, device_active: bool, enabled: bool) -> Decision {
        if !device_active {
            return Decision::Skip;
        }
        match self.entries.get(key) {
            Some(Entry::Applied(done)) if *done == kelvin => Decision::Skip,
            Some(_) => Decision::Write,
            None if kelvin >= IDENTITY_KELVIN && !enabled => Decision::Skip,
            None => Decision::Write,
        }
    }

    /// Records a successful write and ends any failure streak.
    pub fn record_success(&mut self, key: K, kelvin: u32) {
        self.failing.remove(&key);
        self.entries.insert(key, Entry::Applied(kelvin));
    }

    /// Records a failed write (the entry stays as it was, so the next evaluation retries).
    /// Returns whether to log it: only the first failure of a streak.
    pub fn record_failure(&mut self, key: K) -> bool {
        self.failing.insert(key)
    }

    /// Marks `key` stale: its ramp must be re-sent if it was ever tinted. Unknown keys stay unknown.
    pub fn invalidate(&mut self, key: &K) {
        if let Some(entry) = self.entries.get_mut(key) {
            *entry = Entry::Stale;
        }
    }

    /// Marks every entry for which `matches` holds stale.
    pub fn invalidate_where(&mut self, matches: impl Fn(&K) -> bool) {
        for (key, entry) in &mut self.entries {
            if matches(key) {
                *entry = Entry::Stale;
            }
        }
    }

    /// Marks every entry stale.
    pub fn invalidate_all(&mut self) {
        self.invalidate_where(|_| true);
    }

    /// Forgets `key` entirely (its output or device is gone).
    pub fn remove(&mut self, key: &K) {
        self.entries.remove(key);
        self.failing.remove(key);
    }

    /// Forgets every key for which `matches` holds.
    pub fn remove_where(&mut self, matches: impl Fn(&K) -> bool) {
        self.entries.retain(|key, _| !matches(key));
        self.failing.retain(|key| !matches(key));
    }
}

/// An output's identity in the cache: its DRM device and CRTC.
pub type OutputKey = (DrmNode, crtc::Handle);

/// Night-light runtime state, held on [`CrucibleState`].
#[derive(Debug, Default)]
pub struct NightLight {
    /// The manual override set by the `night-light` verb; held until `auto` or a restart.
    pub over: Option<Override>,
    /// What each CRTC was last given.
    pub cache: ApplyCache<OutputKey>,
}

/// Maps the keys-crate verb argument onto the model's.
#[must_use]
pub fn verb_of(arg: NightLightArg) -> Verb {
    match arg {
        NightLightArg::On => Verb::On,
        NightLightArg::Off => Verb::Off,
        NightLightArg::Toggle => Verb::Toggle,
        NightLightArg::Auto => Verb::Auto,
        NightLightArg::Kelvin(k) => Verb::Kelvin(k),
    }
}

/// The local minute of the day (0..1440), or `None` if the C library refuses the time.
fn local_minute_of_day() -> Option<u16> {
    let now = crate::screenshot::Stamp::now_local()?;
    u16::try_from(now.hour * 60 + now.minute).ok()
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// The temperature in force now (override, else schedule, else 6500 K).
    fn night_light_kelvin_now(&self) -> Option<u32> {
        let minute = local_minute_of_day().or_else(|| {
            warn!("night light: local time unavailable");
            // An override does not read the clock; a schedule cannot be evaluated without it.
            (self.config.night_light.is_none() || self.night_light.over.is_some()).then_some(0)
        })?;
        Some(effective_kelvin(
            self.config.night_light.as_ref(),
            self.night_light.over,
            minute,
        ))
    }

    /// Whether night light is in use: a `night-light` block or a verb override.
    #[must_use]
    pub fn night_light_enabled(&self) -> bool {
        self.config.night_light.is_some() || self.night_light.over.is_some()
    }

    /// Re-evaluates the temperature in force and hands it to the backend, which writes only what
    /// changed. Cheap to call: called by the 30 s timer, config applies, the verb and every
    /// cache invalidation.
    pub fn evaluate_night_light(&mut self) {
        if let Some(kelvin) = self.night_light_kelvin_now() {
            B::apply_night_light(self, kelvin);
        }
    }

    /// Arms the 30 s re-evaluation timer (the schedule moves with the clock). Called once at startup.
    pub fn start_night_light_timer(&mut self) {
        let timer = self
            .handle
            .insert_source(Timer::from_duration(TICK), |_, _, data| {
                data.evaluate_night_light();
                TimeoutAction::ToDuration(TICK)
            });
        if let Err(err) = timer {
            error!(%err, "cannot arm the night-light timer; the schedule will not advance");
        }
    }

    /// Executes `night-light <arg>`: resolves the new override against the temperature in force,
    /// then applies it immediately.
    pub fn night_light_verb(&mut self, arg: NightLightArg) {
        let now = self.night_light_kelvin_now().unwrap_or(IDENTITY_KELVIN);
        self.night_light.over = apply_verb(verb_of(arg), now);
        self.evaluate_night_light();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NIGHT: u32 = 4500;

    fn cache() -> ApplyCache<u8> {
        ApplyCache::default()
    }

    #[test]
    fn a_never_tinted_output_is_not_written_at_identity() {
        let c = cache();
        assert_eq!(c.decide(&1, IDENTITY_KELVIN, true, false), Decision::Skip);
        // Enabled (block or verb): an unknown CRTC is written once, healing a stale tint.
        assert_eq!(c.decide(&1, IDENTITY_KELVIN, true, true), Decision::Write);
        assert_eq!(c.decide(&1, NIGHT, true, true), Decision::Write);
    }

    #[test]
    fn the_same_temperature_is_written_once() {
        let mut c = cache();
        c.record_success(1, NIGHT);
        assert_eq!(c.decide(&1, NIGHT, true, true), Decision::Skip);
        assert_eq!(c.decide(&1, 4400, true, true), Decision::Write);
        // Leaving night light writes the identity once, then nothing more.
        assert_eq!(c.decide(&1, IDENTITY_KELVIN, true, true), Decision::Write);
        c.record_success(1, IDENTITY_KELVIN);
        assert_eq!(c.decide(&1, IDENTITY_KELVIN, true, true), Decision::Skip);
    }

    #[test]
    fn a_failed_write_is_retried_and_warned_once_per_streak() {
        let mut c = cache();
        assert_eq!(c.decide(&1, NIGHT, true, true), Decision::Write);
        assert!(c.record_failure(1), "first failure of a streak is logged");
        assert_eq!(c.decide(&1, NIGHT, true, true), Decision::Write, "retried");
        assert!(!c.record_failure(1), "the streak stays quiet");
        c.record_success(1, NIGHT);
        assert!(c.record_failure(1), "a success resets the streak");
        // A failed leave keeps the tinted entry, so the identity is retried.
        c.record_success(1, NIGHT);
        assert_eq!(c.decide(&1, IDENTITY_KELVIN, true, true), Decision::Write);
    }

    #[test]
    fn an_inactive_device_is_skipped_and_left_dirty() {
        let mut c = cache();
        assert_eq!(c.decide(&1, NIGHT, false, true), Decision::Skip);
        // Still dirty once the device is active again.
        assert_eq!(c.decide(&1, NIGHT, true, true), Decision::Write);
        c.record_success(1, NIGHT);
        assert_eq!(c.decide(&1, 4000, false, true), Decision::Skip);
        assert_eq!(c.decide(&1, 4000, true, true), Decision::Write);
    }

    #[test]
    fn invalidation_forces_a_rewrite_even_of_the_identity() {
        let mut c = cache();
        c.record_success(1, NIGHT);
        c.record_success(2, NIGHT);
        c.invalidate(&1);
        assert_eq!(c.decide(&1, NIGHT, true, true), Decision::Write);
        assert_eq!(c.decide(&2, NIGHT, true, true), Decision::Skip);
        assert_eq!(c.decide(&1, IDENTITY_KELVIN, true, true), Decision::Write);
        c.invalidate_all();
        assert_eq!(c.decide(&2, NIGHT, true, true), Decision::Write);
        c.record_success(2, NIGHT);
        c.invalidate_where(|k| *k == 2);
        assert_eq!(c.decide(&2, NIGHT, true, true), Decision::Write);
    }

    #[test]
    fn invalidating_an_unknown_output_does_not_make_it_tinted() {
        let mut c = cache();
        c.invalidate(&9);
        c.invalidate_all();
        assert_eq!(c.decide(&9, IDENTITY_KELVIN, true, false), Decision::Skip);
    }

    #[test]
    fn removal_forgets_the_output_and_its_streak() {
        let mut c = cache();
        c.record_success(1, NIGHT);
        assert!(c.record_failure(1));
        c.remove(&1);
        assert_eq!(c.decide(&1, IDENTITY_KELVIN, true, false), Decision::Skip);
        assert!(
            c.record_failure(1),
            "a replugged output starts a new streak"
        );
        c.record_success(2, NIGHT);
        c.remove_where(|k| *k == 2);
        assert_eq!(c.decide(&2, IDENTITY_KELVIN, true, false), Decision::Skip);
    }

    #[test]
    fn verb_arguments_map_one_to_one() {
        assert_eq!(verb_of(NightLightArg::On), Verb::On);
        assert_eq!(verb_of(NightLightArg::Off), Verb::Off);
        assert_eq!(verb_of(NightLightArg::Toggle), Verb::Toggle);
        assert_eq!(verb_of(NightLightArg::Auto), Verb::Auto);
        assert_eq!(verb_of(NightLightArg::Kelvin(3000)), Verb::Kelvin(3000));
    }
}
