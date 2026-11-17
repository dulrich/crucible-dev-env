//! The login readiness signal (plan 45 WP-1 invariants 3-5) and session channel (plan 46 WP-1).
//!
//! A login daemon starting CDE (or the greeter) on a background VT passes the write end of a pipe
//! as `CDE_LOGIN_READY_FD=<n>`. CDE takes it once at start ([`take_from_env`]: the variable leaves
//! the environment and the fd becomes close-on-exec, so no child inherits either), then writes
//! `ready\n` the first time its ready rule holds ([`crate::startup::login_ready`], or in greeter
//! mode [`crate::startup::greeter_ready`]), or at its [`crate::startup::READY_TIMEOUT`]. The
//! `startup: login ready` line records the time whether or not an fd was passed.
//!
//! A session keeps the fd open after `ready\n`: it is the one-way session channel, which carries
//! at most one more line, `quit\n`, when the user quits ([`QuitPhase`]). The greeter closes it
//! after `ready\n` (it sends no quit notice).

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::File;
use std::io::Write as _;
use std::os::fd::{FromRawFd as _, OwnedFd, RawFd};
use std::sync::atomic::Ordering;
use std::time::Duration;

use smithay::output::Output;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use tracing::{info, warn};

use crate::startup::{
    GreeterReadyInputs, READY_TIMEOUT, ReadyInputs, elapsed_ms, greeter_ready, login_ready,
};
use crate::state::{Backend, CrucibleState};

/// The environment variable naming the inherited readiness fd.
pub const ENV: &str = "CDE_LOGIN_READY_FD";

/// How long a quit with a delivered notice waits for the session pause before exiting anyway.
pub const QUIT_TIMEOUT: Duration = Duration::from_secs(10);

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

/// The inherited readiness fd (the session channel), owned (closed on drop) and close-on-exec.
#[derive(Debug)]
pub struct ReadyFd(File);

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
        Ok(Self(File::from(owned)))
    }

    /// Writes `ready\n`; the fd stays open (the session channel).
    pub fn signal(&self) -> std::io::Result<()> {
        (&self.0).write_all(b"ready\n")
    }

    /// Writes `quit\n`; whether it was delivered (any write error, EPIPE included, is not: Rust
    /// ignores SIGPIPE, so a gone reader is an error here).
    #[must_use]
    pub fn send_quit(&self) -> bool {
        match (&self.0).write_all(b"quit\n") {
            Ok(()) => true,
            Err(err) => {
                warn!(%err, "cannot write the quit notice; exiting at once");
                false
            }
        }
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

/// The quit sequence (plan 46 WP-1 invariant 2): pure, so its transitions are unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QuitPhase {
    /// No quit yet.
    #[default]
    Running,
    /// `quit\n` was delivered; waiting for the session pause (or [`QUIT_TIMEOUT`]).
    AwaitingPause,
    /// The loop must stop.
    Exiting,
}

impl QuitPhase {
    /// A quit request; `notice_delivered` reports whether `quit\n` reached the daemon (only asked
    /// on the first quit). Returns whether to exit now.
    pub fn on_quit(&mut self, notice_delivered: impl FnOnce() -> bool) -> bool {
        *self = match *self {
            Self::Running if notice_delivered() => Self::AwaitingPause,
            _ => Self::Exiting,
        };
        *self == Self::Exiting
    }

    /// The session paused (a VT switch away). Returns whether to exit now.
    pub fn on_pause(&mut self) -> bool {
        self.finish_wait()
    }

    /// The [`QUIT_TIMEOUT`] passed. Returns whether to exit now.
    pub fn on_timeout(&mut self) -> bool {
        self.finish_wait()
    }

    fn finish_wait(&mut self) -> bool {
        if *self == Self::AwaitingPause {
            *self = Self::Exiting;
            true
        } else {
            false
        }
    }

    /// Whether a delivered notice is waiting for the pause.
    #[must_use]
    pub fn awaiting(self) -> bool {
        self == Self::AwaitingPause
    }
}

