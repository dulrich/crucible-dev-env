//! The login readiness signal (plan 45 WP-1 invariants 3-5).
//!
//! A login daemon starting CDE on a background VT passes the write end of a pipe as
//! `CDE_LOGIN_READY_FD=<n>`. CDE takes it once at start ([`take_from_env`]: the variable leaves the
//! environment and the fd becomes close-on-exec, so no child inherits either), then writes
//! `ready\n` and closes it the first time the ready rule ([`crate::startup::login_ready`]) holds,
//! or at its [`crate::startup::READY_TIMEOUT`]. The `startup: login ready` line records the time
//! whether or not an fd was passed.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::File;
use std::io::Write as _;
use std::os::fd::{FromRawFd as _, OwnedFd, RawFd};

use smithay::output::Output;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use tracing::{info, warn};

use crate::startup::{READY_TIMEOUT, ReadyInputs, elapsed_ms, login_ready};
use crate::state::{Backend, CrucibleState};

/// The environment variable naming the inherited readiness fd.
pub const ENV: &str = "CDE_LOGIN_READY_FD";

/// The readiness fd number in `value`: a decimal fd above stdio (`>= 3`), else `None`.
#[must_use]
pub fn parse_fd(value: &OsStr) -> Option<RawFd> {
    value
        .to_str()?
        .trim()
        .parse::<RawFd>()
        .ok()
        .filter(|fd| *fd >= 3)
}

/// The inherited readiness fd, owned (closed on drop) and close-on-exec.
#[derive(Debug)]
pub struct ReadyFd(OwnedFd);

impl ReadyFd {
    /// Takes ownership of the open fd `fd` and sets it close-on-exec; an fd that is not open is an
    /// error (and is not taken).
    pub fn adopt(fd: RawFd) -> std::io::Result<Self> {
        // SAFETY: `fcntl` on an arbitrary fd number only reads/sets its descriptor flags; a closed
        // fd fails with EBADF, which is checked before ownership is taken.
        #[allow(unsafe_code)]
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: as above, on an fd just shown to be open.
        #[allow(unsafe_code)]
        let set = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
        if set < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the fd is open and was handed to this process for CDE alone (the login daemon's
        // pipe end); this is its only owner, read once at start.
        #[allow(unsafe_code)]
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Self(owned))
    }

    /// Writes `ready\n` and closes the fd (consumed: the signal is sent at most once).
    pub fn signal(self) -> std::io::Result<()> {
        File::from(self.0).write_all(b"ready\n")
    }
}

/// Reads `CDE_LOGIN_READY_FD` once: removes it from the environment, then adopts the fd it names.
/// Absent: `None`. An invalid value or a bad fd logs one warning and yields `None`.
///
/// Call it at backend start, before any thread or child is spawned.
pub fn take_from_env() -> Option<ReadyFd> {
    let value = std::env::var_os(ENV)?;
    // SAFETY: called once at backend entry, before CDE spawns any thread or child process, so no
    // concurrent environment access can race this removal.
    #[allow(unsafe_code)]
    unsafe {
        std::env::remove_var(ENV);
    }
    let Some(fd) = parse_fd(&value) else {
        warn!(value = ?value, "{ENV} is not a usable fd number; no readiness signal");
        return None;
    };
    match ReadyFd::adopt(fd) {
        Ok(ready) => Some(ready),
        Err(err) => {
            warn!(fd, %err, "{ENV} names a bad fd; no readiness signal");
            None
        }
    }
}

/// The ready-rule tracking (a field of [`CrucibleState`]); inert until [`CrucibleState::arm_login_ready`].
#[derive(Debug, Default)]
pub struct LoginReady {
    armed: bool,
    done: bool,
    timed_out: bool,
    fd: Option<ReadyFd>,
    /// The PIDs the session step spawned for the active workspaces; `None` until it ran.
    awaited: Option<Vec<u32>>,
    /// Names of the outputs a background-layer buffer landed on.
    backgrounds: HashSet<String>,
}

impl LoginReady {
    /// Whether the rule is still being evaluated (armed and not yet held).
    #[must_use]
    pub fn pending(&self) -> bool {
        self.armed && !self.done
    }

