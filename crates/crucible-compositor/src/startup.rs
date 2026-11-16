//! Startup timing for the udev path: where CDE's first second goes (plan 43 WP-1).
//!
//! One process-start [`Instant`] is captured at the backend entry; every `startup:` `info!` line
//! carries `elapsed_ms` measured from it. Besides the logging, this holds the pure start-up
//! adoption rule (plan 44): [`ADOPTION_TIMEOUT`] and [`ready_to_adopt`].

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// Captures the process-start instant (first call wins; later calls are no-ops).
pub fn mark_process_start() {
    PROCESS_START.get_or_init(Instant::now);
}

/// Whole milliseconds between `start` and `now`, saturating at zero if `now` precedes `start`.
pub fn elapsed_ms_between(start: Instant, now: Instant) -> u64 {
    u64::try_from(now.saturating_duration_since(start).as_millis()).unwrap_or(u64::MAX)
}

/// Milliseconds since [`mark_process_start`] (starts the clock now if it was never marked).
pub fn elapsed_ms() -> u64 {
    elapsed_ms_between(*PROCESS_START.get_or_init(Instant::now), Instant::now())
}

/// A flag that reports `true` exactly once: the first call to [`FirstOnly::take`].
#[derive(Debug, Default)]
pub struct FirstOnly(bool);

impl FirstOnly {
    /// `true` on the first call, `false` on every later one.
    pub fn take(&mut self) -> bool {
        !std::mem::replace(&mut self.0, true)
    }
}

/// The three process-wide wallpaper markers, each logged once.
#[derive(Debug, Default)]
pub struct WallpaperMarks {
    layer_mapped: AtomicBool,
    buffer_committed: AtomicBool,
    flip_after_buffer: AtomicBool,
}

impl WallpaperMarks {
    /// `true` the first time a `background` layer surface is role-mapped.
    pub fn layer_mapped(&self) -> bool {
        !self.layer_mapped.swap(true, Ordering::Relaxed)
    }

    /// `true` the first time a `background` layer surface commits with a buffer.
    pub fn buffer_committed(&self) -> bool {
        !self.buffer_committed.swap(true, Ordering::Relaxed)
    }

    /// `true` for the first page flip completed after the first background buffer commit.
    pub fn flip_completed(&self) -> bool {
        self.buffer_committed.load(Ordering::Relaxed)
            && !self.flip_after_buffer.swap(true, Ordering::Relaxed)
    }
}

static WALLPAPER: WallpaperMarks = WallpaperMarks {
    layer_mapped: AtomicBool::new(false),
    buffer_committed: AtomicBool::new(false),
    flip_after_buffer: AtomicBool::new(false),
};

/// Logs `startup: background layer mapped` the first time (role map only; no buffer yet).
pub fn note_background_layer_mapped() {
    if WALLPAPER.layer_mapped() {
        tracing::info!(
            elapsed_ms = elapsed_ms(),
            "startup: background layer mapped"
        );
    }
}

/// Logs `startup: background buffer committed` the first time a background surface has a buffer.
pub fn note_background_buffer_committed() {
    if WALLPAPER.buffer_committed() {
        tracing::info!(
            elapsed_ms = elapsed_ms(),
            "startup: background buffer committed"
        );
    }
}

/// Logs `startup: first flip after background buffer` on the first flip after that commit.
pub fn note_flip_completed() {
    if WALLPAPER.flip_completed() {
        tracing::info!(
            elapsed_ms = elapsed_ms(),
            "startup: first flip after background buffer"
        );
    }
}

/// How long, from main-loop start, an output's first DRM commit waits for its wallpaper (plan 44).
///
/// At udev start-up each output's adoption (its first commit) waits for a background-layer buffer
/// on that output, so the first frame CDE scans out is the wallpaper; past this deadline every
/// still-pending output is adopted anyway (a host with no wallpaper client starts at most this
/// much later).
pub const ADOPTION_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);

/// The pending outputs to adopt now: all of them once the start-up deadline passed, else the one a
/// background buffer just landed on (if it is pending at all). Pure, so the readiness rule is
/// unit-testable without a compositor.
pub fn ready_to_adopt<K: PartialEq + Copy>(
    pending: &[K],
    buffer_on: Option<K>,
    timed_out: bool,
) -> Vec<K> {
    if timed_out {
        return pending.to_vec();
    }
    buffer_on
        .filter(|key| pending.contains(key))
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn elapsed_is_whole_milliseconds_and_saturates() {
        let t0 = Instant::now();
        assert_eq!(elapsed_ms_between(t0, t0 + Duration::from_micros(2_999)), 2);
        assert_eq!(elapsed_ms_between(t0 + Duration::from_secs(1), t0), 0);
    }

    #[test]
    fn first_only_fires_once() {
        let mut f = FirstOnly::default();
        assert!(f.take());
        assert!(!f.take());
    }

    #[test]
    fn flip_marker_needs_the_buffer_commit_first_and_fires_once() {
        let m = WallpaperMarks::default();
        assert!(!m.flip_completed());
        assert!(m.layer_mapped());
        assert!(!m.layer_mapped());
        assert!(m.buffer_committed());
        assert!(!m.buffer_committed());
        assert!(m.flip_completed());
        assert!(!m.flip_completed());
    }

    #[test]
    fn adoption_waits_for_a_buffer_on_that_output() {
        let pending = [1u8, 2];
        assert_eq!(ready_to_adopt(&pending, Some(2), false), vec![2]);
        assert!(ready_to_adopt(&pending, None, false).is_empty());
    }

    #[test]
    fn a_buffer_on_another_output_adopts_nothing() {
        // Output 3 is already adopted (or not ours): pending 1 and 2 keep waiting.
        assert!(ready_to_adopt(&[1u8, 2], Some(3), false).is_empty());
    }

    #[test]
    fn the_timeout_adopts_every_pending_output() {
        assert_eq!(ready_to_adopt(&[1u8, 2], None, true), vec![1, 2]);
        assert!(ready_to_adopt::<u8>(&[], Some(1), true).is_empty());
    }
}
