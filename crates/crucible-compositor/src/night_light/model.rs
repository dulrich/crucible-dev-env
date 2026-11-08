//! Pure night-light model: no Smithay, no I/O, so it is unit-testable without a compositor.

use crucible_config::NightLightConfig;

/// The temperature at which the ramp is exactly the identity (no tint).
pub const IDENTITY_KELVIN: u32 = 6500;
/// The lowest temperature the schedule or a verb may request.
pub const MIN_KELVIN: u32 = 2500;
/// The night temperature `on` forces when there is no `night-light` block.
pub const DEFAULT_KELVIN: u32 = 4500;

const MINUTES_PER_DAY: i64 = 1440;

/// A manual override set by the `night-light` verb; it holds until `auto` or a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Override {
    /// Force the configured `temperature` ([`DEFAULT_KELVIN`] without a block).
    On,
    /// Force [`IDENTITY_KELVIN`].
    Off,
    /// Force this temperature.
    Kelvin(u32),
}

/// The argument of the `night-light` verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    /// `on`.
    On,
    /// `off`.
    Off,
    /// `toggle`.
    Toggle,
    /// `auto`: clear the override.
    Auto,
    /// `<kelvin>`.
    Kelvin(u32),
}

/// The temperature the schedule alone wants at `minute_of_day` (0..1440; larger values wrap).
///
/// Full `temperature` inside `[start, end)` (wrapping midnight when `end < start`), a linear ramp
/// from 6500 K over the `fade_minutes` before `start` and back up over the `fade_minutes` after
/// `end`, 6500 K otherwise. Where the daytime gap is shorter than `2 * fade_minutes` the two ramps
/// overlap and the lower (warmer) value wins. `fade_minutes == 0` is a hard step.
#[must_use]
pub fn target_kelvin(cfg: &NightLightConfig, minute_of_day: u16) -> u32 {
    let m = i64::from(minute_of_day) % MINUTES_PER_DAY;
    let (start, end) = (i64::from(cfg.start), i64::from(cfg.end));
    let inside = if start < end {
        (start..end).contains(&m)
    } else {
        m >= start || m < end
    };
    let night = i64::from(cfg.temperature);
    if inside {
        return cfg.temperature;
    }
    let fade = i64::from(cfg.fade_minutes);
    let full = i64::from(IDENTITY_KELVIN);
    let mut kelvin = full;
    if fade > 0 {
        let until_start = (start - m).rem_euclid(MINUTES_PER_DAY);
        if until_start < fade {
            kelvin = kelvin.min(rounded_lerp(night, full, until_start, fade));
        }
        let since_end = (m - end).rem_euclid(MINUTES_PER_DAY);
        if since_end < fade {
            kelvin = kelvin.min(rounded_lerp(night, full, since_end, fade));
        }
    }
    u32::try_from(kelvin).expect("kelvin between night and 6500 is positive")
}

/// `a + (b - a) * num / den`, rounded to nearest (`a <= b`, `0 <= num <= den`, `den > 0`).
fn rounded_lerp(a: i64, b: i64, num: i64, den: i64) -> i64 {
    a + ((b - a) * num * 2 + den) / (den * 2)
}

/// The temperature in force: the override if any, else the schedule, else [`IDENTITY_KELVIN`].
#[must_use]
pub fn effective_kelvin(
    schedule: Option<&NightLightConfig>,
    over: Option<Override>,
    minute_of_day: u16,
) -> u32 {
    match over {
        Some(Override::On) => schedule.map_or(DEFAULT_KELVIN, |c| c.temperature),
        Some(Override::Off) => IDENTITY_KELVIN,
        Some(Override::Kelvin(k)) => k.clamp(MIN_KELVIN, IDENTITY_KELVIN),
        None => schedule.map_or(IDENTITY_KELVIN, |c| target_kelvin(c, minute_of_day)),
    }
}

/// The override after a verb, given the temperature in force right now (only `toggle` reads it).
/// `None` means "follow the schedule".
#[must_use]
pub fn apply_verb(verb: Verb, effective_now: u32) -> Option<Override> {
    match verb {
        Verb::On => Some(Override::On),
        Verb::Off => Some(Override::Off),
        Verb::Toggle if effective_now < IDENTITY_KELVIN => Some(Override::Off),
        Verb::Toggle => Some(Override::On),
        Verb::Auto => None,
        Verb::Kelvin(k) => Some(Override::Kelvin(k.clamp(MIN_KELVIN, IDENTITY_KELVIN))),
    }
}

