//! Once-per-process `autostart` command launching.
//!
//! The pure [`AutostartGate`] guarantees the configured `autostart` lines run at most once per CDE
//! process, however many first-XWayland-outcome sites reach it. [`CrucibleState::run_autostart`]
//! is called from each such site in [`crate::xwayland`] and never from config hot reload.

use std::ffi::OsString;

use tracing::{info, warn};

use crate::{
    persist::{SpawnKind, SpawnRequest},
    state::{Backend, CrucibleState},
};

/// A once-only latch: [`Self::take`] returns `true` exactly once.
#[derive(Debug, Default)]
pub struct AutostartGate {
    fired: bool,
}

impl AutostartGate {
    /// `true` on the first call, `false` on every later one.
    pub fn take(&mut self) -> bool {
        !std::mem::replace(&mut self.fired, true)
    }
}

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

/// Decides what to run at `site`. The session starts at the very first site of any kind (at most
/// once); autostart waits for a real XWayland outcome, so the fallback timer never fires it.
pub fn outcome_actions(
    session: &mut AutostartGate,
    autostart: &mut AutostartGate,
    site: OutcomeSite,
) -> OutcomeActions {
    OutcomeActions {
        start_session: session.take(),
        autostart: site != OutcomeSite::Timer && autostart.take(),
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Handles one XWayland outcome site: starts the session at the first site, then runs the
    /// autostart commands at the first real outcome. Each happens at most once per process.
    pub fn on_xwayland_outcome(&mut self, site: OutcomeSite) {
        let actions = outcome_actions(&mut self.session_start_gate, &mut self.autostart_gate, site);
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
            let (mut session, mut auto) = (AutostartGate::default(), AutostartGate::default());
            let a = outcome_actions(&mut session, &mut auto, first);
            assert!(a.start_session, "{first:?}");
            assert_eq!(a.autostart, first != Timer, "{first:?}");
            let mut starts = 1;
            let mut autos = usize::from(a.autostart);
            for later in [Ready, Crashed, StartFailed, Timer, Ready] {
                let a = outcome_actions(&mut session, &mut auto, later);
                starts += usize::from(a.start_session);
                autos += usize::from(a.autostart);
            }
            assert_eq!((starts, autos), (1, 1), "{first:?}");
        }
    }

    #[test]
    fn gate_fires_once_then_never_again() {
        let mut gate = AutostartGate::default();
        assert!(gate.take());
        assert!(!gate.take());
        assert!(!gate.take());
    }
}
