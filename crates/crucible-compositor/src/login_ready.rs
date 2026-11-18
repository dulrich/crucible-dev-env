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

use crate::greeter::panel::LOGOUT_TEXT;
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
    /// The "Logging out" modal shows and the presentation gate is pending; the quit work runs on
    /// release ([`QuitPhase::on_release`]).
    Gated,
    /// `quit\n` was delivered; waiting for the session pause (or [`QUIT_TIMEOUT`]).
    AwaitingPause,
    /// The loop must stop.
    Exiting,
}

/// What a quit request does ([`QuitPhase::request`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitRequest {
    /// The first quit: show the modal and arm the presentation gate.
    Arm,
    /// A repeat before the gate released: nothing (no re-arm, no early exit).
    Pending,
    /// A repeat after release: exit at once.
    Exit,
}

impl QuitPhase {
    /// A quit request (the first quit arms the gate; see [`QuitRequest`]).
    pub fn request(&mut self) -> QuitRequest {
        match *self {
            Self::Running => {
                *self = Self::Gated;
                QuitRequest::Arm
            }
            Self::Gated => QuitRequest::Pending,
            Self::AwaitingPause | Self::Exiting => {
                *self = Self::Exiting;
                QuitRequest::Exit
            }
        }
    }

    /// The gate released: the first quit's work. `notice_delivered` reports whether `quit\n`
    /// reached the daemon (asked only here, once). Returns whether to exit now; nothing unless a
    /// request was pending.
    pub fn on_release(&mut self, notice_delivered: impl FnOnce() -> bool) -> bool {
        if *self != Self::Gated {
            return false;
        }
        *self = if notice_delivered() {
            Self::AwaitingPause
        } else {
            Self::Exiting
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

/// Which ready rule applies, chosen when the rule is armed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyRule {
    /// The session's rule ([`login_ready`]); the fd stays open after `ready\n` (the session channel).
    Session,
    /// The greeter's rule ([`greeter_ready`]); the fd closes after `ready\n`.
    Greeter,
}

/// The readiness signal's phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ReadyPhase {
    /// Not armed yet.
    #[default]
    Inert,
    /// Armed: `rule` is evaluated until it holds or [`READY_TIMEOUT`] passed (`timed_out`).
    Pending { rule: ReadyRule, timed_out: bool },
    /// The rule held (or timed out); `ready\n` went out if an fd was passed.
    Signalled,
}

/// The ready-rule tracking (a field of [`CrucibleState`]); inert until [`CrucibleState::arm_login_ready`].
#[derive(Debug, Default)]
pub struct LoginReady {
    phase: ReadyPhase,
    /// The readiness fd; in a session it stays open after `ready\n` as the session channel.
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
        matches!(self.phase, ReadyPhase::Pending { .. })
    }

    /// Whether the rule held (or timed out) already.
    #[must_use]
    pub fn done(&self) -> bool {
        self.phase == ReadyPhase::Signalled
    }

    fn arm(&mut self, fd: Option<ReadyFd>, rule: ReadyRule) {
        self.phase = ReadyPhase::Pending {
            rule,
            timed_out: false,
        };
        self.fd = fd;
    }

    fn time_out(&mut self) {
        if let ReadyPhase::Pending { timed_out, .. } = &mut self.phase {
            *timed_out = true;
        }
    }

    /// Marks the rule signalled and writes `ready\n` (if an fd was passed); whether one was. A
    /// failed write drops the fd (no channel: a later quit exits at once); the greeter's rule
    /// closes it (no quit notice).
    fn signal(&mut self, rule: ReadyRule) -> bool {
        self.phase = ReadyPhase::Signalled;
        let Some(fd) = self.fd.as_ref() else {
            return false;
        };
        if let Err(err) = fd.signal() {
            warn!(%err, "cannot write the login readiness fd");
            self.fd = None;
        }
        if rule == ReadyRule::Greeter {
            self.fd = None;
        }
        true
    }