/// The ready-rule tracking (a field of [`CrucibleState`]); inert until [`CrucibleState::arm_login_ready`].
#[derive(Debug, Default)]
pub struct LoginReady {
    armed: bool,
    done: bool,
    timed_out: bool,
    /// Greeter mode: the greeter rule, and the fd closes after `ready\n`.
    greeter: bool,
    /// The readiness fd; in a session it stays open after `ready\n` as the session channel.
    fd: Option<ReadyFd>,
    /// The PIDs the session step spawned for the active workspaces; `None` until it ran.
    awaited: Option<Vec<u32>>,
    /// Names of the outputs a background-layer buffer landed on.
    backgrounds: HashSet<String>,
    /// The quit sequence.
    quit: QuitPhase,
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

    /// Sends `quit\n` on the session channel; delivered only once `ready\n` went out on a live
    /// channel (never in greeter mode, whose fd is closed by then).
    fn send_quit(&self) -> bool {
        self.done && self.fd.as_ref().is_some_and(ReadyFd::send_quit)
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Starts the ready-rule tracking with the inherited readiness fd (if any) and arms its
    /// [`READY_TIMEOUT`]; `greeter` selects the greeter's rule. Call just before the main loop.
    pub fn arm_login_ready(&mut self, fd: Option<ReadyFd>, greeter: bool) {
        self.login_ready.armed = true;
        self.login_ready.greeter = greeter;
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

    fn output_counts(&self) -> (usize, usize) {
        let names: Vec<String> = self.space.outputs().map(Output::name).collect();
        let landed = names
            .iter()
            .filter(|n| self.login_ready.backgrounds.contains(*n))
            .count();
        (names.len(), landed)
    }

    /// The ready-rule inputs sampled from the live state.
    fn ready_inputs(&self) -> ReadyInputs {
        let (outputs, outputs_with_background) = self.output_counts();
        let awaited = self.login_ready.awaited.as_deref();
        ReadyInputs {
            session_started: awaited.is_some(),
            outputs,
            outputs_with_background,
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

    /// The greeter's ready-rule inputs sampled from the live state.
    fn greeter_ready_inputs(&self) -> GreeterReadyInputs {
        let (outputs, outputs_with_background) = self.output_counts();
        let swaybg = self.greeter_has_swaybg();
        GreeterReadyInputs {
            panel_prepared: swaybg.is_some(),
            background_expected: swaybg.unwrap_or(false),
            outputs,
            outputs_with_background,
        }
    }

    /// Evaluates the ready rule; the first time it holds, signals the readiness fd (if any) and
    /// logs `startup: login ready`. Cheap no-op once done or when not armed.
    pub fn check_login_ready(&mut self) {
        if !self.login_ready.pending() {
            return;
        }
        let timed_out = self.login_ready.timed_out;
        if self.login_ready.greeter {
            let inputs = self.greeter_ready_inputs();
            if !greeter_ready(inputs, timed_out) {
                return;
            }
            let signalled = self.signal_ready();
            // The greeter has no quit notice: close the fd now.
            self.login_ready.fd = None;
            info!(
                elapsed_ms = elapsed_ms(),
                timed_out,
                signalled,
                greeter = true,
                ?inputs,
                "startup: login ready"
            );
        } else {
            let inputs = self.ready_inputs();
            if !login_ready(inputs, timed_out) {
                return;
            }
            let signalled = self.signal_ready();
            info!(
                elapsed_ms = elapsed_ms(),
                timed_out,
                signalled,
                ?inputs,
                "startup: login ready"
            );
        }
    }

    /// Marks the rule done and writes `ready\n` (if an fd was passed); whether one was. A failed
    /// write drops the fd (no channel: a later quit exits at once).
    fn signal_ready(&mut self) -> bool {
        self.login_ready.done = true;
        let Some(fd) = self.login_ready.fd.as_ref() else {
            return false;
        };
        if let Err(err) = fd.signal() {
            warn!(%err, "cannot write the login readiness fd");
            self.login_ready.fd = None;
        }
        true
    }

    /// `Action::Quit` after the flush: sends the quit notice on a live session channel and keeps
    /// running until the session pauses or [`QUIT_TIMEOUT`]; else (no channel, not delivered, or
    /// a second quit) stops the loop at once.
    pub(crate) fn begin_quit(&mut self) {
        let mut quit = self.login_ready.quit;
        let second = quit.awaiting();
        let exit = quit.on_quit(|| self.login_ready.send_quit());
        self.login_ready.quit = quit;
        if exit {
            if second {
                info!("logout: second quit, exiting");
            }
            self.running.store(false, Ordering::SeqCst);
            return;
        }
        info!("logout: quit notice sent");
        let timer = self
            .handle
            .insert_source(Timer::from_duration(QUIT_TIMEOUT), |_, _, state| {
                if state.login_ready.quit.on_timeout() {
                    info!("logout: timeout, exiting");
                    state.running.store(false, Ordering::SeqCst);
                }
                TimeoutAction::Drop
            });
        if let Err(err) = timer {
            warn!(%err, "cannot arm the logout timeout; exiting at once");
            self.running.store(false, Ordering::SeqCst);
        }
    }

    /// The session paused: after a delivered quit notice, stop the loop.
    pub(crate) fn quit_on_pause(&mut self) {
        if self.login_ready.quit.on_pause() {
            info!("logout: paused, exiting");
            self.running.store(false, Ordering::SeqCst);
        }
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

    fn adopted_pipe() -> (std::io::PipeReader, ReadyFd, RawFd) {
        let (rd, wr) = std::io::pipe().expect("pipe");
        let raw = OwnedFd::from(wr).into_raw_fd();
        let fd = ReadyFd::adopt(raw).expect("adopt an open fd");
        (rd, fd, raw)
    }

    #[test]
    fn channel_carries_ready_then_quit_and_stays_cloexec() {
        let (mut rd, fd, raw) = adopted_pipe();
        // SAFETY: `raw` is open and owned by `fd`; only its flags are read.
        #[allow(unsafe_code)]
        let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
        assert!(flags & libc::FD_CLOEXEC != 0, "close-on-exec once adopted");
        fd.signal().expect("write");
        // SAFETY: as above; the fd is still open after `ready`.
        #[allow(unsafe_code)]
        let still_open = unsafe { libc::fcntl(raw, libc::F_GETFD) };
        assert!(still_open & libc::FD_CLOEXEC != 0, "kept open after ready");
        assert!(fd.send_quit());
        drop(fd);
        let mut got = String::new();
        // EOF after the lines proves the write end was closed on drop.
        rd.read_to_string(&mut got).expect("read to EOF");
        assert_eq!(got, "ready\nquit\n");
    }

    #[test]
    fn quit_to_a_gone_reader_is_not_delivered() {
        let (rd, fd, _) = adopted_pipe();
        drop(rd);
        assert!(!fd.send_quit(), "EPIPE is not delivered");
    }

    #[test]
    fn quit_before_ready_or_without_channel_is_not_delivered() {
        let (_rd, fd, _) = adopted_pipe();
        let mut lr = LoginReady {
            fd: Some(fd),
            ..LoginReady::default()
        };
        assert!(!lr.send_quit(), "ready not sent yet");
        lr.done = true;
        assert!(lr.send_quit());
        lr.fd = None;
        assert!(!lr.send_quit(), "no channel");
    }

    #[test]
    fn quit_notice_then_pause_exits() {
        let mut q = QuitPhase::default();
        assert!(!q.on_pause(), "a pause without a quit is a VT switch");
        assert!(!q.on_quit(|| true));
        assert!(q.awaiting());
        assert!(q.on_pause());
        assert_eq!(q, QuitPhase::Exiting);
        assert!(!q.on_timeout(), "the later timer is a no-op");
    }

    #[test]
    fn quit_notice_then_timeout_exits() {
        let mut q = QuitPhase::default();
        assert!(!q.on_quit(|| true));
        assert!(q.on_timeout());
        assert!(!q.on_pause());
    }

    #[test]
    fn second_quit_exits_without_a_second_notice() {
        let mut q = QuitPhase::default();
        assert!(!q.on_quit(|| true));
        assert!(q.on_quit(|| panic!("no second notice")));
    }

    #[test]
    fn undelivered_quit_exits_at_once() {
        let mut q = QuitPhase::default();
        assert!(q.on_quit(|| false));
        assert_eq!(q, QuitPhase::Exiting);
    }
}