/// Un-normalised blackbody whitepoint approximation (Tanner Helland), each channel 0..=255.
fn whitepoint(kelvin: u32) -> [f64; 3] {
    let t = f64::from(kelvin) / 100.0;
    let red = if t <= 66.0 {
        255.0
    } else {
        329.698_727_446 * (t - 60.0).powf(-0.133_204_759_2)
    };
    let green = if t <= 66.0 {
        99.470_802_586_1 * t.ln() - 161.119_568_166_1
    } else {
        288.122_169_528_3 * (t - 60.0).powf(-0.075_514_849_2)
    };
    let blue = if t >= 66.0 {
        255.0
    } else if t <= 19.0 {
        0.0
    } else {
        138.517_731_223_1 * (t - 10.0).ln() - 305.044_792_730_7
    };
    [red, green, blue].map(|c| c.clamp(0.0, 255.0))
}

/// Per-channel multipliers in `0.0..=1.0`; exactly 1.0 at [`IDENTITY_KELVIN`] and above.
fn multipliers(kelvin: u32) -> [f64; 3] {
    if kelvin >= IDENTITY_KELVIN {
        return [1.0; 3];
    }
    let white = whitepoint(IDENTITY_KELVIN);
    let now = whitepoint(kelvin);
    std::array::from_fn(|i| (now[i] / white[i]).clamp(0.0, 1.0))
}

