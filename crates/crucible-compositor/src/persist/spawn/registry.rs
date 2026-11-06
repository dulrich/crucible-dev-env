//! The PID-provenance registry: every CDE-spawned child's handle with its [`SpawnRecord`]
//! ([`SpawnKind`], program, spawn time, origin), reaped through its own handle.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Child;
use std::time::{Instant, SystemTime};

use crucible_persist::WindowRecord;

/// One CDE-spawned child: its handle and what it was spawned as.
#[derive(Debug)]
struct Spawned {
    child: Child,
    record: SpawnRecord,
}

/// The PID-provenance registry (a field of [`crate::persist::PersistState`]): every CDE-spawned child's
/// handle together with its [`SpawnRecord`]. A child and its record live and die together —
/// [`Self::reap`] drops both, so a recycled PID is never classified from a dead spawn.
#[derive(Debug, Default)]
pub(in crate::persist) struct SpawnRegistry {
    by_pid: HashMap<u32, Spawned>,
}

impl SpawnRegistry {
    /// Registers a freshly spawned `child` with its `record`; returns its PID.
    pub(in crate::persist) fn insert(&mut self, child: Child, record: SpawnRecord) -> u32 {
        let pid = child.id();
        self.by_pid.insert(pid, Spawned { child, record });
        pid
    }

    /// The record of the live (not yet reaped) CDE spawn `pid`.
    #[must_use]
    pub(in crate::persist) fn get(&self, pid: u32) -> Option<&SpawnRecord> {
        self.by_pid.get(&pid).map(|s| &s.record)
    }

    /// Kills the held child `pid` (SIGKILL via its own handle; the next [`Self::reap`] collects
    /// it). Returns `false` when CDE holds no live handle for `pid`.
    pub(in crate::persist) fn kill(&mut self, pid: u32) -> bool {
        self.by_pid
            .get_mut(&pid)
            .is_some_and(|s| s.child.kill().is_ok())
    }

    /// Reaps exited children through their own handles (`try_wait`), dropping each **with its
    /// record** and returning their PIDs. Children still running are kept; a `try_wait` error
    /// drops the child too. Never a reap-any call: XWayland's children are not ours.
    pub(in crate::persist) fn reap(&mut self) -> Vec<u32> {
        let mut reaped = Vec::new();
        self.by_pid.retain(|pid, s| match s.child.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) | Err(_) => {
                reaped.push(*pid);
                false
            }
        });
        reaped.sort_unstable();
        reaped
    }

    /// Whether `pid` itself is a live [`SpawnKind::Terminal`] spawn (exact pid; ancestors never
    /// count, so an app launched from a terminal is not a terminal).
    #[must_use]
    pub(in crate::persist) fn is_terminal_spawn(&self, pid: u32) -> bool {
        self.get(pid)
            .is_some_and(|rec| rec.kind == SpawnKind::Terminal)
    }
}

/// Grant lookup scans the live records (a handful of spawns; no second index to keep in sync with
/// reap): a reaped spawn's grant is gone with its record, a used one is `None`.
impl crate::persist::identity::Grants for SpawnRegistry {
    fn lookup(&self, grant: &str) -> Option<(u32, PathBuf)> {
        self.by_pid.iter().find_map(|(pid, s)| {
            (s.record.grant.as_deref() == Some(grant))
                .then(|| s.record.exe.clone().map(|exe| (*pid, exe)))
                .flatten()
        })
    }

    fn consume(&mut self, grant: &str) {
        for s in self.by_pid.values_mut() {
            if s.record.grant.as_deref() == Some(grant) {
                s.record.grant = None;
            }
        }
    }
}

/// What a CDE-spawned process is, remembered per PID so a mapped window can be classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnKind {
    /// A terminal (captured with its cwd and foreground command).
    Terminal,
    /// An application launched by `run` (recorded as an `app`).
    App,
    /// A CDE overlay such as the picker (never captured).
    Overlay,
}

/// A CDE-spawned process: what it is, what it ran and when (for the spawn-to-map latency line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRecord {
    /// What the process is.
    pub kind: SpawnKind,
    /// The program it was spawned as.
    pub program: String,
    /// Monotonic spawn time.
    pub at: Instant,
    /// Wall-clock spawn time, ms since the UNIX epoch.
    pub epoch_ms: u128,
    /// The saved record this spawn stands for (a restored window, or a relaunched app), carried
    /// to map time so capture records it instead of reverse-engineering CDE's own wrappers.
    pub origin: Option<WindowRecord>,
    /// The single-use window-identity grant (`CRUCIBLE_WINDOW_GRANT`) of a `cterm` terminal spawn;
    /// `None` once a bind consumed it, and gone with the record on reap.
    pub grant: Option<String>,
    /// The canonical executable resolved at mint, which a binding peer must run.
    pub exe: Option<PathBuf>,
    /// The `CRUCIBLE_TERM_SESSION` key exported with the grant (32 lowercase hex chars), copied
    /// into the window's identity at bind. Only a granted `cterm` spawn carries one.
    pub session: Option<String>,
}

