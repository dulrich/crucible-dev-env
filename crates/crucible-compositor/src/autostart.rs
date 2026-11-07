//! Once-per-process `autostart` command launching.
//!
//! The pure [`Startup`] guarantees the configured `autostart` lines run at most once per CDE
//! process, however many first-XWayland-outcome sites reach it. [`CrucibleState::run_autostart`]
//! is called from each such site in [`crate::xwayland`] and never from config hot reload.

use std::ffi::OsString;

use tracing::{info, warn};

use crate::{
    persist::{SpawnKind, SpawnRequest},
    state::{Backend, CrucibleState},
};

/// Where the first XWayland outcome (or its fallback) was observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeSite {
    /// `XWaylandEvent::Ready` (with or without a window manager attached).
    Ready,
    /// `XWaylandEvent::Error`: XWayland crashed on startup.
    Crashed,
    /// `XWayland::spawn` failed, or its source could not be inserted into the event loop.
    StartFailed,
    /// The bounded fallback timer fired with no outcome yet.
    Timer,
}

/// What a site's arrival makes the caller run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OutcomeActions {
    /// Start the session (restore / picker / warm overlay host).
    pub start_session: bool,
    /// Run the `autostart` commands (after the session start).
    pub autostart: bool,
}

/// The startup sequence: the session starts at the very first site of any kind, autostart at the
/// first real XWayland outcome (never the fallback timer, never before the session start), each
/// at most once.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Startup {
    /// No site has arrived.
    #[default]
    Waiting,
    /// The session has started (by the fallback timer); autostart still awaits a real outcome.
    SessionStarted,
    /// Both the session start and autostart have run.
    Done,
}

impl Startup {
    /// Advances on `site` and returns what the caller runs now.
    pub fn advance(&mut self, site: OutcomeSite) -> OutcomeActions {
        let real = site != OutcomeSite::Timer;
        let (next, actions) = match (*self, real) {
            (Self::Waiting, true) => (
                Self::Done,
                OutcomeActions {
                    start_session: true,
                    autostart: true,
                },
            ),
            (Self::Waiting, false) => (
                Self::SessionStarted,
                OutcomeActions {
                    start_session: true,
                    autostart: false,
                },
            ),
            (Self::SessionStarted, true) => (
                Self::Done,
                OutcomeActions {
                    start_session: false,
                    autostart: true,
                },
            ),
            (state, _) => (state, OutcomeActions::default()),
        };
        *self = next;
        actions
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Handles one XWayland outcome site: starts the session at the first site, then runs the
    /// autostart commands at the first real outcome. Each happens at most once per process.
    pub fn on_xwayland_outcome(&mut self, site: OutcomeSite) {
        let actions = self.startup.advance(site);
        if actions.start_session {
            self.start_session();
        }
        if actions.autostart {
            self.run_autostart();
        }
    }

    /// Runs each `autostart` command (as `/bin/sh -c <cmd>`, in file order) once per process.
    ///
    /// Reads the config current at fire time. No `exec` prefix: lines may be compound. A failed
    /// spawn warns and the remaining commands still run.
    pub fn run_autostart(&mut self) {
        let commands = self.config.autostart.clone();
        if commands.is_empty() {
            return;
        }
        info!(count = commands.len(), "running autostart commands");
        for cmd in commands {
            let args = vec![OsString::from("-c"), OsString::from(&cmd)];
            if self
                .spawn(SpawnRequest::new("/bin/sh", args, SpawnKind::App))
                .is_none()
            {
                warn!(command = %cmd, "autostart command failed to spawn");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_starts_once_across_every_outcome_site() {
        use OutcomeSite::*;
        for first in [Ready, Crashed, StartFailed, Timer] {
            let mut startup = Startup::default();
            let a = startup.advance(first);
            assert!(a.start_session, "{first:?}");
            assert_eq!(a.autostart, first != Timer, "{first:?}");
            let mut starts = 1;
            let mut autos = usize::from(a.autostart);
            for later in [Ready, Crashed, StartFailed, Timer, Ready] {
                let a = startup.advance(later);
                starts += usize::from(a.start_session);
                autos += usize::from(a.autostart);
            }
            assert_eq!((starts, autos), (1, 1), "{first:?}");
        }
    }

    #[test]
    fn startup_settles_to_done_and_never_acts_again() {
        let mut startup = Startup::default();
        assert!(!startup.advance(OutcomeSite::Timer).autostart);
        assert_eq!(startup, Startup::SessionStarted);
        // The timer again does nothing; a real outcome then runs autostart only.
        assert_eq!(
            startup.advance(OutcomeSite::Timer),
            OutcomeActions::default()
        );
        let a = startup.advance(OutcomeSite::Ready);
        assert!(!a.start_session && a.autostart);
        assert_eq!(startup, Startup::Done);
        assert_eq!(
            startup.advance(OutcomeSite::Ready),
            OutcomeActions::default()
        );
    }
}
