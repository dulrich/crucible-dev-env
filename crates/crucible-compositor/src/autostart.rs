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

impl<B: Backend + 'static> CrucibleState<B> {
    /// Runs each `autostart` command (as `/bin/sh -c <cmd>`, in file order) once per process.
    ///
    /// Reads the config current at fire time. No `exec` prefix: lines may be compound. A failed
    /// spawn warns and the remaining commands still run.
    pub fn run_autostart(&mut self) {
        if !self.autostart_gate.take() {
            return;
        }
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
    fn gate_fires_once_then_never_again() {
        let mut gate = AutostartGate::default();
        assert!(gate.take());
        assert!(!gate.take());
        assert!(!gate.take());
    }
}
