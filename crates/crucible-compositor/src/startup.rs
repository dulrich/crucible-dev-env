//! Startup timing for the udev path: where CDE's first second goes (plan 43 WP-1).
//!
//! One process-start [`Instant`] is captured at the backend entry; every `startup:` `info!` line
//! carries `elapsed_ms` measured from it. Besides the logging, this holds the pure start-up
//! adoption phases (plan 44/45): [`ADOPTION_TIMEOUT`] and [`StartupPhase`].

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

/// The udev start-up phase (plans 44/45): which DRM adoption rule applies right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupPhase {
    /// Background start: devices paused, nothing reaches DRM, adoption waits for activation.
    AwaitingActivation,
    /// The adoption window is open: an output's adoption waits for its wallpaper or the close.
    AwaitingWallpaper,
    /// The window closed: a connected output adopts at once.
    Running,
}

/// What moves the [`StartupPhase`] on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupEvent {
    /// A libseat `ActivateSession`.
    Activated,
    /// The adoption window closes (`finish_startup_adoption`).
    WindowClosed,
}

impl StartupPhase {
    /// The phase at start: a session inactive at start is a background start.
    #[must_use]
    pub fn at_start(session_active: bool) -> Self {
        if session_active {
            Self::AwaitingWallpaper
        } else {
            Self::AwaitingActivation
        }
    }

    /// The one transition; whether the phase changed. A background start's first activation
    /// keeps the window open while it adds deferred devices; only the close reaches `Running`.
    pub fn advance(&mut self, event: StartupEvent) -> bool {
        let next = match (*self, event) {
            (Self::AwaitingActivation, StartupEvent::Activated) => Self::AwaitingWallpaper,
            (Self::AwaitingWallpaper, StartupEvent::WindowClosed) => Self::Running,
            (phase, _) => phase,
        };
        std::mem::replace(self, next) != next
    }

    /// A background start not yet activated.
    #[must_use]
    pub fn background(self) -> bool {
        self == Self::AwaitingActivation
    }

    /// A wallpaper landing adopts its pending output now (and the timeout is armed).
    #[must_use]
    pub fn adopts_on_wallpaper(self) -> bool {
        self == Self::AwaitingWallpaper
    }

    /// A connected output's adoption waits (the window is open).
    #[must_use]
    pub fn defers_adoption(self) -> bool {
        self != Self::Running
    }
}

/// How long, from main-loop start, the login ready rule waits before it holds anyway (plan 45).
pub const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// What the login ready rule (plan 45 WP-1 invariant 3) looks at, sampled from the compositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReadyInputs {
    /// The startup session step (`start_session`) has run, so the expected surfaces are known.
    pub session_started: bool,
    /// Usable outputs enumerated (mapped in the Space).
    pub outputs: usize,
    /// Of those, the outputs whose background-layer buffer has landed.
    pub outputs_with_background: usize,
    /// Spawns the session step made for the active workspaces (restored windows, the cold-start
    /// picker) that are still alive and have not created their window yet.
    pub awaited_spawns: usize,
    /// Windows shown on the active workspaces (in the Space) without a buffer yet.
    pub windows_without_buffer: usize,
}

/// The login ready rule: the session's first frame is ready, or `timed_out` ([`READY_TIMEOUT`]
/// passed). Pure, so it is unit-testable without a compositor.
pub fn login_ready(inputs: ReadyInputs, timed_out: bool) -> bool {
    timed_out
        || (inputs.session_started
            && inputs.outputs > 0
            && inputs.outputs_with_background >= inputs.outputs
            && inputs.awaited_spawns == 0
            && inputs.windows_without_buffer == 0)
}

/// What the greeter's ready rule (plan 46 WP-1 invariant 3) looks at, sampled from the greeter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GreeterReadyInputs {
    /// Whether a wallpaper client (swaybg) is running, so a background buffer is expected.
    pub background_expected: bool,
    /// Usable outputs enumerated (mapped in the Space).
    pub outputs: usize,
    /// Of those, the outputs whose background-layer buffer has committed.
    pub outputs_with_background: usize,
}