    /// Sends `quit\n` on the session channel; delivered only once `ready\n` went out on a live
    /// channel (never in greeter mode, whose fd is closed by then).
    fn send_quit(&self) -> bool {
        self.done() && self.fd.as_ref().is_some_and(ReadyFd::send_quit)
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Starts the ready-rule tracking with the inherited readiness fd (if any) and arms its
    /// [`READY_TIMEOUT`]; `rule` selects the session's or the greeter's rule. Call just before the
    /// main loop (from [`CrucibleState::finish_mode_start`]).
    pub(crate) fn arm_login_ready(&mut self, fd: Option<ReadyFd>, rule: ReadyRule) {
        self.login_ready.arm(fd, rule);
        let timer =
            self.handle
                .insert_source(Timer::from_duration(READY_TIMEOUT), |_, _, state| {
                    state.login_ready.time_out();
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

    /// The one background-landed hook: a background buffer committed on `output`. Feeds the
    /// start-up diagnostic, the readiness set (while pending) and the backend's adoption.
    pub(crate) fn background_landed(&mut self, output: &Output) {
        crate::startup::note_background_buffer_committed();
        if self.login_ready.pending() {
            self.login_ready.backgrounds.insert(output.name());
        }
        B::background_buffer_committed(self, output);
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

    /// The greeter's ready-rule inputs (armed only after `start_greeter`, so swaybg is known).
    fn greeter_ready_inputs(&self) -> GreeterReadyInputs {
        let (outputs, outputs_with_background) = self.output_counts();
        GreeterReadyInputs {
            background_expected: self.greeter_has_swaybg().unwrap_or(false),
            outputs,
            outputs_with_background,
        }
    }

    /// Evaluates the armed rule once; the first time it holds, signals the readiness fd (if any)
    /// and logs `startup: login ready`. Cheap no-op once done or when not armed.
    pub fn check_login_ready(&mut self) {
        let ReadyPhase::Pending { rule, timed_out } = self.login_ready.phase else {
            return;
        };
        let (holds, inputs): (bool, Box<dyn std::fmt::Debug>) = match rule {
            ReadyRule::Session => {
                let i = self.ready_inputs();
                (login_ready(i, timed_out), Box::new(i))
            }
            ReadyRule::Greeter => {
                let i = self.greeter_ready_inputs();
                (greeter_ready(i, timed_out), Box::new(i))
            }
        };
        if !holds {
            return;
        }
        let signalled = self.login_ready.signal(rule);
        info!(
            elapsed_ms = elapsed_ms(),
            timed_out,
            signalled,
            greeter = rule == ReadyRule::Greeter,
            ?inputs,
            "startup: login ready"
        );
    }

    /// `Action::Quit` after the flush (plan 49): the first quit shows the "Logging out" notice
    /// ([`CrucibleState::show_handoff_notice`]); its gate release runs [`Self::quit_released`]. A repeat before the release does nothing; after it, the
    /// repeat stops the loop at once.
    pub(crate) fn begin_quit(&mut self) {
        match self.quit.request() {
            QuitRequest::Pending => info!("logout: repeat quit while the modal is held; ignored"),
            QuitRequest::Exit => {
                info!("logout: second quit, exiting");
                self.running.store(false, Ordering::SeqCst);
            }
            QuitRequest::Arm => {
                self.clear_pointer_focus();
                // The gate drops its continuation when already armed (never in a session): then
                // the quit work runs at once rather than being lost.
                let then = Box::new(|state: &mut Self| state.quit_released());
                if !self.show_handoff_notice(LOGOUT_TEXT, "logout", then) {
                    warn!("present gate already armed; quitting unpresented");
                    self.quit_released();
                }
            }
        }
    }

    /// The gate released (presented or deadline): what `begin_quit` did before the modal. Sends
    /// the quit notice on a live session channel and keeps running until the session pauses or
    /// [`QUIT_TIMEOUT`]; else (no channel or not delivered) stops the loop at once.
    fn quit_released(&mut self) {
        let login_ready = &self.login_ready;
        if self.quit.on_release(|| login_ready.send_quit()) {
            self.running.store(false, Ordering::SeqCst);
            return;
        }
        if !self.quit.awaiting() {
            return;
        }
        info!("logout: quit notice sent");
        let timer = self
            .handle
            .insert_source(Timer::from_duration(QUIT_TIMEOUT), |_, _, state| {
                if state.quit.on_timeout() {
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
        if self.quit.on_pause() {
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
        lr.phase = ReadyPhase::Signalled;
        assert!(lr.send_quit());
        lr.fd = None;
        assert!(!lr.send_quit(), "no channel");
    }

    #[test]
    fn readiness_goes_pending_then_signalled_and_the_session_keeps_its_channel() {
        let (mut rd, fd, _) = adopted_pipe();
        let mut lr = LoginReady::default();
        assert!(!lr.pending() && !lr.done(), "inert until armed");
        lr.arm(Some(fd), ReadyRule::Session);
        assert!(lr.pending() && !lr.done());
        assert!(lr.signal(ReadyRule::Session), "an fd was passed");
        assert!(lr.done() && !lr.pending());
        assert!(lr.send_quit(), "the session channel stays open");
        drop(lr);
        let mut got = String::new();
        rd.read_to_string(&mut got).expect("read to EOF");
        assert_eq!(got, "ready\nquit\n");
    }

    #[test]
    fn readiness_timeout_is_recorded_only_while_pending() {
        let mut lr = LoginReady::default();
        lr.time_out();
        assert_eq!(lr.phase, ReadyPhase::Inert, "no timeout before arming");
        lr.arm(None, ReadyRule::Greeter);
        lr.time_out();
        assert_eq!(
            lr.phase,
            ReadyPhase::Pending {
                rule: ReadyRule::Greeter,
                timed_out: true
            }
        );
        assert!(!lr.signal(ReadyRule::Greeter), "no fd was passed");
        lr.time_out();
        assert_eq!(lr.phase, ReadyPhase::Signalled);
    }

    #[test]
    fn the_greeter_closes_its_fd_after_ready() {
        let (mut rd, fd, _) = adopted_pipe();
        let mut lr = LoginReady::default();
        lr.arm(Some(fd), ReadyRule::Greeter);
        assert!(lr.signal(ReadyRule::Greeter));
        assert!(
            lr.fd.is_none() && !lr.send_quit(),
            "no quit notice in greeter mode"
        );
        let mut got = String::new();
        // EOF while `lr` is alive: the fd closed right after `ready`.
        rd.read_to_string(&mut got).expect("read to EOF");
        assert_eq!(got, "ready\n");
    }

    #[test]
    fn quit_notice_then_pause_exits() {
        let mut q = QuitPhase::default();
        assert!(!q.on_pause(), "a pause without a quit is a VT switch");
        assert_eq!(q.request(), QuitRequest::Arm);
        assert!(!q.on_release(|| true));
        assert!(q.awaiting());
        assert!(q.on_pause());
        assert_eq!(q, QuitPhase::Exiting);
        assert!(!q.on_timeout(), "the later timer is a no-op");
    }

    #[test]
    fn quit_notice_then_timeout_exits() {
        let mut q = QuitPhase::default();
        assert_eq!(q.request(), QuitRequest::Arm);
        assert!(!q.on_release(|| true));
        assert!(q.on_timeout());
        assert!(!q.on_pause());
    }

    #[test]
    fn second_quit_exits_without_a_second_notice() {
        let mut q = QuitPhase::default();
        assert_eq!(q.request(), QuitRequest::Arm);
        assert!(!q.on_release(|| true));
        assert_eq!(q.request(), QuitRequest::Exit);
        assert!(!q.on_release(|| panic!("no second notice")));
    }

    #[test]
    fn undelivered_quit_exits_at_once() {
        let mut q = QuitPhase::default();
        assert_eq!(q.request(), QuitRequest::Arm);
        assert!(q.on_release(|| false));
        assert_eq!(q, QuitPhase::Exiting);
    }

    #[test]
    fn quit_work_runs_only_after_the_gate_releases() {
        let mut q = QuitPhase::default();
        assert_eq!(q.request(), QuitRequest::Arm);
        assert!(!q.awaiting(), "no notice before release");
        assert!(
            !q.on_pause() && !q.on_timeout(),
            "nothing waits while gated"
        );
        assert!(!q.on_release(|| true));
        assert!(q.awaiting(), "the notice went out on release");
        // Without a channel the loop stops, again only on release.
        let mut q = QuitPhase::default();
        assert_eq!(q.request(), QuitRequest::Arm);
        assert!(q.on_release(|| false));
        assert_eq!(q, QuitPhase::Exiting);
    }

    #[test]
    fn repeat_quit_before_release_is_a_no_op() {
        let mut q = QuitPhase::default();
        assert_eq!(q.request(), QuitRequest::Arm);
        assert_eq!(q.request(), QuitRequest::Pending, "no re-arm");
        assert_eq!(q.request(), QuitRequest::Pending);
        assert_eq!(q, QuitPhase::Gated, "no early exit");
        assert!(!q.on_release(|| true), "one notice, still waiting");
    }

    #[test]
    fn quit_after_release_exits_at_once() {
        let mut q = QuitPhase::default();
        q.request();
        q.on_release(|| true);
        assert_eq!(q.request(), QuitRequest::Exit);
        assert_eq!(q, QuitPhase::Exiting);
        assert!(!q.on_release(|| panic!("no second notice")));
    }
}
