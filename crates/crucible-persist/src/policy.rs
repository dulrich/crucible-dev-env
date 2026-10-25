//! The auto-save throttle: a pure decision function.

use std::time::{Duration, Instant};

/// Whether to save now: `true` when the workspace is `dirty` and it was either never saved or the
/// last save was at least `min_interval` ago. Never saves a clean workspace.
#[must_use]
pub fn should_save(
    dirty: bool,
    last_saved: Option<Instant>,
    now: Instant,
    min_interval: Duration,
) -> bool {
    dirty && last_saved.is_none_or(|t| now.saturating_duration_since(t) >= min_interval)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_never_saves() {
        let now = Instant::now();
        assert!(!should_save(false, None, now, Duration::from_secs(1)));
        assert!(!should_save(false, Some(now), now, Duration::ZERO));
    }

    #[test]
    fn dirty_and_never_saved_saves() {
        assert!(should_save(
            true,
            None,
            Instant::now(),
            Duration::from_secs(1)
        ));
    }

    #[test]
    fn dirty_respects_the_minimum_interval() {
        let t0 = Instant::now();
        let min = Duration::from_secs(1);
        assert!(!should_save(
            true,
            Some(t0),
            t0 + Duration::from_millis(999),
            min
        ));
        assert!(should_save(true, Some(t0), t0 + min, min));
        assert!(should_save(
            true,
            Some(t0),
            t0 + Duration::from_secs(5),
            min
        ));
        // A clock that appears to go backwards does not panic and does not save.
        assert!(!should_save(true, Some(t0 + min), t0, min));
    }
}