impl SpawnRecord {
    /// A record of `program` spawned as `kind` right now.
    #[must_use]
    pub fn now(kind: SpawnKind, program: &str) -> Self {
        Self {
            kind,
            program: program.to_owned(),
            at: Instant::now(),
            epoch_ms: epoch_ms_now(),
            origin: None,
            grant: None,
            exe: None,
            session: None,
        }
    }

    /// Whole milliseconds from the spawn to `now` (0 if `now` is earlier).
    #[must_use]
    pub fn ms_until(&self, now: Instant) -> u128 {
        now.saturating_duration_since(self.at).as_millis()
    }
}

/// Wall-clock milliseconds since the UNIX epoch (0 if the clock is before it).
pub(in crate::persist) fn epoch_ms_now() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn spawn(program: &str, args: &[&str]) -> Child {
        std::process::Command::new(program)
            .args(args)
            .spawn()
            .unwrap()
    }

    fn end_all(reg: &mut SpawnRegistry) {
        for s in reg.by_pid.values_mut() {
            let _ = s.child.kill();
            let _ = s.child.wait();
        }
    }

    #[test]
    fn registry_kill_ends_only_a_held_child() {
        let mut reg = SpawnRegistry::default();
        let pid = reg.insert(
            spawn("/bin/sleep", &["30"]),
            SpawnRecord::now(SpawnKind::App, "sleep"),
        );
        assert!(!reg.kill(pid.wrapping_add(1_000_000)));
        assert!(reg.kill(pid));
        let status = reg.by_pid.get_mut(&pid).unwrap().child.wait().unwrap();
        assert!(!status.success(), "killed, not exited cleanly");
    }

    /// Reaps `reg` until something exits (or ~2 s pass).
    fn reap_some(reg: &mut SpawnRegistry) -> Vec<u32> {
        for _ in 0..40 {
            let reaped = reg.reap();
            if !reaped.is_empty() {
                return reaped;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Vec::new()
    }

    #[test]
    fn registry_reap_drops_only_exited_children_with_their_records() {
        let mut reg = SpawnRegistry::default();
        let quick = reg.insert(
            spawn("/bin/true", &[]),
            SpawnRecord::now(SpawnKind::App, "true"),
        );
        let slow = reg.insert(
            spawn("/bin/sleep", &["30"]),
            SpawnRecord::now(SpawnKind::App, "sleep"),
        );
        assert_eq!(reap_some(&mut reg), vec![quick]);
        assert_eq!(reg.by_pid.len(), 1);
        assert!(reg.get(quick).is_none());
        assert!(reg.get(slow).is_some());
        end_all(&mut reg);
    }

    /// B2: a reaped terminal's record goes with its handle, so the recycled PID of an unrelated
    /// client is not classified from the dead spawn.
    #[test]
    fn reaped_terminal_pid_is_no_longer_a_terminal_spawn() {
        let mut reg = SpawnRegistry::default();
        let pid = reg.insert(
            spawn("/bin/true", &[]),
            SpawnRecord::now(SpawnKind::Terminal, "term"),
        );
        assert!(reg.is_terminal_spawn(pid));
        assert_eq!(reap_some(&mut reg), vec![pid]);
        assert!(!reg.is_terminal_spawn(pid));
        assert!(reg.get(pid).is_none());
        assert!(!reg.kill(pid));
    }

    #[test]
    fn spawn_record_measures_elapsed_ms_and_saturates() {
        let rec = SpawnRecord::now(SpawnKind::App, "x");
        assert_eq!(rec.ms_until(rec.at + Duration::from_millis(250)), 250);
        assert_eq!(rec.ms_until(rec.at), 0);
        assert!(rec.epoch_ms > 0);
    }

    #[test]
    fn is_terminal_spawn_is_the_exact_pid_kind_only() {
        let mut reg = SpawnRegistry::default();
        let t = reg.insert(
            spawn("/bin/sleep", &["30"]),
            SpawnRecord::now(SpawnKind::Terminal, "t"),
        );
        let a = reg.insert(
            spawn("/bin/sleep", &["30"]),
            SpawnRecord::now(SpawnKind::App, "a"),
        );
        let o = reg.insert(
            spawn("/bin/sleep", &["30"]),
            SpawnRecord::now(SpawnKind::Overlay, "o"),
        );
        assert!(reg.is_terminal_spawn(t));
        assert!(!reg.is_terminal_spawn(a));
        assert!(!reg.is_terminal_spawn(o));
        // a pid CDE never spawned (e.g. firefox launched from the terminal) is not a terminal;
        // ancestors never decide the class
        assert!(!reg.is_terminal_spawn(t.wrapping_add(1_000_000)));
        end_all(&mut reg);
    }
}