    /// Whether the rule held (or timed out) already.
    #[must_use]
    pub fn done(&self) -> bool {
        self.done
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Starts the ready-rule tracking with the inherited readiness fd (if any) and arms its
    /// [`READY_TIMEOUT`]. Call just before the main loop starts (never in greeter mode).
    pub fn arm_login_ready(&mut self, fd: Option<ReadyFd>) {
        self.login_ready.armed = true;
        self.login_ready.fd = fd;
        let timer =
            self.handle
                .insert_source(Timer::from_duration(READY_TIMEOUT), |_, _, state| {
                    state.login_ready.timed_out = true;
                    state.check_login_ready();
                    TimeoutAction::Drop
                });
        if let Err(err) = timer {
            warn!(%err, "cannot arm the login ready timeout");
        }
        self.check_login_ready();
    }

    /// Records the session step's spawns for the active workspaces (from `start_session`).
    pub(crate) fn note_session_spawns(&mut self, pids: Vec<u32>) {
        self.login_ready.awaited = Some(pids);
    }

    /// A background-layer buffer landed on `output`.
    pub(crate) fn note_background_landed(&mut self, output: &Output) {
        if self.login_ready.pending() {
            self.login_ready.backgrounds.insert(output.name());
        }
    }

    /// The ready-rule inputs sampled from the live state.
    fn ready_inputs(&self) -> ReadyInputs {
        let names: Vec<String> = self.space.outputs().map(Output::name).collect();
        let awaited = self.login_ready.awaited.as_deref();
        ReadyInputs {
            session_started: awaited.is_some(),
            outputs: names.len(),
            outputs_with_background: names
                .iter()
                .filter(|n| self.login_ready.backgrounds.contains(*n))
                .count(),
            awaited_spawns: awaited
                .unwrap_or_default()
                .iter()
                .filter(|pid| self.spawn_awaits_window(**pid))
                .count(),
            windows_without_buffer: self
                .space
                .elements()
                .filter(|w| {
                    !w.wl_surface()
                        .is_some_and(|s| crate::element::surface_has_buffer(&s))
                })
                .count(),
        }
    }

    /// Evaluates the ready rule; the first time it holds, signals the readiness fd (if any) and
    /// logs `startup: login ready`. Cheap no-op once done or when not armed.
    pub fn check_login_ready(&mut self) {
        if !self.login_ready.pending() {
            return;
        }
        let timed_out = self.login_ready.timed_out;
        let inputs = self.ready_inputs();
        if !login_ready(inputs, timed_out) {
            return;
        }
        self.login_ready.done = true;
        let fd = self.login_ready.fd.take();
        let signalled = fd.is_some();
        if let Some(fd) = fd {
            if let Err(err) = fd.signal() {
                warn!(%err, "cannot write the login readiness fd");
            }
        }
        info!(
            elapsed_ms = elapsed_ms(),
            timed_out,
            signalled,
            ?inputs,
            "startup: login ready"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;
    use std::os::fd::IntoRawFd as _;

    #[test]
    fn parse_accepts_only_fds_above_stdio() {
        assert_eq!(parse_fd(OsStr::new("3")), Some(3));
        assert_eq!(parse_fd(OsStr::new(" 17\n")), Some(17));
        for bad in ["", "0", "2", "-4", "x", "3x"] {
            assert_eq!(parse_fd(OsStr::new(bad)), None, "{bad:?}");
        }
    }

    #[test]
    fn signal_writes_ready_once_then_closes() {
        let (mut rd, wr) = std::io::pipe().expect("pipe");
        let raw = OwnedFd::from(wr).into_raw_fd();
        let fd = ReadyFd::adopt(raw).expect("adopt an open fd");
        // SAFETY: `raw` is open and owned by `fd`; only its flags are read.
        #[allow(unsafe_code)]
        let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
        assert!(flags & libc::FD_CLOEXEC != 0, "close-on-exec once adopted");
        fd.signal().expect("write");
        let mut got = String::new();
        // EOF after the line proves the write end was closed.
        rd.read_to_string(&mut got).expect("read to EOF");
        assert_eq!(got, "ready\n");
    }
}