/// The greeter's ready rule: every output's background buffer committed (when one is expected),
/// or `timed_out` ([`READY_TIMEOUT`]). The rule is armed after `start_greeter` built the panel, so
/// its elements are prepared for the first adoption frame; a rendered panel frame is deliberately
/// not awaited (a background start renders nothing until adoption at activation). Holds before
/// activation. Pure.
pub fn greeter_ready(inputs: GreeterReadyInputs, timed_out: bool) -> bool {
    timed_out
        || (inputs.outputs > 0
            && (!inputs.background_expected || inputs.outputs_with_background >= inputs.outputs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn greeter_ready_waits_for_backgrounds() {
        let full = GreeterReadyInputs {
            background_expected: true,
            outputs: 2,
            outputs_with_background: 2,
        };
        assert!(greeter_ready(full, false));
        let partial = GreeterReadyInputs {
            outputs_with_background: 1,
            ..full
        };
        assert!(!greeter_ready(partial, false));
        assert!(greeter_ready(partial, true), "the timeout always holds");
        let no_outputs = GreeterReadyInputs {
            outputs: 0,
            outputs_with_background: 0,
            ..full
        };
        assert!(!greeter_ready(no_outputs, false));
    }

    #[test]
    fn greeter_ready_without_swaybg_needs_no_background() {
        let inputs = GreeterReadyInputs {
            background_expected: false,
            outputs: 1,
            outputs_with_background: 0,
        };
        assert!(greeter_ready(inputs, false));
    }

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
    fn foreground_start_adopts_on_wallpaper_until_the_timeout_closes_the_window() {
        let mut p = StartupPhase::at_start(true);
        assert_eq!(p, StartupPhase::AwaitingWallpaper);
        assert!(p.adopts_on_wallpaper() && p.defers_adoption() && !p.background());
        assert!(
            !p.advance(StartupEvent::Activated),
            "a VT switch back is no transition"
        );
        assert!(p.advance(StartupEvent::WindowClosed));
        assert_eq!(p, StartupPhase::Running);
        assert!(!p.adopts_on_wallpaper() && !p.defers_adoption());
        assert!(!p.advance(StartupEvent::WindowClosed) && !p.advance(StartupEvent::Activated));
    }

    #[test]
    fn background_start_adopts_only_through_activation() {
        let mut p = StartupPhase::at_start(false);
        assert_eq!(p, StartupPhase::AwaitingActivation);
        assert!(p.background() && p.defers_adoption());
        assert!(
            !p.adopts_on_wallpaper(),
            "a wallpaper landing adopts nothing"
        );
        assert!(
            !p.advance(StartupEvent::WindowClosed),
            "no timeout closes it"
        );
        assert!(p.advance(StartupEvent::Activated));
        // Activation: deferred devices still defer their outputs, then the close adopts them all.
        assert!(!p.background() && p.defers_adoption());
        assert!(
            !p.advance(StartupEvent::Activated),
            "only the first activation"
        );
        assert!(p.advance(StartupEvent::WindowClosed));
        assert_eq!(p, StartupPhase::Running);
    }

    fn ready() -> ReadyInputs {
        ReadyInputs {
            session_started: true,
            outputs: 2,
            outputs_with_background: 2,
            awaited_spawns: 0,
            windows_without_buffer: 0,
        }
    }

    #[test]
    fn login_ready_needs_every_condition_short_of_the_timeout() {
        assert!(login_ready(ready(), false));
        let not = [
            ReadyInputs {
                session_started: false,
                ..ready()
            },
            ReadyInputs {
                outputs: 0,
                outputs_with_background: 0,
                ..ready()
            },
            ReadyInputs {
                outputs_with_background: 1,
                ..ready()
            },
            ReadyInputs {
                awaited_spawns: 1,
                ..ready()
            },
            ReadyInputs {
                windows_without_buffer: 1,
                ..ready()
            },
        ];
        for inputs in not {
            assert!(!login_ready(inputs, false), "{inputs:?}");
            assert!(login_ready(inputs, true), "the timeout holds: {inputs:?}");
        }
    }
}