/// Red, green and blue gamma ramps of `len` entries for `kelvin`.
///
/// At 6500 K and above each is exactly the identity `(i * 65536 / len) as u16`; below, the identity
/// scaled by the channel multiplier (never above identity). `len == 0` yields empty ramps.
#[must_use]
pub fn ramp(kelvin: u32, len: usize) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
    let [mr, mg, mb] = multipliers(kelvin);
    let channel = |mult: f64| -> Vec<u16> {
        (0..len)
            .map(|i| {
                let id = (i as u64 * 65536 / len as u64) as u16;
                if mult >= 1.0 {
                    id
                } else {
                    (f64::from(id) * mult) as u16
                }
            })
            .collect()
    };
    (channel(mr), channel(mg), channel(mb))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(start: &str, end: &str, fade: u16) -> NightLightConfig {
        let hm = |s: &str| {
            let (h, m) = s.split_once(':').unwrap();
            h.parse::<u16>().unwrap() * 60 + m.parse::<u16>().unwrap()
        };
        NightLightConfig {
            temperature: 4500,
            start: hm(start),
            end: hm(end),
            fade_minutes: fade,
        }
    }

    #[test]
    fn identity_at_6500_for_any_len() {
        for len in [256usize, 1024, 17] {
            let (r, g, b) = ramp(6500, len);
            let id: Vec<u16> = (0..len).map(|i| (i * 65536 / len) as u16).collect();
            assert_eq!((&r, &g, &b), (&id, &id, &id), "len {len}");
        }
        let (r, _, _) = ramp(6500, 256);
        assert_eq!((r[0], r[128], r[255]), (0, 32768, 65280));
        assert_eq!(ramp(5000, 0), (vec![], vec![], vec![]));
    }

    #[test]
    fn warmer_cuts_blue_most_and_never_raises_red() {
        let id = ramp(6500, 256);
        for k in [6400, 5500, 4500, 3000, 2500] {
            let (r, g, b) = ramp(k, 256);
            for i in 0..256 {
                assert!(
                    r[i] <= id.0[i] && g[i] <= id.1[i] && b[i] <= id.2[i],
                    "{k} {i}"
                );
            }
            assert_eq!(r[255], id.0[255], "red untouched at {k}");
            assert!(g[255] < id.1[255] && b[255] < id.2[255], "{k}");
            assert!(b[255] <= g[255], "{k}");
        }
    }

    #[test]
    fn lowering_kelvin_never_raises_a_top_entry() {
        let top = |k| {
            let (r, g, b) = ramp(k, 256);
            (r[255], g[255], b[255])
        };
        let mut prev = top(6500);
        for k in (2500..6500).rev().step_by(100) {
            let now = top(k);
            assert!(now.0 <= prev.0 && now.1 <= prev.1 && now.2 <= prev.2, "{k}");
            prev = now;
        }
    }

    #[test]
    fn window_wrapping_midnight() {
        let c = cfg("21:00", "07:00", 0);
        assert_eq!(target_kelvin(&c, 21 * 60), 4500);
        assert_eq!(target_kelvin(&c, 21 * 60 - 1), 6500);
        assert_eq!(target_kelvin(&c, 0), 4500);
        assert_eq!(target_kelvin(&c, 7 * 60 - 1), 4500);
        assert_eq!(target_kelvin(&c, 7 * 60), 6500);
        assert_eq!(target_kelvin(&c, 12 * 60), 6500);
        let same_day = cfg("09:00", "17:00", 0);
        assert_eq!(target_kelvin(&same_day, 12 * 60), 4500);
        assert_eq!(target_kelvin(&same_day, 18 * 60), 6500);
    }

    #[test]
    fn fades_ramp_linearly_and_are_continuous() {
        let c = cfg("21:00", "07:00", 30);
        assert_eq!(target_kelvin(&c, 21 * 60 - 30), 6500);
        assert_eq!(target_kelvin(&c, 21 * 60 - 15), 5500);
        assert_eq!(target_kelvin(&c, 21 * 60), 4500);
        assert_eq!(target_kelvin(&c, 7 * 60), 4500);
        assert_eq!(target_kelvin(&c, 7 * 60 + 15), 5500);
        assert_eq!(target_kelvin(&c, 7 * 60 + 30), 6500);
        let step = 2000u32.div_ceil(30) + 1;
        for m in 0..1440u16 {
            let a = target_kelvin(&c, m);
            let b = target_kelvin(&c, (m + 1) % 1440);
            assert!(a.abs_diff(b) <= step, "jump {a}->{b} at {m}");
            assert!((4500..=6500).contains(&a));
        }
    }

    #[test]
    fn fade_wraps_past_midnight() {
        let c = cfg("00:10", "06:00", 30);
        assert_eq!(target_kelvin(&c, 0), 5167); // 10 min before start of a 30 min fade
        assert_eq!(target_kelvin(&c, 23 * 60 + 40), 6500);
        assert_eq!(target_kelvin(&c, 23 * 60 + 55), 5500);
    }

    #[test]
    fn overlapping_fades_take_the_warmer_ramp() {
        // Daytime gap 07:00..08:00 is 60 min but fades are 180: both ramps are live.
        let c = cfg("08:00", "07:00", 180);
        let after_end = |m: i64| rounded_lerp(4500, 6500, m, 180);
        let before_start = |m: i64| rounded_lerp(4500, 6500, m, 180);
        for (minute, since_end, until_start) in [
            (7 * 60 + 10, 10, 50),
            (7 * 60 + 50, 50, 10),
            (7 * 60 + 30, 30, 30),
        ] {
            let want = after_end(since_end).min(before_start(until_start));
            assert_eq!(i64::from(target_kelvin(&c, minute)), want, "{minute}");
            assert!(target_kelvin(&c, minute) < 6500);
        }
    }

    #[test]
    fn hard_step_with_zero_fade() {
        let c = cfg("21:00", "07:00", 0);
        assert_eq!(target_kelvin(&c, 20 * 60 + 59), 6500);
        assert_eq!(target_kelvin(&c, 21 * 60), 4500);
    }

    #[test]
    fn verbs_resolve_against_the_effective_temperature() {
        let c = cfg("21:00", "07:00", 0);
        assert_eq!(apply_verb(Verb::Auto, 4500), None);
        assert_eq!(apply_verb(Verb::On, 6500), Some(Override::On));
        assert_eq!(apply_verb(Verb::Off, 4500), Some(Override::Off));
        assert_eq!(apply_verb(Verb::Toggle, 4500), Some(Override::Off));
        assert_eq!(apply_verb(Verb::Toggle, 6500), Some(Override::On));
        assert_eq!(
            apply_verb(Verb::Kelvin(4000), 6500),
            Some(Override::Kelvin(4000))
        );
        assert_eq!(
            apply_verb(Verb::Kelvin(100), 6500),
            Some(Override::Kelvin(2500))
        );
        // Resolution of overrides.
        assert_eq!(
            effective_kelvin(Some(&c), Some(Override::On), 12 * 60),
            4500
        );
        assert_eq!(effective_kelvin(None, Some(Override::On), 0), 4500);
        assert_eq!(effective_kelvin(Some(&c), Some(Override::Off), 0), 6500);
        assert_eq!(
            effective_kelvin(Some(&c), Some(Override::Kelvin(3000)), 12 * 60),
            3000
        );
        assert_eq!(effective_kelvin(Some(&c), None, 0), 4500);
        assert_eq!(effective_kelvin(None, None, 0), 6500);
    }
}
